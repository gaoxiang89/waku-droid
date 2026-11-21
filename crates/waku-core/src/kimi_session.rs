//! Kimi Code native-session helpers.
//!
//! Kimi reports an upstream provider failure — an expired plan, a rejected
//! key, a quota wall — only in its own per-session wire log. Over ACP the same
//! turn simply returns `end_turn` carrying no content at all, with nothing on
//! stderr and no JSON-RPC error, so a client that trusts the protocol shows an
//! empty answer and calls it a success. Reading the wire log is what lets Waku
//! name the real cause instead.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::model::{ProviderResumeCursor, ProviderSessionSummary};

/// Kimi appends the turn record just after the ACP response returns — about
/// 50ms in practice — so the lookup has to outlast that gap without stalling a
/// turn that simply has nothing to report.
const TURN_RECORD_WAIT: Duration = Duration::from_millis(1000);
const TURN_RECORD_POLL: Duration = Duration::from_millis(25);

/// How much of the tail to scan. A turn record is written last, so the end of
/// the file is the only part worth reading, and a turn's own transcript can be
/// arbitrarily large.
const WIRE_TAIL_BYTES: u64 = 128 * 1024;

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

fn session_home_from(
    process_value: Option<OsString>,
    shell_env: &[(OsString, OsString)],
    home_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    env_var_from(process_value, shell_env, "KIMI_CODE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir.map(|home| home.join(".kimi-code")))
}

/// Kimi's data directory, matching the CLI's own `KIMI_CODE_HOME` override.
pub fn session_home() -> Option<PathBuf> {
    session_home_from(
        std::env::var_os("KIMI_CODE_HOME"),
        &crate::command_env::shell_environment(),
        dirs::home_dir(),
    )
}

fn provider_timestamp(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_u64)
        .map(|value| {
            if value > 100_000_000_000 {
                value / 1_000
            } else {
                value
            }
        })
        .unwrap_or_default()
}

fn catalog_title(state: &Value, session_id: &str, cwd: &Path) -> String {
    ["title", "lastPrompt"]
        .into_iter()
        .find_map(|key| {
            state
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.lines().next().unwrap_or(value))
                .map(|value| value.chars().take(120).collect::<String>())
        })
        .or_else(|| {
            cwd.file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            format!(
                "Kimi session {}",
                session_id.chars().take(8).collect::<String>()
            )
        })
}

fn catalog_summary(state_path: &Path) -> Option<ProviderSessionSummary> {
    // Do not follow a provider-store symlink into an arbitrary workspace. The
    // catalog must remain confined to Kimi's own hidden data directory.
    if !fs::symlink_metadata(state_path).ok()?.file_type().is_file() {
        return None;
    }
    let state = serde_json::from_slice::<Value>(&fs::read(state_path).ok()?).ok()?;
    if state.get("archived").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let session_id = state
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_owned();
    let cwd = PathBuf::from(state.get("cwd").and_then(Value::as_str)?);
    if !cwd.is_absolute() {
        return None;
    }
    let created_at = provider_timestamp(state.get("createdAt"));
    let updated_at = provider_timestamp(state.get("updatedAt")).max(created_at);
    Some(ProviderSessionSummary {
        cursor: ProviderResumeCursor::Kimi {
            session_id: session_id.clone(),
        },
        title: catalog_title(&state, &session_id, &cwd),
        cwd,
        created_at,
        updated_at,
    })
}

fn list_provider_sessions_in(home: &Path, limit: usize) -> Vec<ProviderSessionSummary> {
    let Ok(workspaces) = fs::read_dir(home.join("sessions")) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut sessions = Vec::new();
    for workspace in workspaces.flatten() {
        if !workspace.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(workspace.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(summary) = catalog_summary(&entry.path().join("state.json")) else {
                continue;
            };
            if seen.insert(summary.cursor.native_id().to_owned()) {
                sessions.push(summary);
            }
        }
    }
    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    sessions.truncate(limit);
    sessions
}

