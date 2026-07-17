//! Transactional new-surface dispatch through arcmux supervision.
//!
//! A raw cmux terminal is created first so every failure has a visible place
//! to report/recover. The worker then resolves that exact `surface:N` to its
//! stable UUID, creates one arcmux session, proves its exact catalog target,
//! durably binds the UUID to the same-device locator, attaches the terminal,
//! and finally sends the goal through arcmux. No title/cwd/recency inference is
//! used anywhere in the identity chain.

use crate::cmux::client::CmuxClient;
use crate::mc_data::arcmux_mesh::RemoteSessionLocator;
use crate::mc_data::surface_kind::SurfaceKind;
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const SURFACE_RESOLVE_ATTEMPTS: usize = 20;
const SURFACE_RESOLVE_DELAY: Duration = Duration::from_millis(100);
const SURFACE_RESOLVE_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_READY_ATTEMPTS: usize = 60;
const SESSION_READY_DELAY: Duration = Duration::from_millis(500);
const MAX_GOAL_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone)]
pub struct NewSurfaceDispatchRequest {
    pub workspace_uuid: String,
    pub workspace_ref: String,
    pub window_ref: String,
    pub cwd: PathBuf,
    pub goal_text: String,
    pub kind: SurfaceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSurfaceDispatchSuccess {
    pub surface_ref: String,
    pub surface_uuid: String,
    pub locator: RemoteSessionLocator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSurfaceDispatchFailure {
    pub surface_ref: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct NewSurfaceDispatchUpdate {
    pub workspace_uuid: String,
    pub goal_text: String,
    pub kind: SurfaceKind,
    pub result: Result<NewSurfaceDispatchSuccess, NewSurfaceDispatchFailure>,
}

#[derive(Debug, Clone)]
struct DispatchCommandRunner {
    arcmux_bin: PathBuf,
    arcmux_cli_bin: PathBuf,
    timeout: Duration,
    reader_drain_timeout: Duration,
}

impl Default for DispatchCommandRunner {
    fn default() -> Self {
        Self {
            arcmux_bin: std::env::var_os("MC_ARCMUX_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("arcmux")),
            arcmux_cli_bin: std::env::var_os("MC_ARCMUX_CLI_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("arcmux-cli")),
            timeout: COMMAND_TIMEOUT,
            reader_drain_timeout: READER_DRAIN_TIMEOUT,
        }
    }
}

impl DispatchCommandRunner {
    #[cfg(test)]
    fn new(arcmux_bin: PathBuf, arcmux_cli_bin: PathBuf, timeout: Duration) -> Self {
        Self {
            arcmux_bin,
            arcmux_cli_bin,
            timeout,
            reader_drain_timeout: timeout.min(READER_DRAIN_TIMEOUT),
        }
    }

    #[cfg(test)]
    fn new_with_reader_drain_timeout(
        arcmux_bin: PathBuf,
        arcmux_cli_bin: PathBuf,
        command_timeout: Duration,
        reader_drain_timeout: Duration,
    ) -> Self {
        Self {
            arcmux_bin,
            arcmux_cli_bin,
            timeout: command_timeout,
            reader_drain_timeout,
        }
    }

    async fn arcmux(&self, args: Vec<OsString>) -> Result<Vec<u8>, String> {
        self.run(&self.arcmux_bin, args, None).await
    }

    async fn cli(&self, args: Vec<OsString>, stdin: Option<Vec<u8>>) -> Result<Vec<u8>, String> {
        self.run(&self.arcmux_cli_bin, args, stdin).await
    }

    async fn run(
        &self,
        bin: &Path,
        args: Vec<OsString>,
        stdin: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, String> {
        let mut command = Command::new(bin);
        command
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|_| "arcmux dispatch command could not start".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "arcmux dispatch stdout unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "arcmux dispatch stderr unavailable".to_string())?;
        let mut stdout_task = BoundedReadTask::new(tokio::spawn(read_bounded(stdout)));
        let mut stderr_task = BoundedReadTask::new(tokio::spawn(read_bounded(stderr)));
        if let Some(input) = stdin {
            let mut child_stdin = child
                .stdin
                .take()
                .ok_or_else(|| "arcmux dispatch stdin unavailable".to_string())?;
            let input_result = timeout(self.timeout, async {
                child_stdin.write_all(&input).await?;
                child_stdin.shutdown().await
            })
            .await;
            let input_error = match input_result {
                Ok(Ok(())) => None,
                Ok(Err(_)) => Some("arcmux dispatch input failed"),
                Err(_) => Some("arcmux dispatch input timed out"),
            };
            if let Some(message) = input_error {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = collect_bounded_readers(
                    &mut stdout_task,
                    &mut stderr_task,
                    self.reader_drain_timeout,
                )
                .await;
                return Err(message.to_string());
            }
        }
        let status = match timeout(self.timeout, child.wait()).await {
            Ok(result) => result.map_err(|_| "arcmux dispatch command failed".to_string())?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = collect_bounded_readers(
                    &mut stdout_task,
                    &mut stderr_task,
                    self.reader_drain_timeout,
                )
                .await;
                return Err("arcmux dispatch command timed out".to_string());
            }
        };
        let ((stdout, stdout_overflow), (stderr, stderr_overflow)) = collect_bounded_readers(
            &mut stdout_task,
            &mut stderr_task,
            self.reader_drain_timeout,
        )
        .await?;
        if stdout_overflow || stderr_overflow {
            return Err("arcmux dispatch output exceeded the safety limit".to_string());
        }
        if !status.success() {
            let detail = bounded_message(&stderr);
            return Err(if detail.is_empty() {
                "arcmux dispatch command failed".to_string()
            } else {
                format!("arcmux dispatch failed: {detail}")
            });
        }
        Ok(stdout)
    }
}

#[derive(Debug, Deserialize)]
struct CreateResponse {
    session_id: String,
    created: bool,
    owner_id: String,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    sessions: Vec<ListSession>,
}

#[derive(Debug, Deserialize)]
struct ListSession {
    session_id: String,
    agent: String,
    cwd: String,
    owner_id: String,
    tmux_target: String,
}

#[derive(Debug, Deserialize)]
struct InfoResponse {
    device_id: String,
    tmux_socket: String,
}

#[derive(Debug, Deserialize)]
struct StoredBinding {
    schema_version: u32,
    local_device_id: String,
    surface_id: String,
    workspace_id: String,
    locator: RemoteSessionLocator,
}

#[derive(Debug, Deserialize)]
struct SendResponse {
    delivered: bool,
}

#[derive(Debug, Deserialize)]
struct ReadyResponse {
    ready: bool,
    state: String,
}

pub async fn run_new_surface_dispatch(
    request: NewSurfaceDispatchRequest,
    cmux: CmuxClient,
    updates: mpsc::Sender<NewSurfaceDispatchUpdate>,
) {
    run_new_surface_dispatch_with_runner(request, cmux, DispatchCommandRunner::default(), updates)
        .await;
}

async fn run_new_surface_dispatch_with_runner(
    request: NewSurfaceDispatchRequest,
    cmux: CmuxClient,
    runner: DispatchCommandRunner,
    updates: mpsc::Sender<NewSurfaceDispatchUpdate>,
) {
    let result = execute_dispatch(&request, &cmux, &runner).await;
    let _ = updates
        .send(NewSurfaceDispatchUpdate {
            workspace_uuid: request.workspace_uuid,
            goal_text: request.goal_text,
            kind: request.kind,
            result,
        })
        .await;
}

async fn execute_dispatch(
    request: &NewSurfaceDispatchRequest,
    cmux: &CmuxClient,
    runner: &DispatchCommandRunner,
) -> Result<NewSurfaceDispatchSuccess, NewSurfaceDispatchFailure> {
    if let Err(message) = validate_request(request) {
        return Err(NewSurfaceDispatchFailure {
            surface_ref: None,
            message,
        });
    }
    let agent = agent_name(request.kind).expect("request kind was validated");
    // F10: `new-surface` requires the workspace ref, not a surface UUID/ref.
    let surface_ref = cmux
        .new_surface(&request.workspace_ref, "terminal")
        .await
        .map_err(|_| NewSurfaceDispatchFailure {
            surface_ref: None,
            message: "not arcmux-supervised: cmux could not create the raw terminal".to_string(),
        })?;
    if !valid_ref(&surface_ref, "surface:") {
        return Err(NewSurfaceDispatchFailure {
            surface_ref: Some(surface_ref),
            message: "not arcmux-supervised: cmux returned an invalid new surface ref".to_string(),
        });
    }

    let mut session_id: Option<String> = None;
    let mut surface_uuid: Option<String> = None;
    // Once bind is invoked, even a timeout is ambiguous. Unbind the brand-new
    // surface during rollback so it never masquerades as supervised.
    let mut bind_invoked = false;
    let owner = format!(
        "mission-control:{}:{}",
        request.workspace_uuid.to_ascii_lowercase(),
        surface_ref
    );
    let short_uuid: String = request
        .workspace_uuid
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(12)
        .collect();
    let session_name = format!("mc-{agent}-{short_uuid}-{}", surface_ref.replace(':', "-"));

    let execution: Result<NewSurfaceDispatchSuccess, String> = async {
        let uuid = timeout(
            SURFACE_RESOLVE_TOTAL_TIMEOUT,
            resolve_new_surface_uuid(cmux, request, &surface_ref),
        )
        .await
        .map_err(|_| "exact cmux surface UUID resolution timed out".to_string())??;
        surface_uuid = Some(uuid.clone());

        let created = runner
            .cli(
                create_args(
                    request,
                    agent,
                    &session_name,
                    &owner,
                    &uuid,
                    cmux.socket_path(),
                ),
                None,
            )
            .await
            .map_err(|message| format!("{message}; reconcile owner {owner}"))?;
        let created: CreateResponse = serde_json::from_slice(&created).map_err(|_| {
            format!("arcmux create returned malformed JSON; reconcile owner {owner}")
        })?;
        if !valid_token(&created.session_id) || !created.created || created.owner_id != owner {
            return Err(format!(
                "arcmux create did not prove one new owned session; reconcile owner {owner}"
            ));
        }
        session_id = Some(created.session_id.clone());

        let listed = runner
            .cli(
                vec!["list".into(), "--owner".into(), owner.clone().into()],
                None,
            )
            .await?;
        let listed: ListResponse = serde_json::from_slice(&listed)
            .map_err(|_| "arcmux list returned malformed JSON".to_string())?;
        let exact: Vec<_> = listed
            .sessions
            .into_iter()
            .filter(|candidate| candidate.session_id == created.session_id)
            .collect();
        if exact.len() != 1 {
            return Err("arcmux catalog did not contain one exact created session".to_string());
        }
        let exact = &exact[0];
        if exact.agent != agent
            || exact.owner_id != owner
            || Path::new(&exact.cwd) != request.cwd
            || !valid_tmux_target(&exact.tmux_target)
        {
            return Err("arcmux catalog identity did not match the created session".to_string());
        }

        let info = runner.arcmux(vec!["info".into(), "--json".into()]).await?;
        let info: InfoResponse = serde_json::from_slice(&info)
            .map_err(|_| "arcmux info omitted the local device or tmux socket".to_string())?;
        if !valid_token(&info.device_id) || !valid_token(&info.tmux_socket) {
            return Err("arcmux info returned invalid local runtime identity".to_string());
        }

        bind_invoked = true;
        let binding = runner
            .arcmux(vec![
                "surface".into(),
                "bind".into(),
                info.device_id.clone().into(),
                "root".into(),
                created.session_id.clone().into(),
                "--surface".into(),
                uuid.clone().into(),
                "--workspace".into(),
                request.workspace_uuid.clone().into(),
            ])
            .await?;
        let binding: StoredBinding = serde_json::from_slice(&binding)
            .map_err(|_| "arcmux surface bind returned malformed JSON".to_string())?;
        let locator = RemoteSessionLocator {
            schema_version: 1,
            device_id: info.device_id.clone(),
            profile_scope: "root".to_string(),
            session_id: created.session_id.clone(),
            transport_binding_id: None,
        };
        if binding.schema_version != 1
            || binding.local_device_id != info.device_id
            || !binding.surface_id.eq_ignore_ascii_case(&uuid)
            || !binding
                .workspace_id
                .eq_ignore_ascii_case(&request.workspace_uuid)
            || binding.locator != locator
            || !binding.locator.valid()
        {
            return Err("arcmux surface bind returned a mismatched identity".to_string());
        }

        // Target values are restricted to token/%pane shapes before entering
        // this shell command. Goal text never passes through the terminal.
        let attach = format!(
            "tmux -L {} attach-session -t {}\r",
            info.tmux_socket, exact.tmux_target
        );
        // F10: cmux `send` requires both the workspace ref and surface ref.
        cmux.send_text(&request.workspace_ref, &surface_ref, &attach)
            .await
            .map_err(|_| "cmux could not attach the supervised arcmux session".to_string())?;

        wait_until_session_ready(runner, &session_name).await?;

        let sent = runner
            .cli(
                vec!["send".into(), created.session_id.clone().into()],
                Some(request.goal_text.as_bytes().to_vec()),
            )
            .await?;
        let sent: SendResponse = serde_json::from_slice(&sent)
            .map_err(|_| "arcmux send returned malformed JSON".to_string())?;
        if !sent.delivered {
            return Err("arcmux did not confirm goal delivery".to_string());
        }

        Ok(NewSurfaceDispatchSuccess {
            surface_ref: surface_ref.clone(),
            surface_uuid: uuid,
            locator,
        })
    }
    .await;

    match execution {
        Ok(success) => Ok(success),
        Err(mut message) => {
            let cleanup = rollback_dispatch(
                runner,
                surface_uuid.as_deref(),
                session_id.as_deref(),
                bind_invoked,
            )
            .await;
            if !cleanup.is_empty() {
                message.push_str("; cleanup uncertain: ");
                message.push_str(&cleanup.join(", "));
            }
            Err(NewSurfaceDispatchFailure {
                surface_ref: Some(surface_ref),
                message: format!("not arcmux-supervised: {message}"),
            })
        }
    }
}

async fn wait_until_session_ready(
    runner: &DispatchCommandRunner,
    session_name: &str,
) -> Result<(), String> {
    for attempt in 0..SESSION_READY_ATTEMPTS {
        let ready = runner
            .cli(
                vec!["ready".into(), "--session".into(), session_name.into()],
                None,
            )
            .await?;
        let ready: ReadyResponse = serde_json::from_slice(&ready)
            .map_err(|_| "arcmux ready returned malformed JSON".to_string())?;
        if ready.ready && ready.state == "idle" {
            return Ok(());
        }
        if matches!(ready.state.as_str(), "failed" | "stuck" | "exited") {
            return Err(format!(
                "arcmux session became {} before goal delivery",
                ready.state
            ));
        }
        if attempt + 1 < SESSION_READY_ATTEMPTS {
            sleep(SESSION_READY_DELAY).await;
        }
    }
    Err("arcmux session did not become ready for goal delivery".to_string())
}

async fn resolve_new_surface_uuid(
    cmux: &CmuxClient,
    request: &NewSurfaceDispatchRequest,
    surface_ref: &str,
) -> Result<String, String> {
    let mut last = None;
    for attempt in 0..SURFACE_RESOLVE_ATTEMPTS {
        match cmux
            .exact_surface_uuid(&request.window_ref, &request.workspace_ref, surface_ref)
            .await
        {
            Ok(uuid) if valid_uuid(&uuid) => return Ok(uuid),
            Ok(_) => last = Some("cmux returned an invalid stable surface UUID".to_string()),
            Err(error) => last = Some(error.to_string()),
        }
        if attempt + 1 < SURFACE_RESOLVE_ATTEMPTS {
            sleep(SURFACE_RESOLVE_DELAY).await;
        }
    }
    Err(last.unwrap_or_else(|| "exact cmux surface UUID is unavailable".to_string()))
}

async fn rollback_dispatch(
    runner: &DispatchCommandRunner,
    surface_uuid: Option<&str>,
    session_id: Option<&str>,
    bind_invoked: bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    let unbind_result = match (bind_invoked, surface_uuid) {
        (true, Some(surface_uuid)) => Some(
            runner
                .arcmux(vec![
                    "surface".into(),
                    "unbind".into(),
                    "--surface".into(),
                    surface_uuid.into(),
                ])
                .await,
        ),
        _ => None,
    };
    if matches!(unbind_result, Some(Err(_))) {
        errors.push("surface unbind failed".to_string());
    }
    let kill_result = match session_id {
        Some(session_id) => Some(
            runner
                .cli(vec!["kill".into(), session_id.into()], None)
                .await,
        ),
        None => None,
    };
    if matches!(kill_result, Some(Err(_))) {
        errors.push("session kill failed".to_string());
    }
    errors
}

fn validate_request(request: &NewSurfaceDispatchRequest) -> Result<(), String> {
    if !valid_uuid(&request.workspace_uuid)
        || !valid_ref(&request.workspace_ref, "workspace:")
        || !valid_ref(&request.window_ref, "window:")
        || !request.cwd.is_absolute()
        || request.cwd.as_os_str().is_empty()
        || request.goal_text.trim().is_empty()
        || request.goal_text.len() > MAX_GOAL_BYTES
        || request.goal_text.contains(['\0', '\r'])
        || agent_name(request.kind).is_none()
    {
        return Err("new agent dispatch context is incomplete or invalid".to_string());
    }
    Ok(())
}

fn create_args(
    request: &NewSurfaceDispatchRequest,
    agent: &str,
    session_name: &str,
    owner: &str,
    surface_uuid: &str,
    cmux_socket_path: &Path,
) -> Vec<OsString> {
    // cmux's hooks key their registry by these exact IDs. The documented
    // CMUX_SOCKET_PATH connection override is also forwarded so a configured
    // non-default cmux instance receives the hook events; no other cmux env is
    // inferred. Values remain individual argv entries and never enter a shell.
    vec![
        "create".into(),
        "--agent".into(),
        agent.into(),
        "--name".into(),
        session_name.into(),
        "--cwd".into(),
        request.cwd.as_os_str().to_owned(),
        "--owner".into(),
        owner.into(),
        "--env".into(),
        env_assignment("CMUX_SURFACE_ID", OsStr::new(surface_uuid)),
        "--env".into(),
        env_assignment("CMUX_WORKSPACE_ID", OsStr::new(&request.workspace_uuid)),
        "--env".into(),
        env_assignment("CMUX_SOCKET_PATH", cmux_socket_path.as_os_str()),
    ]
}

fn env_assignment(key: &str, value: &OsStr) -> OsString {
    let mut assignment = OsString::from(key);
    assignment.push("=");
    assignment.push(value);
    assignment
}

fn agent_name(kind: SurfaceKind) -> Option<&'static str> {
    match kind {
        SurfaceKind::Claude => Some("claude"),
        SurfaceKind::Codex => Some("codex"),
        _ => None,
    }
}

fn valid_ref(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

fn valid_token(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_tmux_target(value: &str) -> bool {
    value
        .strip_prefix('%')
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

fn valid_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].into_iter().all(|idx| bytes[idx] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, byte)| [8, 13, 18, 23].contains(&idx) || byte.is_ascii_hexdigit())
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin) -> Result<(Vec<u8>, bool), String> {
    let mut retained = Vec::new();
    let mut overflow = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|_| "arcmux dispatch output read failed".to_string())?;
        if count == 0 {
            break;
        }
        let remaining = MAX_COMMAND_OUTPUT_BYTES.saturating_sub(retained.len());
        let keep = remaining.min(count);
        retained.extend_from_slice(&chunk[..keep]);
        overflow |= keep < count;
    }
    Ok((retained, overflow))
}

/// Owns a pipe reader so cancelling the command future cannot detach the task.
struct BoundedReadTask(JoinHandle<Result<(Vec<u8>, bool), String>>);

impl BoundedReadTask {
    fn new(task: JoinHandle<Result<(Vec<u8>, bool), String>>) -> Self {
        Self(task)
    }

    fn abort(&self) {
        self.0.abort();
    }

    fn handle_mut(&mut self) -> &mut JoinHandle<Result<(Vec<u8>, bool), String>> {
        &mut self.0
    }
}

impl Drop for BoundedReadTask {
    fn drop(&mut self) {
        self.abort();
    }
}

async fn collect_bounded_readers(
    stdout_task: &mut BoundedReadTask,
    stderr_task: &mut BoundedReadTask,
    drain_timeout: Duration,
) -> Result<((Vec<u8>, bool), (Vec<u8>, bool)), String> {
    let joined = timeout(drain_timeout, async {
        let stdout = stdout_task
            .handle_mut()
            .await
            .map_err(|_| "arcmux dispatch stdout reader failed".to_string())??;
        let stderr = stderr_task
            .handle_mut()
            .await
            .map_err(|_| "arcmux dispatch stderr reader failed".to_string())??;
        Ok::<_, String>((stdout, stderr))
    })
    .await;
    match joined {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            stdout_task.abort();
            stderr_task.abort();
            Err(error)
        }
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            Err("arcmux dispatch output pipe drain timed out".to_string())
        }
    }
}

