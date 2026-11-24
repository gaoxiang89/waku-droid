//! Read-only filesystem reader for Antigravity native swarm boards.
//!
//! Antigravity writes each swarm under the coordinator conversation's brain
//! directory:
//!   `<data_root>/brain/<coordinator_conv>/.system_generated/swarm/`
//!     - `members.json`
//!     - `board.jsonl`
//!     - `view.json` (optional narrator overview; fallback `<narrator_conv>/swarm_view/view.json`)
//! and each member's compact transcript at:
//!   `<data_root>/brain/<member_conv>/.system_generated/logs/transcript.jsonl`
//!
//! Every file read here is strictly bounded in bytes and read-only. All
//! session and conversation IDs are validated against path traversal before
//! joining onto any filesystem path.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::bail;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use waku_protocol::antigravity_swarm::{
    AntigravitySwarmAttentionItem, AntigravitySwarmFailure, AntigravitySwarmFailureKind,
    AntigravitySwarmMember, AntigravitySwarmMemberStatus, AntigravitySwarmMessage,
    AntigravitySwarmOverview, AntigravitySwarmPhase, AntigravitySwarmSnapshot,
    AntigravitySwarmViewItem, AntigravitySwarmWorkstream,
};

const DATA_ROOTS: [&str; 3] = [
    ".gemini/antigravity-cli",
    ".gemini/antigravity-acp",
    ".gemini/antigravity",
];

const MAX_SESSION_ID_LEN: usize = 128;
const MAX_MEMBERS_BYTES: u64 = 512 * 1024;
const MAX_BOARD_TAIL_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BOARD_MESSAGES: usize = 500;
const MAX_VIEW_BYTES: u64 = 64 * 1024;
const MAX_TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;
const MAX_BRAIN_SCAN_DIRS: usize = 512;
const MAX_WORKER_SWARM_CACHE: usize = 256;

fn worker_swarm_cache() -> &'static Mutex<HashMap<String, (PathBuf, String)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (PathBuf, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn locate_cached_worker_swarm(roots: &[PathBuf], session_id: &str) -> Option<LocatedSwarm> {
    let cached = worker_swarm_cache().lock().get(session_id).cloned()?;
    let (cached_root, cached_swarm_id) = cached;
    if !roots.contains(&cached_root) {
        return None;
    }
    let members_path = swarm_dir(&cached_root, &cached_swarm_id).join("members.json");
    if let Some((stamp, roster)) = read_roster(&members_path)
        && let Some(member_name) = member_name_for_conversation(&roster, session_id)
    {
        return Some(LocatedSwarm {
            app_data: cached_root,
            swarm_id: cached_swarm_id,
            roster,
            members_stamp: stamp,
            current_member: Some(member_name),
        });
    }
    let mut cache = worker_swarm_cache().lock();
    if cache
        .get(session_id)
        .is_some_and(|entry| entry.0 == cached_root && entry.1 == cached_swarm_id)
    {
        cache.remove(session_id);
    }
    None
}

fn record_worker_swarm_cache(session_id: &str, app_data: &Path, swarm_id: &str) {
    let mut cache = worker_swarm_cache().lock();
    if cache.len() >= MAX_WORKER_SWARM_CACHE && !cache.contains_key(session_id) {
        cache.clear();
    }
    cache.insert(
        session_id.to_owned(),
        (app_data.to_path_buf(), swarm_id.to_owned()),
    );
}
const MAX_OVERVIEW_ITEMS: usize = 64;
const STEP_ERROR_SETTLE_MS: u64 = 60 * 1000;
const STUCK_MS: u64 = 10 * 60 * 1000;

/// Rejects empty, overly long, or non-`[A-Za-z0-9_-]` IDs so a caller or
/// untrusted roster entry can never escape `brain/`.
pub fn validate_session_id(session_id: &str) -> anyhow::Result<()> {
    if is_valid_session_id(session_id) {
        Ok(())
    } else {
        bail!("invalid Antigravity session id");
    }
}

pub(crate) fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID_LEN
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
}

/// Reads the Antigravity swarm snapshot for `session_id` (whether `session_id`
/// is the coordinator or a worker listed on a swarm's roster).
pub fn read_swarm_snapshot(session_id: &str) -> anyhow::Result<Option<AntigravitySwarmSnapshot>> {
    read_swarm_snapshot_in(session_id, None)
}

/// `config_dir`-scoped variant of [`read_swarm_snapshot`]. When `config_dir` is
/// `Some`, looks only inside that account root (plus its parent app-data root
/// when `config_dir` is a `sidecar_data` directory) without falling back to
/// default Antigravity data roots.
pub fn read_swarm_snapshot_in(
    session_id: &str,
    config_dir: Option<&Path>,
) -> anyhow::Result<Option<AntigravitySwarmSnapshot>> {
    validate_session_id(session_id)?;
    let roots = match config_dir {
        Some(dir) => account_data_roots(dir),
        None => default_data_roots(),
    };
    if roots.is_empty() {
        return Ok(None);
    }
    read_swarm_snapshot_in_roots(&roots, session_id, now_unix_ms())
}

fn account_data_roots(config_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if config_dir.file_name().and_then(|name| name.to_str()) == Some("sidecar_data")
        && let Some(app_data) = app_data_root_from_sidecar_env(config_dir)
        && app_data.join("brain").is_dir()
    {
        roots.push(app_data);
    }
    if config_dir.join("brain").is_dir() && !roots.contains(&config_dir.to_path_buf()) {
        roots.push(config_dir.to_path_buf());
    }
    roots
}

fn default_data_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(exec_data_dir) = std::env::var("ANTIGRAVITY_EXECUTABLE_DATA_DIR")
        && let Some(app_data) = app_data_root_from_sidecar_env(Path::new(&exec_data_dir))
        && app_data.join("brain").is_dir()
    {
        roots.push(app_data);
    }
    if let Some(home) = dirs::home_dir() {
        for relative in DATA_ROOTS {
            let candidate = home.join(relative);
            if candidate.join("brain").is_dir() && !roots.contains(&candidate) {
                roots.push(candidate);
            }
        }
    }
    roots
}

fn app_data_root_from_sidecar_env(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if ancestor.file_name().and_then(|name| name.to_str()) == Some("sidecar_data") {
            return ancestor.parent().map(Path::to_path_buf);
        }
    }
    None
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn swarm_dir(app_data: &Path, swarm_id: &str) -> PathBuf {
    app_data
        .join("brain")
        .join(swarm_id)
        .join(".system_generated")
        .join("swarm")
}

