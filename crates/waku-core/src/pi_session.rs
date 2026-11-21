//! Pi and Oh My Pi native JSONL session discovery and transcript import.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use serde_json::Value;
use uuid::Uuid;

use crate::model::{
    AgentTurn, Message, MessageRole, ProviderKind, ProviderResumeCursor, ProviderSessionHistory,
    ProviderSessionSummary, TurnStatus,
};

#[derive(Debug)]
struct NativeEntry {
    id: String,
    parent_id: Option<String>,
    kind: String,
    role: Option<String>,
    text: Option<String>,
    stop_reason: Option<String>,
    timestamp: u64,
}

#[derive(Debug)]
struct ParsedSession {
    session_id: String,
    cwd: PathBuf,
    title: Option<String>,
    created_at: u64,
    updated_at: u64,
    entries: Vec<NativeEntry>,
}

fn text_content(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

fn seconds(value: &Value) -> u64 {
    value
        .as_u64()
        .map(|value| {
            if value > 100_000_000_000 {
                value / 1000
            } else {
                value
            }
        })
        .or_else(|| {
            value
                .as_i64()
                .and_then(|value| u64::try_from(value).ok())
                .map(|value| {
                    if value > 100_000_000_000 {
                        value / 1000
                    } else {
                        value
                    }
                })
        })
        .or_else(|| {
            value
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .and_then(|value| u64::try_from(value.timestamp()).ok())
        })
        .unwrap_or_default()
}

fn file_timestamp(path: &Path) -> u64 {
    path.metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

fn parse_session(path: &Path) -> anyhow::Result<ParsedSession> {
    let file = fs::File::open(path)
        .with_context(|| format!("could not open native session {}", path.display()))?;
    let mut session_id = None;
    let mut cwd = None;
    let mut title = None;
    let mut created_at = 0;
    let mut updated_at = file_timestamp(path);
    let mut entries = Vec::new();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "title" {
            if let Some(value) = value
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                title = Some(value.to_owned());
            }
            updated_at = updated_at.max(seconds(value.get("updatedAt").unwrap_or(&Value::Null)));
            continue;
        }
        if kind == "session" {
            session_id = value
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            created_at = seconds(value.get("timestamp").unwrap_or(&Value::Null));
            updated_at = updated_at.max(created_at);
            if title.is_none() {
                title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
            }
            continue;
        }
        if matches!(kind, "session_info" | "title_change") {
            let key = if kind == "session_info" {
                "name"
            } else {
                "title"
            };
            title = value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or(title);
        }

        let Some(id) = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let entry_timestamp = seconds(value.get("timestamp").unwrap_or(&Value::Null));
        let message_timestamp =
            seconds(value.pointer("/message/timestamp").unwrap_or(&Value::Null));
        let timestamp = message_timestamp.max(entry_timestamp);
        updated_at = updated_at.max(timestamp);
        entries.push(NativeEntry {
            id: id.to_owned(),
            parent_id: value
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            kind: kind.to_owned(),
            role: value
                .pointer("/message/role")
                .and_then(Value::as_str)
                .map(str::to_owned),
            text: value.pointer("/message/content").and_then(text_content),
            stop_reason: value
                .pointer("/message/stopReason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            timestamp,
        });
    }

    let session_id = session_id.ok_or_else(|| anyhow!("native session has no header ID"))?;
    let cwd = cwd.ok_or_else(|| anyhow!("native session has no working directory"))?;
    if !cwd.is_absolute() {
        bail!("native session working directory is not absolute");
    }
    let created_at = if created_at == 0 {
        updated_at
    } else {
        created_at
    };
    Ok(ParsedSession {
        session_id,
        cwd,
        title,
        created_at,
        updated_at: updated_at.max(created_at),
        entries,
    })
}

fn active_chain(session: &ParsedSession, provider: ProviderKind) -> Vec<&NativeEntry> {
    let by_id = session
        .entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let Some(mut current) = session.entries.last() else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current.id.as_str()) {
            break;
        }
        chain.push(current);
        let Some(parent) = current.parent_id.as_deref() else {
            break;
        };
        let Some(next) = by_id.get(parent).copied() else {
            break;
        };
        current = next;
    }
    chain.reverse();
    if provider == ProviderKind::OhMyPi
        && let Some(boundary) = chain
            .iter()
            .rposition(|entry| entry.kind == "reset_boundary")
    {
        chain.drain(..=boundary);
    }
    chain
}

