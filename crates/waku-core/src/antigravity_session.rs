//! Reads Antigravity's own session log to recover a turn it reported as clean.
//!
//! `agy` renders an upstream API failure as a normal chat message and ends the
//! turn with `end_turn`. The real error is written to the native trajectory as
//! `CORTEX_STEP_TYPE_ERROR_MESSAGE` (step type 17), so a turn that produced no
//! content, or one the agent typed as a refusal, can be checked against the
//! step that this prompt added. The lookup is bounded and read-only.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, params};

/// `CORTEX_STEP_TYPE_ERROR_MESSAGE` in Antigravity's trajectory.
const ERROR_STEP_TYPE: i64 = 17;
/// The harness error text leads the payload; a Go stack trace follows it.
const MAX_ERROR_PAYLOAD_BYTES: usize = 16_384;
const SERVER_LOG_TAIL_BYTES: u64 = 128 * 1024;
const MAX_LOG_ERROR_CHARS: usize = 512;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The highest step index before a prompt, or `-1` when the session log does
/// not exist yet. A read failure is indistinguishable from an empty log here;
/// the post-turn check simply finds no new error step.
pub(crate) fn step_offset(session_id: &str) -> i64 {
    let Some(path) = database_path(session_id) else {
        return -1;
    };
    read_step_index(&path).unwrap_or(-1)
}