fn transcript_path(app_data: &Path, conversation_id: &str) -> PathBuf {
    app_data
        .join("brain")
        .join(conversation_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl")
}

#[derive(Debug, Default, Deserialize)]
struct RawRoster {
    #[serde(default)]
    title: String,
    #[serde(default)]
    members: BTreeMap<String, RawMember>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMember {
    #[serde(default)]
    conversation_id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    applied_model: String,
    #[serde(default)]
    desired_model: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    wake_mode: String,
    #[serde(default)]
    desired_state: String,
    #[serde(default)]
    phase: String,
    #[serde(default)]
    last_read_seq: u64,
    #[serde(default)]
    killed: bool,
}

impl RawMember {
    fn is_killed(&self) -> bool {
        self.killed || self.desired_state == "KILLED" || self.phase == "KILLED"
    }

    fn effective_model(&self) -> String {
        if !self.model.is_empty() {
            self.model.clone()
        } else if !self.applied_model.is_empty() {
            self.applied_model.clone()
        } else {
            self.desired_model.clone()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawBoardLine {
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    time: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    control: Option<Value>,
}

#[derive(Debug, Default)]
struct TranscriptSummary {
    steps: u64,
    activity: Option<String>,
    activity_kind: Option<String>,
    idle: Option<bool>,
    failure: Option<AntigravitySwarmFailure>,
    mtime_ms: Option<u64>,
    file_stamp: String,
}

struct LocatedSwarm {
    app_data: PathBuf,
    swarm_id: String,
    roster: RawRoster,
    members_stamp: String,
    current_member: Option<String>,
}

pub(crate) fn read_swarm_snapshot_in_roots(
    roots: &[PathBuf],
    session_id: &str,
    now_ms: u64,
) -> anyhow::Result<Option<AntigravitySwarmSnapshot>> {
    validate_session_id(session_id)?;
    let Some(located) = locate_swarm(roots, session_id) else {
        return Ok(None);
    };
    let dir = swarm_dir(&located.app_data, &located.swarm_id);

    let member_names: Vec<String> = located.roster.members.keys().cloned().collect();
    let board_path = dir.join("board.jsonl");
    let (board_stamp, board_text, board_skipped_prefix) =
        read_tail_bounded(&board_path, MAX_BOARD_TAIL_BYTES);
    let parsed_board = parse_board(
        &board_text.unwrap_or_default(),
        &member_names,
        board_skipped_prefix,
    );

    let narrator_name = pick_narrator(&located.roster);
    let narrator_conv = narrator_name
        .as_deref()
        .and_then(|name| located.roster.members.get(name))
        .map(|member| member.conversation_id.as_str())
        .filter(|conv| is_valid_session_id(conv));
    let (view_stamp, overview) = read_overview(&located.app_data, &dir, narrator_conv);

    let mut members = Vec::with_capacity(located.roster.members.len());
    let mut transcript_stamps = Vec::with_capacity(located.roster.members.len());
    let mut system_attention = Vec::new();

    for (name, raw) in &located.roster.members {
        let role = normalized_role(name, &raw.role);
        let role_worker = is_role_worker(name, &role);
        let killed = raw.is_killed();
        let summary = if !killed && is_valid_session_id(&raw.conversation_id) {
            summarize_transcript(&located.app_data, &raw.conversation_id, now_ms)
        } else {
            TranscriptSummary::default()
        };
        transcript_stamps.push(format!("{}:{}", name, summary.file_stamp));

        let status = derive_member_status(raw, &summary);
        let posts = parsed_board.posts_by_agent.get(name).copied().unwrap_or(0);
        let unread = parsed_board.head_seq.saturating_sub(raw.last_read_seq);
        let activity = if status == AntigravitySwarmMemberStatus::Working {
            summary.activity.clone()
        } else {
            None
        };
        let activity_kind = if activity.is_some() {
            summary.activity_kind.clone()
        } else {
            None
        };

        if !killed && !role_worker {
            if let Some(failure) = &summary.failure {
                let kind_label = match failure.kind {
                    AntigravitySwarmFailureKind::Quota => "quota",
                    AntigravitySwarmFailureKind::Error => "error",
                };
                system_attention.push(AntigravitySwarmAttentionItem {
                    id: format!("sys-{name}-{kind_label}"),
                    kind: kind_label.to_owned(),
                    severity: "high".to_owned(),
                    title: format!("{name}: last step failed ({kind_label})"),
                    why: failure.message.clone(),
                    member: Some(name.clone()),
                });
            } else if status == AntigravitySwarmMemberStatus::Working
                && let Some(mtime_ms) = summary.mtime_ms
                && now_ms.saturating_sub(mtime_ms) > STUCK_MS
            {
                system_attention.push(AntigravitySwarmAttentionItem {
                    id: format!("sys-{name}-stuck"),
                    kind: "stuck".to_owned(),
                    severity: "med".to_owned(),
                    title: format!("{name}: no recent progress"),
                    why: String::new(),
                    member: Some(name.clone()),
                });
            }
        }

        members.push(AntigravitySwarmMember {
            name: name.clone(),
            conversation_id: raw.conversation_id.clone(),
            model: raw.effective_model(),
            role,
            is_role_worker: role_worker,
            wake_mode: raw.wake_mode.clone(),
            status,
            killed,
            last_read_seq: raw.last_read_seq,
            unread,
            posts,
            steps: summary.steps,
            activity,
            activity_kind,
            failure: summary.failure,
            transcript_mtime_ms: summary.mtime_ms,
        });
    }

    members.sort_by(|left, right| {
        member_sort_rank(left)
            .cmp(&member_sort_rank(right))
            .then_with(|| left.name.cmp(&right.name))
    });

    let fingerprint = format!(
        "{}|{}|{}|{}|{}|{}",
        located.swarm_id,
        located.members_stamp,
        board_stamp,
        view_stamp,
        transcript_stamps.join(","),
        system_attention.len()
    );

    Ok(Some(AntigravitySwarmSnapshot {
        swarm_id: located.swarm_id,
        current_member: located.current_member,
        title: located.roster.title,
        head_seq: parsed_board.head_seq,
        total_messages: parsed_board.total_messages,
        messages_truncated: parsed_board.truncated,
        members,
        messages: parsed_board.messages,
        overview,
        system_attention,
        fingerprint,
    }))
}

fn member_sort_rank(member: &AntigravitySwarmMember) -> (u8, u8) {
    let status_rank = match member.status {
        AntigravitySwarmMemberStatus::Working => 0,
        AntigravitySwarmMemberStatus::Failed => 1,
        AntigravitySwarmMemberStatus::Waiting => 2,
        AntigravitySwarmMemberStatus::Killed => 3,
    };
    let coordinator_rank = if member.role == "coordinator" { 0 } else { 1 };
    (status_rank, coordinator_rank)
}

fn locate_swarm(roots: &[PathBuf], session_id: &str) -> Option<LocatedSwarm> {
    locate_swarm_with_cap(roots, session_id, MAX_BRAIN_SCAN_DIRS)
}

fn locate_swarm_with_cap(
    roots: &[PathBuf],
    session_id: &str,
    max_scan_dirs: usize,
) -> Option<LocatedSwarm> {
    // Fast path: `session_id` directly owns a swarm dir (coordinator).
    for root in roots {
        let members_path = swarm_dir(root, session_id).join("members.json");
        if let Some((stamp, roster)) = read_roster(&members_path) {
            let current_member = member_name_for_conversation(&roster, session_id)
                .or_else(|| roster.members.contains_key("coordinator").then(|| "coordinator".to_owned()));
            return Some(LocatedSwarm {
                app_data: root.clone(),
                swarm_id: session_id.to_owned(),
                roster,
                members_stamp: stamp,
                current_member,
            });
        }
    }

    if let Some(located) = locate_cached_worker_swarm(roots, session_id) {
        return Some(located);
    }

    // Worker path: scan the most recently updated `members.json` files first so
    // non-swarm conversations never crowd out active swarms and updating
    // `members.json` under `.system_generated/swarm/` is reflected even when
    // `brain/<id>`'s top-level directory mtime is older.
    for root in roots {
        let Ok(entries) = fs::read_dir(root.join("brain")) else {
            continue;
        };
        let mut candidates: Vec<(Option<SystemTime>, String)> = entries
            .flatten()
            .filter_map(|entry| {
                let file_name = entry.file_name();
                let candidate_id = file_name.to_str()?;
                if candidate_id == session_id || !is_valid_session_id(candidate_id) {
                    return None;
                }
                let members_path = swarm_dir(root, candidate_id).join("members.json");
                let metadata = fs::metadata(&members_path).ok()?;
                if !metadata.is_file() {
                    return None;
                }
                Some((metadata.modified().ok(), candidate_id.to_owned()))
            })
            .collect();
        candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));

        for (_, candidate_id) in candidates.into_iter().take(max_scan_dirs) {
            let members_path = swarm_dir(root, &candidate_id).join("members.json");
            let Some((stamp, roster)) = read_roster(&members_path) else {
                continue;
            };
            if let Some(member_name) = member_name_for_conversation(&roster, session_id) {
                record_worker_swarm_cache(session_id, root, &candidate_id);
                return Some(LocatedSwarm {
                    app_data: root.clone(),
                    swarm_id: candidate_id,
                    roster,
                    members_stamp: stamp,
                    current_member: Some(member_name),
                });
            }
        }
    }
    None
}

fn member_name_for_conversation(roster: &RawRoster, session_id: &str) -> Option<String> {
    roster
        .members
        .iter()
        .find(|(_, member)| member.conversation_id == session_id)
        .map(|(name, _)| name.clone())
}

fn read_roster(path: &Path) -> Option<(String, RawRoster)> {
    let (stamp, text) = read_full_bounded(path, MAX_MEMBERS_BYTES)?;
    let roster: RawRoster = serde_json::from_str(&text).ok()?;
    Some((stamp, roster))
}

fn file_stamp_and_mtime(metadata: &fs::Metadata) -> (String, Option<u64>) {
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64);
    let stamp = format!("{}:{}", mtime_ms.unwrap_or(0), metadata.len());
    (stamp, mtime_ms)
}

fn read_full_bounded(path: &Path, max_bytes: u64) -> Option<(String, String)> {
    let file = fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    let (stamp, _) = file_stamp_and_mtime(&metadata);
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > max_bytes {
        return None;
    }
    Some((stamp, String::from_utf8_lossy(&bytes).into_owned()))
}

fn read_tail_bounded(path: &Path, max_bytes: u64) -> (String, Option<String>, bool) {
    let Ok(mut file) = fs::File::open(path) else {
        return ("-".to_owned(), None, false);
    };
    let Ok(metadata) = file.metadata() else {
        return ("-".to_owned(), None, false);
    };
    if !metadata.is_file() {
        return ("-".to_owned(), None, false);
    }
    let (stamp, _) = file_stamp_and_mtime(&metadata);
    let len = metadata.len();
    let mut skipped_prefix = false;
    if len > max_bytes {
        if file.seek(SeekFrom::End(-(max_bytes as i64))).is_err() {
            return (stamp, None, false);
        }
        skipped_prefix = true;
    }
    let mut bytes = Vec::new();
    if file.take(max_bytes).read_to_end(&mut bytes).is_err() {
        return (stamp, None, false);
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if skipped_prefix {
        if let Some(newline) = text.find('\n') {
            text = text[newline + 1..].to_owned();
        } else {
            text.clear();
        }
    }
    (stamp, Some(text), skipped_prefix)
}

struct ParsedBoard {
    head_seq: u64,
    total_messages: u64,
    truncated: bool,
    messages: Vec<AntigravitySwarmMessage>,
    posts_by_agent: HashMap<String, u64>,
}

fn parse_board(text: &str, roster_names: &[String], skipped_prefix: bool) -> ParsedBoard {
    let mut parsed = Vec::new();
    let mut posts_by_agent: HashMap<String, u64> = HashMap::new();
    let mut head_seq = 0u64;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawBoardLine>(trimmed) else {
            // Skip torn final lines cleanly; the next poll picks them up.
            continue;
        };
        let seq = if raw.seq > 0 {
            raw.seq
        } else {
            (parsed.len() as u64) + 1
        };
        head_seq = head_seq.max(seq);

        let is_control = raw
            .control
            .as_ref()
            .is_some_and(|value| value.is_object());
        let (mentions, mentions_all) = if is_control {
            (Vec::new(), false)
        } else {
            if !raw.agent.is_empty() {
                *posts_by_agent.entry(raw.agent.clone()).or_insert(0) += 1;
            }
            extract_mentions(&raw.text, roster_names)
        };
        let channel = raw
            .channel
            .filter(|channel| !channel.is_empty() && channel != "main");

        parsed.push(AntigravitySwarmMessage {
            seq,
            agent: raw.agent,
            text: raw.text,
            time: raw.time,
            mentions,
            mentions_all,
            kind: raw.kind.filter(|kind| !kind.is_empty()),
            channel,
        });
    }

    let truncated_by_count = parsed.len() > MAX_BOARD_MESSAGES;
    let truncated = skipped_prefix || truncated_by_count;
    let total_messages = if skipped_prefix {
        (parsed.len() as u64).max(head_seq)
    } else {
        parsed.len() as u64
    };
    let messages = if truncated_by_count {
        parsed.split_off(parsed.len() - MAX_BOARD_MESSAGES)
    } else {
        parsed
    };

    ParsedBoard {
        head_seq,
        total_messages,
        truncated,
        messages,
        posts_by_agent,
    }
}

/// Extracts `@name` (plus `<prefix>` when `name` is `<prefix>-<digits>`) and
/// `@all` at word boundaries, matching `swarm_board`'s `channels.mjs` /
/// `board/mentions.go` semantics.
pub(crate) fn extract_mentions(text: &str, roster_names: &[String]) -> (Vec<String>, bool) {
    let lower = text.to_lowercase();
    let mut mentions = Vec::new();
    for name in roster_names {
        let lower_name = name.to_lowercase();
        if mentions_name(&lower, &lower_name) {
            mentions.push(name.clone());
        }
    }
    let mentions_all = mentions_token(&lower, "all");
    (mentions, mentions_all)
}

fn is_name_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '-'
}

fn mentions_token(lower_text: &str, lower_name: &str) -> bool {
    if lower_name.is_empty() {
        return false;
    }
    let mut offset = 0;
    while let Some(rel) = lower_text[offset..].find('@') {
        let at_index = offset + rel;
        let preceded_by_name = lower_text[..at_index]
            .chars()
            .next_back()
            .is_some_and(is_name_char);
        let after_at = &lower_text[at_index + 1..];
        if !preceded_by_name && after_at.starts_with(lower_name) {
            let tail = &after_at[lower_name.len()..];
            if !tail.chars().next().is_some_and(is_name_char) {
                return true;
            }
        }
        offset = at_index + 1;
    }
    false
}

fn mentions_name(lower_text: &str, lower_name: &str) -> bool {
    if mentions_token(lower_text, lower_name) {
        return true;
    }
    if let Some((prefix, suffix)) = lower_name.rsplit_once('-')
        && !prefix.is_empty()
        && !suffix.is_empty()
        && suffix.chars().all(|character| character.is_ascii_digit())
    {
        return mentions_token(lower_text, prefix);
    }
    false
}

fn normalized_role(name: &str, raw_role: &str) -> String {
    if !raw_role.is_empty() && raw_role != "worker" {
        return raw_role.to_owned();
    }
    if let Some(inferred) = inferred_role_worker(name) {
        return inferred.to_owned();
    }
    if raw_role.is_empty() {
        "worker".to_owned()
    } else {
        raw_role.to_owned()
    }
}

fn inferred_role_worker(name: &str) -> Option<&'static str> {
    for role in ["narrator", "monitor"] {
        if name == role {
            return Some(role);
        }
        if let Some(suffix) = name.strip_prefix(role).and_then(|rest| rest.strip_prefix('-'))
            && !suffix.is_empty()
            && suffix.chars().all(|character| character.is_ascii_digit())
        {
            return Some(role);
        }
    }
    None
}

