//! Reads Antigravity's own session log to recover a turn it reported as clean.
//!
//! `agy` renders an upstream API failure as a normal chat message and ends the
//! turn with `end_turn`. The real error is written to the native trajectory as
//! `CORTEX_STEP_TYPE_ERROR_MESSAGE` (step type 17), so a turn that produced no
//! content, or one the agent typed as a refusal, can be checked against the
//! step that this prompt added. The lookup is bounded and read-only.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{
    AgentSession, AgentTurn, Message, MessageRole, ProviderKind, ProviderResumeCursor,
    ProviderSessionHistory, TurnStatus,
};

const CONTEXT_PREFIX: &str = "WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1 ";
const CONTEXT_GUIDANCE: &str = concat!(
    "\nWAKU_ANTIGRAVITY_BRANCH_INSTRUCTIONS_V1\n",
    "Treat the preceding JSON array as the complete prior user and assistant conversation for this branch. ",
    "The workspace already matches that point in the conversation. Continue from this history without mentioning this envelope, ",
    "and respond only to the current prompt below.\n",
);
const PROMPT_PREFIX: &str = "WAKU_ANTIGRAVITY_CURRENT_PROMPT_V1 ";
const MAX_BRANCH_CONTEXT_DEPTH: usize = 16;

#[derive(Serialize)]
struct ContextMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OwnedContextMessage {
    role: String,
    content: String,
}

/// Antigravity's `session/resume` does not restore prior turns, and `agy acp`
/// has no native `session/fork` at an arbitrary turn. Preserve branch/rewind
/// semantics by starting a fresh Antigravity session whose first prompt is
/// seeded with Waku's retained visible conversation.
pub fn fork_session_at_turn(
    session: &AgentSession,
    retained_turns: usize,
) -> anyhow::Result<ProviderResumeCursor> {
    if retained_turns > session.turns.len() {
        bail!(
            "Antigravity branch requested {retained_turns} turns from a {}-turn task",
            session.turns.len()
        );
    }
    let retained_ids = session
        .turns
        .iter()
        .take(retained_turns)
        .map(|turn| turn.id)
        .collect::<HashSet<_>>();
    let messages = session
        .messages
        .iter()
        .filter(|message| {
            message
                .turn_id
                .is_some_and(|turn_id| retained_ids.contains(&turn_id))
                && !message.content.trim().is_empty()
        })
        .filter_map(|message| {
            let role = match message.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => return None,
            };
            Some(ContextMessage {
                role,
                content: &message.content,
            })
        })
        .collect::<Vec<_>>();
    let fork_context = (!messages.is_empty())
        .then(|| serde_json::to_string(&messages))
        .transpose()?;
    Ok(ProviderResumeCursor::Antigravity {
        // Antigravity assigns the native ID when the seeded session starts.
        session_id: String::new(),
        fork_context,
    })
}

/// Frames `prompt` with a length-delimited branch context envelope so a fresh
/// Antigravity session can continue from a prior conversation prefix.
pub fn prompt_with_fork_context(context: &str, prompt: &str) -> String {
    format!(
        "{CONTEXT_PREFIX}{}\n{context}{CONTEXT_GUIDANCE}{PROMPT_PREFIX}{}\n{prompt}",
        context.len(),
        prompt.len()
    )
}

