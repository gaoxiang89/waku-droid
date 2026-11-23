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
/// The longest provider reason shown for one failed turn.
const MAX_ERROR_MESSAGE_CHARS: usize = 512;
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
    match provider_failure(payload) {
        Some(ProviderFailure::Quota) => quota_message(),
        Some(ProviderFailure::Message(message)) => message,
        None => retry_detail(payload)
            .or_else(|| headline(payload))
            .unwrap_or_else(|| tr!("errors.antigravity_turn_failed")),
    }
}

/// What the text `agy` wrote for a failed turn means for the user.
enum ProviderFailure {
    /// The account has nothing left to spend, whatever words `agy` put on it.
    Quota,
    /// The provider's own reason, without its framing.
    Message(String),
}

/// The failure a Google API status names in `text`. A status line carries the
/// reason; a bare quota status is the only other wording that is certain.
fn provider_failure(text: &str) -> Option<ProviderFailure> {
    if let Some((status, code, message)) = api_status(text) {
        return Some(if code == "429" || status.ends_with("_EXHAUSTED") {
            ProviderFailure::Quota
        } else {
            ProviderFailure::Message(message)
        });
    }
    if text.contains("QUOTA_EXHAUSTED") || text.contains("RESOURCE_EXHAUSTED") {
        return Some(ProviderFailure::Quota);
    }
    None
}

/// A Google API failure as `(status, code, message)`. Since 1.5.x the payload
/// carries `RESOURCE_EXHAUSTED (code 429): Resource has been exhausted (…)`,
/// often behind `API error (attempt 3): `; older builds wrote
/// `Error 429, Message: …, Status: RESOURCE_EXHAUSTED`.
fn api_status(text: &str) -> Option<(&str, &str, String)> {
    grpc_status(text).or_else(|| legacy_api_status(text))
}

/// The `<STATUS> (code <N>): <message>` line, wherever a retry prefix or the
/// protobuf framing around the text put it.
fn grpc_status(text: &str) -> Option<(&str, &str, String)> {
    let mut search_from = 0;
    while let Some(offset) = text[search_from..].find(" (code ") {
        let marker = search_from + offset;
        search_from = marker + " (code ".len();
        let status_len = text[..marker]
            .bytes()
            .rev()
            .take_while(|byte| byte.is_ascii_uppercase() || *byte == b'_')
            .count();
        if status_len == 0 {
            continue;
        }
        let Some((code, message)) = text[search_from..].split_once("): ") else {
            continue;
        };
        if code.is_empty() || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Some(message) = message_text(message) else {
            continue;
        };
        return Some((&text[marker - status_len..marker], code, message));
    }
    None
}

/// `Error 429, Message: ..., Status: RESOURCE_EXHAUSTED`, the shape older
/// builds wrote.
fn legacy_api_status(text: &str) -> Option<(&str, &str, String)> {
    let after_code = text.split_once("Error ")?.1;
    let (code, after_code) = after_code.split_once(", Message: ")?;
    let (message, after_message) = after_code.split_once(", Status: ")?;
    let status = after_message
        .split(|character: char| character.is_whitespace() || character == ')')
        .next()
        .filter(|status| !status.is_empty())?;
    let message = message.trim();
    (!message.is_empty()).then(|| (status, code.trim(), message.to_owned()))
}

/// The reason behind `API error (attempt 3): `, the line a turn ends on when
/// its last retry failed, such as `request failed: …: EOF`.
fn retry_detail(payload: &str) -> Option<String> {
    let (_, attempt) = payload.split_once("API error (attempt ")?;
    let (_, detail) = attempt.split_once("): ")?;
    message_text(detail)
}

/// The harness's own sentence when it words the failure itself, such as
/// `There was a network issue connecting to the server, please try again.`
fn headline(payload: &str) -> Option<String> {
    let mut rest = payload;
    while let Some(start) = sentence_start(rest) {
        let sentence = &rest[start..];
        let end = text_end(sentence);
        let text = &sentence[..end];
        rest = &sentence[end..];
        if let Some(stop) = text.find('.').filter(|stop| text[..*stop].contains(' ')) {
            return Some(text[..=stop].to_owned());
        }
    }
    None
}

/// Where prose starts: an uppercase letter followed by a lowercase one, which
/// skips the ids and framing bytes around the text.
fn sentence_start(text: &str) -> Option<usize> {
    let mut previous = None;
    for (index, character) in text.char_indices() {
        if character.is_ascii_lowercase()
            && previous.is_some_and(|previous: char| previous.is_ascii_uppercase())
        {
            return Some(index - 1);
        }
        previous = Some(character);
    }
    None
}

/// One message of a payload: its text, cut off from the protobuf framing that
/// follows it and shortened to what a transcript can hold.
fn message_text(text: &str) -> Option<String> {
    let end = text_end(text);
    let end = glued_id_start(&text[..end]).unwrap_or(end);
    let message = text[..end].trim();
    (!message.is_empty()).then(|| message.chars().take(MAX_ERROR_MESSAGE_CHARS).collect())
}