/// Returns a provider failure for the steps this prompt added. `refusal` is
/// the typed reason `agy` uses for an exhausted quota, so it fails even when
/// the error step lands after the prompt response. `empty_end_turn` catches
/// harness aborts (such as executor construction or trajectory-store failures)
/// where `agy acp` settles `session/prompt` as `end_turn` without emitting any
/// content or writing an error step to the conversation database.
pub(crate) fn turn_failure(
    session_id: &str,
    offset: i64,
    refusal: bool,
    empty_end_turn: bool,
) -> Option<String> {
    if let Some(path) = database_path(session_id) {
        let deadline = Instant::now() + LOOKUP_TIMEOUT;
        loop {
            if let Some(text) = read_new_error(&path, offset) {
                return Some(failure_message(&text));
            }
            if !refusal || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
    if refusal {
        return Some(quota_message());
    }
    if empty_end_turn {
        return Some(
            server_log_error(session_id).unwrap_or_else(|| tr!("errors.antigravity_turn_failed")),
        );
    }
    None
}

fn quota_message() -> String {
    tr!("errors.antigravity_quota")
}

fn failure_message(payload: &str) -> String {
    if payload.contains("QUOTA_EXHAUSTED") {
        return quota_message();
    }
    match api_error(payload) {
        Some((http_status, _, status))
            if http_status == "429" || status == "RESOURCE_EXHAUSTED" =>
        {
            quota_message()
        }
        Some((_, message, _)) => message,
        None => tr!("errors.antigravity_turn_failed"),
    }
}

/// Parses the harness's rendering of a Google API error:
/// `Error 429, Message: ..., Status: RESOURCE_EXHAUSTED`.
fn api_error(text: &str) -> Option<(&str, String, &str)> {
    let after_code = text.split_once("Error ")?.1;
    let (http_status, after_code) = after_code.split_once(", Message: ")?;
    let (message, after_message) = after_code.split_once(", Status: ")?;
    let status = after_message
        .split(|character: char| character.is_whitespace() || character == ')')
        .next()
        .filter(|status| !status.is_empty())?;
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    Some((http_status.trim(), message.to_owned(), status))
}

const DATA_ROOTS: [&str; 3] = [
    ".gemini/antigravity-cli",
    ".gemini/antigravity-acp",
    ".gemini/antigravity",
];

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

fn database_path(session_id: &str) -> Option<PathBuf> {
    if !valid_session_id(session_id) {
        return None;
    }
    let home = dirs::home_dir()?;
    DATA_ROOTS
        .into_iter()
        .map(|root| {
            home.join(root)
                .join("conversations")
                .join(format!("{session_id}.db"))
        })
        .find(|path| path.is_file())
}

fn server_log_error(session_id: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    server_log_error_in(&home, session_id)
}

fn server_log_error_in(home: &Path, session_id: &str) -> Option<String> {
    if !valid_session_id(session_id) {
        return None;
    }
    let mut logs = Vec::new();
    for root in DATA_ROOTS {
        let Ok(entries) = fs::read_dir(home.join(root).join("log")) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if !name.starts_with("acp-server-") || !name.ends_with(".log") {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !metadata.file_type().is_file() {
                continue;
            }
            logs.push((metadata.modified().ok(), entry.path()));
        }
    }
    logs.sort_by(|left, right| right.0.cmp(&left.0));
    logs.into_iter().take(4).find_map(|(_, path)| {
        read_tail(&path).and_then(|tail| extract_turn_log_error(&tail, session_id))
    })
}

fn read_tail(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > SERVER_LOG_TAIL_BYTES {
        file.seek(SeekFrom::End(-(SERVER_LOG_TAIL_BYTES as i64)))
            .ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn extract_turn_log_error(log: &str, session_id: &str) -> Option<String> {
    if !log.contains(session_id) {
        return None;
    }
    let send_marker = format!("Sending user message to conversation {session_id}");
    let cascade_marker = format!("cascade_id:{session_id}");
    let create_marker = format!("Created conversation {session_id}");
    let turn_start = [
        send_marker.as_str(),
        cascade_marker.as_str(),
        create_marker.as_str(),
    ]
    .into_iter()
    .filter_map(|marker| log.rfind(marker))
    .max();
    let window = match turn_start {
        Some(index) => &log[log[..index].rfind('\n').map_or(0, |newline| newline + 1)..],
        None => log,
    };

    let mut fallback = None;
    for line in window.lines() {
        let Some((header, message)) = glog_error_parts(line) else {
            continue;
        };
        if header.contains("discovery.go:") {
            continue;
        }
        if message.starts_with("failed to construct executor:") {
            return Some(format_log_error(message));
        }
        if message.contains(session_id)
            || (turn_start.is_some()
                && (message.contains("untrusted mount point")
                    || message.starts_with("Failed to create SQLite trajectory store:")))
        {
            fallback = Some(message);
        }
    }
    fallback.map(format_log_error)
}

fn glog_error_parts(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    if bytes.len() < 6
        || bytes[0] != b'E'
        || !bytes[1..5].iter().all(u8::is_ascii_digit)
        || bytes[5] != b' '
    {
        return None;
    }
    let (header, message) = line.split_once("] ")?;
    let message = message.trim();
    (!message.is_empty()).then_some((header, message))
}

fn format_log_error(message: &str) -> String {
    if message.contains("QUOTA_EXHAUSTED") {
        return quota_message();
    }
    if let Some((http_status, parsed, status)) = api_error(message) {
        return if http_status == "429" || status == "RESOURCE_EXHAUSTED" {
            quota_message()
        } else {
            parsed
        };
    }
    message.chars().take(MAX_LOG_ERROR_CHARS).collect()
}

fn open_read_only(path: &Path) -> Option<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = connection.busy_timeout(Duration::from_millis(100));
    Some(connection)
}

fn read_step_index(path: &Path) -> Option<i64> {
    let connection = open_read_only(path)?;
    connection
        .query_row("SELECT max(idx) FROM steps", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .ok()
        .flatten()
}

fn read_new_error(path: &Path, offset: i64) -> Option<String> {
    let connection = open_read_only(path)?;
    let (step_type, payload) = connection
        .query_row(
            "SELECT step_type, substr(step_payload, 1, ?2) \
             FROM steps WHERE idx > ?1 ORDER BY idx DESC LIMIT 1",
            params![offset, MAX_ERROR_PAYLOAD_BYTES],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
        )
        .ok()?;
    if step_type != ERROR_STEP_TYPE {
        return None;
    }
    // The step type is the evidence; an empty payload still means the turn
    // failed, and `failure_message` falls back to its generic wording.
    Some(String::from_utf8_lossy(&payload.unwrap_or_default()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_google_api_error_inside_a_stack_trace() {
        let text = "Error 429, Message: Resource exhausted. Please try again., Status: RESOURCE_EXHAUSTED\n\
                    goroutine 1 [running]:\nmain.main()";
        let (code, message, status) = api_error(text).expect("api error");
        assert_eq!(code, "429");
        assert_eq!(message, "Resource exhausted. Please try again.");
        assert_eq!(status, "RESOURCE_EXHAUSTED");
        assert_eq!(failure_message(text), quota_message());
    }

    #[test]
    fn keeps_a_non_quota_provider_message() {
        let text = "Error 500, Message: Internal upstream failure, Status: INTERNAL";
        assert_eq!(failure_message(text), "Internal upstream failure");
    }

    #[test]
    fn an_error_step_without_a_payload_still_fails_the_turn() {
        assert_eq!(failure_message(""), tr!("errors.antigravity_turn_failed"));
    }

    #[test]
    fn rejects_session_ids_that_could_escape_the_conversations_directory() {
        assert!(database_path("../../secrets").is_none());
        assert!(database_path("").is_none());
        assert!(database_path("session/../../secrets").is_none());
    }

    #[test]
    fn extracts_executor_failure_from_acp_server_log_ignoring_dot_dir_noise() {
        let session_id = "ea82b742-509e-420b-9367-b85b48b375fa";
        let log = format!(
            "I1112 00:41:08.520656 300 latency_breakdown.go:168] SEND_USER_CASCADE_MESSAGE_LATENCY latency breakdown: map[cascade_id:{session_id} status:OK]\n\
             E1112 00:41:08.538965 353 discovery.go:560] Error checking dot dir C:\\brain\\{session_id}\\.agent: untrusted mount point.\n\
             E1112 00:41:08.659997 358 errorreport.go:273] failed to construct executor: open swarm.json: The path cannot be traversed because it contains an untrusted mount point.\n\
             E1112 00:41:08.659997 147 stager.go:117] Failed to save message for {session_id}: mkdir conversations: Cannot create a file when that file already exists.\n"
        );
        assert_eq!(
            extract_turn_log_error(&log, session_id).as_deref(),
            Some(
                "failed to construct executor: open swarm.json: \
                 The path cannot be traversed because it contains an untrusted mount point."
            )
        );
        assert!(extract_turn_log_error(&log, "other-session-id").is_none());
    }

    #[test]
    fn empty_end_turn_without_database_or_server_log_still_reports_failure() {
        assert_eq!(
            turn_failure("nonexistent-antigravity-session", -1, false, true),
            Some(tr!("errors.antigravity_turn_failed"))
        );
        assert_eq!(
            turn_failure("nonexistent-antigravity-session", -1, false, false),
            None
        );
    }
}