/// Decodes a Waku-seeded Antigravity branch prompt into `(context_json, current_prompt)`,
/// or returns `Ok(None)` when `text` is an ordinary user prompt.
pub fn decode_seeded_prompt(text: &str) -> anyhow::Result<Option<(&str, &str)>> {
    let Some(rest) = text.strip_prefix(CONTEXT_PREFIX) else {
        return Ok(None);
    };
    let Some((context_len, rest)) = rest.split_once('\n') else {
        bail!("an Antigravity branch prompt has an invalid context header");
    };
    let context_len = context_len
        .parse::<usize>()
        .context("an Antigravity branch prompt has an invalid context length")?;
    if rest.len() < context_len || !rest.is_char_boundary(context_len) {
        bail!("an Antigravity branch prompt contains truncated context");
    }
    let (context, rest) = rest.split_at(context_len);
    let Some(rest) = rest.strip_prefix(CONTEXT_GUIDANCE) else {
        bail!("an Antigravity branch prompt is missing its context instructions");
    };
    let Some(rest) = rest.strip_prefix(PROMPT_PREFIX) else {
        bail!("an Antigravity branch prompt is missing its current-prompt header");
    };
    let Some((prompt_len, prompt)) = rest.split_once('\n') else {
        bail!("an Antigravity branch prompt has an invalid current-prompt header");
    };
    let prompt_len = prompt_len
        .parse::<usize>()
        .context("an Antigravity branch prompt has an invalid current-prompt length")?;
    if prompt.len() != prompt_len {
        bail!("an Antigravity branch prompt contains truncated current-prompt text");
    }
    Ok(Some((context, prompt)))
}

/// Decodes and recursively expands a Waku-seeded Antigravity branch prompt into
/// `(prior_messages, current_prompt)`, or returns `Ok(None)` for ordinary prompts.
pub fn expand_seeded_prompt(
    text: &str,
) -> anyhow::Result<Option<(Vec<(MessageRole, String)>, String)>> {
    expand_seeded_prompt_depth(text, 0)
}

fn expand_seeded_prompt_depth(
    text: &str,
    depth: usize,
) -> anyhow::Result<Option<(Vec<(MessageRole, String)>, String)>> {
    let Some((context, prompt)) = decode_seeded_prompt(text)? else {
        return Ok(None);
    };
    if depth >= MAX_BRANCH_CONTEXT_DEPTH {
        bail!("Antigravity branch context nesting exceeds maximum depth");
    }
    let nested: Vec<OwnedContextMessage> = serde_json::from_str(context)
        .context("an Antigravity branch contains invalid exported context")?;
    let mut prior = Vec::new();
    for entry in nested {
        let role = match entry.role.as_str() {
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            _ => continue,
        };
        if role == MessageRole::User
            && let Some((nested_prior, nested_prompt)) =
                expand_seeded_prompt_depth(&entry.content, depth + 1)?
        {
            prior.extend(nested_prior);
            if !nested_prompt.trim().is_empty() {
                prior.push((MessageRole::User, nested_prompt));
            }
            continue;
        }
        if !entry.content.trim().is_empty() {
            prior.push((role, entry.content));
        }
    }
    Ok(Some((prior, prompt.to_owned())))
}

/// Strips a `WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1` envelope from an Antigravity
/// session title derived from the first seeded prompt, falling back to `None`
/// when truncated so raw internal markers never leak into the session picker.
pub(crate) fn strip_title_envelope(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.starts_with(CONTEXT_PREFIX) {
        return Some(trimmed.to_owned());
    }
    if let Ok(Some((_, prompt))) = decode_seeded_prompt(trimmed) {
        return prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned);
    }
    if let Some((_, after_prompt_prefix)) = trimmed.split_once(PROMPT_PREFIX)
        && let Some((_, prompt)) = after_prompt_prefix.split_once('\n')
    {
        return prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned);
    }
    None
}

