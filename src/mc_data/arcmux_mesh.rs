//! Read-only consumer for arcmux's local mesh projection.
//!
//! Mission Control deliberately talks only to the arcmux daemon on loopback.
//! It performs three ordinary GETs per refresh (`status`, `sessions`, and
//! `surface-bindings`) and joins them by the stable identities in the protocol.
//! It never asks arcmux to sync and never guesses a binding from a title, cwd,
//! or session name.

use crate::cmux::client::SurfaceInfo;
use crate::mc_data::surface_kind::SurfaceKind;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const LOOPBACK_BASE: &str = "http://127.0.0.1:7777/mesh/";
const REQUEST_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_SAFE_TEXT_CHARS: usize = 240;
const CURRENT_WORK_PROVENANCE: &str = "hook.overall_goal_summarizer.v1";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct RemoteSessionLocator {
    pub schema_version: u32,
    pub device_id: String,
    pub profile_scope: String,
    pub session_id: String,
    #[serde(default)]
    pub transport_binding_id: Option<String>,
}

impl RemoteSessionLocator {
    fn valid(&self) -> bool {
        self.schema_version == 1
            && valid_id(&self.device_id)
            && valid_profile_scope(&self.profile_scope)
            && valid_id(&self.session_id)
            && self
                .transport_binding_id
                .as_deref()
                .is_none_or(valid_id)
    }