fn bounded_message(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn request(cwd: PathBuf) -> NewSurfaceDispatchRequest {
        NewSurfaceDispatchRequest {
            workspace_uuid: "22222222-2222-4222-8222-222222222222".into(),
            workspace_ref: "workspace:9".into(),
            window_ref: "window:3".into(),
            cwd,
            goal_text: "Implement the exact supervised dispatch".into(),
            kind: SurfaceKind::Codex,
        }
    }

    fn bounded_cmux(path: &Path, socket_path: PathBuf) -> CmuxClient {
        CmuxClient::new_with_transaction_limits(
            path.to_string_lossy().into_owned(),
            socket_path,
            Duration::from_millis(500),
            Duration::from_millis(20),
            4096,
        )
    }

    fn cmux_script(log: &Path, surface_ref: &str) -> String {
        format!(
            r#"#!/bin/sh
printf 'cmux:%s\n' "$*" >> '{}'
case "$1" in
  new-surface) printf '%s\n' 'OK {} pane:7 workspace:9' ;;
  tree) printf '%s\n' '{{"windows":[{{"ref":"window:3","workspaces":[{{"ref":"workspace:9","panes":[{{"surfaces":[{{"ref":"{}","id":"11111111-1111-4111-8111-111111111111","title":"misleading newest codex"}}]}}]}}]}}]}}' ;;
  send) exit 0 ;;
  *) exit 2 ;;