fn title_from_prompt(prompt: &str) -> String {
    let mut title = prompt
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    if title.chars().count() > 58 {
        title = format!("{}…", title.chars().take(57).collect::<String>());
    }
    title
}

fn summary_from_session(
    provider: ProviderKind,
    path: &Path,
    session: &ParsedSession,
) -> Option<ProviderSessionSummary> {
    let first_prompt = active_chain(session, provider)
        .into_iter()
        .find(|entry| entry.kind == "message" && entry.role.as_deref() == Some("user"))
        .and_then(|entry| entry.text.as_deref())
        .map(title_from_prompt)
        .filter(|title| !title.is_empty());
    let title = session
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .or(first_prompt)?;
    let cursor = match provider {
        ProviderKind::Pi => ProviderResumeCursor::Pi {
            session_id: session.session_id.clone(),
            session_file: Some(path.to_path_buf()),
        },
        ProviderKind::OhMyPi => ProviderResumeCursor::OhMyPi {
            session_id: session.session_id.clone(),
            session_file: Some(path.to_path_buf()),
        },
        _ => return None,
    };
    Some(ProviderSessionSummary {
        cursor,
        title,
        cwd: session.cwd.clone(),
        created_at: session.created_at,
        updated_at: session.updated_at,
    })
}

fn env_var_from(
    process_value: Option<OsString>,
    shell_env: &[(OsString, OsString)],
    name: &str,
) -> Option<OsString> {
    if let Some(value) = process_value
        .filter(|value| !value.is_empty() && value.to_str().is_none_or(|s| !s.trim().is_empty()))
    {
        return Some(value);
    }
    shell_env
        .iter()
        .rev()
        .find(|(key, value)| {
            !value.is_empty()
                && value.to_str().is_none_or(|s| !s.trim().is_empty())
                && {
                    #[cfg(windows)]
                    {
                        key.to_string_lossy().eq_ignore_ascii_case(name)
                    }
                    #[cfg(not(windows))]
                    {
                        key == name
                    }
                }
        })
        .map(|(_, value)| value.clone())
}

fn session_roots_with(
    provider: ProviderKind,
    home: &Path,
    process_agent_dir: Option<OsString>,
    process_session_dir: Option<OsString>,
    shell_env: &[(OsString, OsString)],
) -> anyhow::Result<Vec<PathBuf>> {
    let expand_home = |path: PathBuf| {
        if path == Path::new("~") {
            return home.to_path_buf();
        }
        path.strip_prefix("~")
            .ok()
            .map(|suffix| home.join(suffix))
            .unwrap_or(path)
    };
    let configured_agent = env_var_from(process_agent_dir, shell_env, "PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .map(&expand_home);
    match provider {
        ProviderKind::Pi => {
            if let Some(path) = env_var_from(
                process_session_dir,
                shell_env,
                "PI_CODING_AGENT_SESSION_DIR",
            ) {
                return Ok(vec![expand_home(PathBuf::from(path))]);
            }
            let agent = configured_agent.unwrap_or_else(|| home.join(".pi/agent"));
            let default = agent.join("sessions");
            let settings = agent.join("settings.json");
            let configured = fs::read(&settings)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|value| value.get("sessionDir")?.as_str().map(PathBuf::from))
                .map(&expand_home)
                .filter(|path| path.is_absolute());
            Ok(configured.into_iter().chain([default]).collect())
        }
        ProviderKind::OhMyPi => {
            let mut roots = vec![
                configured_agent
                    .unwrap_or_else(|| home.join(".omp/agent"))
                    .join("sessions"),
            ];
            let profiles = home.join(".omp/profiles");
            if let Ok(entries) = fs::read_dir(profiles) {
                roots.extend(
                    entries
                        .flatten()
                        .map(|entry| entry.path().join("agent/sessions")),
                );
            }
            Ok(roots)
        }
        _ => bail!("{} is not a Pi session provider", provider.display_name()),
    }
}