    fn identity(&self) -> RemoteSessionIdentity {
        RemoteSessionIdentity {
            device_id: self.device_id.clone(),
            profile_scope: self.profile_scope.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RemoteSessionIdentity {
    device_id: String,
    profile_scope: String,
    session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteFreshness {
    Syncing,
    Fresh,
    Stale,
    Gone,
}

impl RemoteFreshness {
    pub fn label(self) -> &'static str {
        match self {
            Self::Syncing => "syncing",
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Gone => "gone",
        }
    }

    pub fn is_offline(self) -> bool {
        !matches!(self, Self::Fresh)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteActivity {
    Actionable,
    Working,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSurfaceState {
    pub surface_uuid: String,
    pub workspace_uuid: String,
    pub locator: RemoteSessionLocator,
    pub name: Option<String>,
    pub agent: Option<String>,
    pub state: Option<String>,
    pub health: Option<String>,
    pub launch_cwd: Option<String>,
    pub current_work: Option<String>,
    pub freshness: RemoteFreshness,
}

impl RemoteSurfaceState {
    pub fn surface_kind(&self) -> SurfaceKind {
        match self
            .agent
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("claude") => SurfaceKind::Claude,
            Some("codex") => SurfaceKind::Codex,
            Some("opencode") => SurfaceKind::OtherAgent,
            Some(_) => SurfaceKind::OtherAgent,
            None => SurfaceKind::Remote,
        }
    }

    pub fn display_name<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback)
    }

    pub fn stable_title(&self, fallback: &str) -> String {
        format!(
            "{} · {}/{}",
            self.display_name(fallback),
            self.locator.device_id,
            self.locator.profile_scope
        )
    }

    pub fn state_label(&self) -> &str {
        self.state
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
    }

    pub fn activity(&self) -> RemoteActivity {
        if self.freshness != RemoteFreshness::Fresh {
            return RemoteActivity::Idle;
        }
        match self.state_label() {
            // arcmux's native `idle` means the agent is ready after a turn,
            // which is the same human-facing state as waiting for input.
            "idle" | "waiting" | "needs_input" | "blocked" | "stuck" | "escalated"
            | "failed" => RemoteActivity::Actionable,
            "working" | "starting" | "handshaking" => RemoteActivity::Working,
            _ => RemoteActivity::Idle,
        }
    }

    pub fn mark_stale(&mut self) {
        if self.freshness != RemoteFreshness::Gone {
            self.freshness = RemoteFreshness::Stale;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RemoteMeshSnapshot {
    bindings: HashMap<String, SurfaceBinding>,
    sessions: HashMap<RemoteSessionIdentity, SessionProjection>,
    peer_states: HashMap<String, PeerConnectionState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerConnectionState {
    Connected,
    Connecting,
    Offline,
}

impl RemoteMeshSnapshot {
    /// Join exact cmux UUIDs to exact arcmux bindings for one workspace.
    /// A binding carrying a different workspace UUID is ignored even when its
    /// surface UUID is present, preventing cross-workspace contamination.
    pub fn resolve_workspace(
        &self,
        workspace_uuid: &str,
        surfaces: &[SurfaceInfo],
    ) -> HashMap<String, RemoteSurfaceState> {
        let mut resolved = HashMap::new();
        for surface in surfaces {
            let Some(surface_uuid) = surface.uuid.as_deref() else {
                continue;
            };
            let Some(binding) = self.bindings.get(&surface_uuid.to_ascii_lowercase()) else {
                continue;
            };
            if !binding.workspace_id.eq_ignore_ascii_case(workspace_uuid) {
                continue;
            }
            let projection = self.sessions.get(&binding.locator.identity());
            let metadata = projection.map(|p| &p.metadata);
            // Peer connectivity is the effective freshness ceiling. Cached
            // session projections intentionally survive a disconnect, so a
            // projection that still says `fresh` must not make an offline
            // device look live. `gone` remains terminal regardless of peer
            // state so history never reappears as syncing/stale.
            let projected_freshness = projection.map(|p| p.freshness);
            let peer_state = self.peer_states.get(&binding.locator.device_id).copied();
            let freshness = match (projected_freshness, peer_state) {
                (Some(RemoteFreshness::Gone), _) => RemoteFreshness::Gone,
                (Some(value), Some(PeerConnectionState::Connected)) => value,
                (Some(_), Some(PeerConnectionState::Connecting)) => RemoteFreshness::Syncing,
                (Some(_), Some(PeerConnectionState::Offline) | None) => RemoteFreshness::Stale,
                (None, Some(PeerConnectionState::Connected | PeerConnectionState::Connecting)) => {
                    RemoteFreshness::Syncing
                }
                (None, Some(PeerConnectionState::Offline) | None) => RemoteFreshness::Stale,
            };
            resolved.insert(
                surface.ref_id.clone(),
                RemoteSurfaceState {
                    surface_uuid: binding.surface_id.clone(),
                    workspace_uuid: binding.workspace_id.clone(),
                    locator: binding.locator.clone(),
                    name: metadata.and_then(|m| m.name.clone()),
                    agent: metadata.and_then(|m| m.agent.clone()),
                    state: metadata.and_then(|m| m.state.clone()),
                    health: metadata.and_then(|m| m.health.clone()),
                    launch_cwd: metadata.and_then(|m| m.launch_cwd.clone()),
                    current_work: metadata.and_then(SessionMetadata::safe_current_work),
                    freshness,
                },
            );
        }
        resolved
    }
}

#[derive(Debug, Clone)]
pub struct MeshFetch {
    pub snapshot: Option<RemoteMeshSnapshot>,
    pub warning: Option<String>,
    /// True when the top-level projection was valid but one or more records
    /// were skipped. Consumers retain unmatched exact identities as stale.
    pub partial: bool,
    pub observed_at: Instant,
}

impl Default for MeshFetch {
    fn default() -> Self {
        Self {
            snapshot: None,
            warning: None,
            partial: false,
            observed_at: Instant::now(),
        }
    }
}

#[derive(Clone)]
pub struct ArcmuxMeshClient {
    http: Client,
    base: Url,
}

impl Default for ArcmuxMeshClient {
    fn default() -> Self {
        Self::new(LOOPBACK_BASE).expect("static arcmux loopback URL is valid")
    }
}

impl ArcmuxMeshClient {
    pub fn new(base: &str) -> Result<Self, &'static str> {
        let base = Url::parse(base).map_err(|_| "invalid arcmux loopback URL")?;
        if base.scheme() != "http"
            || !matches!(
                base.host_str(),
                Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
            )
        {
            return Err("arcmux mesh consumer requires a loopback HTTP URL");
        }
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| "failed to create arcmux loopback client")?;
        Ok(Self { http, base })
    }

    /// Fetch the three read-only mesh projections concurrently. There is no
    /// POST/sync side effect and no per-binding request fanout.
    pub async fn fetch(&self) -> MeshFetch {
        // Capture ordering before any request starts. If this fetch stalls and
        // a newer poll completes first, the late older result must be rejected
        // by App rather than receiving a misleadingly new completion stamp.
        let observed_at = Instant::now();
        let (status, sessions, bindings) = tokio::join!(
            self.get_value("status"),
            self.get_value("sessions"),
            self.get_value("surface-bindings"),
        );
        if status.is_err() || sessions.is_err() || bindings.is_err() {
            let endpoint = if status.is_err() {
                "status"
            } else if sessions.is_err() {
                "sessions"
            } else {
                "surface-bindings"
            };
            return MeshFetch {
                snapshot: None,
                warning: Some(format!("arcmux mesh unavailable ({endpoint})")),
                partial: false,
                observed_at,
            };
        }
        let (Ok(status), Ok(sessions), Ok(bindings)) = (status, sessions, bindings) else {
            unreachable!("all mesh results were checked above")
        };
        if !valid_projection_shape(&status, &sessions, &bindings) {
            return MeshFetch {
                snapshot: None,
                warning: Some("arcmux mesh projection malformed".to_string()),
                partial: false,
                observed_at,
            };
        }
        let decoded = decode_snapshot(&status, &sessions, &bindings);
        MeshFetch {
            snapshot: Some(decoded.snapshot),
            warning: (decoded.skipped > 0).then(|| {
                format!(
                    "arcmux mesh skipped {} malformed record{}",
                    decoded.skipped,
                    if decoded.skipped == 1 { "" } else { "s" }
                )
            }),
            partial: decoded.skipped > 0,
            observed_at,
        }
    }

    async fn get_value(&self, endpoint: &'static str) -> Result<Value, ()> {
        let url = self.base.join(endpoint).map_err(|_| ())?;
        let mut response = self.http.get(url).send().await.map_err(|_| ())?;
        if !response.status().is_success() {
            return Err(());
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| ())
    }
}

fn valid_projection_shape(status: &Value, sessions: &Value, bindings: &Value) -> bool {
    status.get("peers").is_some_and(Value::is_array)
        && sessions.get("sessions").is_some_and(Value::is_array)
        && bindings
            .get("surface_bindings")
            .is_some_and(Value::is_array)
}

#[derive(Debug)]
struct DecodedSnapshot {
    snapshot: RemoteMeshSnapshot,
    skipped: usize,
}

fn decode_snapshot(status: &Value, sessions: &Value, bindings: &Value) -> DecodedSnapshot {
    let mut out = RemoteMeshSnapshot::default();
    let mut skipped = 0;

    for value in status
        .get("peers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match serde_json::from_value::<PeerStatus>(value.clone()) {
            Ok(peer) if valid_id(&peer.peer_id) && valid_peer_state(&peer.state) => {
                let state = match peer.state.as_str() {
                    "connected" => PeerConnectionState::Connected,
                    "connecting" => PeerConnectionState::Connecting,
                    "disconnected" | "error" => PeerConnectionState::Offline,
                    _ => unreachable!("peer state was validated"),
                };
                out.peer_states.insert(peer.peer_id, state);
            }
            _ => skipped += 1,
        }
    }

    for value in sessions
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match serde_json::from_value::<RawSessionProjection>(value.clone()) {
            Ok(raw) if raw.valid() => {
                let projection = raw.into_projection();
                let identity = projection.locator.identity();
                match out.sessions.get(&identity) {
                    Some(existing) if existing.source_revision >= projection.source_revision => {}
                    _ => {
                        out.sessions.insert(identity, projection);
                    }
                }
            }
            _ => skipped += 1,
        }
    }

    for value in bindings
        .get("surface_bindings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match serde_json::from_value::<SurfaceBinding>(value.clone()) {
            Ok(binding)
                if binding.valid()
                    && !out
                        .bindings
                        .contains_key(&binding.surface_id.to_ascii_lowercase()) =>
            {
                out.bindings
                    .insert(binding.surface_id.to_ascii_lowercase(), binding);
            }
            _ => skipped += 1,
        }
    }

    DecodedSnapshot {
        snapshot: out,
        skipped,
    }
}

#[derive(Debug, Clone, Deserialize)]
struct PeerStatus {
    peer_id: String,
    state: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SurfaceBinding {
    schema_version: u32,
    binding_id: String,
    local_device_id: String,
    mux: String,
    surface_id: String,
    workspace_id: String,
    locator: RemoteSessionLocator,
    source: String,
    created_at: String,
    updated_at: String,
}

impl SurfaceBinding {
    fn valid(&self) -> bool {
        self.schema_version == 1
            && valid_id(&self.binding_id)
            && valid_id(&self.local_device_id)
            && self.mux == "cmux"
            && valid_uuid(&self.surface_id)
            && valid_uuid(&self.workspace_id)
            && self.locator.valid()
            && safe_text(&self.source, 64).is_some()
            && valid_timestamp(&self.created_at)
            && valid_timestamp(&self.updated_at)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawSessionProjection {
    schema_version: u32,
    locator: RemoteSessionLocator,
    metadata: Value,
    received_at: String,
    freshness_changed_at: String,
    source_epoch: String,
    source_revision: u64,
    freshness: RemoteFreshness,
}

impl RawSessionProjection {
    fn valid(&self) -> bool {
        self.schema_version == 1
            && self.locator.valid()
            && valid_id(&self.source_epoch)
            && self.source_revision > 0
            && self.metadata.is_object()
            && valid_timestamp(&self.received_at)
            && valid_timestamp(&self.freshness_changed_at)
            && serde_json::from_value::<RawSessionMetadata>(self.metadata.clone()).is_ok()
    }

    fn into_projection(self) -> SessionProjection {
        let metadata = SessionMetadata::from(
            serde_json::from_value::<RawSessionMetadata>(self.metadata)
                .expect("metadata was validated before conversion"),
        );
        SessionProjection {
            locator: self.locator,
            metadata,
            source_revision: self.source_revision,
            freshness: self.freshness,
        }
    }
}

#[derive(Debug, Clone)]
struct SessionProjection {
    locator: RemoteSessionLocator,
    metadata: SessionMetadata,
    source_revision: u64,
    freshness: RemoteFreshness,
}

#[derive(Debug, Clone, Default)]
struct SessionMetadata {
    name: Option<String>,
    agent: Option<String>,
    state: Option<String>,
    health: Option<String>,
    launch_cwd: Option<String>,
    current_work: Option<CurrentWork>,
}

impl SessionMetadata {
    fn safe_current_work(&self) -> Option<String> {
        let current = self.current_work.as_ref()?;
        if current.provenance != CURRENT_WORK_PROVENANCE
            || chrono::DateTime::parse_from_rfc3339(current.updated_at.trim()).is_err()
        {
            return None;
        }
        safe_text(&current.summary, MAX_SAFE_TEXT_CHARS)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawSessionMetadata {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    health: Option<String>,
    #[serde(default)]
    launch_cwd: Option<String>,
    #[serde(default)]
    current_work: Option<CurrentWork>,
}

impl From<RawSessionMetadata> for SessionMetadata {
    fn from(raw: RawSessionMetadata) -> Self {
        Self {
            name: raw.name.and_then(|v| safe_text(&v, 128)),
            agent: raw.agent.and_then(|v| safe_token(&v, 32)),
            state: raw.state.and_then(|v| safe_token(&v, 32)),
            health: raw.health.and_then(|v| safe_token(&v, 32)),
            launch_cwd: raw.launch_cwd.and_then(|v| safe_text(&v, 240)),
            current_work: raw.current_work,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CurrentWork {
    summary: String,
    provenance: String,
    updated_at: String,
}

fn safe_token(value: &str, max_chars: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > max_chars
        || trimmed.chars().any(char::is_control)
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn safe_text(value: &str, max_chars: usize) -> Option<String> {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty()
        || collapsed.chars().count() > max_chars
        || collapsed.chars().any(char::is_control)
    {
        return None;
    }
    Some(collapsed)
}

fn valid_id(value: &str) -> bool {
    safe_token(value, 128).is_some()
}

fn valid_profile_scope(value: &str) -> bool {
    if value == "root" {
        return true;
    }
    let Some(name) = value.strip_prefix("profile:") else {
        return false;
    };
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'-')
            })
}

fn valid_peer_state(value: &str) -> bool {
    matches!(value, "connected" | "connecting" | "disconnected" | "error")
}

fn valid_timestamp(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value.trim()).is_ok()
}

fn valid_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].into_iter().all(|idx| bytes[idx] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, b)| [8, 13, 18, 23].contains(&idx) || b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        serde_json::from_str(match name {
            "status" => include_str!("../../tests/fixtures/arcmux_mesh/status.json"),
            "sessions" => include_str!("../../tests/fixtures/arcmux_mesh/sessions.json"),
            "bindings" => include_str!("../../tests/fixtures/arcmux_mesh/surface-bindings.json"),
            _ => unreachable!(),
        })
        .unwrap()
    }

    fn surface(uuid: &str, ref_id: &str, title: &str) -> SurfaceInfo {
        SurfaceInfo {
            title: title.to_string(),
            ref_id: ref_id.to_string(),
            uuid: Some(uuid.to_string()),
            pane_ref: Some("pane:1".to_string()),
            tty: Some("ttys001".to_string()),
            kind: SurfaceKind::Remote,
            selected: false,
            focused: false,
            active: false,
            index: Some(0),
            index_in_pane: Some(0),
            surface_type: Some("terminal".to_string()),
        }
    }

    #[test]
    fn fixture_joins_exact_surface_workspace_and_locator() {
        let decoded = decode_snapshot(
            &fixture("status"),
            &fixture("sessions"),
            &fixture("bindings"),
        );
        assert_eq!(decoded.skipped, 0);
        let states = decoded.snapshot.resolve_workspace(
            "22222222-2222-4222-8222-222222222222",
            &[surface(
                "11111111-1111-4111-8111-111111111111",
                "surface:14",
                "misleading local title",
            )],
        );
        let state = states.get("surface:14").unwrap();
        assert_eq!(state.locator.device_id, "devbox");
        assert_eq!(state.locator.profile_scope, "root");
        assert_eq!(state.locator.session_id, "s-working");
        assert_eq!(state.agent.as_deref(), Some("codex"));
        assert_eq!(state.state.as_deref(), Some("working"));
        assert_eq!(state.freshness, RemoteFreshness::Fresh);
        assert_eq!(
            state.current_work.as_deref(),
            Some("Wire native remote surfaces into Mission Control")
        );
    }

    #[test]
    fn workspace_uuid_isolation_beats_matching_surface_uuid() {
        let decoded = decode_snapshot(
            &fixture("status"),
            &fixture("sessions"),
            &fixture("bindings"),
        );
        let states = decoded.snapshot.resolve_workspace(
            "99999999-9999-4999-8999-999999999999",
            &[surface(
                "11111111-1111-4111-8111-111111111111",
                "surface:14",
                "codex devbox",
            )],
        );
        assert!(states.is_empty());
    }

    #[test]
    fn unbound_title_never_infers_remote_identity() {
        let decoded = decode_snapshot(
            &fixture("status"),
            &fixture("sessions"),
            &fixture("bindings"),
        );
        let states = decoded.snapshot.resolve_workspace(
            "22222222-2222-4222-8222-222222222222",
            &[surface(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "surface:99",
                "[devbox] codex s-working",
            )],
        );
        assert!(states.is_empty());
    }

    #[test]
    fn exact_locator_keeps_devices_and_profiles_separate() {
        let decoded = decode_snapshot(
            &fixture("status"),
            &fixture("sessions"),
            &fixture("bindings"),
        );
        let states = decoded.snapshot.resolve_workspace(
            "44444444-4444-4444-8444-444444444444",
            &[surface(
                "33333333-3333-4333-8333-333333333333",
                "surface:15",
                "same session name",
            )],
        );
        let state = states.get("surface:15").unwrap();
        assert_eq!(state.locator.device_id, "labs");
        assert_eq!(state.locator.profile_scope, "profile:boyan");
        assert_eq!(state.state.as_deref(), Some("idle"));
        assert_eq!(state.activity(), RemoteActivity::Actionable);
    }

    #[test]
    fn native_arcmux_states_map_to_human_activity() {
        let mut state = RemoteSurfaceState {
            surface_uuid: "11111111-1111-4111-8111-111111111111".to_string(),
            workspace_uuid: "22222222-2222-4222-8222-222222222222".to_string(),
            locator: RemoteSessionLocator {
                schema_version: 1,
                device_id: "devbox".to_string(),
                profile_scope: "root".to_string(),
                session_id: "s-native".to_string(),
                transport_binding_id: None,
            },
            name: None,
            agent: Some("codex".to_string()),
            state: None,
            health: None,
            launch_cwd: None,
            current_work: None,
            freshness: RemoteFreshness::Fresh,
        };
        for native in ["working", "starting", "handshaking"] {
            state.state = Some(native.to_string());
            assert_eq!(state.activity(), RemoteActivity::Working, "{native}");
        }
        for native in ["idle", "stuck", "escalated", "failed"] {
            state.state = Some(native.to_string());
            assert_eq!(state.activity(), RemoteActivity::Actionable, "{native}");
        }
        state.state = Some("exited".to_string());
        assert_eq!(state.activity(), RemoteActivity::Idle);
    }

    #[test]
    fn uuid_join_is_case_insensitive_but_still_exact() {
        let mut bindings = fixture("bindings");
        bindings["surface_bindings"][0]["surface_id"] =
            serde_json::json!("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA");
        bindings["surface_bindings"][0]["workspace_id"] =
            serde_json::json!("BBBBBBBB-BBBB-4BBB-8BBB-BBBBBBBBBBBB");
        let decoded = decode_snapshot(&fixture("status"), &fixture("sessions"), &bindings);
        let states = decoded.snapshot.resolve_workspace(
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &[surface(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "surface:case",
                "ignored",
            )],
        );
        assert!(states.contains_key("surface:case"));
    }

    #[test]
    fn missing_projection_retains_binding_as_syncing_or_stale() {
        let sessions = serde_json::json!({"sessions": []});
        let decoded = decode_snapshot(&fixture("status"), &sessions, &fixture("bindings"));
        let states = decoded.snapshot.resolve_workspace(
            "22222222-2222-4222-8222-222222222222",
            &[surface(
                "11111111-1111-4111-8111-111111111111",
                "surface:14",
                "ignored",
            )],
        );
        assert_eq!(
            states.get("surface:14").unwrap().freshness,
            RemoteFreshness::Syncing
        );

        let connecting = serde_json::json!({"peers":[{"peer_id":"devbox","state":"connecting"}]});
        let decoded = decode_snapshot(&connecting, &sessions, &fixture("bindings"));
        let states = decoded.snapshot.resolve_workspace(
            "22222222-2222-4222-8222-222222222222",
            &[surface(
                "11111111-1111-4111-8111-111111111111",
                "surface:14",
                "ignored",
            )],
        );
        assert_eq!(
            states.get("surface:14").unwrap().freshness,
            RemoteFreshness::Syncing
        );

        let offline = serde_json::json!({"peers":[{"peer_id":"devbox","state":"disconnected"}]});
        let decoded = decode_snapshot(&offline, &sessions, &fixture("bindings"));
        let states = decoded.snapshot.resolve_workspace(
            "22222222-2222-4222-8222-222222222222",
            &[surface(
                "11111111-1111-4111-8111-111111111111",
                "surface:14",
                "ignored",
            )],
        );
        assert_eq!(
            states.get("surface:14").unwrap().freshness,
            RemoteFreshness::Stale
        );
    }

    #[test]
    fn peer_state_caps_cached_projection_freshness() {
        let surface = surface(
            "11111111-1111-4111-8111-111111111111",
            "surface:14",
            "ignored",
        );
        let workspace = "22222222-2222-4222-8222-222222222222";

        let disconnected = serde_json::json!({
            "peers":[{"peer_id":"devbox","state":"disconnected"}]
        });
        let decoded = decode_snapshot(
            &disconnected,
            &fixture("sessions"),
            &fixture("bindings"),
        );
        assert_eq!(
            decoded
                .snapshot
                .resolve_workspace(workspace, std::slice::from_ref(&surface))["surface:14"]
                .freshness,
            RemoteFreshness::Stale
        );

        let connecting = serde_json::json!({
            "peers":[{"peer_id":"devbox","state":"connecting"}]
        });
        let decoded = decode_snapshot(
            &connecting,
            &fixture("sessions"),
            &fixture("bindings"),
        );
        assert_eq!(
            decoded
                .snapshot
                .resolve_workspace(workspace, std::slice::from_ref(&surface))["surface:14"]
                .freshness,
            RemoteFreshness::Syncing
        );

        let mut gone_sessions = fixture("sessions");
        gone_sessions["sessions"][0]["freshness"] = serde_json::json!("gone");
        let decoded = decode_snapshot(&connecting, &gone_sessions, &fixture("bindings"));
        assert_eq!(
            decoded.snapshot.resolve_workspace(workspace, &[surface])["surface:14"].freshness,
            RemoteFreshness::Gone
        );
    }

    #[test]
    fn reconnect_refreshes_same_locator_without_retargeting() {
        let decoded = decode_snapshot(
            &fixture("status"),
            &fixture("sessions"),
            &fixture("bindings"),
        );
        let surface = surface(
            "11111111-1111-4111-8111-111111111111",
            "surface:14",
            "title changes do not matter",
        );
        let mut disconnected = decoded.snapshot.resolve_workspace(
            "22222222-2222-4222-8222-222222222222",
            std::slice::from_ref(&surface),
        );
        disconnected.get_mut("surface:14").unwrap().mark_stale();

        let reconnected = decoded
            .snapshot
            .resolve_workspace("22222222-2222-4222-8222-222222222222", &[surface]);
        let before = disconnected.get("surface:14").unwrap();
        let after = reconnected.get("surface:14").unwrap();
        assert_eq!(before.locator.identity(), after.locator.identity());
        assert_eq!(before.freshness, RemoteFreshness::Stale);
        assert_eq!(after.freshness, RemoteFreshness::Fresh);
    }

    #[test]
    fn malformed_records_are_skipped_without_losing_valid_records() {
        let mut sessions = fixture("sessions");
        sessions["sessions"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "schema_version": 99,
                "metadata": {"secret": "must-not-appear"}
            }));
        let decoded = decode_snapshot(&fixture("status"), &sessions, &fixture("bindings"));
        assert_eq!(decoded.skipped, 1);
        assert_eq!(decoded.snapshot.sessions.len(), 2);
    }

    #[test]
    fn malformed_metadata_and_required_timestamps_skip_with_warning_count() {
        let mut sessions = fixture("sessions");
        sessions["sessions"][0]["metadata"]["agent"] = serde_json::json!(["codex"]);
        sessions["sessions"][1]
            .as_object_mut()
            .unwrap()
            .remove("received_at");
        let mut bindings = fixture("bindings");
        bindings["surface_bindings"][0]
            .as_object_mut()
            .unwrap()
            .remove("updated_at");

        let decoded = decode_snapshot(&fixture("status"), &sessions, &bindings);
        assert_eq!(decoded.skipped, 3);
        assert!(decoded.snapshot.sessions.is_empty());
        assert_eq!(decoded.snapshot.bindings.len(), 1);
    }

    #[test]
    fn malformed_binding_is_skipped_without_losing_exact_bindings() {
        let mut bindings = fixture("bindings");
        bindings["surface_bindings"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "schema_version": 1,
                "binding_id": "bad-binding",
                "local_device_id": "ref",
                "mux": "cmux",
                "surface_id": "not-a-uuid",
                "workspace_id": "22222222-2222-4222-8222-222222222222",
                "locator": {
                    "schema_version": 1,
                    "device_id": "devbox",
                    "profile_scope": "root",
                    "session_id": "s-working"
                },
                "source": "test"
            }));
        let decoded = decode_snapshot(&fixture("status"), &fixture("sessions"), &bindings);
        assert_eq!(decoded.skipped, 1);
        assert_eq!(decoded.snapshot.bindings.len(), 2);
    }