/// The payload is protobuf, so its text ends at the first control byte.
fn text_end(text: &str) -> usize {
    text.find(|character: char| character.is_control() || character == '\u{FFFD}')
        .unwrap_or(text.len())
}

/// Where the `<tag><length><id>` framing after a message starts, when its
/// bytes happen to be printable and read as part of the text. Real text does
/// not glue an id to a digit.
fn glued_id_start(text: &str) -> Option<usize> {
    const UUID_LEN: usize = 36;
    let bytes = text.as_bytes();
    let mut index = 2;
    while index + UUID_LEN <= bytes.len() {
        if is_uuid(&bytes[index..]) {
            return (bytes[index - 2].is_ascii_digit() && bytes[index - 1] != b' ')
                .then_some(index - 2);
        }
        index += 1;
    }
    None
}

/// `bytes` starts with the 36 characters of a UUID. The caller checks the length.
fn is_uuid(bytes: &[u8]) -> bool {
    bytes[..36].iter().enumerate().all(|(index, &byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
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
    match provider_failure(message) {
        Some(ProviderFailure::Quota) => quota_message(),
        Some(ProviderFailure::Message(message)) => message,
        None => message.chars().take(MAX_ERROR_MESSAGE_CHARS).collect(),
    }
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
    fn reads_the_older_google_api_error_inside_a_stack_trace() {
        let text = "Error 429, Message: Resource exhausted. Please try again., Status: RESOURCE_EXHAUSTED\n\
                    goroutine 1 [running]:\nmain.main()";
        assert_eq!(
            api_status(text),
            Some((
                "RESOURCE_EXHAUSTED",
                "429",
                "Resource exhausted. Please try again.".to_owned()
            ))
        );
        assert_eq!(failure_message(text), quota_message());
    }

    #[test]
    fn keeps_a_non_quota_provider_message() {
        let text = "Error 500, Message: Internal upstream failure, Status: INTERNAL";
        assert_eq!(failure_message(text), "Internal upstream failure");
    }

    /// Error steps as `agy` 1.5.15 wrote them: protobuf framing around the
    /// text, printable often enough to pass for part of the message.
    #[test]
    fn reads_the_status_line_agy_writes_for_a_google_api_error() {
        assert_eq!(
            failure_message(
                "/Your AI credits balance is too low to continue.\
                 \nNRESOURCE_EXHAUSTED (code 429): Resource has been exhausted (e.g. check quota).\
                 \nHTTP 429 Too Many Requests"
            ),
            quota_message()
        );
        assert_eq!(
            failure_message(
                "(Agent execution terminated due to error.\
                 \nOFAILED_PRECONDITION (code 400): User location is not supported for the API use.\
                 \nHTTP 400 Bad Request"
            ),
            "User location is not supported for the API use."
        );
        assert_eq!(
            failure_message(
                "TAPI error (attempt 1): UNAVAILABLE (code 503): The service is currently unavailable.2(bd384c9f-6d24-4f67-93cd-a7cca89bc9b2a-240"
            ),
            "The service is currently unavailable."
        );
    }

    #[test]
    fn keeps_the_reason_of_a_retry_or_network_failure() {
        assert_eq!(
            failure_message(
                "b$c33e1f12-3e75-4b93-a78c-ff981d3352d0$e14c3d32-23c5-40ba-bc7d-32c8d9a5e235\"$085cf15b-1da9-488c-8ccc-a5593fb5da3e\
                 \\API error (attempt 1): request failed: Post \"https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse\": EOF2'e14c3d32-23c5-40ba-bc7d-32c8d9a5e235-32"
            ),
            "request failed: Post \"https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse\": EOF"
        );
        assert_eq!(
            failure_message(
                "EThere was a network issue connecting to the server, please try again.\
                 \nagent executor error: generating and executing: request failed: net/http: TLS handshake timeout"
            ),
            "There was a network issue connecting to the server, please try again."
        );
    }

    #[test]
    fn framing_without_a_reason_is_not_mistaken_for_one() {
        let framing = "b$c33e1f12-3e75-4b93-a78c-ff981d3352d0$e14c3d32-23c5-40ba-bc7d-32c8d9a5e235\
                       \"$085cf15b-1da9-488c-8ccc-a5593fb5da3e";
        assert_eq!(
            failure_message(framing),
            tr!("errors.antigravity_turn_failed")
        );
    }

    #[test]
    fn a_status_in_the_server_log_names_the_failure_too() {
        assert_eq!(
            format_log_error(
                "RESOURCE_EXHAUSTED (code 429): Resource has been exhausted (e.g. check quota)."
            ),
            quota_message()
        );
        assert_eq!(
            format_log_error("UNAVAILABLE (code 503): The service is currently unavailable."),
            "The service is currently unavailable."
        );
        assert_eq!(
            format_log_error("failed to construct executor: open swarm.json: missing"),
            "failed to construct executor: open swarm.json: missing"
        );
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