fn is_role_worker(name: &str, normalized_role: &str) -> bool {
    (!normalized_role.is_empty()
        && normalized_role != "worker"
        && normalized_role != "coordinator")
        || inferred_role_worker(name).is_some()
}

fn narrator_rank(name: &str, raw_role: &str) -> Option<u32> {
    if normalized_role(name, raw_role) != "narrator" {
        return None;
    }
    if name == "narrator" {
        return Some(1);
    }
    if let Some((_, suffix)) = name.rsplit_once('-')
        && let Ok(num) = suffix.parse::<u32>()
    {
        return Some(num);
    }
    Some(1)
}

fn pick_narrator(roster: &RawRoster) -> Option<String> {
    let mut live: Option<(u32, String)> = None;
    let mut dead: Option<(u32, String)> = None;
    for (name, member) in &roster.members {
        let Some(rank) = narrator_rank(name, &member.role) else {
            continue;
        };
        let slot = if member.is_killed() { &mut dead } else { &mut live };
        if slot.as_ref().is_none_or(|(best, _)| rank > *best) {
            *slot = Some((rank, name.clone()));
        }
    }
    live.or(dead).map(|(_, name)| name)
}

fn derive_member_status(raw: &RawMember, summary: &TranscriptSummary) -> AntigravitySwarmMemberStatus {
    if raw.is_killed() {
        return AntigravitySwarmMemberStatus::Killed;
    }
    if summary.failure.is_some() {
        return AntigravitySwarmMemberStatus::Failed;
    }
    if let Some(idle) = summary.idle {
        return if idle {
            AntigravitySwarmMemberStatus::Waiting
        } else {
            AntigravitySwarmMemberStatus::Working
        };
    }
    if raw.wake_mode == "mentions" || raw.wake_mode == "anything" {
        AntigravitySwarmMemberStatus::Waiting
    } else {
        AntigravitySwarmMemberStatus::Working
    }
}

