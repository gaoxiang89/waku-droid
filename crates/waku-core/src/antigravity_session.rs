//! Reads Antigravity's own session log to recover a turn it reported as clean.
//!
//! `agy` renders an upstream API failure as a normal chat message and ends the
//! turn with `end_turn`. The real error is written to the native trajectory as
//! `CORTEX_STEP_TYPE_ERROR_MESSAGE` (step type 17), so a turn that produced no
//! content, or one the agent typed as a refusal, can be checked against the
//! step that this prompt added. The lookup is bounded and read-only.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, params};

/// `CORTEX_STEP_TYPE_ERROR_MESSAGE` in Antigravity's trajectory.
const ERROR_STEP_TYPE: i64 = 17;
/// The harness error text leads the payload; a Go stack trace follows it.
const MAX_ERROR_PAYLOAD_BYTES: usize = 16_384;
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
/// the error step lands after the prompt response.
pub(crate) fn turn_failure(session_id: &str, offset: i64, refusal: bool) -> Option<String> {
    let Some(path) = database_path(session_id) else {
        return refusal.then(quota_message);
    };
    let deadline = Instant::now() + LOOKUP_TIMEOUT;
    loop {
        if let Some(text) = read_new_error(&path, offset) {
            return Some(failure_message(&text));
        }
        if !refusal || Instant::now() >= deadline {
            return refusal.then(quota_message);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
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

fn database_path(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || !session_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return None;
    }
    let home = dirs::home_dir()?;
    [
        ".gemini/antigravity-cli/conversations",
        ".gemini/antigravity-acp/conversations",
        ".gemini/antigravity/conversations",
    ]
    .into_iter()
    .map(|root| home.join(root).join(format!("{session_id}.db")))
    .find(|path| path.is_file())
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
}
