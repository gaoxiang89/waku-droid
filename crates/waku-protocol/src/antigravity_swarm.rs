//! Read-only wire snapshot types for Antigravity native swarm boards.
//!
//! These structures represent the on-disk state stored under an Antigravity
//! conversation's `.system_generated/swarm/` directory (`members.json`,
//! `board.jsonl`, optional `view.json`, and member `transcript.jsonl` tails),
//! delivered from the daemon over [`crate::WorkspaceOperation::ReadAntigravitySwarm`]
//! so local, SSH, and WSL workspaces work identically without touching the UI
//! thread.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Live lifecycle status of one swarm member, derived from `members.json` and
/// the member's `transcript.jsonl` tail.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum AntigravitySwarmMemberStatus {
    Working,
    #[default]
    Waiting,
    Killed,
    Failed,
}

/// Classification of a member's latest failure step.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum AntigravitySwarmFailureKind {
    Error,
    Quota,
}

/// Failure details extracted from a member's newest transcript step.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySwarmFailure {
    pub kind: AntigravitySwarmFailureKind,
    pub message: String,
}

/// One member in the swarm roster.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySwarmMember {
    pub name: String,
    pub conversation_id: String,
    pub model: String,
    pub role: String,
    /// System observer roles (e.g., `narrator`, `monitor`) excluded from
    /// task-worker counts and status summaries.
    #[serde(default)]
    pub is_role_worker: bool,
    pub wake_mode: String,
    pub status: AntigravitySwarmMemberStatus,
    pub killed: bool,
    pub last_read_seq: u64,
    pub unread: u64,
    pub posts: u64,
    pub steps: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<AntigravitySwarmFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_mtime_ms: Option<u64>,
}

/// One message entry from `board.jsonl`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySwarmMessage {
    pub seq: u64,
    pub agent: String,
    pub text: String,
    pub time: String,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default)]
    pub mentions_all: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// Optional phase progress reported by the narrator's `view.json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySwarmPhase {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
}

/// An attention item surfaced either by the narrator (`view.json`) or by
/// system transcript checks (errors, quota exhaustion, stuck workers).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySwarmAttentionItem {
    pub id: String,
    pub kind: String,
    pub severity: String,
    pub title: String,
    #[serde(default)]
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
}

/// One tracked workstream from the narrator's `view.json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySwarmWorkstream {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub owners: Vec<String>,
    pub state: String,
    #[serde(default)]
    pub note: String,
}

/// One milestone or risk entry from the narrator's `view.json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySwarmViewItem {
    pub id: String,
    pub text: String,
}

/// Parsed, sanitized summary from `view.json` when a narrator has published one.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySwarmOverview {
    pub as_of_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub headline: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<AntigravitySwarmPhase>,
    #[serde(default)]
    pub attention: Vec<AntigravitySwarmAttentionItem>,
    #[serde(default)]
    pub workstreams: Vec<AntigravitySwarmWorkstream>,
    #[serde(default)]
    pub milestones: Vec<AntigravitySwarmViewItem>,
    #[serde(default)]
    pub risks: Vec<AntigravitySwarmViewItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_ack: Option<String>,
}