/// Rewrites an imported Antigravity transcript in place so any Waku-seeded
/// branch envelope (`WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1`) is decoded into prior
/// turns and clean messages without leaking raw envelope markers or breaking
/// `TranscriptBlock::after_message` alignment.
pub(crate) fn expand_imported_history(history: &mut ProviderSessionHistory) {
    let old_messages = std::mem::take(&mut history.messages);
    let old_turns = std::mem::take(&mut history.turns);
    let mut new_messages: Vec<Message> = Vec::with_capacity(old_messages.len());
    let mut prior_turns_before: HashMap<Uuid, Vec<AgentTurn>> = HashMap::new();
    let mut trailing_prior_turns: Vec<AgentTurn> = Vec::new();
    let mut index_map = Vec::with_capacity(old_messages.len() + 1);
    index_map.push(0);

    for mut message in old_messages {
        if message.role == MessageRole::User && message.content.starts_with(CONTEXT_PREFIX) {
            match expand_seeded_prompt(&message.content) {
                Ok(Some((prior_entries, current_prompt))) => {
                    let mut inserted_turns: Vec<AgentTurn> = Vec::new();
                    for (role, content) in prior_entries {
                        match role {
                            MessageRole::User => {
                                let turn_id = Uuid::new_v4();
                                inserted_turns.push(AgentTurn {
                                    id: turn_id,
                                    turn_count: 0,
                                    status: TurnStatus::Completed,
                                    provider_turn_started: true,
                                    provider_resume_at: None,
                                    started_at: 0,
                                    completed_at: Some(0),
                                    checkpoint: None,
                                    provider: Some(ProviderKind::Antigravity),
                                    account_id: None,
                                    handoff: None,
                                    native_conversation_start: false,
                                    stream_stats: None,
                                    handoff_preamble: None,
                                });
                                new_messages.push(Message::new_for_turn(
                                    MessageRole::User,
                                    content,
                                    turn_id,
                                ));
                            }
                            MessageRole::Assistant => {
                                let Some(turn_id) = inserted_turns.last().map(|turn| turn.id)
                                else {
                                    continue;
                                };
                                new_messages.push(Message::new_for_turn(
                                    MessageRole::Assistant,
                                    content,
                                    turn_id,
                                ));
                            }
                            MessageRole::System => {}
                        }
                    }
                    if !inserted_turns.is_empty() {
                        if let Some(target_turn) = message.turn_id {
                            prior_turns_before
                                .entry(target_turn)
                                .or_default()
                                .extend(inserted_turns);
                        } else {
                            trailing_prior_turns.extend(inserted_turns);
                        }
                    }
                    message.content = current_prompt;
                }
                Ok(None) => {}
                Err(_) => {
                    if let Ok(Some((_, current_prompt))) = decode_seeded_prompt(&message.content) {
                        message.content = current_prompt.to_owned();
                    }
                }
            }
        }
        new_messages.push(message);
        index_map.push(new_messages.len());
    }

    let mut new_turns = Vec::with_capacity(old_turns.len() + trailing_prior_turns.len());
    for turn in old_turns {
        if let Some(prior) = prior_turns_before.remove(&turn.id) {
            new_turns.extend(prior);
        }
        new_turns.push(turn);
    }
    new_turns.extend(trailing_prior_turns);
    for (index, turn) in new_turns.iter_mut().enumerate() {
        turn.turn_count = index + 1;
    }

    let max_old = index_map.len().saturating_sub(1);
    for block in &mut history.transcript_blocks {
        block.after_message = index_map[block.after_message.min(max_old)];
    }
    history.turns = new_turns;
    history.messages = new_messages;
}

/// `CORTEX_STEP_TYPE_ERROR_MESSAGE` in Antigravity's trajectory.
const ERROR_STEP_TYPE: i64 = 17;
/// The harness error text leads the payload; a Go stack trace follows it.
const MAX_ERROR_PAYLOAD_BYTES: usize = 16_384;
const SERVER_LOG_TAIL_BYTES: u64 = 512 * 1024;
/// The longest provider reason shown for one failed turn.
const MAX_ERROR_MESSAGE_CHARS: usize = 512;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The highest step index before a prompt, or `-1` when the session log does
/// not exist yet. A read failure is indistinguishable from an empty log here;
/// the post-turn check simply finds no new error step.
pub fn step_offset(session_id: &str) -> i64 {
    step_offset_in(session_id, None)
}

