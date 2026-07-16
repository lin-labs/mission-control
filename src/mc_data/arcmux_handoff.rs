//! Verified arcmux handoff command adapter.
//!
//! The TUI owns selection and presentation; this module owns the external CLI
//! contract and its safety ordering. Every command is argv-only, output is
//! drained with a strict retained-size bound, and the source is retired only
//! after the target reports both acceptance and context-loaded verification.

use crate::mc_data::arcmux_mesh::RemoteSessionLocator;
use serde::Deserialize;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;

const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
const HANDOFF_WAIT: &str = "90s";
const PREPARE_TIMEOUT: Duration = Duration::from_secs(100);
const SIMPLE_TIMEOUT: Duration = Duration::from_secs(10);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(100);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(100);
const RETIRE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffSourceContext {
    pub workspace_uuid: String,
    pub surface_uuid: String,
    pub locator: RemoteSessionLocator,
    pub agent: String,
    pub project: String,
    pub goal: String,
    pub history: String,
    pub conversation_id: Option<String>,
    pub parent_handoff_id: Option<String>,
    pub validation: String,
    pub observation: HandoffSourceObservation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffSourceObservation {
    pub turn_count: u64,
    pub updated_at: String,
    pub last_turn_end_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffTarget {
    pub peer_id: String,
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffPlan {
    pub source: HandoffSourceContext,
    pub target: HandoffTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffOperation {
    pub generation: u64,
    pub plan: HandoffPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffStage {
    Preparing,
    CheckingIdentity,
    Launching,
    VerifyingContext,
    RetiringSource,
}

impl HandoffStage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Preparing => "preparing repository and history",
            Self::CheckingIdentity => "verifying exact source and target",
            Self::Launching => "launching target agent",
            Self::VerifyingContext => "waiting for target context",
            Self::RetiringSource => "retiring exact source session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HandoffLocator {
    pub device_id: String,
    pub profile_scope: String,
    pub session_id: String,
}

impl HandoffLocator {
    pub fn display(&self) -> String {
        format!(
            "{}/{}/{}",
            self.device_id, self.profile_scope, self.session_id
        )
    }

    fn valid(&self) -> bool {
        valid_token(&self.device_id)
            && valid_profile_scope(&self.profile_scope)
            && valid_token(&self.session_id)
    }

    fn matches_source(&self, source: &RemoteSessionLocator) -> bool {
        self.device_id == source.device_id
            && self.profile_scope == source.profile_scope
            && self.session_id == source.session_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffSuccess {
    pub handoff_id: String,
    pub source_locator: HandoffLocator,
    pub target_locator: HandoffLocator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffFailure {
    pub message: String,
    pub handoff_id: Option<String>,
    pub source_locator: HandoffLocator,
    pub target_locator: Option<HandoffLocator>,
    pub target_uncertain: bool,
    pub duplicate_live: bool,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffUpdateKind {
    Progress(HandoffStage),
    Finished(Result<HandoffSuccess, HandoffFailure>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffUpdate {
    pub workspace_uuid: String,
    pub generation: u64,
    pub kind: HandoffUpdateKind,
}

#[derive(Debug, Clone)]
pub struct HandoffCommandRunner {
    bin: PathBuf,
    mux_state_dir: PathBuf,
}

impl Default for HandoffCommandRunner {
    fn default() -> Self {
        let bin = std::env::var_os("MC_ARCMUX_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("arcmux"));
        Self {
            bin,
            mux_state_dir: crate::mc_data::mux_state::session_state_dir(),
        }
    }
}

impl HandoffCommandRunner {
    #[cfg(test)]
    fn new(bin: impl Into<PathBuf>) -> Self {
        let bin = bin.into();
        let mux_state_dir = bin.parent().unwrap_or_else(|| std::path::Path::new("."));
        Self {
            bin: bin.clone(),
            mux_state_dir: mux_state_dir.to_path_buf(),
        }
    }

    async fn run(&self, spec: CommandSpec) -> Result<Vec<u8>, String> {
        let mut command = Command::new(&self.bin);
        command
            .args(&spec.args)
            .stdin(if spec.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|_| "arcmux handoff command could not start".to_string())?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "arcmux stdout unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "arcmux stderr unavailable".to_string())?;
        let stdout_task = tokio::spawn(read_bounded(stdout));
        let stderr_task = tokio::spawn(read_bounded(stderr));

        if let Some(input) = spec.stdin {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| "arcmux stdin unavailable".to_string())?;
            stdin
                .write_all(&input)
                .await
                .map_err(|_| "arcmux handoff input failed".to_string())?;
            stdin
                .shutdown()
                .await
                .map_err(|_| "arcmux handoff input failed".to_string())?;
        }

        let status = match timeout(spec.timeout, child.wait()).await {
            Ok(result) => result.map_err(|_| "arcmux handoff command failed".to_string())?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err("arcmux handoff command timed out".to_string());
            }
        };
        let (stdout, stdout_overflow) = stdout_task
            .await
            .map_err(|_| "arcmux stdout reader failed".to_string())??;
        let (stderr, stderr_overflow) = stderr_task
            .await
            .map_err(|_| "arcmux stderr reader failed".to_string())??;
        if stdout_overflow || stderr_overflow {
            return Err("arcmux handoff output exceeded the safety limit".to_string());
        }
        if !status.success() {
            let detail = bounded_message(&stderr);
            return Err(if detail.is_empty() {
                "arcmux handoff command failed".to_string()
            } else {
                format!("arcmux handoff failed: {detail}")
            });
        }
        Ok(stdout)
    }
}

#[derive(Debug)]
struct CommandSpec {
    args: Vec<OsString>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
}

fn prepare_spec(plan: &HandoffPlan) -> CommandSpec {
    let mut args = vec![
        "handoff".into(),
        "prepare".into(),
        plan.target.peer_id.clone().into(),
        plan.source.locator.profile_scope.clone().into(),
        plan.source.locator.session_id.clone().into(),
        "--project".into(),
        plan.source.project.clone().into(),
        "--agent".into(),
        plan.target.profile.clone().into(),
        "--goal-file".into(),
        "-".into(),
        "--history".into(),
        plan.source.history.clone().into(),
        "--validation".into(),
        plan.source.validation.clone().into(),
        "--wait".into(),
        HANDOFF_WAIT.into(),
    ];
    if let Some(conversation) = plan.source.conversation_id.as_deref() {
        args.extend(["--conversation".into(), conversation.into()]);
    }
    if let Some(parent) = plan.source.parent_handoff_id.as_deref() {
        args.extend(["--parent".into(), parent.into()]);
    }
    CommandSpec {
        args,
        stdin: Some(plan.source.goal.as_bytes().to_vec()),
        timeout: PREPARE_TIMEOUT,
    }
}

fn show_spec(handoff_id: &str) -> CommandSpec {
    simple_spec("show", handoff_id, SIMPLE_TIMEOUT)
}

fn surface_show_spec(surface_uuid: &str) -> CommandSpec {
    CommandSpec {
        args: vec![
            "surface".into(),
            "show".into(),
            "--surface".into(),
            surface_uuid.into(),
        ],
        stdin: None,
        timeout: SIMPLE_TIMEOUT,
    }
}

fn launch_spec(handoff_id: &str) -> CommandSpec {
    wait_spec("launch", handoff_id, HANDOFF_WAIT, LAUNCH_TIMEOUT)
}

fn verify_spec(handoff_id: &str) -> CommandSpec {
    wait_spec("verify", handoff_id, HANDOFF_WAIT, VERIFY_TIMEOUT)
}

fn retire_spec(handoff_id: &str) -> CommandSpec {
    CommandSpec {
        args: vec![
            "handoff".into(),
            "retire".into(),
            handoff_id.into(),
            "--timeout".into(),
            "10s".into(),
        ],
        stdin: None,
        timeout: RETIRE_TIMEOUT,
    }
}

fn simple_spec(verb: &str, handoff_id: &str, command_timeout: Duration) -> CommandSpec {
    CommandSpec {
        args: vec!["handoff".into(), verb.into(), handoff_id.into()],
        stdin: None,
        timeout: command_timeout,
    }
}

fn wait_spec(verb: &str, handoff_id: &str, wait: &str, command_timeout: Duration) -> CommandSpec {
    CommandSpec {
        args: vec![
            "handoff".into(),
            verb.into(),
            handoff_id.into(),
            "--wait".into(),
            wait.into(),
        ],
        stdin: None,
        timeout: command_timeout,
    }
}

#[derive(Debug, Clone, Deserialize)]
struct HandoffStatus {
    handoff_id: String,
    manifest_digest: String,
    state: String,
    #[serde(default)]
    target_device: Option<String>,
    #[serde(default)]
    target_profile: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    source_locator: Option<HandoffLocator>,
    #[serde(default)]
    target_locator: Option<HandoffLocator>,
    #[serde(default)]
    verification_state: Option<String>,
    #[serde(default)]
    context_loaded: bool,
    #[serde(default)]
    retirement_state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SurfaceShowResponse {
    binding: SurfaceBindingProof,
}

#[derive(Debug, Deserialize)]
struct SurfaceBindingProof {
    surface_id: String,
    workspace_id: String,
    local_device_id: String,
    locator: RemoteSessionLocator,
}

fn parse_status(bytes: &[u8], expected_state: &str) -> Result<HandoffStatus, String> {
    let status: HandoffStatus = serde_json::from_slice(bytes)
        .map_err(|_| "arcmux returned malformed handoff JSON".to_string())?;
    if !valid_token(&status.handoff_id) || status.state != expected_state {
        return Err(format!("arcmux did not reach {expected_state}"));
    }
    Ok(status)
}

fn verify_show_identity(
    status: &HandoffStatus,
    plan: &HandoffPlan,
    handoff_id: &str,
    manifest_digest: &str,
) -> Result<(), String> {
    if status.handoff_id != handoff_id
        || status.manifest_digest != manifest_digest
        || !valid_digest(&status.manifest_digest)
        || status.target_device.as_deref() != Some(plan.target.peer_id.as_str())
        || status.target_profile.as_deref() != Some(plan.target.profile.as_str())
        || status.project.as_deref() != Some(plan.source.project.as_str())
        || !status
            .source_locator
            .as_ref()
            .is_some_and(|locator| locator.valid() && locator.matches_source(&plan.source.locator))
    {
        return Err(
            "arcmux handoff identity did not match the selected source and target".to_string(),
        );
    }
    Ok(())
}

fn verify_authoritative_source_binding(
    bytes: &[u8],
    source: &HandoffSourceContext,
) -> Result<(), String> {
    let response: SurfaceShowResponse = serde_json::from_slice(bytes)
        .map_err(|_| "arcmux returned malformed surface binding JSON".to_string())?;
    let binding = response.binding;
    if !binding
        .surface_id
        .eq_ignore_ascii_case(&source.surface_uuid)
        || !binding
            .workspace_id
            .eq_ignore_ascii_case(&source.workspace_uuid)
        || binding.local_device_id != source.locator.device_id
        || binding.locator != source.locator
        || !binding.locator.valid()
    {
        return Err("authoritative arcmux source binding changed".to_string());
    }
    Ok(())
}

fn target_locator_matches_plan(locator: &HandoffLocator, plan: &HandoffPlan) -> bool {
    locator.valid() && locator.device_id == plan.target.peer_id
}

fn target_context_matches(
    status: &HandoffStatus,
    plan: &HandoffPlan,
    target_locator: &HandoffLocator,
) -> bool {
    target_locator_matches_plan(target_locator, plan)
        && status.target_locator.as_ref() == Some(target_locator)
        && status.verification_state.as_deref() == Some("context_loaded")
        && status.context_loaded
}

fn failure(
    message: String,
    handoff_id: Option<&str>,
    source_locator: &HandoffLocator,
    target_locator: Option<HandoffLocator>,
    launch_invoked: bool,
    target_uncertain: bool,
) -> HandoffFailure {
    HandoffFailure {
        message,
        handoff_id: handoff_id.map(str::to_string),
        source_locator: source_locator.clone(),
        target_locator,
        target_uncertain,
        duplicate_live: launch_invoked,
        retryable: !launch_invoked,
    }
}

fn nonretryable_source_proof_failure(
    message: String,
    handoff_id: Option<&str>,
    source_locator: &HandoffLocator,
) -> HandoffFailure {
    let mut failure = failure(message, handoff_id, source_locator, None, false, false);
    failure.retryable = false;
    failure
}

fn verify_source_observation(
    state_dir: &std::path::Path,
    source: &HandoffSourceContext,
) -> Result<(), String> {
    let current =
        crate::mc_data::mux_state::load_session_in_dir(state_dir, &source.locator.session_id)
            .map_err(|_| "source session state could not be re-read before retirement".to_string())?
            .ok_or_else(|| "source session disappeared before retirement".to_string())?;
    let current_last_turn_end = current
        .last_turn_end_at
        .as_ref()
        .map(chrono::DateTime::to_rfc3339);
    let has_unfinished_prompt = current
        .last_prompt_submit_at
        .as_ref()
        .zip(current.last_turn_end_at.as_ref())
        .is_some_and(|(prompt, turn_end)| prompt > turn_end);
    if current.session_id != source.locator.session_id
        || !current.agent.eq_ignore_ascii_case(&source.agent)
        || current.working
        || !current.has_ended_turn()
        || has_unfinished_prompt
        || current.turn_count != source.observation.turn_count
        || current.updated_at.to_rfc3339() != source.observation.updated_at
        || current_last_turn_end != source.observation.last_turn_end_at
    {
        return Err("source session or completed turn changed before retirement".to_string());
    }
    Ok(())
}

pub async fn run_handoff(operation: HandoffOperation, updates: mpsc::Sender<HandoffUpdate>) {
    run_handoff_with_runner(operation, HandoffCommandRunner::default(), updates).await;
}

async fn run_handoff_with_runner(
    operation: HandoffOperation,
    runner: HandoffCommandRunner,
    updates: mpsc::Sender<HandoffUpdate>,
) {
    let workspace_uuid = operation.plan.source.workspace_uuid.clone();
    let generation = operation.generation;
    let source_locator = HandoffLocator {
        device_id: operation.plan.source.locator.device_id.clone(),
        profile_scope: operation.plan.source.locator.profile_scope.clone(),
        session_id: operation.plan.source.locator.session_id.clone(),
    };
    let finish = |result| HandoffUpdate {
        workspace_uuid: workspace_uuid.clone(),
        generation,
        kind: HandoffUpdateKind::Finished(result),
    };
    if let Err(message) = validate_plan(&operation.plan) {
        let _ = updates
            .send(finish(Err(failure(
                message,
                None,
                &source_locator,
                None,
                false,
                false,
            ))))
            .await;
        return;
    }

    let initial_source_binding = match runner
        .run(surface_show_spec(&operation.plan.source.surface_uuid))
        .await
    {
        Ok(bytes) => bytes,
        Err(message) => {
            let _ = updates
                .send(finish(Err(nonretryable_source_proof_failure(
                    message,
                    None,
                    &source_locator,
                ))))
                .await;
            return;
        }
    };
    if let Err(message) =
        verify_authoritative_source_binding(&initial_source_binding, &operation.plan.source)
    {
        let _ = updates
            .send(finish(Err(nonretryable_source_proof_failure(
                message,
                None,
                &source_locator,
            ))))
            .await;
        return;
    }

    if !send_progress(
        &updates,
        &workspace_uuid,
        generation,
        HandoffStage::Preparing,
    )
    .await
    {
        return;
    }
    let prepared = match runner.run(prepare_spec(&operation.plan)).await {
        Ok(bytes) => match parse_status(&bytes, "remote_prepared") {
            Ok(status) => status,
            Err(message) => {
                let _ = updates
                    .send(finish(Err(failure(
                        message,
                        None,
                        &source_locator,
                        None,
                        false,
                        false,
                    ))))
                    .await;
                return;
            }
        },
        Err(message) => {
            let _ = updates
                .send(finish(Err(failure(
                    message,
                    None,
                    &source_locator,
                    None,
                    false,
                    false,
                ))))
                .await;
            return;
        }
    };
    let handoff_id = prepared.handoff_id.clone();
    let manifest_digest = prepared.manifest_digest.clone();
    if let Err(message) =
        verify_show_identity(&prepared, &operation.plan, &handoff_id, &manifest_digest)
    {
        let _ = updates
            .send(finish(Err(failure(
                message,
                Some(&handoff_id),
                &source_locator,
                None,
                false,
                false,
            ))))
            .await;
        return;
    }

    if !send_progress(
        &updates,
        &workspace_uuid,
        generation,
        HandoffStage::CheckingIdentity,
    )
    .await
    {
        return;
    }
    let shown = match runner.run(show_spec(&handoff_id)).await {
        Ok(bytes) => match parse_status(&bytes, "remote_prepared") {
            Ok(status) => status,
            Err(message) => {
                let _ = updates
                    .send(finish(Err(failure(
                        message,
                        Some(&handoff_id),
                        &source_locator,
                        None,
                        false,
                        false,
                    ))))
                    .await;
                return;
            }
        },
        Err(message) => {
            let _ = updates
                .send(finish(Err(failure(
                    message,
                    Some(&handoff_id),
                    &source_locator,
                    None,
                    false,
                    false,
                ))))
                .await;
            return;
        }
    };
    if let Err(message) =
        verify_show_identity(&shown, &operation.plan, &handoff_id, &manifest_digest)
    {
        let _ = updates
            .send(finish(Err(failure(
                message,
                Some(&handoff_id),
                &source_locator,
                None,
                false,
                false,
            ))))
            .await;
        return;
    }

    if !send_progress(
        &updates,
        &workspace_uuid,
        generation,
        HandoffStage::Launching,
    )
    .await
    {
        return;
    }
    let launched = match runner.run(launch_spec(&handoff_id)).await {
        Ok(bytes) => match parse_status(&bytes, "accepted") {
            Ok(status) => status,
            Err(message) => {
                let _ = updates
                    .send(finish(Err(failure(
                        message,
                        Some(&handoff_id),
                        &source_locator,
                        None,
                        true,
                        true,
                    ))))
                    .await;
                return;
            }
        },
        Err(message) => {
            let _ = updates
                .send(finish(Err(failure(
                    message,
                    Some(&handoff_id),
                    &source_locator,
                    None,
                    true,
                    true,
                ))))
                .await;
            return;
        }
    };
    let target_locator = match launched
        .target_locator
        .clone()
        .filter(|locator| target_locator_matches_plan(locator, &operation.plan))
    {
        Some(locator) => locator,
        None => {
            let _ = updates
                .send(finish(Err(failure(
                    "arcmux accepted the target without a valid locator on the selected peer"
                        .to_string(),
                    Some(&handoff_id),
                    &source_locator,
                    launched.target_locator.clone(),
                    true,
                    true,
                ))))
                .await;
            return;
        }
    };
    if let Err(message) =
        verify_show_identity(&launched, &operation.plan, &handoff_id, &manifest_digest)
    {
        let _ = updates
            .send(finish(Err(failure(
                message,
                Some(&handoff_id),
                &source_locator,
                Some(target_locator),
                true,
                false,
            ))))
            .await;
        return;
    }

    if !send_progress(
        &updates,
        &workspace_uuid,
        generation,
        HandoffStage::VerifyingContext,
    )
    .await
    {
        return;
    }
    let verified = match runner.run(verify_spec(&handoff_id)).await {
        Ok(bytes) => match parse_status(&bytes, "accepted") {
            Ok(status) => status,
            Err(message) => {
                let _ = updates
                    .send(finish(Err(failure(
                        message,
                        Some(&handoff_id),
                        &source_locator,
                        Some(target_locator),
                        true,
                        false,
                    ))))
                    .await;
                return;
            }
        },
        Err(message) => {
            let _ = updates
                .send(finish(Err(failure(
                    message,
                    Some(&handoff_id),
                    &source_locator,
                    Some(target_locator),
                    true,
                    false,
                ))))
                .await;
            return;
        }
    };
    if verify_show_identity(&verified, &operation.plan, &handoff_id, &manifest_digest).is_err()
        || !target_context_matches(&verified, &operation.plan, &target_locator)
    {
        let _ = updates
            .send(finish(Err(failure(
                "arcmux verified a different target locator".to_string(),
                Some(&handoff_id),
                &source_locator,
                Some(target_locator),
                true,
                false,
            ))))
            .await;
        return;
    }

    let final_source_binding = match runner
        .run(surface_show_spec(&operation.plan.source.surface_uuid))
        .await
    {
        Ok(bytes) => bytes,
        Err(message) => {
            let _ = updates
                .send(finish(Err(failure(
                    message,
                    Some(&handoff_id),
                    &source_locator,
                    Some(target_locator),
                    true,
                    false,
                ))))
                .await;
            return;
        }
    };
    if let Err(message) =
        verify_authoritative_source_binding(&final_source_binding, &operation.plan.source)
    {
        let _ = updates
            .send(finish(Err(failure(
                message,
                Some(&handoff_id),
                &source_locator,
                Some(target_locator),
                true,
                false,
            ))))
            .await;
        return;
    }
    if let Err(message) = verify_source_observation(&runner.mux_state_dir, &operation.plan.source) {
        let _ = updates
            .send(finish(Err(failure(
                message,
                Some(&handoff_id),
                &source_locator,
                Some(target_locator),
                true,
                false,
            ))))
            .await;
        return;
    }

    if !send_progress(
        &updates,
        &workspace_uuid,
        generation,
        HandoffStage::RetiringSource,
    )
    .await
    {
        return;
    }
    let retirement = match runner.run(retire_spec(&handoff_id)).await {
        Ok(bytes) => match parse_status(&bytes, "accepted") {
            Ok(status) => status,
            Err(message) => {
                let _ = updates
                    .send(finish(Err(failure(
                        message,
                        Some(&handoff_id),
                        &source_locator,
                        Some(target_locator),
                        true,
                        false,
                    ))))
                    .await;
                return;
            }
        },
        Err(message) => {
            let _ = updates
                .send(finish(Err(failure(
                    message,
                    Some(&handoff_id),
                    &source_locator,
                    Some(target_locator),
                    true,
                    false,
                ))))
                .await;
            return;
        }
    };
    if retirement.retirement_state.as_deref() != Some("retired") {
        let _ = updates
            .send(finish(Err(failure(
                "exact source retirement was not confirmed".to_string(),
                Some(&handoff_id),
                &source_locator,
                Some(target_locator),
                true,
                false,
            ))))
            .await;
        return;
    }
    let retired = retirement;
    if verify_show_identity(&retired, &operation.plan, &handoff_id, &manifest_digest).is_err()
        || !retired
            .source_locator
            .as_ref()
            .is_some_and(|locator| locator.matches_source(&operation.plan.source.locator))
        || !target_context_matches(&retired, &operation.plan, &target_locator)
    {
        let _ = updates
            .send(finish(Err(failure(
                "arcmux retirement response changed source or verified target identity".to_string(),
                Some(&handoff_id),
                &source_locator,
                Some(target_locator),
                true,
                false,
            ))))
            .await;
        return;
    }

    let _ = updates
        .send(finish(Ok(HandoffSuccess {
            handoff_id,
            source_locator,
            target_locator,
        })))
        .await;
}

async fn send_progress(
    updates: &mpsc::Sender<HandoffUpdate>,
    workspace_uuid: &str,
    generation: u64,
    stage: HandoffStage,
) -> bool {
    updates
        .send(HandoffUpdate {
            workspace_uuid: workspace_uuid.to_string(),
            generation,
            kind: HandoffUpdateKind::Progress(stage),
        })
        .await
        .is_ok()
}

fn validate_plan(plan: &HandoffPlan) -> Result<(), String> {
    if !plan.source.locator.valid()
        || !valid_token(&plan.source.workspace_uuid)
        || !valid_uuid(&plan.source.surface_uuid)
        || !valid_token(&plan.source.agent)
        || !valid_token(&plan.source.project)
        || !valid_token(&plan.target.peer_id)
        || !valid_token(&plan.target.profile)
        || !valid_history_basename(&plan.source.history)
        || !matches!(
            plan.source.validation.as_str(),
            "not_run" | "passed" | "failed"
        )
        || plan.source.goal.trim().is_empty()
        || plan.source.goal.chars().count() > 2048
        || plan.source.goal.chars().any(char::is_control)
        || !valid_source_observation(&plan.source.observation)
    {
        return Err("handoff preflight context is incomplete or invalid".to_string());
    }
    if plan.source.locator.device_id == plan.target.peer_id {
        return Err("handoff target must be a different device".to_string());
    }
    for optional in [
        plan.source.conversation_id.as_deref(),
        plan.source.parent_handoff_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !valid_token(optional) {
            return Err("handoff preflight context is incomplete or invalid".to_string());
        }
    }
    Ok(())
}

fn valid_source_observation(observation: &HandoffSourceObservation) -> bool {
    let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(&observation.updated_at) else {
        return false;
    };
    let Some(Ok(last_turn_end_at)) = observation
        .last_turn_end_at
        .as_deref()
        .map(chrono::DateTime::parse_from_rfc3339)
    else {
        return false;
    };
    observation.turn_count > 0 && last_turn_end_at <= updated_at
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin) -> Result<(Vec<u8>, bool), String> {
    let mut retained = Vec::new();
    let mut overflow = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|_| "arcmux output read failed".to_string())?;
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

fn bounded_message(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
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

fn valid_profile_scope(value: &str) -> bool {
    value == "root"
        || value
            .strip_prefix("profile:")
            .is_some_and(|profile| valid_token(profile) && profile == profile.to_ascii_lowercase())
}

fn valid_history_basename(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.contains(['/', '\\', '\0', '\r', '\n'])
        && value != "."
        && value != ".."
        && value.ends_with(".md")
        && !value.starts_with(crate::session::file::HANDOFF_TRANSPORT_HISTORY_PREFIX)
}

fn valid_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn plan() -> HandoffPlan {
        HandoffPlan {
            source: HandoffSourceContext {
                workspace_uuid: "22222222-2222-4222-8222-222222222222".into(),
                surface_uuid: "11111111-1111-4111-8111-111111111111".into(),
                locator: RemoteSessionLocator {
                    schema_version: 1,
                    device_id: "ref".into(),
                    profile_scope: "root".into(),
                    session_id: "s-source".into(),
                    transport_binding_id: None,
                },
                agent: "codex".into(),
                project: "mission-control".into(),
                goal: "Continue the verified handoff implementation".into(),
                history: "2026-07-15-20-surface-handoff.md".into(),
                conversation_id: Some("conversation-1".into()),
                parent_handoff_id: None,
                validation: "not_run".into(),
                observation: HandoffSourceObservation {
                    turn_count: 3,
                    updated_at: "2026-07-15T20:01:00-07:00".into(),
                    last_turn_end_at: Some("2026-07-15T20:01:00-07:00".into()),
                },
            },
            target: HandoffTarget {
                peer_id: "devbox".into(),
                profile: "codex".into(),
            },
        }
    }

    fn args(spec: &CommandSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn prepare_uses_argv_and_goal_stdin() {
        let plan = plan();
        let spec = prepare_spec(&plan);
        assert_eq!(
            args(&spec),
            vec![
                "handoff",
                "prepare",
                "devbox",
                "root",
                "s-source",
                "--project",
                "mission-control",
                "--agent",
                "codex",
                "--goal-file",
                "-",
                "--history",
                "2026-07-15-20-surface-handoff.md",
                "--validation",
                "not_run",
                "--wait",
                "90s",
                "--conversation",
                "conversation-1",
            ]
        );
        assert_eq!(spec.stdin.as_deref(), Some(plan.source.goal.as_bytes()));
    }

    #[test]
    fn controller_retire_is_immediate_and_never_after_turn_end() {
        let actual = args(&retire_spec("handoff-1"));
        assert_eq!(
            actual,
            vec!["handoff", "retire", "handoff-1", "--timeout", "10s"]
        );
        assert!(!actual.iter().any(|arg| arg == "--after-turn-end"));
    }

    #[test]
    fn authoritative_source_preflight_uses_exact_surface_argv_and_identity() {
        assert_eq!(
            args(&surface_show_spec("11111111-1111-4111-8111-111111111111")),
            vec![
                "surface",
                "show",
                "--surface",
                "11111111-1111-4111-8111-111111111111"
            ]
        );
        let valid = br#"{"binding":{"surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","local_device_id":"ref","locator":{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}"#;
        assert!(verify_authoritative_source_binding(valid, &plan().source).is_ok());

        let rebound = br#"{"binding":{"surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","local_device_id":"ref","locator":{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"s-replacement"}}}"#;
        assert_eq!(
            verify_authoritative_source_binding(rebound, &plan().source).unwrap_err(),
            "authoritative arcmux source binding changed"
        );
    }

    #[test]
    fn plan_rejects_transport_snapshot_as_conversation_history() {
        let mut plan = plan();
        plan.source.history = "arcmux-handoff-sha256-deadbeef.md".into();
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn show_identity_must_match_exact_source_and_target() {
        let plan = plan();
        let status = HandoffStatus {
            handoff_id: "handoff-1".into(),
            manifest_digest: "a".repeat(64),
            state: "remote_prepared".into(),
            target_device: Some("devbox".into()),
            target_profile: Some("codex".into()),
            project: Some("mission-control".into()),
            source_locator: Some(HandoffLocator {
                device_id: "ref".into(),
                profile_scope: "root".into(),
                session_id: "s-source".into(),
            }),
            target_locator: None,
            verification_state: Some("not_ready".into()),
            context_loaded: false,
            retirement_state: Some("not_requested".into()),
        };
        assert!(verify_show_identity(&status, &plan, "handoff-1", &"a".repeat(64)).is_ok());
        let mut wrong = status.clone();
        wrong.source_locator.as_mut().unwrap().session_id = "s-other".into();
        assert!(verify_show_identity(&wrong, &plan, "handoff-1", &"a".repeat(64)).is_err());
        let mut wrong_digest = status.clone();
        wrong_digest.manifest_digest = "b".repeat(64);
        assert!(verify_show_identity(&wrong_digest, &plan, "handoff-1", &"a".repeat(64)).is_err());
        let mut malformed_digest = status;
        malformed_digest.manifest_digest = "not-a-digest".into();
        assert!(
            verify_show_identity(&malformed_digest, &plan, "handoff-1", "not-a-digest").is_err()
        );
    }

    #[test]
    fn target_locator_and_retirement_context_are_pinned_to_selected_peer() {
        let plan = plan();
        let target = HandoffLocator {
            device_id: "devbox".into(),
            profile_scope: "root".into(),
            session_id: "s-target".into(),
        };
        let status = HandoffStatus {
            handoff_id: "handoff-1".into(),
            manifest_digest: "a".repeat(64),
            state: "accepted".into(),
            target_device: Some("devbox".into()),
            target_profile: Some("codex".into()),
            project: Some("mission-control".into()),
            source_locator: Some(HandoffLocator {
                device_id: "ref".into(),
                profile_scope: "root".into(),
                session_id: "s-source".into(),
            }),
            target_locator: Some(target.clone()),
            verification_state: Some("context_loaded".into()),
            context_loaded: true,
            retirement_state: Some("retired".into()),
        };

        assert!(target_locator_matches_plan(&target, &plan));
        assert!(target_context_matches(&status, &plan, &target));

        let mut wrong_device = target.clone();
        wrong_device.device_id = "labs".into();
        assert!(!target_locator_matches_plan(&wrong_device, &plan));

        let mut changed_retirement = status.clone();
        changed_retirement.target_locator = Some(HandoffLocator {
            session_id: "s-replaced".into(),
            ..target.clone()
        });
        assert!(!target_context_matches(&changed_retirement, &plan, &target));
        changed_retirement.target_locator = Some(target.clone());
        changed_retirement.context_loaded = false;
        assert!(!target_context_matches(&changed_retirement, &plan, &target));
    }

    fn fake_runner(script: &str) -> (tempfile::TempDir, HandoffCommandRunner) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("arcmux-fake");
        std::fs::write(&path, script).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        write_idle_source_state(temp.path(), 3);
        let runner = HandoffCommandRunner::new(path);
        (temp, runner)
    }

    fn write_idle_source_state(dir: &std::path::Path, turn_count: u64) {
        std::fs::write(
            dir.join("s-source.json"),
            format!(
                r#"{{"session_id":"s-source","agent":"codex","created_at":"2026-07-15T20:00:00-07:00","updated_at":"2026-07-15T20:01:00-07:00","last_event":"turn_end","working":false,"turn_count":{turn_count},"last_turn_end_at":"2026-07-15T20:01:00-07:00"}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn source_turn_observation_fails_closed_when_turn_advances() {
        let temp = tempfile::tempdir().unwrap();
        write_idle_source_state(temp.path(), 3);
        let plan = plan();
        assert!(verify_source_observation(temp.path(), &plan.source).is_ok());

        write_idle_source_state(temp.path(), 4);
        assert_eq!(
            verify_source_observation(temp.path(), &plan.source).unwrap_err(),
            "source session or completed turn changed before retirement"
        );
    }

    #[tokio::test]
    async fn fake_executable_completes_only_after_verify_then_retire() {
        let script = r#"#!/bin/sh
if [ "$1" = "surface" ]; then printf '%s\n' '{"binding":{"surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","local_device_id":"ref","locator":{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}'; exit 0; fi
verb="$2"
case "$verb" in
  prepare) cat >/dev/null; printf '%s\n' '{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{"device_id":"ref","profile_scope":"root","session_id":"s-source"},"verification_state":"not_ready","context_loaded":false,"retirement_state":"not_requested"}' ;;
  show) printf '%s\n' '{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{"device_id":"ref","profile_scope":"root","session_id":"s-source"},"verification_state":"not_ready","context_loaded":false,"retirement_state":"not_requested"}' ;;
  launch) printf '%s\n' '{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{"device_id":"ref","profile_scope":"root","session_id":"s-source"},"target_locator":{"device_id":"devbox","profile_scope":"root","session_id":"s-target"},"verification_state":"pending","context_loaded":false,"retirement_state":"not_requested"}' ;;
  verify) printf '%s\n' '{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{"device_id":"ref","profile_scope":"root","session_id":"s-source"},"target_locator":{"device_id":"devbox","profile_scope":"root","session_id":"s-target"},"verification_state":"context_loaded","context_loaded":true,"retirement_state":"not_requested"}' ;;
  retire) printf '%s\n' '{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{"device_id":"ref","profile_scope":"root","session_id":"s-source"},"target_locator":{"device_id":"devbox","profile_scope":"root","session_id":"s-target"},"verification_state":"context_loaded","context_loaded":true,"retirement_state":"retired"}' ;;
  *) exit 2 ;;
esac
"#;
        let (_temp, runner) = fake_runner(script);
        let (tx, mut rx) = mpsc::channel(16);
        run_handoff_with_runner(
            HandoffOperation {
                generation: 7,
                plan: plan(),
            },
            runner,
            tx,
        )
        .await;
        let updates: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            updates
                .iter()
                .filter_map(|update| match update.kind {
                    HandoffUpdateKind::Progress(stage) => Some(stage),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![
                HandoffStage::Preparing,
                HandoffStage::CheckingIdentity,
                HandoffStage::Launching,
                HandoffStage::VerifyingContext,
                HandoffStage::RetiringSource,
            ]
        );
        assert!(matches!(
            updates.last().map(|update| &update.kind),
            Some(HandoffUpdateKind::Finished(Ok(success)))
                if success.target_locator.session_id == "s-target"
        ));
    }

    #[tokio::test]
    async fn source_turn_advancing_after_verify_prevents_retire() {
        let temp = tempfile::tempdir().unwrap();
        write_idle_source_state(temp.path(), 3);
        let log = temp.path().join("verbs");
        let state = temp.path().join("s-source.json");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "surface" ]; then printf '%s\n' '{{"binding":{{"surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","local_device_id":"ref","locator":{{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}}}'; exit 0; fi
verb="$2"
printf '%s\n' "$verb" >> '{}'
case "$verb" in
  prepare) cat >/dev/null; printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}' ;;
  show) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}' ;;
  launch) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"target_locator":{{"device_id":"devbox","profile_scope":"root","session_id":"s-target"}}}}' ;;
  verify)
    printf '%s\n' '{{"session_id":"s-source","agent":"codex","created_at":"2026-07-15T20:00:00-07:00","updated_at":"2026-07-15T20:02:00-07:00","last_event":"turn_end","working":false,"turn_count":4,"last_turn_end_at":"2026-07-15T20:02:00-07:00"}}' > '{}'
    printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"target_locator":{{"device_id":"devbox","profile_scope":"root","session_id":"s-target"}},"verification_state":"context_loaded","context_loaded":true}}'
    ;;
  retire) exit 99 ;;
  *) exit 2 ;;