/// Read Kimi's provider-owned state files without opening any recorded cwd.
/// ACP `session/list` is workspace-scoped and makes the agent inspect each
/// historical project, which can trigger macOS Desktop/Downloads/Documents
/// permission dialogs just by opening Waku's Resume picker.
pub fn list_provider_sessions(limit: usize) -> anyhow::Result<Vec<ProviderSessionSummary>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let Some(home) = session_home() else {
        return Ok(Vec::new());
    };
    Ok(list_provider_sessions_in(&home, limit))
}

/// Where the current length of a session's wire log is read, so a later
/// failure lookup can ignore everything an earlier turn already wrote.
pub fn wire_offset(session_id: &str) -> u64 {
    session_home()
        .and_then(|home| wire_log(&home, session_id))
        .and_then(|path| path.metadata().ok())
        .map_or(0, |metadata| metadata.len())
}

/// The provider error that ended this turn, or `None` when the turn did not
/// record a failure. Polls briefly because the record lands just after the ACP
/// response; call it only for a turn that produced nothing, so a healthy turn
/// never pays the wait.
pub fn turn_failure(session_id: &str, from_offset: u64) -> Option<String> {
    let home = session_home()?;
    turn_failure_in(&home, session_id, from_offset, TURN_RECORD_WAIT)
}

fn turn_failure_in(
    home: &Path,
    session_id: &str,
    from_offset: u64,
    wait: Duration,
) -> Option<String> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(path) = wire_log(home, session_id)
            && let Some(failure) = recorded_failure(&path, from_offset)
        {
            return Some(failure);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(TURN_RECORD_POLL);
    }
}

/// Validates that a Kimi session ID is a single safe path segment on every
/// platform, rejecting path separators, traversal sequences, NUL bytes, and
/// drive or root prefixes.
fn is_safe_session_id(session_id: &str) -> bool {
    if session_id.is_empty()
        || session_id.contains(['/', '\\', '\0', ':'])
        || session_id.contains("..")
        || matches!(session_id, "." | "..")
    {
        return false;
    }
    let mut components = Path::new(session_id).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

/// Sessions are filed under a per-workspace directory whose name carries a
/// hash Waku cannot reproduce, so the session id is matched by scanning.
fn wire_log(home: &Path, session_id: &str) -> Option<PathBuf> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    std::fs::read_dir(home.join("sessions"))
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .find_map(|entry| {
            let candidate = entry
                .path()
                .join(session_id)
                .join("agents")
                .join("main")
                .join("wire.jsonl");
            candidate.is_file().then_some(candidate)
        })
}

fn recorded_failure(path: &Path, from_offset: u64) -> Option<String> {
    parse_turn_failure(&read_tail(path, from_offset)?)
}

fn read_tail(path: &Path, from_offset: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    if length <= from_offset {
        return None;
    }
    let start = from_offset.max(length.saturating_sub(WIRE_TAIL_BYTES));
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::with_capacity((length - start) as usize);
    file.read_to_end(&mut tail).ok()?;
    Some(String::from_utf8_lossy(&tail).into_owned())
}