fn summarize_transcript(app_data: &Path, conversation_id: &str, now_ms: u64) -> TranscriptSummary {
    let path = transcript_path(app_data, conversation_id);
    let Ok(mut file) = fs::File::open(&path) else {
        return TranscriptSummary {
            file_stamp: "-".to_owned(),
            ..Default::default()
        };
    };
    let Ok(metadata) = file.metadata() else {
        return TranscriptSummary {
            file_stamp: "-".to_owned(),
            ..Default::default()
        };
    };
    if !metadata.is_file() {
        return TranscriptSummary {
            file_stamp: "-".to_owned(),
            ..Default::default()
        };
    }
    let (file_stamp, mtime_ms) = file_stamp_and_mtime(&metadata);
    let len = metadata.len();
    let mut skipped_prefix = false;
    if len > MAX_TRANSCRIPT_TAIL_BYTES {
        if file
            .seek(SeekFrom::End(-(MAX_TRANSCRIPT_TAIL_BYTES as i64)))
            .is_err()
        {
            return TranscriptSummary {
                file_stamp,
                mtime_ms,
                ..Default::default()
            };
        }
        skipped_prefix = true;
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_TRANSCRIPT_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return TranscriptSummary {
            file_stamp,
            mtime_ms,
            ..Default::default()
        };
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if skipped_prefix {
        if let Some(newline) = text.find('\n') {
            text = text[newline + 1..].to_owned();
        } else {
            text.clear();
        }
    }

    let mut summary = summarize_transcript_text(&text, mtime_ms, now_ms);
    summary.file_stamp = file_stamp;
    summary
}

fn summarize_transcript_text(
    text: &str,
    mtime_ms: Option<u64>,
    now_ms: u64,
) -> TranscriptSummary {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return TranscriptSummary {
            mtime_ms,
            ..Default::default()
        };
    }

    let mut steps = lines.len() as u64;
    let mut last_step: Option<(String, String, String)> = None;
    let mut activity: Option<String> = None;
    let mut activity_kind: Option<String> = None;

    for (index, line) in lines.iter().enumerate().rev() {
        let Ok(step) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(obj) = step.as_object() else {
            continue;
        };
        if last_step.is_none() {
            if let Some(step_index) = obj.get("step_index").and_then(Value::as_u64) {
                let trailing = (lines.len() - 1 - index) as u64;
                steps = steps.max(step_index.saturating_add(1).saturating_add(trailing));
            }
            let step_type = obj
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let status = obj
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let error = obj
                .get("error")
                .and_then(Value::as_str)
                .map(|value| one_line(value, 240))
                .unwrap_or_default();
            last_step = Some((step_type, status, error));
        }

        let step_type = obj.get("type").and_then(Value::as_str).unwrap_or_default();
        if step_type == "USER_INPUT" {
            break;
        }
        if step_type != "PLANNER_RESPONSE" {
            continue;
        }
        let tool_calls = obj
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if tool_calls.iter().any(|call| {
            call.get("name").and_then(Value::as_str) == Some("swarm_wait")
        }) {
            break;
        }
        let labeled_tool = tool_calls
            .iter()
            .rev()
            .find_map(describe_tool_call);
        if let Some(label) = labeled_tool {
            activity = Some(label);
            activity_kind = Some("tool".to_owned());
            break;
        }
        if let Some(thinking) = obj.get("thinking").and_then(Value::as_str)
            && let Some(heading) = thinking_heading(thinking)
        {
            activity = Some(heading);
            activity_kind = Some("thinking".to_owned());
            break;
        }
    }

    let idle = Some(transcript_turn_ended(&lines));
    let failure = last_step.and_then(|(step_type, status, error)| {
        let model_error = step_type == "ERROR_MESSAGE";
        let settled_tool_error = status == "ERROR"
            && mtime_ms.is_some_and(|mtime| now_ms.saturating_sub(mtime) >= STEP_ERROR_SETTLE_MS);
        if !model_error && !settled_tool_error {
            return None;
        }
        let message = if !error.is_empty() {
            error
        } else if model_error {
            "model error".to_owned()
        } else {
            "step failed".to_owned()
        };
        let kind = if looks_like_quota_error(&message) {
            AntigravitySwarmFailureKind::Quota
        } else {
            AntigravitySwarmFailureKind::Error
        };
        Some(AntigravitySwarmFailure { kind, message })
    });

    TranscriptSummary {
        steps,
        activity,
        activity_kind,
        idle,
        failure,
        mtime_ms,
        file_stamp: String::new(),
    }
}