esac
"#,
            log.display(),
            state.display()
        );
        let path = temp.path().join("arcmux-fake");
        std::fs::write(&path, script).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        run_handoff_with_runner(
            HandoffOperation {
                generation: 10,
                plan: plan(),
            },
            HandoffCommandRunner::new(path),
            tx,
        )
        .await;

        let updates: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let failure = match &updates.last().unwrap().kind {
            HandoffUpdateKind::Finished(Err(failure)) => failure,
            other => panic!("unexpected final update: {other:?}"),
        };
        assert_eq!(
            failure.message,
            "source session or completed turn changed before retirement"
        );
        assert!(failure.duplicate_live);
        assert!(!failure.retryable);
        let verbs = std::fs::read_to_string(log).unwrap();
        assert!(!verbs.lines().any(|verb| verb == "retire"));
    }

    #[tokio::test]
    async fn authoritative_rebind_after_launch_preserves_both_sessions_in_order() {
        let temp = tempfile::tempdir().unwrap();
        write_idle_source_state(temp.path(), 3);
        let log = temp.path().join("calls");
        let surface_seen = temp.path().join("surface-seen");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "surface" ]; then
  printf '%s\n' 'surface-show' >> '{}'
  if [ -f '{}' ]; then
    printf '%s\n' '{{"binding":{{"surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","local_device_id":"ref","locator":{{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"s-replacement"}}}}}}'
  else
    : > '{}'
    printf '%s\n' '{{"binding":{{"surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","local_device_id":"ref","locator":{{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}}}'
  fi
  exit 0