    #[test]
    fn current_work_requires_exact_provenance_and_rfc3339_timestamp() {
        let raw = RawSessionMetadata {
            current_work: Some(CurrentWork {
                summary: "private-looking but summarized".to_string(),
                provenance: "raw.prompt".to_string(),
                updated_at: "2026-07-15T12:00:00Z".to_string(),
            }),
            ..Default::default()
        };
        assert!(SessionMetadata::from(raw).safe_current_work().is_none());

        let raw = RawSessionMetadata {
            current_work: Some(CurrentWork {
                summary: "safe summary".to_string(),
                provenance: CURRENT_WORK_PROVENANCE.to_string(),
                updated_at: "not-a-timestamp".to_string(),
            }),
            ..Default::default()
        };
        assert!(SessionMetadata::from(raw).safe_current_work().is_none());
    }

    #[test]
    fn malformed_top_level_projection_is_rejected() {
        assert!(!valid_projection_shape(
            &fixture("status"),
            &serde_json::json!({"sessions": {}}),
            &fixture("bindings")
        ));
    }

    #[test]
    fn non_loopback_base_is_rejected() {
        assert!(ArcmuxMeshClient::new("https://devbox:7777/mesh/").is_err());
        let local = ArcmuxMeshClient::new("http://127.0.0.1:7777/mesh/").unwrap();
        assert_eq!(local.base.host_str(), Some("127.0.0.1"));
        assert!(ArcmuxMeshClient::new("http://[::1]:7777/mesh/").is_ok());
    }

    #[test]
    fn locator_validation_matches_arcmux_profile_and_transport_rules() {
        let locator = |profile: &str, transport: Option<&str>| RemoteSessionLocator {
            schema_version: 1,
            device_id: "devbox".to_string(),
            profile_scope: profile.to_string(),
            session_id: "s-1".to_string(),
            transport_binding_id: transport.map(str::to_string),
        };
        assert!(locator("root", None).valid());
        assert!(locator("profile:boyan_dev-1", Some("binding-1")).valid());
        assert!(!locator("profile:Boyan", None).valid());
        assert!(!locator("profile:-boyan", None).valid());
        assert!(!locator("profile:boyan-", None).valid());
        assert!(!locator("profile:boyan", Some("not a safe id")).valid());
    }
}
