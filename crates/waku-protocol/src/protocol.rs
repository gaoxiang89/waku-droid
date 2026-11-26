use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

use crate::attachments::{AttachmentUpload, StoredAttachment};
use crate::computer_use::ComputerPermissions;
use crate::model::{
    AgentSession, GoalOperation, Project, ProviderKind, ProviderProbe, ProviderResumeCursor,
    ProviderSessionHistory, ProviderSessionSummary, SubmittedHandoff, UserInputAnswer,
};
use crate::persistence::{ComposerDraftChange, ComposerDrafts, SessionMessageMatch};
use crate::provider_session::{ProviderSessionFork, ProviderSessionForkRequest};
use crate::settings::DaemonSettings;
use crate::skills::SkillsCatalog;
use crate::usage::PlanUsage;
use crate::usage_history::{UsageHistory, UsageWindow};
use crate::workspace::{WorkspaceOperation, WorkspaceResult};

pub const PROTOCOL_VERSION: u32 = 13;
pub const MAX_WIRE_MESSAGE_BYTES: usize = 48 * 1024 * 1024;
pub const DAEMON_TOKEN_ENV: &str = "WAKU_DAEMON_TOKEN";
pub const DAEMON_ADDRESS_ENV: &str = "WAKU_DAEMON_ADDRESS";
pub const APP_EXECUTABLE_ENV: &str = "WAKU_APP_EXECUTABLE";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientKind {
    #[default]
    Other,
    Desktop,
    Web,
    Mobile,
    Cli,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DaemonReady {
    pub address: String,
    pub protocol_version: u32,
    pub pid: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        token: String,
        client_id: Uuid,
        #[serde(default)]
        resume_from: Vec<ReplayCursor>,
        #[serde(default)]
        client_kind: ClientKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_pid: Option<u32>,
    },
    Request(Request),
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    pub command: Command,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCursor {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Identifies the daemon process that assigned `sequence`.
    pub epoch: Uuid,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    /// Resolve the daemon-owned provider runtime for an existing task.
    ///
    /// Clients use this after reconnecting or opening the same daemon from a
    /// second app. It observes the session actor without starting, replacing,
    /// or otherwise mutating the provider process.
    AttachSession,
    Start {
        options: WireDriverStartOptions,
    },
    Prompt {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript_message: Option<String>,
        /// The ids the submitting client already gave this turn and its user
        /// message. The daemon republishes them with the submission so every
        /// other client attached to the runtime mirrors the same rows instead
        /// of minting its own; older clients omit them and the daemon mints.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<Uuid>,
        /// The handoff notice and record this turn opened with, republished
        /// with the submission so attached clients record the same rows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handoff: Option<SubmittedHandoff>,
    },
    Steer {
        prompt: String,
    },
    Cancel,
    CancelComputerUse,
    RefreshBackgroundWork,
    StopBackgroundWork {
        key: Value,
        control_id: String,
    },
    Respond {
        request_id: String,
        option_id: String,
    },
    RespondUserInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    /// Ask the live provider runtime to read or mutate its persisted thread
    /// goal. Fire-and-forget: the outcome arrives as a `goalUpdated` driver
    /// event, or an `error` event when the provider refuses.
    Goal {
        operation: GoalOperation,
    },
    RunComputerTool {
        request: WireComputerToolRequest,
    },
    RejectComputerTool {
        request: WireComputerToolRequest,
        reason: String,
    },
    ApplyOptions {
        options: WireSessionOptions,
    },
    Rollback {
        turns: usize,
    },
    Fork {
        turns_to_remove: usize,
    },
    GetSettings,
    UpdateSettings {
        settings: DaemonSettings,
    },
    ProbeProvider {
        provider: ProviderKind,
        binary_override: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_dir: Option<String>,
        discover_models: bool,
        probe_version: bool,
    },
    FetchPlanUsage {
        provider: ProviderKind,
        binary_override: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_dir: Option<String>,
        cli_version: Option<String>,
    },
    ProbeComputerPermissions {
        prompt: bool,
    },
    LoadUsageHistory {
        window: UsageWindow,
        project_roots: Vec<PathBuf>,
    },
    LoadSkills {
        projects: Vec<(String, PathBuf)>,
    },
    SetSkillsEnabled {
        dirs: Vec<PathBuf>,
        enabled: bool,
    },
    TrashSkills {
        dirs: Vec<PathBuf>,
    },
    LoadTaskState,
    SaveTaskState {
        projects: Vec<Project>,
        live_session_ids: Vec<Uuid>,
        sessions: Vec<AgentSession>,
    },
    /// Explicitly remove one daemon-owned task. Ordinary state saves are
    /// merge-only so a stale client snapshot cannot delete tasks another
    /// client just created.
    RemoveSession,
    /// Delete the workspace catalog entry and all Waku tasks, retaining disk files.
    RemoveProject {
        project_id: Uuid,
    },
    HydrateSession {
        session_id: Uuid,
    },
    SearchSessionMessages {
        query: String,
        limit: usize,
    },
    /// List one provider's resumable CLI conversations on the daemon host.
    ListProviderSessions {
        provider: ProviderKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        limit: usize,
    },
    /// Load the user-visible transcript for one provider-native conversation.
    LoadProviderSession {
        cursor: ProviderResumeCursor,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        cwd: PathBuf,
    },
    LoadComposerDrafts,
    SaveComposerDrafts {
        drafts: ComposerDrafts,
        generation: u64,
    },
    ApplyComposerDraftChanges {
        changes: Vec<ComposerDraftChange>,
    },
    StoreBlob {
        mime_type: String,
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        bytes: Vec<u8>,
    },
    ImportAttachment {
        name: String,
        upload: AttachmentUpload,
    },
    ImportPathAttachment {
        #[ts(type = "string")]
        path: PathBuf,
    },
    ReadBlob {
        reference: String,
    },
    ReadAttachment {
        reference: String,
        path: PathBuf,
    },
    SweepBlobs,
    /// Fork a persisted task through one completed provider turn.
    ///
    /// This is intentionally a daemon-owned operation: provider-native
    /// conversation state, Git checkpoint refs, and SQLite all live on the
    /// daemon host and must move together for remote clients.
    ForkSessionFromResponse {
        turn_count: usize,
    },
    /// Restore a task and its provider conversation to immediately before a
    /// prior user message. The client can then submit the edited replacement
    /// as an ordinary new turn.
    RewindSessionToMessage {
        turn_count: usize,
    },
    ForkProviderSession {
        request: ProviderSessionForkRequest,
    },
    Workspace {
        operation: WorkspaceOperation,
    },
    /// Ask connected desktop apps to focus and optionally open a workspace path
    /// on this host (or on `host` when forwarded through the local daemon).
    OpenWorkspace {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
    },
    OpenTerminal {
        #[ts(type = "string")]
        cwd: PathBuf,
        cols: u16,
        rows: u16,
    },
    WriteTerminal {
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        data: Vec<u8>,
    },
    ResizeTerminal {
        cols: u16,
        rows: u16,
    },
    CloseTerminal,
    /// Reattach without creating or replacing a process. Output offsets are
    /// terminal-owned and independent of a transport's event sequence.
    AttachTerminal {
        after: Option<u64>,
    },
    ListTerminals,
    CloseSession,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverStartOptions {
    pub provider: String,
    pub binary: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    pub cwd: PathBuf,
    pub mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
    pub agent_preset: Option<String>,
    pub computer_use_enabled: bool,
    pub provider_cursor: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireSessionOptions {
    pub mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireComputerToolRequest {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverEvent {
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

impl WireDriverEvent {
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SequencedEvent {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Changes whenever the daemon restarts, so a reused runtime id can begin
    /// again at sequence one without being mistaken for an old event.
    pub epoch: Uuid,
    pub sequence: u64,
    pub event: WireDriverEvent,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    Hello {
        protocol_version: u32,
        daemon_version: String,
    },
    Rejected {
        message: String,
    },
    Response {
        request_id: Uuid,
        outcome: ResponseOutcome,
    },
    Event(SequencedEvent),
    /// The daemon-owned project/task catalog changed through another client.
    /// Clients should invalidate their lightweight task-state snapshot; live
    /// runtime events continue through [`Self::Event`].
    TaskStateChanged {
        revision: u64,
    },
    /// A CLI or other client asked connected desktop apps to activate and
    /// optionally open a workspace path.
    OpenWorkspace {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
    },
    ShuttingDown,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponseOutcome {
    Ok { payload: ResponsePayload },
    Error { error: RpcError },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponsePayload {
    Ack,
    Terminals {
        terminals: Vec<TerminalInfo>,
    },
    TerminalOutput {
        snapshot: TerminalOutput,
    },
    SessionRuntime {
        runtime_id: Option<Uuid>,
        supports_steer: bool,
    },
    Started {
        supports_steer: bool,
    },
    OptionsApplied {
        applied: bool,
    },
    Cursor {
        cursor: Option<Value>,
    },
    Settings {
        settings: DaemonSettings,
    },
    ProviderProbe {
        probe: ProviderProbe,
        version: Option<String>,
    },
    PlanUsage {
        usage: Option<PlanUsage>,
    },
    ComputerPermissions {
        permissions: ComputerPermissions,
    },
    UsageHistory {
        history: UsageHistory,
    },
    SkillsCatalog {
        catalog: SkillsCatalog,
    },
    TaskState {
        #[serde(default)]
        deleted_project_ids: Vec<Uuid>,
        projects: Vec<Project>,
        sessions: Vec<AgentSession>,
        default_cwd: PathBuf,
        projectless_root: Option<PathBuf>,
    },
    TaskStateSaved {
        sessions: Vec<AgentSession>,
    },
    Session {
        session: Option<AgentSession>,
    },
    SessionMessageMatches {
        matches: Vec<SessionMessageMatch>,
    },
    ProviderSessions {
        sessions: Vec<ProviderSessionSummary>,
    },
    ProviderSessionHistory {
        history: ProviderSessionHistory,
    },
    ComposerDrafts {
        drafts: ComposerDrafts,
    },
    BlobStored {
        reference: String,
        path: PathBuf,
    },
    AttachmentStored {
        attachment: StoredAttachment,
    },
    BlobData {
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        bytes: Vec<u8>,
    },
    ProviderSessionForked {
        result: ProviderSessionFork,
    },
    SessionForked {
        session: AgentSession,
        checkpoint_warning: Option<String>,
    },
    SessionRewound {
        session: AgentSession,
        cleanup_warning: Option<String>,
    },
    Workspace {
        result: WorkspaceResult,
    },
    WorkspaceOpened {
        delivered: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        desktop_pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_path: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInfo {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    pub cwd: PathBuf,
    pub exited: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutput {
    pub offset: u64,
    pub reset: bool,
    pub cols: u16,
    pub rows: u16,
    pub exited: bool,
    #[serde(with = "base64_bytes")]
    #[ts(type = "string")]
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct RpcError {
    pub message: String,
}

impl From<anyhow::Error> for RpcError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_payloads_use_base64_json_strings() {
        let payload = ResponsePayload::BlobData {
            bytes: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["bytes"], "AAEC/w==");
        let ResponsePayload::BlobData { bytes } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected payload variant");
        };
        assert_eq!(bytes, vec![0, 1, 2, 255]);

        let command = Command::WriteTerminal {
            data: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["type"], "writeTerminal");
        assert_eq!(json["data"], "AAEC/w==");
        let Command::WriteTerminal { data } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected command variant");
        };
        assert_eq!(data, vec![0, 1, 2, 255]);
    }

    #[test]
    fn response_fork_command_uses_stable_camel_case_fields() {
        let json =
            serde_json::to_value(Command::ForkSessionFromResponse { turn_count: 7 }).unwrap();

        assert_eq!(json["type"], "forkSessionFromResponse");
        assert_eq!(json["turnCount"], 7);
        assert_eq!(PROTOCOL_VERSION, 13);
    }

    #[test]
    fn message_rewind_command_uses_stable_camel_case_fields() {
        let json = serde_json::to_value(Command::RewindSessionToMessage { turn_count: 4 }).unwrap();

        assert_eq!(json["type"], "rewindSessionToMessage");
        assert_eq!(json["turnCount"], 4);
        assert_eq!(PROTOCOL_VERSION, 13);
    }

    #[test]
    fn provider_session_commands_use_stable_wire_fields() {
        let list = serde_json::to_value(Command::ListProviderSessions {
            provider: ProviderKind::Codex,
            account_id: Some("work".into()),
            limit: 250,
        })
        .unwrap();
        assert_eq!(list["type"], "listProviderSessions");
        assert_eq!(list["provider"], "codex");
        assert_eq!(list["accountId"], "work");
        assert_eq!(list["limit"], 250);

        let load = serde_json::to_value(Command::LoadProviderSession {
            cursor: ProviderResumeCursor::Codex {
                thread_id: "01900000-0000-7000-8000-000000000001".into(),
            },
            cwd: PathBuf::from("/tmp/project"),
            account_id: None,
        })
        .unwrap();
        assert_eq!(load["type"], "loadProviderSession");
        assert_eq!(load["cursor"]["provider"], "codex");
        assert_eq!(
            load["cursor"]["threadId"],
            "01900000-0000-7000-8000-000000000001"
        );
        assert_eq!(load["cwd"], "/tmp/project");
    }

    #[test]
    fn handshake_and_replay_field_names_are_stable() {
        let session_id = Uuid::nil();
        let runtime_id = Uuid::from_u128(1);
        let message = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: "secret".into(),
            client_id: Uuid::from_u128(2),
            resume_from: vec![ReplayCursor {
                session_id,
                runtime_id,
                epoch: Uuid::from_u128(3),
                sequence: 9,
            }],
            client_kind: ClientKind::Desktop,
            client_pid: Some(4242),
        };
        let json = serde_json::to_value(message).unwrap();

        assert_eq!(json["type"], "hello");
        assert_eq!(json["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(json["clientKind"], "desktop");
        assert_eq!(json["clientPid"], 4242);
        assert_eq!(json["resumeFrom"][0]["sessionId"], session_id.to_string());
        assert_eq!(json["resumeFrom"][0]["runtimeId"], runtime_id.to_string());
        assert_eq!(
            json["resumeFrom"][0]["epoch"],
            Uuid::from_u128(3).to_string()
        );
        assert!(json.get("protocol_version").is_none());

        let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
            "type": "hello",
            "protocolVersion": PROTOCOL_VERSION,
            "token": "secret",
            "clientId": Uuid::from_u128(2),
        }))
        .unwrap();
        let ClientMessage::Hello {
            client_kind,
            client_pid,
            resume_from,
            ..
        } = minimal
        else {
            panic!("expected hello");
        };
        assert_eq!(client_kind, ClientKind::Other);
        assert_eq!(client_pid, None);
        assert!(resume_from.is_empty());
    }

    #[test]
    fn open_workspace_messages_use_stable_camel_case_fields() {
        let command = Command::OpenWorkspace {
            path: Some("/tmp/project".into()),
            host: Some("ssh:devbox".into()),
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["type"], "openWorkspace");
        assert_eq!(json["path"], "/tmp/project");
        assert_eq!(json["host"], "ssh:devbox");
        let Command::OpenWorkspace { path, host } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected command variant");
        };
        assert_eq!(path.as_deref(), Some("/tmp/project"));
        assert_eq!(host.as_deref(), Some("ssh:devbox"));

        let focus_only = serde_json::to_value(Command::OpenWorkspace {
            path: None,
            host: None,
        })
        .unwrap();
        assert_eq!(focus_only["type"], "openWorkspace");
        assert!(focus_only.get("path").is_none());
        assert!(focus_only.get("host").is_none());

        let response = ResponsePayload::WorkspaceOpened {
            delivered: true,
            desktop_pid: Some(9876),
            resolved_path: Some("/tmp/project".into()),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["type"], "workspaceOpened");
        assert_eq!(json["delivered"], true);
        assert_eq!(json["desktopPid"], 9876);
        assert_eq!(json["resolvedPath"], "/tmp/project");
        let ResponsePayload::WorkspaceOpened {
            delivered,
            desktop_pid,
            resolved_path,
        } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected payload variant");
        };
        assert!(delivered);
        assert_eq!(desktop_pid, Some(9876));
        assert_eq!(resolved_path.as_deref(), Some("/tmp/project"));

        let server_msg = ServerMessage::OpenWorkspace {
            path: Some("/tmp/project".into()),
            host: None,
        };
        let json = serde_json::to_value(&server_msg).unwrap();
        assert_eq!(json["type"], "openWorkspace");
        assert_eq!(json["path"], "/tmp/project");
        assert!(json.get("host").is_none());
        let ServerMessage::OpenWorkspace { path, host } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected server message variant");
        };
        assert_eq!(path.as_deref(), Some("/tmp/project"));
        assert_eq!(host, None);

        let resolve_op = serde_json::to_value(WorkspaceOperation::ResolvePath {
            path: "~/repo".into(),
        })
        .unwrap();
        assert_eq!(resolve_op["type"], "resolvePath");
        assert_eq!(resolve_op["path"], "~/repo");
        let resolve_res = serde_json::to_value(WorkspaceResult::ResolvedPath {
            path: "/home/u/repo".into(),
        })
        .unwrap();
        assert_eq!(resolve_res["type"], "resolvedPath");
        assert_eq!(resolve_res["path"], "/home/u/repo");
    }

    #[test]
    fn composer_draft_changes_have_stable_wire_keys() {
        let project_id = Uuid::from_u128(7);
        let command = Command::ApplyComposerDraftChanges {
            changes: vec![ComposerDraftChange {
                target: crate::persistence::ComposerDraftTarget::NewSession { project_id },
                draft: Some(crate::persistence::ComposerDraft {
                    text: "unfinished".into(),
                    attachments: Vec::new(),
                }),
            }],
        };
        let json = serde_json::to_value(command).unwrap();

        assert_eq!(json["type"], "applyComposerDraftChanges");
        assert_eq!(json["changes"][0]["target"]["type"], "newSession");
        assert_eq!(
            json["changes"][0]["target"]["projectId"],
            project_id.to_string()
        );
        assert_eq!(json["changes"][0]["draft"]["text"], "unfinished");
    }

    #[test]
    fn prompt_command_serializes_optional_transcript_message_in_camel_case() {
        let plain = serde_json::to_value(Command::Prompt {
            prompt: "hello".into(),
            transcript_message: None,
            turn_id: None,
            message_id: None,
            handoff: None,
        })
        .unwrap();
        assert_eq!(plain["type"], "prompt");
        assert_eq!(plain["prompt"], "hello");
        assert!(plain.get("transcriptMessage").is_none());

        let with_handoff = serde_json::to_value(Command::Prompt {
            prompt: "<handoff>\nhello".into(),
            transcript_message: Some("hello".into()),
            turn_id: None,
            message_id: None,
            handoff: None,
        })
        .unwrap();
        assert_eq!(with_handoff["transcriptMessage"], "hello");
        let Command::Prompt {
            prompt,
            transcript_message,
            ..
        } = serde_json::from_value(with_handoff).unwrap()
        else {
            panic!("unexpected command variant");
        };
        assert_eq!(prompt, "<handoff>\nhello");
        assert_eq!(transcript_message.as_deref(), Some("hello"));
    }

    #[test]
    fn probe_provider_and_fetch_plan_usage_round_trip_with_optional_config_dir() {
        let probe = Command::ProbeProvider {
            provider: ProviderKind::Claude,
            binary_override: Some("/usr/local/bin/claude".into()),
            config_dir: Some("~/.claude-work".into()),
            discover_models: true,
            probe_version: true,
        };
        let json = serde_json::to_value(&probe).unwrap();
        assert_eq!(json["type"], "probeProvider");
        assert_eq!(json["provider"], "claude");
        assert_eq!(json["binaryOverride"], "/usr/local/bin/claude");
        assert_eq!(json["configDir"], "~/.claude-work");
        assert_eq!(json["discoverModels"], true);
        assert_eq!(json["probeVersion"], true);
        let Command::ProbeProvider {
            provider,
            binary_override,
            config_dir,
            discover_models,
            probe_version,
        } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected command variant");
        };
        assert_eq!(provider, ProviderKind::Claude);
        assert_eq!(binary_override.as_deref(), Some("/usr/local/bin/claude"));
        assert_eq!(config_dir.as_deref(), Some("~/.claude-work"));
        assert!(discover_models);
        assert!(probe_version);

        let probe_default = serde_json::to_value(Command::ProbeProvider {
            provider: ProviderKind::Codex,
            binary_override: None,
            config_dir: None,
            discover_models: false,
            probe_version: true,
        })
        .unwrap();
        assert!(probe_default.get("configDir").is_none());
        let Command::ProbeProvider { config_dir, .. } =
            serde_json::from_value(probe_default).unwrap()
        else {
            panic!("unexpected command variant");
        };
        assert_eq!(config_dir, None);

        let usage = Command::FetchPlanUsage {
            provider: ProviderKind::Codex,
            binary_override: Some("/opt/codex".into()),
            config_dir: Some("~/.codex-work".into()),
            cli_version: Some("0.98.0".into()),
        };
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(json["type"], "fetchPlanUsage");
        assert_eq!(json["provider"], "codex");
        assert_eq!(json["binaryOverride"], "/opt/codex");
        assert_eq!(json["configDir"], "~/.codex-work");
        assert_eq!(json["cliVersion"], "0.98.0");
        let Command::FetchPlanUsage {
            provider,
            binary_override,
            config_dir,
            cli_version,
        } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected command variant");
        };
        assert_eq!(provider, ProviderKind::Codex);
        assert_eq!(binary_override.as_deref(), Some("/opt/codex"));
        assert_eq!(config_dir.as_deref(), Some("~/.codex-work"));
        assert_eq!(cli_version.as_deref(), Some("0.98.0"));

        let usage_default = serde_json::to_value(Command::FetchPlanUsage {
            provider: ProviderKind::Claude,
            binary_override: None,
            config_dir: None,
            cli_version: None,
        })
        .unwrap();
        assert!(usage_default.get("configDir").is_none());
        let Command::FetchPlanUsage { config_dir, .. } =
            serde_json::from_value(usage_default).unwrap()
        else {
            panic!("unexpected command variant");
        };
        assert_eq!(config_dir, None);
    }

    #[test]
    fn update_settings_round_trips_provider_accounts() {
        let mut settings = DaemonSettings::default();
        settings.computer_use_enabled = true;
        settings.disabled_providers = vec![ProviderKind::Amp];
        settings
            .provider_binary_overrides
            .insert(ProviderKind::Claude, "/usr/local/bin/claude".into());
        settings.provider_accounts = vec![
            crate::settings::ProviderAccount {
                id: "work".into(),
                provider: ProviderKind::Claude,
                label: "Work".into(),
                config_dir: "~/.claude-work".into(),
                binary_override: Some("/opt/claude-work".into()),
            },
            crate::settings::ProviderAccount {
                id: "personal".into(),
                provider: ProviderKind::Codex,
                label: "Personal".into(),
                config_dir: "~/.codex-personal".into(),
                binary_override: None,
            },
        ];

        let json = serde_json::to_value(Command::UpdateSettings {
            settings: settings.clone(),
        })
        .unwrap();
        assert_eq!(json["type"], "updateSettings");
        assert_eq!(
            json["settings"]["provider_accounts"][0]["configDir"],
            "~/.claude-work"
        );
        assert_eq!(
            json["settings"]["provider_accounts"][0]["binaryOverride"],
            "/opt/claude-work"
        );
        assert!(
            json["settings"]["provider_accounts"][1]
                .get("binaryOverride")
                .is_none()
        );

        let Command::UpdateSettings { settings: decoded } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected command variant");
        };
        assert_eq!(decoded, settings);
    }

    #[test]
    fn wire_driver_start_options_round_trip_with_and_without_config_dir() {
        let options = WireDriverStartOptions {
            provider: "claude".into(),
            binary: PathBuf::from("/usr/local/bin/claude"),
            config_dir: Some("~/.claude-work".into()),
            cwd: PathBuf::from("/tmp/repo"),
            mode: "fullAccess".into(),
            model: Some("claude-sonnet-5".into()),
            reasoning_effort: Some("high".into()),
            service_tier: Some("fast".into()),
            context_window: Some("1m".into()),
            agent_preset: Some("code".into()),
            computer_use_enabled: true,
            provider_cursor: Some(serde_json::json!({
                "provider": "claude",
                "sessionId": "sess-42"
            })),
        };
        let json = serde_json::to_value(Command::Start {
            options: options.clone(),
        })
        .unwrap();
        assert_eq!(json["type"], "start");
        assert_eq!(json["options"]["configDir"], "~/.claude-work");
        assert_eq!(json["options"]["reasoningEffort"], "high");
        assert_eq!(json["options"]["serviceTier"], "fast");
        assert_eq!(json["options"]["contextWindow"], "1m");
        assert_eq!(json["options"]["agentPreset"], "code");
        assert_eq!(json["options"]["computerUseEnabled"], true);

        let Command::Start { options: decoded } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected command variant");
        };
        assert_eq!(decoded.provider, options.provider);
        assert_eq!(decoded.binary, options.binary);
        assert_eq!(decoded.config_dir, options.config_dir);
        assert_eq!(decoded.cwd, options.cwd);
        assert_eq!(decoded.mode, options.mode);
        assert_eq!(decoded.model, options.model);
        assert_eq!(decoded.reasoning_effort, options.reasoning_effort);
        assert_eq!(decoded.service_tier, options.service_tier);
        assert_eq!(decoded.context_window, options.context_window);
        assert_eq!(decoded.agent_preset, options.agent_preset);
        assert_eq!(decoded.computer_use_enabled, options.computer_use_enabled);
        assert_eq!(decoded.provider_cursor, options.provider_cursor);

        let without_config_dir = WireDriverStartOptions {
            config_dir: None,
            ..options
        };
        let json = serde_json::to_value(&without_config_dir).unwrap();
        assert!(json.get("configDir").is_none());
        let decoded: WireDriverStartOptions = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.config_dir, None);
    }

    #[test]
    fn provider_session_fork_request_round_trips_all_five_variants() {
        let source_session = AgentSession::new(Uuid::from_u128(55), ProviderKind::Cursor);

        let claude_with_dir = Command::ForkProviderSession {
            request: ProviderSessionForkRequest::Claude {
                session_id: "claude-sess-1".into(),
                resume_at: Some("msg-3".into()),
                turn_count: 2,
                title: "Forked Claude".into(),
                config_dir: Some(PathBuf::from("/home/user/.claude-work")),
            },
        };
        let json = serde_json::to_value(&claude_with_dir).unwrap();
        assert_eq!(json["type"], "forkProviderSession");
        assert_eq!(json["request"]["provider"], "claude");
        assert_eq!(json["request"]["config_dir"], "/home/user/.claude-work");
        let Command::ForkProviderSession {
            request:
                ProviderSessionForkRequest::Claude {
                    session_id,
                    resume_at,
                    turn_count,
                    title,
                    config_dir,
                },
        } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected fork request variant");
        };
        assert_eq!(session_id, "claude-sess-1");
        assert_eq!(resume_at.as_deref(), Some("msg-3"));
        assert_eq!(turn_count, 2);
        assert_eq!(title, "Forked Claude");
        assert_eq!(config_dir, Some(PathBuf::from("/home/user/.claude-work")));

        let claude_no_dir = serde_json::to_value(ProviderSessionForkRequest::Claude {
            session_id: "claude-sess-2".into(),
            resume_at: None,
            turn_count: 1,
            title: "Default".into(),
            config_dir: None,
        })
        .unwrap();
        assert!(claude_no_dir.get("config_dir").is_none());
        let ProviderSessionForkRequest::Claude { config_dir, .. } =
            serde_json::from_value(claude_no_dir).unwrap()
        else {
            panic!("unexpected fork request variant");
        };
        assert_eq!(config_dir, None);

        let amp = Command::ForkProviderSession {
            request: ProviderSessionForkRequest::Amp {
                binary: PathBuf::from("/usr/local/bin/amp"),
                cwd: PathBuf::from("/tmp/repo"),
                thread_id: "amp-thread-1".into(),
                fork_context: Some("prior summary".into()),
                turn_count: 3,
            },
        };
        let json = serde_json::to_value(&amp).unwrap();
        assert_eq!(json["request"]["provider"], "amp");
        let Command::ForkProviderSession {
            request:
                ProviderSessionForkRequest::Amp {
                    binary,
                    cwd,
                    thread_id,
                    fork_context,
                    turn_count,
                },
        } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected fork request variant");
        };
        assert_eq!(binary, PathBuf::from("/usr/local/bin/amp"));
        assert_eq!(cwd, PathBuf::from("/tmp/repo"));
        assert_eq!(thread_id, "amp-thread-1");
        assert_eq!(fork_context.as_deref(), Some("prior summary"));
        assert_eq!(turn_count, 3);

        let cursor = Command::ForkProviderSession {
            request: ProviderSessionForkRequest::Cursor {
                source: source_session.clone(),
                turn_count: 4,
            },
        };
        let json = serde_json::to_value(&cursor).unwrap();
        assert_eq!(json["request"]["provider"], "cursor");
        let Command::ForkProviderSession {
            request: ProviderSessionForkRequest::Cursor { source, turn_count },
        } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected fork request variant");
        };
        assert_eq!(source.id, source_session.id);
        assert_eq!(source.provider, ProviderKind::Cursor);
        assert_eq!(turn_count, 4);

        let opencode = Command::ForkProviderSession {
            request: ProviderSessionForkRequest::OpenCode {
                binary: PathBuf::from("/usr/local/bin/opencode"),
                session_id: "oc-sess-1".into(),
                turn_count: 5,
            },
        };
        let json = serde_json::to_value(&opencode).unwrap();
        assert_eq!(json["request"]["provider"], "openCode");
        let Command::ForkProviderSession {
            request:
                ProviderSessionForkRequest::OpenCode {
                    binary,
                    session_id,
                    turn_count,
                },
        } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected fork request variant");
        };
        assert_eq!(binary, PathBuf::from("/usr/local/bin/opencode"));
        assert_eq!(session_id, "oc-sess-1");
        assert_eq!(turn_count, 5);

        // Legacy openCode2 tag remains accepted as an alias.
        let legacy_opencode: ProviderSessionForkRequest =
            serde_json::from_value(serde_json::json!({
                "provider": "openCode2",
                "binary": "/usr/local/bin/opencode",
                "session_id": "oc-legacy",
                "turn_count": 2
            }))
            .unwrap();
        assert!(matches!(
            legacy_opencode,
            ProviderSessionForkRequest::OpenCode { session_id, turn_count: 2, .. }
                if session_id == "oc-legacy"
        ));

        let grok = Command::ForkProviderSession {
            request: ProviderSessionForkRequest::Grok {
                binary: PathBuf::from("/usr/local/bin/grok"),
                cwd: PathBuf::from("/tmp/grok-repo"),
                session_id: "grok-sess-1".into(),
                turn_count: 6,
            },
        };
        let json = serde_json::to_value(&grok).unwrap();
        assert_eq!(json["request"]["provider"], "grok");
        let Command::ForkProviderSession {
            request:
                ProviderSessionForkRequest::Grok {
                    binary,
                    cwd,
                    session_id,
                    turn_count,
                },
        } = serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected fork request variant");
        };
        assert_eq!(binary, PathBuf::from("/usr/local/bin/grok"));
        assert_eq!(cwd, PathBuf::from("/tmp/grok-repo"));
        assert_eq!(session_id, "grok-sess-1");
        assert_eq!(turn_count, 6);
    }
}