fi
verb="$2"
printf '%s\n' "$verb" >> '{}'
case "$verb" in
  prepare) cat >/dev/null; printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}' ;;
  show) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}' ;;
  launch) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"target_locator":{{"device_id":"devbox","profile_scope":"root","session_id":"s-target"}}}}' ;;
  verify) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"target_locator":{{"device_id":"devbox","profile_scope":"root","session_id":"s-target"}},"verification_state":"context_loaded","context_loaded":true}}' ;;
  retire) exit 99 ;;
  *) exit 2 ;;
esac
"#,
            log.display(),
            surface_seen.display(),
            surface_seen.display(),
            log.display()
        );
        let path = temp.path().join("arcmux-fake");
        std::fs::write(&path, script).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        run_handoff_with_runner(
            HandoffOperation {
                generation: 11,
                plan: plan(),
            },
            HandoffCommandRunner::new(path),
            tx,
        )
        .await;

        let updates: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let failure = match &updates.last().unwrap().kind {
            HandoffUpdateKind::Finished(Err(failure)) => failure,
            other => panic!("unexpected final update: {other:?}"),
        };
        assert_eq!(
            failure.message,
            "authoritative arcmux source binding changed"
        );
        assert_eq!(failure.handoff_id.as_deref(), Some("handoff-1"));
        assert!(failure.duplicate_live);
        assert!(!failure.retryable);
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec![
                "surface-show",
                "prepare",
                "show",
                "launch",
                "verify",
                "surface-show"
            ]
        );
    }

    #[tokio::test]
    async fn pending_retirement_is_failure_and_is_never_polled() {
        let temp = tempfile::tempdir().unwrap();
        write_idle_source_state(temp.path(), 3);
        let log = temp.path().join("verbs");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "surface" ]; then printf '%s\n' '{{"binding":{{"surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","local_device_id":"ref","locator":{{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}}}'; exit 0; fi