/// The last `turn.ended` record in the scanned region, when it failed. A
/// partially flushed final line is simply skipped and retried by the caller.
fn parse_turn_failure(tail: &str) -> Option<String> {
    tail.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record.get("type").and_then(Value::as_str) == Some("turn.ended"))
        .filter(|record| record.get("reason").and_then(Value::as_str) == Some("failed"))
        .and_then(|record| {
            let error = record.get("error")?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|message| !message.is_empty())?;
            Some(message.to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_provider_error_off_a_failed_turn() {
        let tail = concat!(
            r#"{"type":"turn.prompt","turnId":0}"#,
            "\n",
            r#"{"type":"turn.ended","turnId":0,"reason":"failed","error":{"code":"provider.api_error","message":"402 membership inactive","retryable":false}}"#,
            "\n"
        );

        assert_eq!(
            parse_turn_failure(tail).as_deref(),
            Some("402 membership inactive")
        );
    }

    #[test]
    fn ignores_a_turn_that_did_not_fail() {
        let tail = concat!(
            r#"{"type":"turn.ended","turnId":0,"reason":"failed","error":{"message":"stale"}}"#,
            "\n",
            r#"{"type":"turn.ended","turnId":1,"reason":"completed"}"#,
            "\n"
        );

        assert!(parse_turn_failure(tail).is_none());
    }

    #[test]
    fn waits_out_a_record_that_has_not_been_flushed_yet() {
        // The last line is still being written, so it does not parse. The
        // caller must not read the previous turn's failure as this turn's.
        let tail = concat!(
            r#"{"type":"turn.ended","turnId":0,"reason":"completed"}"#,
            "\n",
            r#"{"type":"turn.ended","turnId":1,"reas"#
        );

        assert!(parse_turn_failure(tail).is_none());
    }

    #[test]
    fn a_missing_session_never_blocks_past_its_deadline() {
        let started = Instant::now();
        let failure = turn_failure_in(
            Path::new("/nonexistent-kimi-home"),
            "session_missing",
            0,
            Duration::from_millis(80),
        );

        assert!(failure.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn lists_kimi_owned_state_without_opening_the_recorded_workspace() {
        let root = std::env::temp_dir().join(format!("waku-kimi-catalog-{}", uuid::Uuid::new_v4()));
        let session = root.join("sessions/wd-protected/session-native");
        let recorded_cwd = std::env::temp_dir().join(format!(
            "waku-kimi-protected-workspace-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("state.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "session-native",
                "title": "Protected workspace session",
                "cwd": recorded_cwd,
                "createdAt": 1_700_000_000_000_u64,
                "updatedAt": 1_700_000_100_000_u64,
                "archived": false
            }))
            .unwrap(),
        )
        .unwrap();

        let sessions = list_provider_sessions_in(&root, 10);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].cursor.native_id(), "session-native");
        assert_eq!(sessions[0].created_at, 1_700_000_000);
        assert_eq!(sessions[0].updated_at, 1_700_000_100);
        assert_eq!(sessions[0].cwd, recorded_cwd);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn wire_log_rejects_cross_platform_traversal_and_unsafe_session_ids() {
        let root = std::env::temp_dir().join(format!("waku-kimi-wire-sec-{}", uuid::Uuid::new_v4()));
        let valid_wire = root.join("sessions/ws-hash/valid-session_1/agents/main/wire.jsonl");
        let escaped_wire = root.join("sessions/agents/main/wire.jsonl");
        let sibling_wire = root.join("sessions/other/agents/main/wire.jsonl");
        fs::create_dir_all(valid_wire.parent().unwrap()).unwrap();
        fs::create_dir_all(escaped_wire.parent().unwrap()).unwrap();
        fs::create_dir_all(sibling_wire.parent().unwrap()).unwrap();
        fs::write(&valid_wire, "{}\n").unwrap();
        fs::write(&escaped_wire, "{}\n").unwrap();
        fs::write(&sibling_wire, "{}\n").unwrap();

        assert_eq!(wire_log(&root, "valid-session_1"), Some(valid_wire));

        for malicious in [
            "",
            ".",
            "..",
            "../other",
            "..\\other",
            "a/b",
            "a\\b",
            "foo..bar",
            "/etc/passwd",
            "\\Windows\\System32",
            "C:\\Windows\\System32",
            "C:relative",
            "valid\0bad",
        ] {
            assert!(
                !is_safe_session_id(malicious),
                "expected `{malicious:?}` to be rejected as unsafe session id"
            );
            assert!(
                wire_log(&root, malicious).is_none(),
                "expected wire_log to reject `{malicious:?}`"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn session_home_filters_empty_env_and_falls_back_to_shell_environment() {
        let home = PathBuf::from("/fake/home");
        let shell_env = vec![(
            OsString::from("KIMI_CODE_HOME"),
            OsString::from("/shell/kimi-home"),
        )];

        assert_eq!(
            session_home_from(Some(OsString::from("")), &shell_env, Some(home.clone())),
            Some(PathBuf::from("/shell/kimi-home"))
        );
        assert_eq!(
            session_home_from(Some(OsString::from("   ")), &shell_env, Some(home.clone())),
            Some(PathBuf::from("/shell/kimi-home"))
        );
        assert_eq!(
            session_home_from(
                Some(OsString::from("/proc/kimi")),
                &shell_env,
                Some(home.clone())
            ),
            Some(PathBuf::from("/proc/kimi"))
        );
        assert_eq!(
            session_home_from(None, &[], Some(home.clone())),
            Some(home.join(".kimi-code"))
        );
    }
}