fn session_roots(provider: ProviderKind) -> anyhow::Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory could not be located"))?;
    let shell_env = crate::command_env::shell_environment();
    session_roots_with(
        provider,
        &home,
        std::env::var_os("PI_CODING_AGENT_DIR"),
        std::env::var_os("PI_CODING_AGENT_SESSION_DIR"),
        &shell_env,
    )
}

fn session_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            files.push(path);
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        if let Ok(children) = fs::read_dir(path) {
            files.extend(
                children.flatten().map(|entry| entry.path()).filter(|path| {
                    path.extension().and_then(|value| value.to_str()) == Some("jsonl")
                }),
            );
        }
    }
    files
}

pub fn list_provider_sessions(
    provider: ProviderKind,
    limit: usize,
) -> anyhow::Result<Vec<ProviderSessionSummary>> {
    let mut seen = HashSet::new();
    let mut sessions = Vec::new();
    for root in session_roots(provider)? {
        for path in session_files(&root) {
            let Ok(parsed) = parse_session(&path) else {
                continue;
            };
            if !seen.insert(parsed.session_id.clone()) {
                continue;
            }
            if let Some(summary) = summary_from_session(provider, &path, &parsed) {
                sessions.push(summary);
            }
        }
    }
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.truncate(limit);
    Ok(sessions)
}

fn status_from_reason(reason: Option<&str>) -> TurnStatus {
    match reason {
        Some("error") => TurnStatus::Failed,
        Some("aborted" | "cancelled" | "length") => TurnStatus::Interrupted,
        _ => TurnStatus::Completed,
    }
}

fn history_from_session(provider: ProviderKind, session: &ParsedSession) -> ProviderSessionHistory {
    let mut history = ProviderSessionHistory::default();
    for entry in active_chain(session, provider) {
        if entry.kind == "message" && entry.role.as_deref() == Some("user") {
            let turn_id = Uuid::new_v4();
            history.turns.push(AgentTurn {
                id: turn_id,
                turn_count: history.turns.len() + 1,
                status: TurnStatus::Interrupted,
                provider_turn_started: true,
                provider_resume_at: None,
                started_at: entry.timestamp,
                completed_at: None,
                checkpoint: None,
                provider: Some(provider),
                account_id: None,
                handoff: None,
                native_conversation_start: false,
                stream_stats: None,
                handoff_preamble: None,
            });
            if let Some(text) = entry.text.as_deref() {
                let mut message = Message::new_for_turn(MessageRole::User, text, turn_id);
                message.created_at = entry.timestamp;
                history.messages.push(message);
            }
            continue;
        }
        if entry.kind != "message" || entry.role.as_deref() != Some("assistant") {
            continue;
        }
        let Some(turn) = history.turns.last_mut() else {
            continue;
        };
        turn.status = status_from_reason(entry.stop_reason.as_deref());
        turn.completed_at = Some(entry.timestamp.max(turn.started_at));
        let Some(text) = entry.text.as_deref() else {
            continue;
        };
        if let Some(previous) = history.messages.last_mut().filter(|message| {
            message.role == MessageRole::Assistant && message.turn_id == Some(turn.id)
        }) {
            if !previous.content.is_empty() {
                previous.content.push_str("\n\n");
            }
            previous.content.push_str(text);
            previous.created_at = previous.created_at.max(entry.timestamp);
        } else {
            let mut message = Message::new_for_turn(MessageRole::Assistant, text, turn.id);
            message.created_at = entry.timestamp;
            history.messages.push(message);
        }
    }
    history
}

fn validate_session_file_in(
    provider: ProviderKind,
    session_file: &Path,
    roots: &[PathBuf],
) -> anyhow::Result<PathBuf> {
    if session_file.as_os_str().is_empty() || session_file.to_string_lossy().contains('\0') {
        bail!("{} session file path is invalid", provider.display_name());
    }
    let canonical_file = fs::canonicalize(session_file).with_context(|| {
        format!(
            "could not resolve {} session file {}",
            provider.display_name(),
            session_file.display()
        )
    })?;
    let metadata = fs::metadata(&canonical_file).with_context(|| {
        format!(
            "could not inspect {} session file {}",
            provider.display_name(),
            session_file.display()
        )
    })?;
    if !metadata.is_file() {
        bail!(
            "{} session file {} is not a regular file",
            provider.display_name(),
            session_file.display()
        );
    }
    let inside_root = roots.iter().any(|root| {
        fs::canonicalize(root)
            .is_ok_and(|canonical_root| canonical_file.starts_with(&canonical_root))
    });
    if !inside_root {
        bail!(
            "{} session file {} is outside the allowed session directories",
            provider.display_name(),
            session_file.display()
        );
    }
    Ok(canonical_file)
}