verb="$2"
printf '%s\n' "$verb" >> '{}'
case "$verb" in
  prepare) cat >/dev/null; printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"verification_state":"not_ready","context_loaded":false,"retirement_state":"not_requested"}}' ;;
  show) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"verification_state":"not_ready","context_loaded":false,"retirement_state":"not_requested"}}' ;;
  launch) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"target_locator":{{"device_id":"devbox","profile_scope":"root","session_id":"s-target"}},"verification_state":"pending","context_loaded":false,"retirement_state":"not_requested"}}' ;;
  verify) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"target_locator":{{"device_id":"devbox","profile_scope":"root","session_id":"s-target"}},"verification_state":"context_loaded","context_loaded":true,"retirement_state":"not_requested"}}' ;;
  retire) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"target_locator":{{"device_id":"devbox","profile_scope":"root","session_id":"s-target"}},"verification_state":"context_loaded","context_loaded":true,"retirement_state":"pending"}}' ;;
  *) exit 2 ;;
esac
"#,
            log.display()
        );
        let path = temp.path().join("arcmux-fake");
        std::fs::write(&path, script).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        run_handoff_with_runner(
            HandoffOperation {
                generation: 8,
                plan: plan(),
            },
            HandoffCommandRunner::new(path),
            tx,
        )
        .await;

        let updates: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let failure = match &updates.last().unwrap().kind {
            HandoffUpdateKind::Finished(Err(failure)) => failure,
            other => panic!("unexpected final update: {other:?}"),
        };
        assert_eq!(failure.message, "exact source retirement was not confirmed");
        assert!(failure.duplicate_live);
        assert!(!failure.retryable);
        assert_eq!(failure.source_locator.session_id, "s-source");
        assert_eq!(
            failure.target_locator.as_ref().unwrap().session_id,
            "s-target"
        );

        let verbs = std::fs::read_to_string(log).unwrap();
        assert_eq!(verbs.lines().filter(|verb| *verb == "show").count(), 1);
        assert_eq!(verbs.lines().filter(|verb| *verb == "retire").count(), 1);
    }

    #[tokio::test]
    async fn ambiguous_launch_failure_keeps_reconciliation_id_and_blocks_retry() {
        let script = r#"#!/bin/sh
if [ "$1" = "surface" ]; then printf '%s\n' '{"binding":{"surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","local_device_id":"ref","locator":{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}'; exit 0; fi
verb="$2"
case "$verb" in
  prepare) cat >/dev/null; printf '%s\n' '{"handoff_id":"handoff-uncertain","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{"device_id":"ref","profile_scope":"root","session_id":"s-source"}}' ;;
  show) printf '%s\n' '{"handoff_id":"handoff-uncertain","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{"device_id":"ref","profile_scope":"root","session_id":"s-source"}}' ;;
  launch) exit 70 ;;
  *) exit 99 ;;