/// `config_dir`-scoped variant of [`step_offset`]. When `config_dir` is `Some`,
/// looks only inside that account root (`<config_dir>/conversations/<id>.db`).
pub fn step_offset_in(session_id: &str, config_dir: Option<&Path>) -> i64 {
    let Some(path) = database_path_in(session_id, config_dir) else {
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
pub fn turn_failure(
    session_id: &str,
    offset: i64,
    refusal: bool,
    empty_end_turn: bool,
) -> Option<String> {
    turn_failure_in(session_id, offset, refusal, empty_end_turn, None)
}

/// `config_dir`-scoped variant of [`turn_failure`]. When `config_dir` is `Some`,
/// looks only inside that account root (`<config_dir>/conversations/<id>.db`
/// and `<config_dir>/log/acp-server-*.log`).
pub fn turn_failure_in(
    session_id: &str,
    offset: i64,
    refusal: bool,
    empty_end_turn: bool,
    config_dir: Option<&Path>,
) -> Option<String> {
    if let Some(path) = database_path_in(session_id, config_dir) {
        let deadline = Instant::now() + LOOKUP_TIMEOUT;
        loop {
            if let Some(text) = read_new_error(&path, offset) {
                return Some(failure_message(&text));
            }
            if (!refusal && !empty_end_turn) || Instant::now() >= deadline {
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
            server_log_error(session_id, config_dir)
                .unwrap_or_else(|| tr!("errors.antigravity_turn_failed")),
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

#[cfg(test)]
fn database_path(session_id: &str) -> Option<PathBuf> {
    database_path_in(session_id, None)
}

fn database_path_in(session_id: &str, config_dir: Option<&Path>) -> Option<PathBuf> {
    if !valid_session_id(session_id) {
        return None;
    }
    if let Some(dir) = config_dir {
        let candidate = dir.join("conversations").join(format!("{session_id}.db"));
        return candidate.is_file().then_some(candidate);
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

fn server_log_error(session_id: &str, config_dir: Option<&Path>) -> Option<String> {
    if let Some(dir) = config_dir {
        return server_log_error_in_roots([dir.to_path_buf()], session_id);
    }
    let home = dirs::home_dir()?;
    server_log_error_in_roots(
        DATA_ROOTS.into_iter().map(|root| home.join(root)),
        session_id,
    )
}

fn server_log_error_in_roots(
    roots: impl IntoIterator<Item = PathBuf>,
    session_id: &str,
) -> Option<String> {
    if !valid_session_id(session_id) {
        return None;
    }
    let mut logs = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root.join("log")) else {
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

    #[test]
    fn antigravity_branch_context_keeps_only_the_selected_turn_prefix() {
        use crate::model::{AgentSession, Project};

        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Antigravity);
        session.begin_turn("first prompt");
        session.mark_active_turn_provider_started();
        session.push_message(MessageRole::Assistant, "first answer");
        session.finish_active_turn(TurnStatus::Completed);
        session.begin_turn("second prompt");
        session.mark_active_turn_provider_started();
        session.push_message(MessageRole::Assistant, "second answer");
        session.finish_active_turn(TurnStatus::Completed);

        let ProviderResumeCursor::Antigravity {
            session_id,
            fork_context: Some(context),
        } = fork_session_at_turn(&session, 1).unwrap()
        else {
            panic!("expected a managed Antigravity branch cursor");
        };
        assert!(session_id.is_empty());
        assert!(context.contains("first prompt"));
        assert!(context.contains("first answer"));
        assert!(!context.contains("second prompt"));
    }

    #[test]
    fn antigravity_seed_envelope_is_length_delimited() {
        let prompt = prompt_with_fork_context("[{\"role\":\"user\"}]", "continue 🚀");
        assert!(prompt.starts_with("WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1 17\n"));
        assert!(prompt.ends_with("WAKU_ANTIGRAVITY_CURRENT_PROMPT_V1 13\ncontinue 🚀"));
    }

    #[test]
    fn decode_and_expand_seeded_prompt_round_trips_nested_branches() {
        let first_context = serde_json::to_string(&vec![
            ContextMessage {
                role: "user",
                content: "one",
            },
            ContextMessage {
                role: "assistant",
                content: "done",
            },
        ])
        .unwrap();
        let first_seeded = prompt_with_fork_context(&first_context, "two 🚀");
        let (decoded_context, decoded_prompt) =
            decode_seeded_prompt(&first_seeded).unwrap().unwrap();
        assert_eq!(decoded_context, first_context);
        assert_eq!(decoded_prompt, "two 🚀");

        let nested_context = serde_json::to_string(&vec![
            ContextMessage {
                role: "user",
                content: &first_seeded,
            },
            ContextMessage {
                role: "assistant",
                content: "second done",
            },
        ])
        .unwrap();
        let second_seeded = prompt_with_fork_context(&nested_context, "three");
        let (prior, current) = expand_seeded_prompt(&second_seeded).unwrap().unwrap();
        assert_eq!(
            prior,
            vec![
                (MessageRole::User, "one".to_owned()),
                (MessageRole::Assistant, "done".to_owned()),
                (MessageRole::User, "two 🚀".to_owned()),
                (MessageRole::Assistant, "second done".to_owned()),
            ]
        );
        assert_eq!(current, "three");
    }

    #[test]
    fn decode_seeded_prompt_rejects_malformed_envelopes_and_ignores_plain_prompts() {
        assert_eq!(decode_seeded_prompt("plain prompt").unwrap(), None);
        assert!(decode_seeded_prompt("WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1 999\nshort").is_err());
        assert!(decode_seeded_prompt("WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1 bad\n[]").is_err());
        assert!(
            decode_seeded_prompt("WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1 2\n[]missing guidance")
                .is_err()
        );
    }

    #[test]
    fn expand_imported_history_strips_envelope_and_shifts_transcript_blocks() {
        use crate::model::{ActivityItem, ActivityKind, TranscriptBlock};

        let context = serde_json::to_string(&vec![
            ContextMessage {
                role: "user",
                content: "first prompt",
            },
            ContextMessage {
                role: "assistant",
                content: "first answer",
            },
        ])
        .unwrap();
        let seeded = prompt_with_fork_context(&context, "second prompt");
        let turn_id = Uuid::new_v4();
        let mut history = ProviderSessionHistory {
            turns: vec![AgentTurn {
                id: turn_id,
                turn_count: 1,
                status: TurnStatus::Completed,
                provider_turn_started: true,
                provider_resume_at: None,
                started_at: 0,
                completed_at: Some(0),
                checkpoint: None,
                provider: Some(ProviderKind::Antigravity),
                account_id: None,
                handoff: None,
                native_conversation_start: false,
                stream_stats: None,
                handoff_preamble: None,
            }],
            messages: vec![
                Message::new_for_turn(MessageRole::User, seeded, turn_id),
                Message::new_for_turn(MessageRole::Assistant, "second answer", turn_id),
            ],
            transcript_blocks: vec![TranscriptBlock {
                after_message: 1,
                turn_id: Some(turn_id),
                activities: vec![ActivityItem::new(
                    None,
                    ActivityKind::FileRead,
                    "read foo.rs",
                    None,
                    true,
                )],
            }],
        };

        expand_imported_history(&mut history);

        assert_eq!(history.turns.len(), 2);
        assert_eq!(history.turns[0].turn_count, 1);
        assert_eq!(history.turns[0].provider, Some(ProviderKind::Antigravity));
        assert_eq!(history.turns[1].turn_count, 2);
        assert_eq!(history.turns[1].id, turn_id);
        assert_eq!(history.messages.len(), 4);
        assert_eq!(history.messages[0].content, "first prompt");
        assert_eq!(history.messages[1].content, "first answer");
        assert_eq!(history.messages[2].content, "second prompt");
        assert_eq!(history.messages[3].content, "second answer");
        assert!(
            history
                .messages
                .iter()
                .all(|m| !m.content.contains("WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1"))
        );
        assert_eq!(history.transcript_blocks.len(), 1);
        assert_eq!(history.transcript_blocks[0].after_message, 3);
    }

    #[test]
    fn strip_title_envelope_hides_raw_markers_even_when_truncated() {
        let seeded = prompt_with_fork_context("[]", "actual title\nsecond line");
        assert_eq!(
            strip_title_envelope(&seeded).as_deref(),
            Some("actual title")
        );
        assert_eq!(
            strip_title_envelope("WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1 500\n[{\"role\":\"user\"..."),
            None
        );
        assert_eq!(
            strip_title_envelope("Normal title").as_deref(),
            Some("Normal title")
        );
    }

    #[test]
    fn step_offset_in_and_turn_failure_in_isolate_to_config_dir() {
        let root = std::env::temp_dir().join(format!("waku-agy-session-{}", Uuid::new_v4()));
        let account_a = root.join("account-a");
        let account_b = root.join("account-b");
        fs::create_dir_all(account_a.join("conversations")).unwrap();
        fs::create_dir_all(account_b.join("log")).unwrap();

        let session_a = "session-account-a";
        let db_path = account_a
            .join("conversations")
            .join(format!("{session_a}.db"));
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (idx INTEGER NOT NULL, step_type INTEGER NOT NULL, step_payload BLOB);
             INSERT INTO steps (idx, step_type, step_payload) VALUES (0, 1, X'00');
             INSERT INTO steps (idx, step_type, step_payload) VALUES (1, 17, CAST('Error 429, Message: quota, Status: RESOURCE_EXHAUSTED' AS BLOB));",
        )
        .unwrap();
        drop(conn);

        let session_b = "session-account-b";
        fs::write(
            account_b.join("log").join("acp-server-1.log"),
            format!(
                "I1112 00:41:08.520656 300 latency.go:1] cascade_id:{session_b}\n\
                 E1112 00:41:08.659997 358 errorreport.go:273] failed to construct executor: missing swarm.json\n"
            ),
        )
        .unwrap();

        assert_eq!(step_offset_in(session_a, Some(&account_a)), 1);
        assert_eq!(step_offset_in(session_a, Some(&account_b)), -1);

        assert_eq!(
            turn_failure_in(session_a, 0, false, false, Some(&account_a)),
            Some(quota_message())
        );
        assert_eq!(
            turn_failure_in(session_a, 0, false, false, Some(&account_b)),
            None
        );

        assert_eq!(
            turn_failure_in(session_b, -1, false, true, Some(&account_b)).as_deref(),
            Some("failed to construct executor: missing swarm.json")
        );
        assert_eq!(
            turn_failure_in(session_b, -1, false, true, Some(&account_a)),
            Some(tr!("errors.antigravity_turn_failed"))
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_end_turn_polls_for_delayed_error_step() {
        let root = std::env::temp_dir().join(format!("waku-agy-delayed-err-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("conversations")).unwrap();
        let session_id = "delayed-err-session";
        let db_path = root.join("conversations").join(format!("{session_id}.db"));
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (idx INTEGER NOT NULL, step_type INTEGER NOT NULL, step_payload BLOB);
             INSERT INTO steps (idx, step_type, step_payload) VALUES (0, 14, X'00');",
        )
        .unwrap();
        drop(conn);

        let bg_path = db_path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            let conn = Connection::open(&bg_path).unwrap();
            conn.execute(
                "INSERT INTO steps (idx, step_type, step_payload) VALUES (1, 17, CAST(?1 AS BLOB))",
                params!["UNAVAILABLE (code 503): Delayed upstream failure."],
            )
            .unwrap();
        });

        let recovered = turn_failure_in(session_id, 0, false, true, Some(&root));
        writer.join().unwrap();
        assert_eq!(recovered.as_deref(), Some("Delayed upstream failure."));

        let _ = fs::remove_dir_all(&root);
    }
}
