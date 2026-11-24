//! Provider-neutral ACP session discovery and transcript replay.
//!
//! ACP agents own their storage migrations and visible-history projection. A
//! one-shot `session/list` or `session/load` therefore stays more accurate than
//! reading their private stores, while typed updates let Waku discard private
//! reasoning and provider-only tool records by construction.

use std::collections::{HashMap, HashSet};
use std::path::{MAIN_SEPARATOR, MAIN_SEPARATOR_STR, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ClientCapabilities, Implementation, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
    SessionNotification,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo};
use anyhow::{Context as _, anyhow};
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

use crate::model::{
    ActivityItem, ActivityKind, AgentTurn, Message, MessageRole, ProviderKind,
    ProviderResumeCursor, ProviderSessionHistory, ProviderSessionSummary, ReasoningBlock,
    TranscriptBlock, TurnStatus,
};

const MAX_CATALOG_PAGES: usize = 20;
const MAX_CATALOG_WORKSPACES: usize = 100;
const ACP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const LOAD_REPLAY_QUIET_WINDOW: Duration = Duration::from_millis(150);
const LOAD_REPLAY_MAX_DRAIN: Duration = Duration::from_secs(2);

/// Empty, app-owned cwd for provider catalog subprocesses.
///
/// Agent CLIs may index their process cwd during startup. Launching a catalog
/// from `$HOME` therefore makes a harmless session-list request recursively
/// touch Desktop, Documents, Downloads, Photos, and other macOS TCC locations.
/// Keeping discovery in an isolated temp directory prevents that implicit
/// workspace scan; actual session loading still receives the selected cwd.
pub(crate) fn catalog_working_directory() -> anyhow::Result<PathBuf> {
    let directory = std::env::temp_dir().join(format!(
        "waku-provider-session-catalog-{}",
        std::process::id()
    ));
    crate::fs_ext::create_private_dir_all(&directory).with_context(|| {
        format!(
            "could not create provider-session catalog directory {}",
            directory.display()
        )
    })?;
    crate::fs_ext::restrict_to_owner(&directory).with_context(|| {
        format!(
            "could not secure provider-session catalog directory {}",
            directory.display()
        )
    })?;
    Ok(directory)
}

fn initialize_request() -> InitializeRequest {
    InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(ClientCapabilities::new().terminal(false))
        .client_info(Implementation::new("waku", env!("CARGO_PKG_VERSION")))
}

fn timestamp(value: Option<&str>) -> u64 {
    value
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .and_then(|value| u64::try_from(value.timestamp()).ok())
        .unwrap_or_default()
}

pub(crate) fn session_title(
    provider: ProviderKind,
    title: Option<&str>,
    session_id: &str,
) -> String {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .and_then(|title| match provider {
            ProviderKind::Antigravity => crate::antigravity_session::strip_title_envelope(title),
            ProviderKind::Cursor => crate::cursor_session::strip_title_envelope(title),
            _ => Some(title.to_owned()),
        })
        .unwrap_or_else(|| {
            let short_id = session_id.chars().take(8).collect::<String>();
            format!("{} session {short_id}", provider.short_name())
        })
}

/// The native absolute path a session's `cwd` stands for, or `None` when the
/// agent reported something Waku cannot treat as one.
///
/// `agy` on Windows answers `session/list` with a URL path: a leading slash
/// before the drive and every special character percent-encoded
/// (`/c%3A/Users/me/My%20Docs`). Rust reads none of that as absolute, so
/// without this the agent's whole history drops out of the Resume picker.
fn native_session_cwd(raw: &Path) -> Option<PathBuf> {
    if raw.is_absolute() {
        return Some(raw.to_path_buf());
    }
    let decoded = percent_decode(raw.to_string_lossy().strip_prefix('/')?);
    let bytes = decoded.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'/' | b'\\')
    {
        return None;
    }
    let path = PathBuf::from(format!(
        "{}:{MAIN_SEPARATOR}{}",
        (bytes[0] as char).to_ascii_uppercase(),
        decoded[3..].replace(['/', '\\'], MAIN_SEPARATOR_STR)
    ));
    path.is_absolute().then_some(path)
}