esac
"#;
        let (_temp, runner) = fake_runner(script);
        let (tx, mut rx) = mpsc::channel(16);
        run_handoff_with_runner(
            HandoffOperation {
                generation: 9,
                plan: plan(),
            },
            runner,
            tx,
        )
        .await;

        let updates: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let failure = match &updates.last().unwrap().kind {
            HandoffUpdateKind::Finished(Err(failure)) => failure,
            other => panic!("unexpected final update: {other:?}"),
        };
        assert_eq!(failure.handoff_id.as_deref(), Some("handoff-uncertain"));
        assert!(failure.target_locator.is_none());
        assert!(failure.target_uncertain);
        assert!(failure.duplicate_live);
        assert!(!failure.retryable);
    }

    #[tokio::test]
    async fn verify_failure_never_runs_retire_and_warns_duplicate_live() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("verbs");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "surface" ]; then printf '%s\n' '{{"binding":{{"surface_id":"11111111-1111-4111-8111-111111111111","workspace_id":"22222222-2222-4222-8222-222222222222","local_device_id":"ref","locator":{{"schema_version":1,"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}}}'; exit 0; fi
verb="$2"
printf '%s\n' "$verb" >> '{}'
case "$verb" in
  prepare) cat >/dev/null; printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}' ;;
  show) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"remote_prepared","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}}}}' ;;
  launch) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"accepted","target_device":"devbox","target_profile":"codex","project":"mission-control","source_locator":{{"device_id":"ref","profile_scope":"root","session_id":"s-source"}},"target_locator":{{"device_id":"devbox","profile_scope":"root","session_id":"s-target"}}}}' ;;
  verify) printf '%s\n' '{{"handoff_id":"handoff-1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"failed"}}' ;;
  retire) exit 99 ;;