esac
"#,
            log.display(),
            surface_ref,
            surface_ref
        )
    }

    fn success_cli_script(log: &Path, goal: &Path, cwd: &Path) -> String {
        format!(
            r#"#!/bin/sh
printf 'cli:%s\n' "$*" >> '{}'
case "$1" in
  create) printf '%s\n' '{{"session_id":"s-created","state":"starting","created":true,"owner_id":"mission-control:22222222-2222-4222-8222-222222222222:surface:42"}}' ;;
  list) printf '%s\n' '{{"sessions":[{{"session_id":"s-created","agent":"codex","cwd":"{}","owner_id":"mission-control:22222222-2222-4222-8222-222222222222:surface:42","tmux_target":"%77"}}],"count":1}}' ;;
  ready) printf '%s\n' '{{"ready":true,"reason":"ready:idle","state":"idle","session":"mc-codex-222222222222-surface-42"}}' ;;
  send) cat > '{}'; printf '%s\n' '{{"delivered":true,"state":"working"}}' ;;
  kill) printf '%s\n' '{{"killed":true,"final_state":"exited"}}' ;;
  *) exit 2 ;;
esac
"#,
            log.display(),
            cwd.display(),
            goal.display()
        )
    }

    fn arcmux_script(log: &Path, mismatch: bool) -> String {
        let session = if mismatch { "s-other" } else { "s-created" };
        format!(
            r#"#!/bin/sh
printf 'arcmux:%s\n' "$*" >> '{}'
if [ "$1" = "info" ]; then
  printf '%s\n' '{{"device_id":"ref","tmux_socket":"arcmux"}}'
elif [ "$1" = "surface" ] && [ "$2" = "bind" ]; then
  printf '%s\n' '{{"schema_version":1,"local_device_id":"ref","surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","locator":{{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"{}"}}}}'
elif [ "$1" = "surface" ] && [ "$2" = "unbind" ]; then
  printf '%s\n' '{{"removed":true}}'
else
  exit 2
fi
"#,
            log.display(),
            session
        )
    }

    #[tokio::test]
    async fn exact_create_bind_attach_send_order_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("calls");
        let goal = temp.path().join("goal");
        let injected = temp.path().join("must-not-exist");
        let user_goal = format!(
            "Keep this literal: $(touch '{}'); touch '{}'\nsecond line",
            injected.display(),
            injected.display()
        );
        let cmux_bin = temp.path().join("cmux");
        let cli_bin = temp.path().join("arcmux-cli");
        let arcmux_bin = temp.path().join("arcmux");
        executable(&cmux_bin, &cmux_script(&log, "surface:42"));
        executable(&cli_bin, &success_cli_script(&log, &goal, temp.path()));
        executable(&arcmux_bin, &arcmux_script(&log, false));
        let cmux = CmuxClient::new(
            cmux_bin.to_string_lossy().into_owned(),
            temp.path().join("cmux.sock"),
        );
        let (tx, mut rx) = mpsc::channel(1);
        let mut dispatch_request = request(temp.path().to_path_buf());
        dispatch_request.goal_text = user_goal.clone();
        run_new_surface_dispatch_with_runner(
            dispatch_request,
            cmux,
            DispatchCommandRunner::new(arcmux_bin, cli_bin, Duration::from_secs(2)),
            tx,
        )
        .await;

        let update = rx.recv().await.unwrap();
        let success = update.result.unwrap();
        assert_eq!(success.surface_ref, "surface:42");
        assert_eq!(success.locator.session_id, "s-created");
        assert_eq!(std::fs::read_to_string(goal).unwrap(), user_goal);
        assert_eq!(update.goal_text, user_goal);
        assert!(
            !injected.exists(),
            "goal text must never be shell-evaluated"
        );
        let calls = std::fs::read_to_string(log).unwrap();
        let lines: Vec<_> = calls.lines().collect();
        assert!(lines[0].starts_with("cmux:new-surface --type terminal --workspace workspace:9"));
        assert!(lines[1].starts_with("cmux:tree --all --json --id-format both"));
        assert_eq!(
            lines[2],
            format!(
                "cli:create --agent codex --name mc-codex-222222222222-surface-42 --cwd {} --owner mission-control:22222222-2222-4222-8222-222222222222:surface:42 --env CMUX_SURFACE_ID=11111111-1111-4111-8111-111111111111 --env CMUX_WORKSPACE_ID=22222222-2222-4222-8222-222222222222 --env CMUX_SOCKET_PATH={}",
                temp.path().display(),
                temp.path().join("cmux.sock").display()
            )
        );
        assert!(lines[3].starts_with("cli:list --owner mission-control:"));
        assert_eq!(lines[4], "arcmux:info --json");
        assert!(lines[5].contains("surface bind ref root s-created"));
        assert!(lines[6].contains("cmux:send --workspace workspace:9 --surface surface:42 tmux -L arcmux attach-session -t %77"));
        assert!(lines[7].starts_with("cli:ready --session mc-codex-"));
        assert_eq!(lines[8], "cli:send s-created");
    }

    #[tokio::test]
    async fn mismatched_bind_rolls_back_before_kill_and_never_attaches() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("calls");
        let cmux_bin = temp.path().join("cmux");
        let cli_bin = temp.path().join("arcmux-cli");
        let arcmux_bin = temp.path().join("arcmux");
        executable(&cmux_bin, &cmux_script(&log, "surface:42"));
        executable(
            &cli_bin,
            &success_cli_script(&log, &temp.path().join("goal"), temp.path()),
        );
        executable(&arcmux_bin, &arcmux_script(&log, true));
        let cmux = CmuxClient::new(
            cmux_bin.to_string_lossy().into_owned(),
            temp.path().join("cmux.sock"),
        );
        let (tx, mut rx) = mpsc::channel(1);
        run_new_surface_dispatch_with_runner(
            request(temp.path().to_path_buf()),
            cmux,
            DispatchCommandRunner::new(arcmux_bin, cli_bin, Duration::from_secs(2)),
            tx,
        )
        .await;
        let failure = rx.recv().await.unwrap().result.unwrap_err();
        assert_eq!(failure.surface_ref.as_deref(), Some("surface:42"));
        assert!(failure.message.starts_with("not arcmux-supervised:"));
        let calls = std::fs::read_to_string(log).unwrap();
        let lines: Vec<_> = calls.lines().collect();
        let unbind = lines
            .iter()
            .position(|line| line.contains("surface unbind"))
            .unwrap();
        let kill = lines
            .iter()
            .position(|line| line == &"cli:kill s-created")
            .unwrap();
        assert!(unbind < kill);
        assert!(!lines.iter().any(|line| line.starts_with("cmux:send")));
    }

    #[tokio::test]
    async fn command_timeout_leaves_visible_raw_surface_with_reconciliation_owner() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("calls");
        let cmux_bin = temp.path().join("cmux");
        let cli_bin = temp.path().join("arcmux-cli");
        let arcmux_bin = temp.path().join("arcmux");
        executable(&cmux_bin, &cmux_script(&log, "surface:42"));
        executable(&cli_bin, "#!/bin/sh\nsleep 1\n");
        executable(&arcmux_bin, &arcmux_script(&log, false));
        let cmux = CmuxClient::new(
            cmux_bin.to_string_lossy().into_owned(),
            temp.path().join("cmux.sock"),
        );
        let (tx, mut rx) = mpsc::channel(1);
        run_new_surface_dispatch_with_runner(
            request(temp.path().to_path_buf()),
            cmux,
            DispatchCommandRunner::new(arcmux_bin, cli_bin, Duration::from_millis(20)),
            tx,
        )
        .await;
        let failure = rx.recv().await.unwrap().result.unwrap_err();
        assert_eq!(failure.surface_ref.as_deref(), Some("surface:42"));
        assert!(failure.message.contains("timed out"));
        assert!(failure.message.contains("reconcile owner mission-control:"));
    }

    #[tokio::test]
    async fn stalled_cmux_new_surface_returns_visible_unsupervised_failure() {
        let temp = tempfile::tempdir().unwrap();
        let cmux_bin = temp.path().join("cmux");
        let arcmux_bin = temp.path().join("arcmux");
        let cli_bin = temp.path().join("arcmux-cli");
        executable(&cmux_bin, "#!/bin/sh\nsleep 1\n");
        executable(&arcmux_bin, "#!/bin/sh\nexit 99\n");
        executable(&cli_bin, "#!/bin/sh\nexit 99\n");
        let (tx, mut rx) = mpsc::channel(1);

        run_new_surface_dispatch_with_runner(
            request(temp.path().to_path_buf()),
            bounded_cmux(&cmux_bin, temp.path().join("cmux.sock")),
            DispatchCommandRunner::new(arcmux_bin, cli_bin, Duration::from_secs(1)),
            tx,
        )
        .await;

        let failure = rx.recv().await.unwrap().result.unwrap_err();
        assert!(failure.surface_ref.is_none());
        assert!(failure.message.starts_with("not arcmux-supervised:"));
    }

    #[tokio::test]
    async fn stalled_cmux_tree_leaves_created_surface_visibly_unsupervised() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("calls");
        let cmux_bin = temp.path().join("cmux");
        let arcmux_bin = temp.path().join("arcmux");
        let cli_bin = temp.path().join("arcmux-cli");
        executable(
            &cmux_bin,
            &format!(
                "#!/bin/sh\nprintf 'cmux:%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\n  new-surface) printf 'OK surface:42 pane:7 workspace:9\\n' ;;\n  tree) sleep 1 ;;\n  *) exit 2 ;;\nesac\n",
                log.display()
            ),
        );
        executable(&arcmux_bin, "#!/bin/sh\nexit 99\n");
        executable(
            &cli_bin,
            &format!(
                "#!/bin/sh\nprintf 'cli:%s\\n' \"$*\" >> '{}'\nexit 99\n",
                log.display()
            ),
        );
        let (tx, mut rx) = mpsc::channel(1);

        run_new_surface_dispatch_with_runner(
            request(temp.path().to_path_buf()),
            bounded_cmux(&cmux_bin, temp.path().join("cmux.sock")),
            DispatchCommandRunner::new(arcmux_bin, cli_bin, Duration::from_secs(1)),
            tx,
        )
        .await;

        let failure = rx.recv().await.unwrap().result.unwrap_err();
        assert_eq!(failure.surface_ref.as_deref(), Some("surface:42"));
        assert!(failure.message.starts_with("not arcmux-supervised:"));
        assert!(!std::fs::read_to_string(log).unwrap().contains("cli:create"));
    }

    #[tokio::test]
    async fn outer_surface_timeout_aborts_readers_held_by_stalled_cmux_descendant() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("pipe-state");
        let cmux_bin = temp.path().join("cmux");
        executable(
            &cmux_bin,
            &format!(
                r#"#!/usr/bin/env python3
import os, time
if os.fork() == 0:
    time.sleep(1)
    try:
        os.write(1, b'late output')
        state = 'reader-open'
    except BrokenPipeError:
        state = 'reader-closed'
    with open('{}', 'w') as output:
        output.write(state)
    os._exit(0)
time.sleep(5)
"#,
                state.display()
            ),
        );
        let cmux = CmuxClient::new_with_transaction_limits(
            cmux_bin.to_string_lossy().into_owned(),
            temp.path().join("cmux.sock"),
            Duration::from_secs(5),
            Duration::from_secs(1),
            4096,
        );
        let request = request(temp.path().to_path_buf());
        let started = std::time::Instant::now();

        let resolution = timeout(
            Duration::from_millis(500),
            resolve_new_surface_uuid(&cmux, &request, "surface:42"),
        )
        .await;

        assert!(resolution.is_err(), "the outer resolver timeout must win");
        assert!(started.elapsed() < Duration::from_secs(2));
        for _ in 0..30 {
            if state.exists() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            std::fs::read_to_string(state).unwrap(),
            "reader-closed",
            "outer cancellation must abort the detached stdout reader"
        );
    }

    #[tokio::test]
    async fn stalled_cmux_attach_rolls_back_exact_binding_and_session() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("calls");
        let cmux_bin = temp.path().join("cmux");
        let cli_bin = temp.path().join("arcmux-cli");
        let arcmux_bin = temp.path().join("arcmux");
        let stalled_send =
            cmux_script(&log, "surface:42").replace("send) exit 0 ;;", "send) sleep 1 ;;");
        executable(&cmux_bin, &stalled_send);
        executable(
            &cli_bin,
            &success_cli_script(&log, &temp.path().join("goal"), temp.path()),
        );
        executable(&arcmux_bin, &arcmux_script(&log, false));
        let (tx, mut rx) = mpsc::channel(1);

        run_new_surface_dispatch_with_runner(
            request(temp.path().to_path_buf()),
            bounded_cmux(&cmux_bin, temp.path().join("cmux.sock")),
            DispatchCommandRunner::new(arcmux_bin, cli_bin, Duration::from_secs(1)),
            tx,
        )
        .await;

        let failure = rx.recv().await.unwrap().result.unwrap_err();
        assert_eq!(failure.surface_ref.as_deref(), Some("surface:42"));
        assert!(failure.message.starts_with("not arcmux-supervised:"));
        let calls = std::fs::read_to_string(log).unwrap();
        let lines: Vec<_> = calls.lines().collect();
        let unbind = lines
            .iter()
            .position(|line| line.contains("surface unbind"))
            .unwrap();
        let kill = lines
            .iter()
            .position(|line| line == &"cli:kill s-created")
            .unwrap();
        assert!(unbind < kill);
    }

    #[tokio::test]
    async fn misleading_title_never_substitutes_for_exact_surface_ref() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("calls");
        let cmux_bin = temp.path().join("cmux");
        let cli_bin = temp.path().join("arcmux-cli");
        let arcmux_bin = temp.path().join("arcmux");
        executable(&cmux_bin, &cmux_script(&log, "surface:other"));
        executable(
            &cli_bin,
            &success_cli_script(&log, &temp.path().join("goal"), temp.path()),
        );
        executable(&arcmux_bin, &arcmux_script(&log, false));
        let cmux = CmuxClient::new(
            cmux_bin.to_string_lossy().into_owned(),
            temp.path().join("cmux.sock"),
        );
        let (tx, mut rx) = mpsc::channel(1);
        // new-surface returns surface:other too in this helper, so rewrite the
        // tree result to a different ref with a tempting title.
        let body = cmux_script(&log, "surface:42")
            .replace("\"ref\":\"surface:42\"", "\"ref\":\"surface:other\"");
        executable(temp.path().join("cmux").as_path(), &body);
        run_new_surface_dispatch_with_runner(
            request(temp.path().to_path_buf()),
            cmux,
            DispatchCommandRunner::new(arcmux_bin, cli_bin, Duration::from_secs(2)),
            tx,
        )
        .await;
        let failure = rx.recv().await.unwrap().result.unwrap_err();
        assert_eq!(failure.surface_ref.as_deref(), Some("surface:42"));
        assert!(failure.message.contains("exact cmux surface"));
        assert!(
            !std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .any(|line| line.starts_with("cli:create"))
        );
    }

    #[tokio::test]
    async fn malformed_new_surface_ref_never_reaches_arcmux() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("calls");
        let cmux_bin = temp.path().join("cmux");
        let cli_bin = temp.path().join("arcmux-cli");
        let arcmux_bin = temp.path().join("arcmux");
        executable(&cmux_bin, &cmux_script(&log, "surface:not-a-number"));
        executable(
            &cli_bin,
            &success_cli_script(&log, &temp.path().join("goal"), temp.path()),
        );
        executable(&arcmux_bin, &arcmux_script(&log, false));
        let cmux = CmuxClient::new(
            cmux_bin.to_string_lossy().into_owned(),
            temp.path().join("cmux.sock"),
        );
        let (tx, mut rx) = mpsc::channel(1);
        run_new_surface_dispatch_with_runner(
            request(temp.path().to_path_buf()),
            cmux,
            DispatchCommandRunner::new(arcmux_bin, cli_bin, Duration::from_secs(2)),
            tx,
        )
        .await;

        let failure = rx.recv().await.unwrap().result.unwrap_err();
        assert_eq!(failure.surface_ref.as_deref(), Some("surface:not-a-number"));
        assert!(failure.message.contains("invalid new surface ref"));
        assert!(!std::fs::read_to_string(log).unwrap().contains("cli:create"));
    }

    #[tokio::test]
    async fn oversized_output_is_rejected_without_parsing_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("huge");
        executable(
            &bin,
            "#!/bin/sh\ni=0; while [ $i -lt 270000 ]; do printf x; i=$((i+1)); done\n",
        );
        let runner = DispatchCommandRunner::new(bin.clone(), bin, Duration::from_secs(3));
        assert!(
            runner
                .arcmux(vec!["info".into(), "--json".into()])
                .await
                .unwrap_err()
                .contains("safety limit")
        );
    }

    #[tokio::test]
    async fn descendant_retained_pipe_cannot_hang_dispatch_runner() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("retained-pipe");
        executable(
            &bin,
            "#!/usr/bin/env python3\nimport os, time\nif os.fork() == 0:\n    time.sleep(5)\n    os._exit(0)\nprint('{\"device_id\":\"ref\",\"tmux_socket\":\"arcmux\"}', flush=True)\nos._exit(0)\n",
        );
        let runner = DispatchCommandRunner::new_with_reader_drain_timeout(
            bin.clone(),
            bin,
            Duration::from_secs(2),
            Duration::from_millis(20),
        );
        let started = std::time::Instant::now();
        assert_eq!(
            runner
                .arcmux(vec!["info".into(), "--json".into()])
                .await
                .unwrap_err(),
            "arcmux dispatch output pipe drain timed out"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn blocked_stdin_is_bounded_by_command_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("blocked");
        executable(&bin, "#!/bin/sh\nsleep 1\n");
        let runner = DispatchCommandRunner::new(bin.clone(), bin, Duration::from_millis(20));
        assert_eq!(
            runner
                .cli(
                    vec!["send".into(), "s-created".into()],
                    Some(vec![b'x'; 1_000_000])
                )
                .await
                .unwrap_err(),
            "arcmux dispatch input timed out"
        );
    }

    #[test]
    fn only_interactive_claude_and_codex_requests_are_valid() {
        let mut request = request(PathBuf::from("/tmp"));
        assert!(validate_request(&request).is_ok());
        for invalid in [
            SurfaceKind::Shell,
            SurfaceKind::Remote,
            SurfaceKind::Unknown,
        ] {
            request.kind = invalid;
            assert!(validate_request(&request).is_err());
        }
    }

    #[test]
    fn create_env_is_exact_argv_and_excludes_goal_text() {
        let mut request = request(PathBuf::from("/tmp/project;$(touch pwned)"));
        request.goal_text = "$(touch should-never-be-an-argument)".to_string();
        let args = create_args(
            &request,
            "codex",
            "mc-codex-session",
            "mission-control:owner",
            "11111111-1111-4111-8111-111111111111",
            Path::new("/tmp/cmux socket;literal.sock"),
        );

        assert_eq!(
            args,
            vec![
                OsString::from("create"),
                OsString::from("--agent"),
                OsString::from("codex"),
                OsString::from("--name"),
                OsString::from("mc-codex-session"),
                OsString::from("--cwd"),
                OsString::from("/tmp/project;$(touch pwned)"),
                OsString::from("--owner"),
                OsString::from("mission-control:owner"),
                OsString::from("--env"),
                OsString::from("CMUX_SURFACE_ID=11111111-1111-4111-8111-111111111111"),
                OsString::from("--env"),
                OsString::from("CMUX_WORKSPACE_ID=22222222-2222-4222-8222-222222222222"),
                OsString::from("--env"),
                OsString::from("CMUX_SOCKET_PATH=/tmp/cmux socket;literal.sock"),
            ]
        );
        assert!(!args.iter().any(|arg| arg == OsStr::new(&request.goal_text)));
    }

    #[test]
    fn missing_device_id_is_not_inferred_from_other_runtime_fields() {
        let malformed = serde_json::from_slice::<InfoResponse>(br#"{"tmux_socket":"arcmux"}"#);
        assert!(malformed.is_err());
    }

    #[test]
    fn arcmux_info_fixture_matches_authoritative_de8249a_shape() {
        let info: InfoResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/arcmux_info/de8249a.json"
        ))
        .unwrap();
        assert_eq!(info.device_id, "ref");
        assert_eq!(info.tmux_socket, "arcmux");
    }
}