/// Decodes `%XX` bytes and leaves everything else, including a lone `%`, as is.
fn percent_decode(text: &str) -> String {
    let hex = |byte: u8| match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => byte - b'A' + 10,
    };
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes.get(index..index + 3) {
            Some([b'%', high, low]) if high.is_ascii_hexdigit() && low.is_ascii_hexdigit() => {
                decoded.push(hex(*high) << 4 | hex(*low));
                index += 3;
            }
            _ => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn strip_windows_verbatim_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let raw = path.to_string_lossy();
        if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = raw.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

fn normalized_comparable_path(path: &Path) -> PathBuf {
    let target = strip_windows_verbatim_path(
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
    );
    let mut normalized = PathBuf::new();
    for component in target.components() {
        match component {
            std::path::Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    normalized
}

fn paths_match_normalized(left: &Path, right: &Path) -> bool {
    let left = normalized_comparable_path(left);
    let right = normalized_comparable_path(right);
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&right.to_string_lossy().replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// Lists every session exposed by an ACP agent. An empty `cwd_filters` list
/// requests the provider's global catalog; providers such as Kimi that require
/// a cwd pass their native workspace index here instead.
pub fn list_provider_sessions(
    provider: ProviderKind,
    binary: &Path,
    cwd_filters: &[PathBuf],
    limit: usize,
) -> anyhow::Result<Vec<ProviderSessionSummary>> {
    list_provider_sessions_in(provider, binary, cwd_filters, limit, None)
}

/// `config_dir`-scoped variant of [`list_provider_sessions`].
pub fn list_provider_sessions_in(
    provider: ProviderKind,
    binary: &Path,
    cwd_filters: &[PathBuf],
    limit: usize,
    config_dir: Option<&Path>,
) -> anyhow::Result<Vec<ProviderSessionSummary>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let catalog_cwd = catalog_working_directory()?;
    let agent = crate::driver::acp::catalog_agent_in(provider, binary, &catalog_cwd, config_dir)?;
    list_provider_sessions_with_agent_in(provider, agent, cwd_filters, limit, Some(&catalog_cwd))
}

#[cfg(test)]
fn list_provider_sessions_with_agent<C>(
    provider: ProviderKind,
    agent: C,
    cwd_filters: &[PathBuf],
    limit: usize,
) -> anyhow::Result<Vec<ProviderSessionSummary>>
where
    C: ConnectTo<Client> + 'static,
{
    list_provider_sessions_with_agent_in(provider, agent, cwd_filters, limit, None)
}

fn list_provider_sessions_with_agent_in<C>(
    provider: ProviderKind,
    agent: C,
    cwd_filters: &[PathBuf],
    limit: usize,
    catalog_cwd: Option<&Path>,
) -> anyhow::Result<Vec<ProviderSessionSummary>>
where
    C: ConnectTo<Client> + 'static,
{
    if limit == 0 {
        return Ok(Vec::new());
    }
    let catalog_cwd = catalog_cwd.map(Path::to_path_buf);
    let filters = if cwd_filters.is_empty() {
        vec![None]
    } else {
        cwd_filters
            .iter()
            .take(MAX_CATALOG_WORKSPACES)
            .cloned()
            .map(Some)
            .collect()
    };

    let request = Client.builder().name("waku-session-catalog").connect_with(
        agent,
        async move |connection: ConnectionTo<Agent>| {
            let initialize = connection
                .send_request(initialize_request())
                .block_task()
                .await?;
            if initialize
                .agent_capabilities
                .session_capabilities
                .list
                .is_none()
            {
                return Ok(Vec::new());
            }

            let mut found = Vec::new();
            let mut seen = HashSet::new();
            for cwd in filters {
                let mut cursor = None;
                let mut seen_cursors = HashSet::new();
                for _ in 0..MAX_CATALOG_PAGES {
                    let mut request = ListSessionsRequest::new().cursor(cursor.clone());
                    if let Some(cwd) = cwd.clone() {
                        request = request.cwd(cwd);
                    }
                    let response = connection.send_request(request).block_task().await?;
                    for session in response.sessions {
                        let session_id = session.session_id.to_string();
                        if session_id.trim().is_empty() {
                            continue;
                        }
                        let Some(cwd) = native_session_cwd(&session.cwd) else {
                            continue;
                        };
                        if catalog_cwd
                            .as_deref()
                            .is_some_and(|catalog| paths_match_normalized(&cwd, catalog))
                        {
                            continue;
                        }
                        if !seen.insert(session_id.clone()) {
                            continue;
                        }
                        let updated_at = timestamp(session.updated_at.as_deref());
                        found.push(ProviderSessionSummary {
                            cursor: ProviderResumeCursor::from_session_id(
                                provider,
                                session_id.clone(),
                            ),
                            title: session_title(provider, session.title.as_deref(), &session_id),
                            cwd,
                            created_at: updated_at,
                            updated_at,
                        });
                    }
                    let Some(next) = response.next_cursor.filter(|next| !next.is_empty()) else {
                        break;
                    };
                    if !seen_cursors.insert(next.clone()) {
                        break;
                    }
                    cursor = Some(next);
                }
            }
            found.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            found.truncate(limit);
            Ok(found)
        },
    );
    smol::block_on(smol::future::race(
        async move { request.await.map_err(anyhow::Error::new) },
        async move {
            smol::Timer::after(ACP_REQUEST_TIMEOUT).await;
            Err(anyhow!(
                "{} ACP session catalog timed out",
                provider.display_name()
            ))
        },
    ))
    .with_context(|| format!("{} could not list ACP sessions", provider.display_name()))
}

/// Replays the provider's user-visible transcript through ACP `session/load`.
pub fn provider_session_history(
    provider: ProviderKind,
    binary: &Path,
    cwd: &Path,
    session_id: &str,
    visible_turn_limit: usize,
) -> anyhow::Result<ProviderSessionHistory> {
    provider_session_history_in(provider, binary, cwd, session_id, visible_turn_limit, None)
}

async fn drain_trailing_replay(
    last_update_at: &Mutex<Option<Instant>>,
    update_rx: &smol::channel::Receiver<()>,
    quiet_window: Duration,
    max_drain: Duration,
) {
    while update_rx.try_recv().is_ok() {}
    let drain_start = Instant::now();
    let hard_deadline = drain_start + max_drain;
    loop {
        let now = Instant::now();
        if now >= hard_deadline {
            break;
        }
        let reference = last_update_at
            .lock()
            .map_or(drain_start, |last| last.max(drain_start));
        let quiet_deadline = (reference + quiet_window).min(hard_deadline);
        if now >= quiet_deadline {
            break;
        }
        let saw_more = smol::future::or(
            async { update_rx.recv().await.is_ok() },
            async {
                smol::Timer::at(quiet_deadline).await;
                false
            },
        )
        .await;
        if !saw_more {
            break;
        }
    }
}

/// `config_dir`-scoped variant of [`provider_session_history`].
pub fn provider_session_history_in(
    provider: ProviderKind,
    binary: &Path,
    cwd: &Path,
    session_id: &str,
    visible_turn_limit: usize,
    config_dir: Option<&Path>,
) -> anyhow::Result<ProviderSessionHistory> {
    if session_id.trim().is_empty() || visible_turn_limit == 0 {
        return Ok(ProviderSessionHistory::default());
    }
    let agent = crate::driver::acp::catalog_agent_in(provider, binary, cwd, config_dir)?;
    provider_session_history_with_agent(
        provider,
        agent,
        cwd,
        session_id,
        visible_turn_limit,
        LOAD_REPLAY_QUIET_WINDOW,
        LOAD_REPLAY_MAX_DRAIN,
    )
}

fn provider_session_history_with_agent<C>(
    provider: ProviderKind,
    agent: C,
    cwd: &Path,
    session_id: &str,
    visible_turn_limit: usize,
    quiet_window: Duration,
    max_drain: Duration,
) -> anyhow::Result<ProviderSessionHistory>
where
    C: ConnectTo<Client> + 'static,
{
    if session_id.trim().is_empty() || visible_turn_limit == 0 {
        return Ok(ProviderSessionHistory::default());
    }
    let updates = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&updates);
    let last_update_at = Arc::new(Mutex::new(None::<Instant>));
    let last_update_seen = Arc::clone(&last_update_at);
    let (update_tx, update_rx) = smol::channel::bounded::<()>(1);
    let cwd = cwd.to_path_buf();
    let session_id = session_id.to_owned();

    let request = Client
        .builder()
        .name("waku-session-import")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                if let Ok(update) = serde_json::to_value(notification.update) {
                    captured.lock().push(update);
                    *last_update_seen.lock() = Some(Instant::now());
                    let _ = update_tx.try_send(());
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |connection: ConnectionTo<Agent>| {
            let initialize = connection
                .send_request(initialize_request())
                .block_task()
                .await?;
            if !initialize.agent_capabilities.load_session {
                return Err(agent_client_protocol::Error::new(
                    agent_client_protocol::ErrorCode::MethodNotFound.into(),
                    "the agent does not support session/load",
                ));
            }
            connection
                .send_request(LoadSessionRequest::new(session_id, cwd))
                .block_task()
                .await?;
            // ACP ordering places replay before the load response, but some
            // providers flush trailing queued notifications after it lands.
            drain_trailing_replay(&last_update_at, &update_rx, quiet_window, max_drain).await;
            Ok(())
        });
    smol::block_on(smol::future::race(
        async move { request.await.map_err(anyhow::Error::new) },
        async move {
            smol::Timer::after(ACP_REQUEST_TIMEOUT).await;
            Err(anyhow!(
                "{} ACP session replay timed out",
                provider.display_name()
            ))
        },
    ))
    .with_context(|| format!("{} could not replay the session", provider.display_name()))?;

    let updates = std::mem::take(&mut *updates.lock());
    let mut history = history_from_updates(provider, &updates);
    retain_recent_messages(&mut history, visible_turn_limit);
    Ok(history)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReplayPhase {
    Text,
    Reasoning,
    Activity,
}

fn push_replay_activity(
    history: &mut ProviderSessionHistory,
    turn_id: Uuid,
    item: ActivityItem,
    continuing_work: bool,
) {
    let after_message = history.messages.len();
    if continuing_work
        && let Some(block) = history.transcript_blocks.last_mut()
        && block.after_message == after_message
        && block.turn_id == Some(turn_id)
    {
        block.activities.push(item);
    } else {
        history.transcript_blocks.push(TranscriptBlock {
            after_message,
            turn_id: Some(turn_id),
            activities: vec![item],
        });
    }
}

fn upsert_replay_activity(
    history: &mut ProviderSessionHistory,
    turn_id: Uuid,
    item: ActivityItem,
    continuing_work: bool,
) {
    for block in history.transcript_blocks.iter_mut().rev() {
        if block.turn_id != Some(turn_id) {
            continue;
        }
        let matching = block.activities.iter_mut().rev().find(|activity| {
            item.source_id
                .as_ref()
                .is_some_and(|id| activity.source_id.as_ref() == Some(id))
                || (item.source_id.is_none() && activity.title == item.title && !activity.complete)
        });
        if let Some(activity) = matching {
            let has_arguments = item.arguments.is_some();
            if item.kind != ActivityKind::Tool || activity.kind == ActivityKind::Tool {
                activity.kind = item.kind;
            }
            let incoming_title = item.title.trim();
            let incoming_is_generic_tool = incoming_title.eq_ignore_ascii_case("tool")
                || incoming_title == tr!("activity.tool");
            let existing_title = activity.title.trim();
            let existing_is_meaningful = !existing_title.is_empty()
                && !existing_title.eq_ignore_ascii_case("tool")
                && existing_title != tr!("activity.tool");
            if !incoming_title.is_empty()
                && !(incoming_is_generic_tool && existing_is_meaningful)
            {
                activity.title = item.title;
            }
            if item.tool_name.is_some() {
                activity.tool_name = item.tool_name;
            }
            if item.mcp_server.is_some() {
                activity.mcp_server = item.mcp_server;
            }
            activity.complete = item.complete;
            activity.failed = item.failed;
            if item.detail.is_some() {
                activity.detail = item.detail;
            }
            if item.arguments.is_some() {
                activity.arguments = item.arguments;
            }
            if item.output.is_some() {
                activity.output = item.output;
            }
            if !item.image_urls.is_empty() {
                activity.image_urls = item.image_urls;
            }
            if !item.file_changes.is_empty() {
                activity.file_changes = item.file_changes;
            }
            if item.display_target.is_some() && (activity.display_target.is_none() || has_arguments)
            {
                activity.display_target = item.display_target;
            }
            if item.display_description.is_some()
                && (activity.display_description.is_none() || has_arguments)
            {
                activity.display_description = item.display_description;
            }
            return;
        }
    }
    push_replay_activity(history, turn_id, item, continuing_work);
}

fn history_from_updates(provider: ProviderKind, updates: &[Value]) -> ProviderSessionHistory {
    let mut history = ProviderSessionHistory::default();
    let mut saw_agent_activity = true;
    let mut phase: Option<ReplayPhase> = None;
    let mut tools = HashMap::new();

    for update in updates {
        let kind = update.get("sessionUpdate").and_then(Value::as_str);
        if kind == Some("user_message_chunk") {
            let Some(text) = update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            else {
                continue;
            };
            if history.turns.is_empty() || saw_agent_activity {
                let turn_id = Uuid::new_v4();
                history.turns.push(AgentTurn {
                    id: turn_id,
                    turn_count: history.turns.len() + 1,
                    status: TurnStatus::Completed,
                    provider_turn_started: true,
                    provider_resume_at: None,
                    started_at: 0,
                    completed_at: Some(0),
                    checkpoint: None,
                    provider: Some(provider),
                    account_id: None,
                    handoff: None,
                    native_conversation_start: false,
                    stream_stats: None,
                    handoff_preamble: None,
                });
                history
                    .messages
                    .push(Message::new_for_turn(MessageRole::User, text, turn_id));
                saw_agent_activity = false;
                phase = None;
            } else if let Some(message) = history.messages.last_mut().filter(|message| {
                message.role == MessageRole::User
                    && message.turn_id == history.turns.last().map(|turn| turn.id)
            }) {
                message.content.push_str(text);
            }
            continue;
        }

        if matches!(
            kind,
            Some(
                "agent_message_chunk"
                    | "agent_thought_chunk"
                    | "tool_call"
                    | "tool_call_update"
                    | "plan"
            )
        ) {
            saw_agent_activity = true;
        }
        let Some(turn_id) = history.turns.last().map(|turn| turn.id) else {
            continue;
        };

        match kind {
            Some("agent_thought_chunk") => {
                let Some(text) = update
                    .pointer("/content/text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
                let continuing = phase == Some(ReplayPhase::Reasoning);
                if !continuing && text.trim().is_empty() {
                    continue;
                }
                if continuing
                    && let Some(reasoning) = history
                        .transcript_blocks
                        .last_mut()
                        .filter(|block| block.turn_id == Some(turn_id))
                        .and_then(|block| block.activities.last_mut())
                        .and_then(|activity| activity.reasoning.as_mut())
                {
                    reasoning.content.push_str(text);
                } else {
                    push_replay_activity(
                        &mut history,
                        turn_id,
                        ActivityItem::from_reasoning(
                            ReasoningBlock {
                                content: text.to_owned(),
                                started_at_ms: 0,
                                finished_at_ms: 0,
                            },
                            true,
                        ),
                        matches!(phase, Some(ReplayPhase::Reasoning | ReplayPhase::Activity)),
                    );
                }
                phase = Some(ReplayPhase::Reasoning);
            }
            Some("tool_call" | "tool_call_update") => {
                let item = crate::driver::build_tool_activity_item(update, &mut tools, None);
                upsert_replay_activity(
                    &mut history,
                    turn_id,
                    item,
                    matches!(phase, Some(ReplayPhase::Reasoning | ReplayPhase::Activity)),
                );
                phase = Some(ReplayPhase::Activity);
            }
            Some("plan") => {
                let item = crate::driver::plan_activity(update);
                upsert_replay_activity(
                    &mut history,
                    turn_id,
                    item,
                    matches!(phase, Some(ReplayPhase::Reasoning | ReplayPhase::Activity)),
                );
                phase = Some(ReplayPhase::Activity);
            }
            Some("agent_message_chunk") => {
                let Some(text) = update
                    .pointer("/content/text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
                if provider == ProviderKind::Fx
                    && (text.starts_with("[context] ")
                        || text.starts_with("skill discovery warning: "))
                {
                    continue;
                }
                if phase == Some(ReplayPhase::Text)
                    && let Some(message) = history.messages.last_mut().filter(|message| {
                        message.role == MessageRole::Assistant && message.turn_id == Some(turn_id)
                    })
                {
                    message.content.push_str(text);
                } else {
                    history.messages.push(Message::new_for_turn(
                        MessageRole::Assistant,
                        text,
                        turn_id,
                    ));
                }
                phase = Some(ReplayPhase::Text);
            }
            _ => {}
        }
    }

    for block in &mut history.transcript_blocks {
        for activity in &mut block.activities {
            activity.complete = true;
        }
    }
    match provider {
        ProviderKind::Antigravity => {
            crate::antigravity_session::expand_imported_history(&mut history);
        }
        ProviderKind::Cursor => {
            crate::cursor_session::expand_imported_history(&mut history);
        }
        _ => {}
    }
    history
}

fn retain_recent_messages(history: &mut ProviderSessionHistory, limit: usize) {
    let retained = history
        .turns
        .iter()
        .rev()
        .take(limit)
        .map(|turn| turn.id)
        .collect::<HashSet<_>>();
    let dropped_messages = history
        .messages
        .iter()
        .take_while(|message| !message.turn_id.is_some_and(|id| retained.contains(&id)))
        .count();
    history
        .messages
        .retain(|message| message.turn_id.is_some_and(|id| retained.contains(&id)));
    history
        .transcript_blocks
        .retain(|block| block.turn_id.is_some_and(|id| retained.contains(&id)));
    let message_count = history.messages.len();
    for block in &mut history.transcript_blocks {
        block.after_message = block
            .after_message
            .saturating_sub(dropped_messages)
            .min(message_count);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use waku_protocol::model::ActivityKind;

    use super::*;

    #[test]
    fn catalog_processes_use_an_isolated_temp_working_directory() {
        let directory = catalog_working_directory().unwrap();
        assert!(directory.starts_with(std::env::temp_dir()));
        assert!(directory.is_dir());
        if let Some(home) = dirs::home_dir() {
            assert_ne!(directory, home);
            for protected in [
                "Desktop",
                "Documents",
                "Downloads",
                "Movies",
                "Music",
                "Pictures",
            ] {
                assert!(!directory.starts_with(home.join(protected)));
            }
        }
    }

    #[test]
    fn imports_only_visible_acp_chunks_and_preserves_empty_turn_shells() {
        let history = history_from_updates(
            ProviderKind::Kimi,
            &[
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"one"}}),
                json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"private"}}),
                json!({"sessionUpdate":"tool_call","toolCallId":"call-1","title":"read"}),
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}),
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"two"}}),
                json!({"sessionUpdate":"tool_call","toolCallId":"call-2","title":"bash"}),
            ],
        );

        assert_eq!(history.turns.len(), 2);
        assert_eq!(history.messages.len(), 3);
        assert_eq!(history.messages[0].content, "one");
        assert_eq!(history.messages[1].content, "done");
        assert_eq!(history.messages[2].content, "two");
        assert!(
            history
                .messages
                .iter()
                .all(|message| !message.content.contains("private"))
        );
    }

    #[test]
    fn joins_consecutive_content_blocks_without_inventing_turns() {
        let history = history_from_updates(
            ProviderKind::Kimi,
            &[
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello "}}),
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"world"}}),
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"part 1"}}),
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":" + part 2"}}),
            ],
        );

        assert_eq!(history.turns.len(), 1);
        assert_eq!(history.messages[0].content, "hello world");
        assert_eq!(history.messages[1].content, "part 1 + part 2");
    }

    #[test]
    fn reconstructs_transcript_blocks_and_retains_recent() {
        let mut history = history_from_updates(
            ProviderKind::Cursor,
            &[
                // Turn 1
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"prompt 1"}}),
                json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking 1"}}),
                json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":" more"}}),
                json!({"sessionUpdate":"tool_call","toolCallId":"call-1","title":"read_file","rawInput":{"path":"foo.rs"}}),
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer 1"}}),
                // Turn 2
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"prompt 2"}}),
                json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking 2"}}),
                json!({"sessionUpdate":"tool_call","toolCallId":"call-2","title":"write_file","rawInput":{"path":"bar.rs"}}),
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer 2"}}),
            ],
        );

        assert_eq!(history.turns.len(), 2);
        assert_eq!(history.messages.len(), 4);
        assert_eq!(history.transcript_blocks.len(), 2);

        // Turn 1 block
        let block1 = &history.transcript_blocks[0];
        assert_eq!(block1.after_message, 1); // after "prompt 1"
        assert_eq!(block1.activities.len(), 2); // 1 reasoning, 1 tool call
        assert_eq!(
            block1.activities[0].reasoning.as_ref().unwrap().content,
            "thinking 1 more"
        );
        assert_eq!(block1.activities[1].kind, ActivityKind::FileRead);
        assert_eq!(block1.activities[1].title, "read_file");

        // Turn 2 block
        let block2 = &history.transcript_blocks[1];
        assert_eq!(block2.after_message, 3); // after "prompt 2"
        assert_eq!(block2.activities.len(), 2);
        assert_eq!(
            block2.activities[0].reasoning.as_ref().unwrap().content,
            "thinking 2"
        );
        assert_eq!(block2.activities[1].kind, ActivityKind::FileChange);
        assert_eq!(block2.activities[1].title, "write_file");

        // Now test retain_recent_messages with limit 1
        retain_recent_messages(&mut history, 1);
        assert_eq!(history.messages.len(), 2);
        assert_eq!(history.messages[0].content, "prompt 2");
        assert_eq!(history.messages[1].content, "answer 2");
        assert_eq!(history.transcript_blocks.len(), 1);
        assert_eq!(history.transcript_blocks[0].after_message, 1);
    }

    #[test]
    fn catalog_directory_restricts_permissions_on_unix() {
        let directory = catalog_working_directory().unwrap();
        assert!(directory.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn list_provider_sessions_fetches_all_pages_before_sorting_and_truncating() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, InitializeResponse, ListSessionsResponse, SessionCapabilities,
            SessionInfo, SessionListCapabilities,
        };

        let cwd = std::env::temp_dir();
        let cwd_for_agent = cwd.clone();
        let pages_requested = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let pages_seen = Arc::clone(&pages_requested);

        let mock_agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    let caps = AgentCapabilities::new().session_capabilities(
                        SessionCapabilities::new().list(SessionListCapabilities::new()),
                    );
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(caps),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ListSessionsRequest, responder, _cx| {
                    pages_seen.lock().push(req.cursor.clone());
                    let response = match req.cursor.as_deref() {
                        None => ListSessionsResponse::new(vec![
                            SessionInfo::new("old-1", cwd_for_agent.clone())
                                .title("Oldest")
                                .updated_at("2026-01-01T00:00:01Z"),
                            SessionInfo::new("mid-2", cwd_for_agent.clone())
                                .title("Middle")
                                .updated_at("2026-01-02T00:00:01Z"),
                        ])
                        .next_cursor("page-2".to_owned()),
                        Some("page-2") => ListSessionsResponse::new(vec![
                            SessionInfo::new("newest-3", cwd_for_agent.clone())
                                .title("Newest")
                                .updated_at("2026-01-03T00:00:01Z"),
                        ]),
                        Some(_) => ListSessionsResponse::new(Vec::new()),
                    };
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            );

        let sessions =
            list_provider_sessions_with_agent(ProviderKind::Cursor, mock_agent, &[], 2).unwrap();
        assert_eq!(
            *pages_requested.lock(),
            vec![None, Some("page-2".to_owned())],
            "pagination must not stop at `limit` before fetching later pages"
        );
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].cursor.native_id(), "newest-3");
        assert_eq!(sessions[1].cursor.native_id(), "mid-2");
    }

    #[test]
    fn session_cwd_keeps_absolute_paths_and_drops_the_rest() {
        let native = std::env::temp_dir();
        assert_eq!(native_session_cwd(&native), Some(native));
        assert_eq!(native_session_cwd(Path::new("relative/dir")), None);
        // A drive without a separator is drive-relative, not absolute.
        assert_eq!(native_session_cwd(Path::new("c:relative")), None);
    }

    #[cfg(windows)]
    #[test]
    fn session_cwd_restores_the_native_path_from_agy_url_forms() {
        // The shapes `agy` 1.5.15 answers `session/list` with on Windows.
        assert_eq!(
            native_session_cwd(Path::new("/c:/Users/me/git/waku")),
            Some(PathBuf::from(r"C:\Users\me\git\waku"))
        );
        assert_eq!(
            native_session_cwd(Path::new("/C%3A/Users/me/My%20Docs/repo")),
            Some(PathBuf::from(r"C:\Users\me\My Docs\repo"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn lists_sessions_whose_cwd_is_a_url_path() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, InitializeResponse, ListSessionsResponse, SessionCapabilities,
            SessionInfo, SessionListCapabilities,
        };

        let mock_agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    let caps = AgentCapabilities::new().session_capabilities(
                        SessionCapabilities::new().list(SessionListCapabilities::new()),
                    );
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(caps),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ListSessionsRequest, responder, _cx| {
                    responder.respond(ListSessionsResponse::new(vec![
                        SessionInfo::new("native-1", r"C:\Users\me\git\waku"),
                        SessionInfo::new("url-2", "/c:/Users/me/git/waku"),
                        SessionInfo::new("url-3", "/c%3A/Users/me/git/waku"),
                        SessionInfo::new("relative-4", "Users/me/git"),
                    ]))
                },
                agent_client_protocol::on_receive_request!(),
            );

        let sessions =
            list_provider_sessions_with_agent(ProviderKind::Antigravity, mock_agent, &[], 10)
                .unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.cursor.native_id())
                .collect::<Vec<_>>(),
            ["native-1", "url-2", "url-3"]
        );
        assert_eq!(sessions[2].cwd, PathBuf::from(r"C:\Users\me\git\waku"));
    }

    #[test]
    fn cursor_replay_strips_branch_envelope_and_keeps_block_alignment() {
        let context = serde_json::to_string(&vec![
            json!({"role": "user", "content": "prior prompt"}),
            json!({"role": "assistant", "content": "prior answer"}),
        ])
        .unwrap();
        let seeded = crate::cursor_session::prompt_with_fork_context(&context, "branch follow-up");
        let (first_half, second_half) = seeded.split_at(seeded.len() / 2);

        let history = history_from_updates(
            ProviderKind::Cursor,
            &[
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":first_half}}),
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":second_half}}),
                json!({"sessionUpdate":"tool_call","toolCallId":"c1","title":"read_file","rawInput":{"path":"lib.rs"}}),
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"branch reply"}}),
            ],
        );

        assert_eq!(history.turns.len(), 2);
        assert_eq!(history.messages.len(), 4);
        assert_eq!(history.messages[0].content, "prior prompt");
        assert_eq!(history.messages[1].content, "prior answer");
        assert_eq!(history.messages[2].content, "branch follow-up");
        assert_eq!(history.messages[3].content, "branch reply");
        assert!(
            history
                .messages
                .iter()
                .all(|m| !m.content.contains("WAKU_CURSOR_BRANCH_CONTEXT_V1"))
        );
        assert_eq!(history.transcript_blocks.len(), 1);
        assert_eq!(history.transcript_blocks[0].after_message, 3);
        assert_eq!(
            session_title(ProviderKind::Cursor, Some(&seeded), "cursor-12345678"),
            "branch follow-up"
        );
    }

    #[test]
    fn antigravity_replay_strips_branch_envelope_and_keeps_block_alignment() {
        let context = serde_json::to_string(&vec![
            json!({"role": "user", "content": "prior prompt"}),
            json!({"role": "assistant", "content": "prior answer"}),
        ])
        .unwrap();
        let seeded =
            crate::antigravity_session::prompt_with_fork_context(&context, "branch follow-up");
        let (first_half, second_half) = seeded.split_at(seeded.len() / 2);

        let history = history_from_updates(
            ProviderKind::Antigravity,
            &[
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":first_half}}),
                json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":second_half}}),
                json!({"sessionUpdate":"tool_call","toolCallId":"c1","title":"read_file","rawInput":{"path":"lib.rs"}}),
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"branch reply"}}),
            ],
        );

        assert_eq!(history.turns.len(), 2);
        assert_eq!(history.turns[0].provider, Some(ProviderKind::Antigravity));
        assert_eq!(history.messages.len(), 4);
        assert_eq!(history.messages[0].content, "prior prompt");
        assert_eq!(history.messages[1].content, "prior answer");
        assert_eq!(history.messages[2].content, "branch follow-up");
        assert_eq!(history.messages[3].content, "branch reply");
        assert!(
            history
                .messages
                .iter()
                .all(|m| !m.content.contains("WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1"))
        );
        assert_eq!(history.transcript_blocks.len(), 1);
        assert_eq!(history.transcript_blocks[0].after_message, 3);
        assert_eq!(
            session_title(ProviderKind::Antigravity, Some(&seeded), "agy-12345678"),
            "branch follow-up"
        );
        assert_eq!(
            session_title(
                ProviderKind::Antigravity,
                Some("WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1 500\n[{\"role\":\"user\"..."),
                "agy-12345678"
            ),
            "Antigravity session agy-1234"
        );
    }

    #[test]
    fn droid_replay_preserves_tool_metadata_across_completed_call_and_sparse_update() {
        let history = history_from_updates(
            ProviderKind::Droid,
            &[
                json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": { "type": "text", "text": "Inspect lib.rs and run tests" }
                }),
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "droid-read-1",
                    "status": "completed",
                    "kind": "read",
                    "title": "Read src/lib.rs",
                    "rawInput": { "file_path": "src/lib.rs" }
                }),
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "droid-read-1",
                    "status": "completed",
                    "content": [
                        { "type": "content", "content": { "type": "text", "text": "fn main() {}" } }
                    ],
                    "rawOutput": "fn main() {}"
                }),
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "droid-exec-2",
                    "status": "completed",
                    "kind": "execute",
                    "title": "Run cargo test",
                    "rawInput": { "command": "cargo test" }
                }),
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "droid-exec-2",
                    "status": "failed",
                    "content": [
                        { "type": "content", "content": { "type": "text", "text": "exit status 101" } }
                    ],
                    "rawOutput": "exit status 101"
                }),
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "Tests failed." }
                }),
            ],
        );

        assert_eq!(history.transcript_blocks.len(), 1);
        let activities = &history.transcript_blocks[0].activities;
        assert_eq!(activities.len(), 2);

        assert_eq!(activities[0].kind, ActivityKind::FileRead);
        assert_eq!(activities[0].title, "Read src/lib.rs");
        assert!(activities[0].arguments.is_some());
        assert_eq!(activities[0].output.as_deref(), Some("fn main() {}"));
        assert!(activities[0].complete);
        assert!(!activities[0].failed);

        assert_eq!(activities[1].kind, ActivityKind::Command);
        assert_eq!(activities[1].title, "Run cargo test");
        assert!(activities[1].arguments.is_some());
        assert_eq!(activities[1].output.as_deref(), Some("exit status 101"));
        assert!(activities[1].complete);
        assert!(activities[1].failed);
    }

    #[test]
    fn upsert_replay_activity_guards_against_generic_fallback_clobber() {
        let mut history = ProviderSessionHistory::default();
        let turn_id = Uuid::new_v4();
        let initial = ActivityItem::new(
            Some("tool-1".to_owned()),
            ActivityKind::FileChange,
            "Edit src/main.rs",
            None,
            true,
        );
        upsert_replay_activity(&mut history, turn_id, initial, false);

        let mut sparse_generic = ActivityItem::new(
            Some("tool-1".to_owned()),
            ActivityKind::Tool,
            "Tool",
            None,
            true,
        );
        sparse_generic.output = Some("updated diff".to_owned());
        upsert_replay_activity(&mut history, turn_id, sparse_generic, true);

        let mut empty_title = ActivityItem::new(
            Some("tool-1".to_owned()),
            ActivityKind::Tool,
            "   ",
            None,
            true,
        );
        empty_title.failed = true;
        upsert_replay_activity(&mut history, turn_id, empty_title, true);

        let activity = &history.transcript_blocks[0].activities[0];
        assert_eq!(activity.kind, ActivityKind::FileChange);
        assert_eq!(activity.title, "Edit src/main.rs");
        assert_eq!(activity.output.as_deref(), Some("updated diff"));
        assert!(activity.failed);

        // Conversely, a specific update should upgrade an initially generic activity,
        // while a whitespace-only update must never clobber an existing `"Tool"` label.
        let generic_first = ActivityItem::new(
            Some("tool-2".to_owned()),
            ActivityKind::Tool,
            "Tool",
            None,
            false,
        );
        upsert_replay_activity(&mut history, turn_id, generic_first, true);

        let blank_over_tool = ActivityItem::new(
            Some("tool-2".to_owned()),
            ActivityKind::Tool,
            "   ",
            None,
            false,
        );
        upsert_replay_activity(&mut history, turn_id, blank_over_tool, true);
        assert_eq!(history.transcript_blocks[0].activities[1].title, "Tool");

        let specific_later = ActivityItem::new(
            Some("tool-2".to_owned()),
            ActivityKind::Search,
            "Search workspace",
            None,
            true,
        );
        upsert_replay_activity(&mut history, turn_id, specific_later, true);

        let upgraded = &history.transcript_blocks[0].activities[1];
        assert_eq!(upgraded.kind, ActivityKind::Search);
        assert_eq!(upgraded.title, "Search workspace");

        // Failed status is preserved across a follow-up sparse update that omits `status`.
        let user_json = serde_json::json!({
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": "Run check" }
        });
        let failed_json = serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tool-3",
            "status": "failed",
            "kind": "execute",
            "title": "Run cargo check",
            "rawInput": { "command": "cargo check" }
        });
        let sparse_follow_up = serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tool-3",
            "rawOutput": "compilation failed"
        });
        let replay = history_from_updates(
            ProviderKind::Droid,
            &[user_json, failed_json, sparse_follow_up],
        );
        let replayed = &replay.transcript_blocks[0].activities[0];
        assert!(replayed.complete);
        assert!(replayed.failed);
        assert_eq!(replayed.output.as_deref(), Some("compilation failed"));
    }

    #[test]
    fn drain_trailing_replay_exits_promptly_when_sender_closes() {
        let last_update_at = Mutex::new(None);
        let (tx, rx) = smol::channel::bounded::<()>(1);
        drop(tx);
        let started = Instant::now();
        smol::block_on(drain_trailing_replay(
            &last_update_at,
            &rx,
            Duration::from_secs(2),
            Duration::from_secs(2),
        ));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "closed update channel must exit immediately instead of spinning"
        );
    }

    #[test]
    fn session_history_drains_trailing_replay_chunks_past_50ms() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, ContentBlock, InitializeResponse, LoadSessionResponse,
            SessionCapabilities, TextContent,
        };

        let mock_agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    let caps = AgentCapabilities::new()
                        .load_session(true)
                        .session_capabilities(SessionCapabilities::new());
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(caps),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: LoadSessionRequest, responder, cx| {
                    let session_id = req.session_id.clone();
                    responder.respond(LoadSessionResponse::new())?;
                    // Emit trailing replay chunks after `session/load` responds,
                    // spaced past the old fixed 50ms sleep.
                    let cx_bg = cx.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(80));
                        let _ = cx_bg.send_notification(SessionNotification::new(
                            session_id.clone(),
                            agent_client_protocol::schema::v1::SessionUpdate::UserMessageChunk(
                                agent_client_protocol::schema::v1::ContentChunk::new(
                                    ContentBlock::Text(TextContent::new("delayed prompt")),
                                ),
                            ),
                        ));
                        std::thread::sleep(Duration::from_millis(90));
                        let _ = cx_bg.send_notification(SessionNotification::new(
                            session_id,
                            agent_client_protocol::schema::v1::SessionUpdate::AgentMessageChunk(
                                agent_client_protocol::schema::v1::ContentChunk::new(
                                    ContentBlock::Text(TextContent::new("delayed answer")),
                                ),
                            ),
                        ));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            );

        let history = provider_session_history_with_agent(
            ProviderKind::Antigravity,
            mock_agent,
            &std::env::temp_dir(),
            "sess-delayed",
            10,
            // Give loaded CI runners enough scheduling slack above the 80ms/90ms sleeps.
            Duration::from_millis(350),
            Duration::from_secs(2),
        )
        .unwrap();

        assert_eq!(history.messages.len(), 2);
        assert_eq!(history.messages[0].content, "delayed prompt");
        assert_eq!(history.messages[1].content, "delayed answer");
    }

    #[test]
    fn list_provider_sessions_filters_ephemeral_catalog_working_directory() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, InitializeResponse, ListSessionsResponse, SessionCapabilities,
            SessionInfo, SessionListCapabilities,
        };

        let temp_root = std::env::temp_dir().join(format!(
            "waku-acp-session-catalog-filter-{}",
            Uuid::new_v4()
        ));
        let catalog_dir = temp_root.join("waku-provider-session-catalog-9999");
        let real_workspace = temp_root.join("real-workspace");
        std::fs::create_dir_all(&catalog_dir).unwrap();
        std::fs::create_dir_all(&real_workspace).unwrap();

        #[cfg(windows)]
        let catalog_variants = {
            let raw = catalog_dir.to_string_lossy().into_owned();
            let lowercase = raw.to_ascii_lowercase();
            let bytes = raw.as_bytes();
            let url_form = if bytes.len() >= 3 && bytes[1] == b':' {
                format!(
                    "/{}%3A/{}",
                    (bytes[0] as char).to_ascii_lowercase(),
                    raw[3..].replace('\\', "/")
                )
            } else {
                raw.replace('\\', "/")
            };
            vec![lowercase, url_form]
        };
        #[cfg(not(windows))]
        let catalog_variants = vec![catalog_dir.to_string_lossy().into_owned()];

        let real_workspace_str = real_workspace.to_string_lossy().into_owned();
        let mock_agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    let caps = AgentCapabilities::new().session_capabilities(
                        SessionCapabilities::new().list(SessionListCapabilities::new()),
                    );
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(caps),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ListSessionsRequest, responder, _cx| {
                    let mut entries: Vec<SessionInfo> = catalog_variants
                        .iter()
                        .enumerate()
                        .map(|(index, cwd)| {
                            SessionInfo::new(format!("projectless-{index}"), cwd.as_str())
                                .title("Projectless session")
                        })
                        .collect();
                    entries.push(
                        SessionInfo::new("real-1", real_workspace_str.as_str())
                            .title("Real workspace session"),
                    );
                    responder.respond(ListSessionsResponse::new(entries))
                },
                agent_client_protocol::on_receive_request!(),
            );

        let sessions = list_provider_sessions_with_agent_in(
            ProviderKind::Antigravity,
            mock_agent,
            &[],
            10,
            Some(&catalog_dir),
        )
        .unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].cursor.native_id(), "real-1");
        assert_eq!(sessions[0].cwd, real_workspace);

        let _ = std::fs::remove_dir_all(&temp_root);
    }
}