esac
"#,
            log.display()
        );
        let path = temp.path().join("arcmux-fake");
        std::fs::write(&path, script).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        run_handoff_with_runner(
            HandoffOperation {
                generation: 8,
                plan: plan(),
            },
            HandoffCommandRunner::new(path),
            tx,
        )
        .await;
        let updates: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let failure = match &updates.last().unwrap().kind {
            HandoffUpdateKind::Finished(Err(failure)) => failure,
            other => panic!("unexpected final update: {other:?}"),
        };
        assert!(failure.duplicate_live);
        assert!(!failure.retryable);
        assert_eq!(
            failure.target_locator.as_ref().unwrap().session_id,
            "s-target"
        );
        let verbs = std::fs::read_to_string(log).unwrap();
        assert!(!verbs.lines().any(|verb| verb == "retire"));
    }

    #[tokio::test]
    async fn command_runner_rejects_malformed_and_oversized_output() {
        let (_temp, runner) = fake_runner("#!/bin/sh\nprintf 'not-json\\n'\n");
        let bytes = runner.run(show_spec("handoff-1")).await.unwrap();
        assert!(parse_status(&bytes, "remote_prepared").is_err());

        let (_temp, runner) =
            fake_runner("#!/bin/sh\ni=0; while [ $i -lt 70000 ]; do printf x; i=$((i+1)); done\n");
        assert!(
            runner
                .run(show_spec("handoff-1"))
                .await
                .unwrap_err()
                .contains("safety limit")
        );
    }
}