/// Mirrors `turnEnded` from `swarm_board/main.mjs`:
/// - a final `PLANNER_RESPONSE` with zero tool calls and zero trailing results -> ended
/// - a `PLANNER_RESPONSE` with `swarm_wait` at call index `w`, followed by at
///   least `calls.len()` `GENERIC` results where `results[w].status == "DONE"` -> ended
/// - anything else (including torn trailing JSON) -> still mid-turn (`false`).
fn transcript_turn_ended(lines: &[&str]) -> bool {
    let mut results_rev: Vec<String> = Vec::new();
    for line in lines.iter().rev() {
        let Ok(step) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        let Some(obj) = step.as_object() else {
            return false;
        };
        let step_type = obj.get("type").and_then(Value::as_str).unwrap_or_default();
        if step_type == "GENERIC" {
            let status = obj
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            results_rev.push(status);
            continue;
        }
        if step_type != "PLANNER_RESPONSE" {
            return false;
        }
        let calls = obj
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if calls.is_empty() {
            return results_rev.is_empty();
        }
        let Some(wait_index) = calls
            .iter()
            .position(|call| call.get("name").and_then(Value::as_str) == Some("swarm_wait"))
        else {
            return false;
        };
        results_rev.reverse();
        return results_rev.len() >= calls.len()
            && results_rev.get(wait_index).map(String::as_str) == Some("DONE");
    }
    false
}

fn looks_like_quota_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("resource_exhausted")
        || lower.contains("quota")
        || lower.contains("exhausted your capacity")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("rate-limit")
        || contains_word(&lower, "429")
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut offset = 0;
    while let Some(rel) = haystack[offset..].find(needle) {
        let start = offset + rel;
        let end = start + needle.len();
        let left_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|character| !character.is_ascii_alphanumeric());
        let right_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|character| !character.is_ascii_alphanumeric());
        if left_ok && right_ok {
            return true;
        }
        offset = start + 1;
    }
    false
}