fn provider_session_history_in(
    provider: ProviderKind,
    session_id: &str,
    session_file: &Path,
    visible_turn_limit: usize,
    roots: &[PathBuf],
) -> anyhow::Result<ProviderSessionHistory> {
    let canonical_file = validate_session_file_in(provider, session_file, roots)?;
    let session = parse_session(&canonical_file)?;
    if session.session_id != session_id {
        bail!(
            "{} session file belongs to a different session",
            provider.display_name()
        );
    }
    let mut history = history_from_session(provider, &session);
    let retained = history
        .turns
        .iter()
        .rev()
        .take(visible_turn_limit)
        .map(|turn| turn.id)
        .collect::<HashSet<_>>();
    history
        .messages
        .retain(|message| message.turn_id.is_some_and(|id| retained.contains(&id)));
    Ok(history)
}

pub fn provider_session_history(
    provider: ProviderKind,
    session_id: &str,
    session_file: &Path,
    visible_turn_limit: usize,
) -> anyhow::Result<ProviderSessionHistory> {
    let roots = session_roots(provider)?;
    provider_session_history_in(
        provider,
        session_id,
        session_file,
        visible_turn_limit,
        &roots,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(lines: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waku-pi-session-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    #[test]
    fn imports_the_active_pi_branch_without_reasoning_or_tools() {
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "session-1",
            "timestamp": "2026-01-01T00:00:00Z",
            "cwd": std::env::temp_dir(),
        })
        .to_string();
        let path = write_session(&[
            &header,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"one","timestamp":1767225601000}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private"},{"type":"text","text":"answer"}],"stopReason":"stop"}}"#,
            r#"{"type":"message","id":"old","parentId":"u1","timestamp":"2026-01-01T00:00:03Z","message":{"role":"user","content":"abandoned"}}"#,
            r#"{"type":"message","id":"u2","parentId":"a1","timestamp":"2026-01-01T00:00:04Z","message":{"role":"user","content":"two"}}"#,
        ]);
        let session = parse_session(&path).unwrap();
        let history = history_from_session(ProviderKind::Pi, &session);
        assert_eq!(history.turns.len(), 2);
        assert_eq!(history.messages.len(), 3);
        assert_eq!(history.messages[1].content, "answer");
        assert!(
            history
                .messages
                .iter()
                .all(|message| message.content != "abandoned")
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oh_my_pi_reset_hides_the_earlier_transcript_and_uses_the_title_slot() {
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "session-2",
            "timestamp": "2026-01-01T00:00:00Z",
            "cwd": std::env::temp_dir(),
        })
        .to_string();
        let path = write_session(&[
            r#"{"type":"title","v":1,"title":"Current title","updatedAt":"2026-01-01T00:00:05Z","pad":""}"#,
            &header,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"old"}}"#,
            r#"{"type":"reset_boundary","id":"r1","parentId":"u1","timestamp":"2026-01-01T00:00:02Z"}"#,
            r#"{"type":"message","id":"u2","parentId":"r1","timestamp":"2026-01-01T00:00:03Z","message":{"role":"user","content":"new"}}"#,
        ]);
        let session = parse_session(&path).unwrap();
        let summary = summary_from_session(ProviderKind::OhMyPi, &path, &session).unwrap();
        let history = history_from_session(ProviderKind::OhMyPi, &session);
        assert_eq!(summary.title, "Current title");
        assert_eq!(history.turns.len(), 1);
        assert_eq!(history.messages[0].content, "new");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn active_chain_terminates_on_cyclic_parent_ids() {
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "session-cycle",
            "timestamp": "2026-01-01T00:00:00Z",
            "cwd": std::env::temp_dir(),
        })
        .to_string();

        // Self-cycle: u1 -> u1
        let self_cycle_path = write_session(&[
            &header,
            r#"{"type":"message","id":"u1","parentId":"u1","timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"self"}}"#,
        ]);
        let self_cycle = parse_session(&self_cycle_path).unwrap();
        let chain = active_chain(&self_cycle, ProviderKind::Pi);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, "u1");
        let _ = fs::remove_dir_all(self_cycle_path.parent().unwrap());

        // Lasso cycle: u2 -> a1 -> u1 -> a1
        let lasso_path = write_session(&[
            &header,
            r#"{"type":"message","id":"u1","parentId":"a1","timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"first"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":"reply"}}"#,
            r#"{"type":"message","id":"u2","parentId":"a1","timestamp":"2026-01-01T00:00:03Z","message":{"role":"user","content":"second"}}"#,
        ]);
        let lasso = parse_session(&lasso_path).unwrap();
        let chain = active_chain(&lasso, ProviderKind::Pi);
        assert_eq!(
            chain.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["u1", "a1", "u2"]
        );
        let history = history_from_session(ProviderKind::Pi, &lasso);
        assert_eq!(history.turns.len(), 2);
        let _ = fs::remove_dir_all(lasso_path.parent().unwrap());
    }

    #[test]
    fn provider_session_history_confines_reads_to_allowed_roots() {
        let base = std::env::temp_dir().join(format!("waku-pi-sec-{}", Uuid::new_v4()));
        let allowed_root = base.join("sessions");
        let nested_dir = allowed_root.join("project-a");
        let outside_dir = base.join("outside");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let header = |id: &str| {
            serde_json::json!({
                "type": "session",
                "version": 3,
                "id": id,
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": base,
            })
            .to_string()
        };
        let inside_file = nested_dir.join("ok.jsonl");
        fs::write(
            &inside_file,
            format!(
                "{}\n{}\n",
                header("inside-id"),
                r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"hello"}}"#
            ),
        )
        .unwrap();
        let outside_file = outside_dir.join("secret.jsonl");
        fs::write(
            &outside_file,
            format!(
                "{}\n{}\n",
                header("outside-id"),
                r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"secret"}}"#
            ),
        )
        .unwrap();

        let roots = vec![allowed_root.clone()];
        let history =
            provider_session_history_in(ProviderKind::Pi, "inside-id", &inside_file, 10, &roots)
                .unwrap();
        assert_eq!(history.messages.len(), 1);
        assert_eq!(history.messages[0].content, "hello");

        // Direct outside path and traversal path must both fail.
        assert!(
            provider_session_history_in(ProviderKind::Pi, "outside-id", &outside_file, 10, &roots)
                .is_err()
        );
        let traversal = allowed_root.join("../outside/secret.jsonl");
        assert!(
            provider_session_history_in(ProviderKind::Pi, "outside-id", &traversal, 10, &roots)
                .is_err()
        );

        // Symlink inside allowed_root pointing outside must fail.
        let link = allowed_root.join("escape.jsonl");
        if crate::fs_ext::symlink(&outside_file, &link).is_ok() {
            assert!(
                provider_session_history_in(ProviderKind::Pi, "outside-id", &link, 10, &roots)
                    .is_err()
            );
        }

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn session_roots_falls_back_to_shell_environment() {
        let home = PathBuf::from("/fake/home");
        let shell_env = vec![
            (
                OsString::from("PI_CODING_AGENT_DIR"),
                OsString::from("~/custom-agent"),
            ),
            (
                OsString::from("PI_CODING_AGENT_SESSION_DIR"),
                OsString::from("~/custom-sessions"),
            ),
        ];

        assert_eq!(
            session_roots_with(
                ProviderKind::Pi,
                &home,
                Some(OsString::from("")),
                Some(OsString::from("")),
                &shell_env,
            )
            .unwrap(),
            vec![home.join("custom-sessions")]
        );
        assert_eq!(
            session_roots_with(
                ProviderKind::OhMyPi,
                &home,
                Some(OsString::from("")),
                None,
                &shell_env,
            )
            .unwrap(),
            vec![home.join("custom-agent").join("sessions")]
        );
        assert_eq!(
            session_roots_with(
                ProviderKind::Pi,
                &home,
                None,
                Some(OsString::from("/proc/sessions")),
                &shell_env,
            )
            .unwrap(),
            vec![PathBuf::from("/proc/sessions")]
        );
    }
}