/// Complete read-only snapshot of an Antigravity native swarm.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySwarmSnapshot {
    /// Coordinator conversation ID owning this swarm directory.
    pub swarm_id: String,
    /// Roster name of the conversation that requested this snapshot, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_member: Option<String>,
    pub title: String,
    pub head_seq: u64,
    pub total_messages: u64,
    #[serde(default)]
    pub messages_truncated: bool,
    pub members: Vec<AntigravitySwarmMember>,
    pub messages: Vec<AntigravitySwarmMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<AntigravitySwarmOverview>,
    #[serde(default)]
    pub system_attention: Vec<AntigravitySwarmAttentionItem>,
    /// Change token computed from on-disk metadata so clients can skip
    /// redundant list resets when polls observe identical state.
    pub fingerprint: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{WorkspaceOperation, WorkspaceResult};

    #[test]
    fn swarm_snapshot_roundtrips_through_json_and_workspace_wire() {
        let snapshot = AntigravitySwarmSnapshot {
            swarm_id: "3669f3f6-500f-4952-8e81-3ff1cd80366c".to_owned(),
            current_member: Some("coordinator".to_owned()),
            title: "Antigravity CLI & Swarm".to_owned(),
            head_seq: 12,
            total_messages: 12,
            messages_truncated: false,
            members: vec![
                AntigravitySwarmMember {
                    name: "coordinator".to_owned(),
                    conversation_id: "3669f3f6-500f-4952-8e81-3ff1cd80366c".to_owned(),
                    model: "MODEL_PLACEHOLDER".to_owned(),
                    role: "coordinator".to_owned(),
                    is_role_worker: false,
                    wake_mode: "mentions".to_owned(),
                    status: AntigravitySwarmMemberStatus::Waiting,
                    killed: false,
                    last_read_seq: 12,
                    unread: 0,
                    posts: 5,
                    steps: 14,
                    activity: None,
                    activity_kind: None,
                    failure: None,
                    transcript_mtime_ms: Some(1_763_882_700_000),
                },
                AntigravitySwarmMember {
                    name: "swarm-ui".to_owned(),
                    conversation_id: "worker-1".to_owned(),
                    model: "MODEL_PLACEHOLDER".to_owned(),
                    role: "worker".to_owned(),
                    is_role_worker: false,
                    wake_mode: "working".to_owned(),
                    status: AntigravitySwarmMemberStatus::Failed,
                    killed: false,
                    last_read_seq: 10,
                    unread: 2,
                    posts: 3,
                    steps: 22,
                    activity: Some("Unit test execution".to_owned()),
                    activity_kind: Some("tool".to_owned()),
                    failure: Some(AntigravitySwarmFailure {
                        kind: AntigravitySwarmFailureKind::Quota,
                        message: "RESOURCE_EXHAUSTED".to_owned(),
                    }),
                    transcript_mtime_ms: Some(1_763_882_705_000),
                },
            ],
            messages: vec![AntigravitySwarmMessage {
                seq: 12,
                agent: "swarm-ui".to_owned(),
                text: "@coordinator [done] Swarm panel ready".to_owned(),
                time: "2026-11-23 15:30:00".to_owned(),
                mentions: vec!["coordinator".to_owned()],
                mentions_all: false,
                kind: None,
                channel: None,
            }],
            overview: Some(AntigravitySwarmOverview {
                as_of_seq: 12,
                updated_at: Some("2026-11-23T15:30:00Z".to_owned()),
                headline: "All tracks converging".to_owned(),
                phase: Some(AntigravitySwarmPhase {
                    label: "Implementation".to_owned(),
                    index: Some(2),
                    total: Some(3),
                }),
                attention: vec![AntigravitySwarmAttentionItem {
                    id: "att-1".to_owned(),
                    kind: "review".to_owned(),
                    severity: "med".to_owned(),
                    title: "Verify protocol bindings".to_owned(),
                    why: "New workspace operation added".to_owned(),
                    member: None,
                }],
                workstreams: vec![AntigravitySwarmWorkstream {
                    id: "ws-1".to_owned(),
                    title: "Swarm UI".to_owned(),
                    owners: vec!["swarm-ui".to_owned()],
                    state: "active".to_owned(),
                    note: "Building panel".to_owned(),
                }],
                milestones: vec![AntigravitySwarmViewItem {
                    id: "m-1".to_owned(),
                    text: "Protocol types landed".to_owned(),
                }],
                risks: vec![AntigravitySwarmViewItem {
                    id: "r-1".to_owned(),
                    text: "Quota ceiling".to_owned(),
                }],
                feedback_ack: Some("Acknowledged #8".to_owned()),
            }),
            system_attention: vec![AntigravitySwarmAttentionItem {
                id: "sys-swarm-ui-quota".to_owned(),
                kind: "quota".to_owned(),
                severity: "high".to_owned(),
                title: "swarm-ui: last step failed (quota)".to_owned(),
                why: "RESOURCE_EXHAUSTED".to_owned(),
                member: Some("swarm-ui".to_owned()),
            }],
            fingerprint: "fp-12".to_owned(),
        };

        let op = WorkspaceOperation::ReadAntigravitySwarm {
            session_id: snapshot.swarm_id.clone(),
            config_dir: Some("~/.gemini/antigravity-work".into()),
        };
        let op_json = serde_json::to_string(&op).expect("serialize operation");
        let op_decoded: WorkspaceOperation =
            serde_json::from_str(&op_json).expect("deserialize operation");
        assert!(matches!(
            op_decoded,
            WorkspaceOperation::ReadAntigravitySwarm {
                session_id,
                config_dir: Some(config_dir),
            } if session_id == snapshot.swarm_id && config_dir == "~/.gemini/antigravity-work"
        ));

        let result = WorkspaceResult::AntigravitySwarm {
            snapshot: Some(snapshot.clone()),
        };
        let res_json = serde_json::to_string(&result).expect("serialize result");
        let res_decoded: WorkspaceResult =
            serde_json::from_str(&res_json).expect("deserialize result");
        match res_decoded {
            WorkspaceResult::AntigravitySwarm {
                snapshot: Some(decoded),
            } => assert_eq!(decoded, snapshot),
            other => panic!("unexpected workspace result: {other:?}"),
        }
    }
}