fn unquote_arg(value: &Value) -> String {
    match value {
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
                serde_json::from_str::<String>(trimmed)
                    .unwrap_or_else(|_| trimmed[1..trimmed.len() - 1].to_owned())
            } else {
                trimmed.to_owned()
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn one_line(input: &str, max_chars: usize) -> String {
    let cleaned = input.replace('`', "").replace("**", "");
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        let truncated: String = prefix.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", truncated.trim_end())
    } else {
        prefix
    }
}

fn describe_tool_call(call: &Value) -> Option<String> {
    let name = call.get("name").and_then(Value::as_str)?;
    if name.is_empty() {
        return None;
    }
    match name {
        "swarm_wait" => return Some("Waiting for posts".to_owned()),
        "read_board" => return Some("Reading the board".to_owned()),
        "post_message" => return Some("Posting to the board".to_owned()),
        _ => {}
    }
    if let Some(args) = call.get("args").and_then(Value::as_object) {
        for key in ["toolSummary", "toolAction"] {
            if let Some(value) = args.get(key) {
                let summary = unquote_arg(value);
                if !summary.is_empty() {
                    return Some(one_line(&summary, 96));
                }
            }
        }
        for key in ["Path", "AbsolutePath", "TargetFile"] {
            if let Some(value) = args.get(key) {
                let path = unquote_arg(value);
                if !path.is_empty() {
                    let verb = match name {
                        "read_file" => Some("Read"),
                        "write_file" => Some("Write"),
                        "edit_file" => Some("Edit"),
                        _ => None,
                    };
                    if let Some(verb) = verb {
                        let basename = path
                            .rsplit(['/', '\\'])
                            .find(|part| !part.is_empty())
                            .unwrap_or(&path);
                        return Some(one_line(&format!("{verb} {basename}"), 96));
                    }
                }
            }
        }
        if name == "code_search"
            && let Some(query) = args.get("Query").map(unquote_arg)
            && !query.is_empty()
        {
            return Some(one_line(&format!("Search {query}"), 96));
        }
        if name == "swarm"
            && let Some(action) = args.get("action").map(unquote_arg)
            && !action.is_empty()
        {
            let target = args.get("name").map(unquote_arg).unwrap_or_default();
            return Some(one_line(&format!("swarm {action} {target}"), 96));
        }
    }
    Some(one_line(name, 96))
}

fn thinking_heading(thinking: &str) -> Option<String> {
    let mut last = None;
    for line in thinking.lines() {
        let trimmed = line.trim();
        if let Some(inner) = trimmed
            .strip_prefix("**")
            .and_then(|rest| rest.strip_suffix("**"))
            && inner.len() >= 3
            && inner.len() <= 96
            && !inner.contains('*')
        {
            last = Some(one_line(inner, 96));
        }
    }
    last
}

#[derive(Debug, Default, Deserialize)]
struct RawView {
    #[serde(default)]
    as_of_seq: u64,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    headline: String,
    #[serde(default)]
    phase: Option<RawViewPhase>,
    #[serde(default)]
    attention: Vec<RawViewAttention>,
    #[serde(default)]
    workstreams: Vec<RawViewWorkstream>,
    #[serde(default)]
    milestones: Vec<RawViewItem>,
    #[serde(default)]
    risks: Vec<RawViewItem>,
    #[serde(default)]
    feedback_ack: Option<RawFeedbackAck>,
}

#[derive(Debug, Default, Deserialize)]
struct RawViewPhase {
    #[serde(default)]
    label: String,
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    total: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct RawViewAttention {
    #[serde(default)]
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    why: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawViewWorkstream {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    owners: Vec<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawViewItem {
    #[serde(default)]
    id: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawFeedbackAck {
    #[serde(default)]
    text: String,
}

fn read_overview(
    app_data: &Path,
    dir: &Path,
    narrator_conv: Option<&str>,
) -> (String, Option<AntigravitySwarmOverview>) {
    let primary = dir.join("view.json");
    let loaded = read_full_bounded(&primary, MAX_VIEW_BYTES).or_else(|| {
        let conv = narrator_conv?;
        let fallback = app_data
            .join("brain")
            .join(conv)
            .join("swarm_view")
            .join("view.json");
        read_full_bounded(&fallback, MAX_VIEW_BYTES)
    });
    let Some((stamp, text)) = loaded else {
        return ("-".to_owned(), None);
    };
    let Ok(raw) = serde_json::from_str::<RawView>(&text) else {
        return (stamp, None);
    };
    let mut attention: Vec<AntigravitySwarmAttentionItem> = raw
        .attention
        .into_iter()
        .take(MAX_OVERVIEW_ITEMS)
        .filter(|item| !item.title.trim().is_empty())
        .enumerate()
        .map(|(index, item)| AntigravitySwarmAttentionItem {
            id: if item.id.trim().is_empty() {
                format!("att-{index}")
            } else {
                one_line(&item.id, 64)
            },
            kind: sanitize_attention_kind(&item.kind).to_owned(),
            severity: sanitize_severity(&item.severity).to_owned(),
            title: one_line(&item.title, 200),
            why: one_line(&item.why, 400),
            member: None,
        })
        .collect();
    attention.sort_by_key(|item| attention_rank(&item.kind));

    let workstreams: Vec<AntigravitySwarmWorkstream> = raw
        .workstreams
        .into_iter()
        .take(MAX_OVERVIEW_ITEMS)
        .filter(|ws| !ws.title.trim().is_empty())
        .enumerate()
        .map(|(index, ws)| AntigravitySwarmWorkstream {
            id: if ws.id.trim().is_empty() {
                format!("ws-{index}")
            } else {
                one_line(&ws.id, 64)
            },
            title: one_line(&ws.title, 160),
            owners: ws
                .owners
                .into_iter()
                .take(16)
                .map(|owner| one_line(&owner, 48))
                .filter(|owner| !owner.is_empty())
                .collect(),
            state: sanitize_workstream_state(&ws.state).to_owned(),
            note: one_line(&ws.note, 320),
        })
        .collect();

    let milestones = raw
        .milestones
        .into_iter()
        .take(MAX_OVERVIEW_ITEMS)
        .filter(|item| !item.text.trim().is_empty())
        .enumerate()
        .map(|(index, item)| AntigravitySwarmViewItem {
            id: if item.id.trim().is_empty() {
                format!("m-{index}")
            } else {
                one_line(&item.id, 64)
            },
            text: one_line(&item.text, 240),
        })
        .collect();

    let risks = raw
        .risks
        .into_iter()
        .take(MAX_OVERVIEW_ITEMS)
        .filter(|item| !item.text.trim().is_empty())
        .enumerate()
        .map(|(index, item)| AntigravitySwarmViewItem {
            id: if item.id.trim().is_empty() {
                format!("r-{index}")
            } else {
                one_line(&item.id, 64)
            },
            text: one_line(&item.text, 240),
        })
        .collect();

    let phase = raw
        .phase
        .filter(|phase| !phase.label.trim().is_empty())
        .map(|phase| AntigravitySwarmPhase {
            label: one_line(&phase.label, 80),
            index: phase.index,
            total: phase.total,
        });

    let feedback_ack = raw
        .feedback_ack
        .map(|ack| one_line(&ack.text, 240))
        .filter(|text| !text.is_empty());

    let overview = AntigravitySwarmOverview {
        as_of_seq: raw.as_of_seq,
        updated_at: raw
            .updated_at
            .map(|value| one_line(&value, 64))
            .filter(|value| !value.is_empty()),
        headline: one_line(&raw.headline, 240),
        phase,
        attention,
        workstreams,
        milestones,
        risks,
        feedback_ack,
    };

    (stamp, Some(overview))
}

fn sanitize_attention_kind(kind: &str) -> &'static str {
    match kind {
        "decision" => "decision",
        "failure" => "failure",
        "review" => "review",
        "stuck" => "stuck",
        "quota" => "quota",
        "error" => "error",
        _ => "fyi",
    }
}

fn attention_rank(kind: &str) -> u8 {
    match kind {
        "decision" => 0,
        "failure" => 1,
        "review" => 2,
        "stuck" => 3,
        "quota" | "error" => 4,
        _ => 5,
    }
}

fn sanitize_severity(severity: &str) -> &'static str {
    match severity {
        "high" => "high",
        "med" => "med",
        _ => "low",
    }
}

fn sanitize_workstream_state(state: &str) -> &'static str {
    match state {
        "active" => "active",
        "blocked" => "blocked",
        "review" => "review",
        "done" => "done",
        "failed" => "failed",
        _ => "queued",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waku-swarm-test-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, content).expect("write fixture file");
    }

    #[test]
    fn rejects_path_traversal_and_invalid_session_ids() {
        for bad in [
            "",
            "../secret",
            "..\\secret",
            "abc/def",
            "abc\\def",
            "abc.def",
            "abc def",
            "abc%2fdef",
            "abc\0def",
        ] {
            assert!(
                validate_session_id(bad).is_err(),
                "expected invalid session id to be rejected: {bad:?}"
            );
            assert!(read_swarm_snapshot_in_roots(&[], bad, 0).is_err());
        }
        assert!(validate_session_id("3669f3f6-500f-4952-8e81-3ff1cd80366c").is_ok());
        assert!(validate_session_id("session_01-AZ").is_ok());
    }

    #[test]
    fn extracts_mentions_including_copy_prefixes_and_word_boundaries() {
        let names = vec![
            "coordinator".to_owned(),
            "worker-1".to_owned(),
            "worker-2".to_owned(),
            "swarm-ui".to_owned(),
        ];
        let (mentions, all) = extract_mentions(
            "Hey @worker and @SWARM-UI, ignore email@coordinator.com, ping @all!",
            &names,
        );
        assert_eq!(mentions, vec!["worker-1", "worker-2", "swarm-ui"]);
        assert!(all);

        let (mentions, all) = extract_mentions("No partial @swarm-ui-extra match", &names);
        assert!(mentions.is_empty());
        assert!(!all);
    }

    #[test]
    fn derives_member_status_activity_and_failure_from_transcripts() {
        let root = temp_root();
        let coord_id = "coord-1111-2222-3333";
        let worker_active = "work-active-1111";
        let worker_waiting = "work-wait-2222";
        let worker_failed = "work-fail-3333";
        let worker_killed = "work-kill-4444";

        let swarm_path = swarm_dir(&root, coord_id);
        write_file(
            &swarm_path.join("members.json"),
            &serde_json::json!({
                "title": "Test Swarm",
                "members": {
                    "coordinator": {
                        "conversation_id": coord_id,
                        "model": "gemini-pro",
                        "role": "coordinator",
                        "wake_mode": "mentions",
                        "last_read_seq": 2,
                        "killed": false
                    },
                    "alpha": {
                        "conversation_id": worker_active,
                        "model": "gemini-pro",
                        "role": "worker",
                        "wake_mode": "working",
                        "last_read_seq": 1,
                        "killed": false
                    },
                    "beta": {
                        "conversation_id": worker_waiting,
                        "model": "gemini-flash",
                        "role": "worker",
                        "wake_mode": "mentions",
                        "last_read_seq": 2,
                        "killed": false
                    },
                    "gamma": {
                        "conversation_id": worker_failed,
                        "model": "gemini-pro",
                        "role": "worker",
                        "wake_mode": "working",
                        "last_read_seq": 0,
                        "killed": false
                    },
                    "old-worker": {
                        "conversation_id": worker_killed,
                        "model": "gemini-pro",
                        "role": "worker",
                        "wake_mode": "working",
                        "last_read_seq": 0,
                        "killed": true
                    }
                }
            })
            .to_string(),
        );
        write_file(
            &swarm_path.join("board.jsonl"),
            concat!(
                r#"{"seq":1,"agent":"coordinator","text":"@alpha @beta kick off","time":"2026-11-23 15:00:00","kind":"brief"}"#,
                "\n",
                r#"{"seq":2,"agent":"alpha","text":"@coordinator working on tests","time":"2026-11-23 15:01:00"}"#,
                "\n",
                r#"{"seq":3,"agent":"torn"#
            ),
        );
        write_file(
            &swarm_path.join("view.json"),
            &serde_json::json!({
                "as_of_seq": 2,
                "headline": "Swarm running smoothly",
                "phase": { "label": "Build", "index": 1, "total": 2 },
                "attention": [
                    { "id": "a1", "kind": "fyi", "severity": "low", "title": "Info item", "why": "context" },
                    { "id": "a2", "kind": "decision", "severity": "high", "title": "Pick API", "why": "blocking" }
                ],
                "workstreams": [
                    { "id": "w1", "title": "Reader", "owners": ["alpha"], "state": "active", "note": "In progress" }
                ],
                "milestones": [{ "id": "m1", "text": "Schema ready" }],
                "risks": [{ "id": "r1", "text": "Quota pressure" }],
                "feedback_ack": { "through_seq": 2, "text": "Noted #2" }
            })
            .to_string(),
        );

        // Coordinator: final reply with no tool calls => waiting.
        write_file(
            &transcript_path(&root, coord_id),
            r#"{"step_index":0,"type":"PLANNER_RESPONSE","content":"Dispatched workers.","tool_calls":[]}"#,
        );
        // Active worker: mid-turn tool call with toolSummary.
        write_file(
            &transcript_path(&root, worker_active),
            concat!(
                r#"{"step_index":0,"type":"USER_INPUT"}"#,
                "\n",
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","thinking":"**Checking parser**\nDetails","tool_calls":[{"name":"run_command","args":{"toolSummary":"\"Unit test execution\""}}]}"#,
                "\n",
                r#"{"step_index":2,"type":"GENERIC","status":"RUNNING"}"#
            ),
        );
        // Waiting worker: parked on swarm_wait with DONE result.
        write_file(
            &transcript_path(&root, worker_waiting),
            concat!(
                r#"{"step_index":0,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"post_message","args":{}},{"name":"swarm_wait","args":{}}]}"#,
                "\n",
                r#"{"step_index":1,"type":"GENERIC","status":"DONE"}"#,
                "\n",
                r#"{"step_index":2,"type":"GENERIC","status":"DONE"}"#
            ),
        );
        // Failed worker: ERROR_MESSAGE with quota error.
        write_file(
            &transcript_path(&root, worker_failed),
            r#"{"step_index":4,"type":"ERROR_MESSAGE","status":"ERROR","error":"Error 429: RESOURCE_EXHAUSTED"}"#,
        );

        let now_ms = now_unix_ms();
        let snapshot = read_swarm_snapshot_in_roots(&[root.clone()], coord_id, now_ms)
            .expect("read coordinator snapshot")
            .expect("snapshot exists");

        assert_eq!(snapshot.swarm_id, coord_id);
        assert_eq!(snapshot.current_member.as_deref(), Some("coordinator"));
        assert_eq!(snapshot.title, "Test Swarm");
        assert_eq!(snapshot.head_seq, 2);
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.messages[0].mentions, vec!["alpha", "beta"]);
        assert_eq!(snapshot.messages[0].kind.as_deref(), Some("brief"));

        let by_name: HashMap<&str, &AntigravitySwarmMember> = snapshot
            .members
            .iter()
            .map(|member| (member.name.as_str(), member))
            .collect();
        assert_eq!(by_name["coordinator"].status, AntigravitySwarmMemberStatus::Waiting);
        assert_eq!(by_name["alpha"].status, AntigravitySwarmMemberStatus::Working);
        assert_eq!(
            by_name["alpha"].activity.as_deref(),
            Some("Unit test execution")
        );
        assert_eq!(by_name["alpha"].unread, 1);
        assert_eq!(by_name["beta"].status, AntigravitySwarmMemberStatus::Waiting);
        assert_eq!(by_name["beta"].activity, None);
        assert_eq!(by_name["gamma"].status, AntigravitySwarmMemberStatus::Failed);
        assert_eq!(
            by_name["gamma"].failure.as_ref().map(|f| f.kind),
            Some(AntigravitySwarmFailureKind::Quota)
        );
        assert_eq!(by_name["old-worker"].status, AntigravitySwarmMemberStatus::Killed);

        // Overview attention is ranked decision > fyi.
        let overview = snapshot.overview.as_ref().expect("overview parsed");
        assert_eq!(overview.headline, "Swarm running smoothly");
        assert_eq!(overview.attention[0].kind, "decision");
        assert_eq!(overview.attention[1].kind, "fyi");
        assert_eq!(overview.feedback_ack.as_deref(), Some("Noted #2"));

        // System attention includes gamma's quota failure.
        assert_eq!(snapshot.system_attention.len(), 1);
        assert_eq!(snapshot.system_attention[0].kind, "quota");
        assert_eq!(snapshot.system_attention[0].member.as_deref(), Some("gamma"));

        // Querying by worker conversation ID resolves the same swarm and identifies `alpha`.
        let worker_view = read_swarm_snapshot_in_roots(&[root.clone()], worker_active, now_ms)
            .expect("read worker snapshot")
            .expect("worker snapshot exists");
        assert_eq!(worker_view.swarm_id, coord_id);
        assert_eq!(worker_view.current_member.as_deref(), Some("alpha"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn caps_board_messages_to_bounded_tail() {
        let root = temp_root();
        let coord_id = "coord-cap-test";
        let swarm_path = swarm_dir(&root, coord_id);
        write_file(
            &swarm_path.join("members.json"),
            r#"{"title":"Big","members":{"coordinator":{"conversation_id":"coord-cap-test","role":"coordinator"}}}"#,
        );
        let mut board = String::new();
        for seq in 1..=(MAX_BOARD_MESSAGES as u64 + 25) {
            board.push_str(&format!(
                r#"{{"seq":{seq},"agent":"coordinator","text":"msg {seq}","time":"2026-11-23 15:00:00"}}"#
            ));
            board.push('\n');
        }
        write_file(&swarm_path.join("board.jsonl"), &board);

        let snapshot = read_swarm_snapshot_in_roots(&[root.clone()], coord_id, now_unix_ms())
            .expect("read capped snapshot")
            .expect("snapshot exists");
        assert!(snapshot.messages_truncated);
        assert_eq!(snapshot.messages.len(), MAX_BOARD_MESSAGES);
        assert_eq!(snapshot.messages[0].seq, 26);
        assert_eq!(
            snapshot.head_seq,
            MAX_BOARD_MESSAGES as u64 + 25
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn honors_model_fallbacks_killed_states_and_narrator_fallback_view() {
        let root = temp_root();
        let coord_id = "coord-fallback-1";
        let narrator_id = "narrator-conv-2";
        let swarm_path = swarm_dir(&root, coord_id);
        write_file(
            &swarm_path.join("members.json"),
            &serde_json::json!({
                "title": "Fallback Test",
                "members": {
                    "coordinator": {
                        "conversation_id": coord_id,
                        "applied_model": "gemini-applied",
                        "role": "coordinator",
                        "wake_mode": "working",
                        "killed": false
                    },
                    "narrator-2": {
                        "conversation_id": narrator_id,
                        "desired_model": "gemini-desired",
                        "role": "worker",
                        "wake_mode": "mentions",
                        "killed": false
                    },
                    "retired": {
                        "conversation_id": "retired-3",
                        "model": "gemini-pro",
                        "role": "worker",
                        "desired_state": "KILLED",
                        "killed": false
                    }
                }
            })
            .to_string(),
        );
        write_file(
            &root
                .join("brain")
                .join(narrator_id)
                .join("swarm_view")
                .join("view.json"),
            &serde_json::json!({
                "as_of_seq": 4,
                "headline": "From narrator dir"
            })
            .to_string(),
        );

        let snapshot = read_swarm_snapshot_in_roots(&[root.clone()], coord_id, now_unix_ms())
            .expect("read snapshot")
            .expect("snapshot exists");
        let by_name: HashMap<&str, &AntigravitySwarmMember> = snapshot
            .members
            .iter()
            .map(|member| (member.name.as_str(), member))
            .collect();
        assert_eq!(by_name["coordinator"].model, "gemini-applied");
        assert_eq!(by_name["narrator-2"].model, "gemini-desired");
        assert_eq!(by_name["narrator-2"].role, "narrator");
        assert!(by_name["narrator-2"].is_role_worker);
        assert!(by_name["retired"].killed);
        assert_eq!(
            by_name["retired"].status,
            AntigravitySwarmMemberStatus::Killed
        );
        assert_eq!(
            snapshot.overview.as_ref().map(|o| o.headline.as_str()),
            Some("From narrator dir")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bounded_tail_reports_skipped_prefix_and_truncates_large_boards() {
        let root = temp_root();
        let file_path = root.join("large-board.jsonl");
        let payload = concat!(
            r#"{"seq":1,"agent":"coordinator","text":"first line padding 01234567890123456789"}"#,
            "\n",
            r#"{"seq":2,"agent":"alpha","text":"second line"}"#,
            "\n"
        );
        write_file(&file_path, payload);

        let (_, tail, skipped) = read_tail_bounded(&file_path, 64);
        assert!(skipped);
        let parsed = parse_board(&tail.unwrap_or_default(), &[], skipped);
        assert!(parsed.truncated);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].seq, 2);
        assert_eq!(parsed.total_messages, 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_swarm_snapshot_in_isolates_to_specified_config_dir() {
        let base = temp_root();
        let account_a = base.join("account-a");
        let account_b = base.join("account-b");
        let sidecar_a = account_a.join("sidecar_data");
        fs::create_dir_all(&sidecar_a).unwrap();

        let coord_a = "coord-account-a";
        let coord_b = "coord-account-b";
        write_file(
            &swarm_dir(&account_a, coord_a).join("members.json"),
            r#"{"title":"Swarm A","members":{"coordinator":{"conversation_id":"coord-account-a","role":"coordinator"}}}"#,
        );
        write_file(
            &swarm_dir(&account_b, coord_b).join("members.json"),
            r#"{"title":"Swarm B","members":{"coordinator":{"conversation_id":"coord-account-b","role":"coordinator"}}}"#,
        );

        let snap_a = read_swarm_snapshot_in(coord_a, Some(&account_a))
            .expect("read account_a")
            .expect("swarm A exists");
        assert_eq!(snap_a.title, "Swarm A");

        let snap_sidecar = read_swarm_snapshot_in(coord_a, Some(&sidecar_a))
            .expect("read sidecar_data")
            .expect("swarm A resolves from sidecar_data parent");
        assert_eq!(snap_sidecar.title, "Swarm A");

        assert!(
            read_swarm_snapshot_in(coord_a, Some(&account_b))
                .expect("read account_b for coord_a")
                .is_none(),
            "config_dir isolation must not leak across account roots"
        );
        assert!(
            read_swarm_snapshot_in(coord_b, Some(&account_a))
                .expect("read account_a for coord_b")
                .is_none(),
            "config_dir isolation must not leak to account_b"
        );

        let empty_account = base.join("empty-account");
        fs::create_dir_all(&empty_account).unwrap();
        assert!(
            read_swarm_snapshot_in(coord_a, Some(&empty_account))
                .expect("read empty account")
                .is_none()
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn locate_swarm_prioritizes_recent_dirs_and_direct_session_lookup_when_over_cap() {
        let root = temp_root();
        let brain = root.join("brain");
        fs::create_dir_all(&brain).unwrap();

        let coord_id = "zzz-coord-old-dir";
        let worker_id = format!("worker-in-recent-{}", Uuid::new_v4());
        // Pre-create the coordinator's `.system_generated/swarm` dir BEFORE the
        // other swarms so `brain/zzz-coord-old-dir` has an older dir mtime.
        let coord_swarm_dir = swarm_dir(&root, coord_id);
        fs::create_dir_all(&coord_swarm_dir).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Create non-swarm brain dirs and older swarm dirs that sort earlier.
        for index in 0..5 {
            fs::create_dir_all(brain.join(format!("000-plain-{index:03}"))).unwrap();
            write_file(
                &swarm_dir(&root, &format!("000-old-swarm-{index:03}")).join("members.json"),
                r#"{"title":"Old Swarm","members":{"coordinator":{"conversation_id":"old-coord","role":"coordinator"}}}"#,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Write `members.json` later inside the pre-existing `zzz-coord-old-dir`.
        write_file(
            &coord_swarm_dir.join("members.json"),
            &format!(
                r#"{{"title":"Recent Swarm","members":{{"coordinator":{{"conversation_id":"{coord_id}","role":"coordinator"}},"alpha":{{"conversation_id":"{worker_id}","role":"worker"}}}}}}"#
            ),
        );

        // Fast path locates coordinator directly even if worker scan cap is 0.
        let coord_located = locate_swarm_with_cap(&[root.clone()], coord_id, 0)
            .expect("coordinator fast path bypasses scan cap");
        assert_eq!(coord_located.swarm_id, coord_id);

        // Worker lookup with cap=1 skips the 5 older swarms and finds the newest `members.json`.
        let worker_located = locate_swarm_with_cap(&[root.clone()], &worker_id, 1)
            .expect("worker scan prioritizes recently modified members.json");
        assert_eq!(worker_located.swarm_id, coord_id);
        assert_eq!(worker_located.current_member.as_deref(), Some("alpha"));

        // Cached worker hit must NOT leak across account roots.
        let other_root = temp_root();
        assert!(locate_swarm_with_cap(&[other_root.clone()], &worker_id, 1).is_none());

        // Subsequent lookup on the owning root still succeeds via cache even with cap=0.
        let cached_hit = locate_swarm_with_cap(&[root.clone()], &worker_id, 0)
            .expect("cached worker lookup succeeds within matching root");
        assert_eq!(cached_hit.swarm_id, coord_id);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&other_root);
    }
}
