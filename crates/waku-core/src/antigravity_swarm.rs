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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::bail;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use waku_protocol::antigravity_swarm::{
    AntigravitySwarmAttentionItem, AntigravitySwarmFailure, AntigravitySwarmFailureKind,
    AntigravitySwarmMember, AntigravitySwarmMemberStatus, AntigravitySwarmMessage,
    AntigravitySwarmOverview, AntigravitySwarmPhase, AntigravitySwarmScreenshot,
    AntigravitySwarmSnapshot, AntigravitySwarmViewItem, AntigravitySwarmWorkstream,
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
const MAX_SCREENSHOTS: usize = 64;
const NEGATIVE_SWARM_CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct NegativeSwarmEntry {
    checked_at: Instant,
    roots: Vec<NegativeRootStamp>,
}

#[derive(Clone, Eq, PartialEq)]
struct NegativeRootStamp {
    brain_dir: PathBuf,
    brain_mtime: Option<SystemTime>,
    swarm_stamps: Vec<(PathBuf, Option<(Option<SystemTime>, u64)>)>,
}

fn negative_swarm_cache() -> &'static Mutex<HashMap<(Vec<PathBuf>, String), NegativeSwarmEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<(Vec<PathBuf>, String), NegativeSwarmEntry>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn file_mtime_and_len(path: &Path) -> Option<(Option<SystemTime>, u64)> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some((metadata.modified().ok(), metadata.len()))
}

fn dir_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn is_negative_swarm_cached(roots: &[PathBuf], session_id: &str) -> bool {
    let key = (roots.to_vec(), session_id.to_owned());
    let entry = negative_swarm_cache().lock().get(&key).cloned();
    let Some(entry) = entry else {
        return false;
    };
    if entry.checked_at.elapsed() >= NEGATIVE_SWARM_CACHE_TTL || entry.roots.len() != roots.len() {
        return false;
    }
    let still_fresh = entry.roots.iter().all(|root| {
        dir_mtime(&root.brain_dir) == root.brain_mtime
            && root
                .swarm_stamps
                .iter()
                .all(|(members_path, expected)| file_mtime_and_len(members_path) == *expected)
    });
    if !still_fresh {
        negative_swarm_cache().lock().remove(&key);
    }
    still_fresh
}

fn record_negative_swarm_cache(
    roots: &[PathBuf],
    session_id: &str,
    root_stamps: Vec<NegativeRootStamp>,
) {
    let key = (roots.to_vec(), session_id.to_owned());
    let mut cache = negative_swarm_cache().lock();
    if cache.len() >= MAX_WORKER_SWARM_CACHE && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(
        key,
        NegativeSwarmEntry {
            checked_at: Instant::now(),
            roots: root_stamps,
        },
    );
}

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
    channels: Option<Value>,
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
    #[serde(default)]
    channels: Option<Vec<String>>,
}

impl RawMember {
    fn is_killed(&self) -> bool {
        self.killed
            || self.desired_state.eq_ignore_ascii_case("KILLED")
            || self.phase.eq_ignore_ascii_case("KILLED")
    }

    fn is_stopped(&self) -> bool {
        !self.is_killed()
            && (self.desired_state.eq_ignore_ascii_case("STOPPED")
                || self.phase.eq_ignore_ascii_case("STOPPED")
                || self.wake_mode.eq_ignore_ascii_case("stopped"))
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

    let (plan_stamp, plan) = read_plan(&dir);

    let mut members = Vec::with_capacity(located.roster.members.len());
    let mut transcript_stamps = Vec::with_capacity(located.roster.members.len());
    let mut system_attention = Vec::new();
    let mut channel_set: BTreeSet<String> = parsed_board.channels.iter().cloned().collect();
    collect_roster_channels(&located.roster, &mut channel_set);

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
        let has_posted_done = parsed_board
            .done_seq
            .get(name)
            .is_some_and(|done| {
                parsed_board
                    .brief_seq
                    .get(name)
                    .is_none_or(|brief| *done >= *brief)
            });
        let task = plan
            .as_ref()
            .and_then(|raw_plan| raw_plan.tasks.get(name))
            .map(|text| one_line(text, 240))
            .filter(|text| !text.is_empty());
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
            if status == AntigravitySwarmMemberStatus::Failed
                && let Some(failure) = &summary.failure
            {
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
            has_posted_done,
            task,
        });
    }

    members.sort_by(|left, right| {
        member_sort_rank(left)
            .cmp(&member_sort_rank(right))
            .then_with(|| left.name.cmp(&right.name))
    });

    let narrator_name = pick_narrator(&located.roster);
    let narrator_conv = narrator_name
        .as_deref()
        .and_then(|name| located.roster.members.get(name))
        .map(|member| member.conversation_id.as_str())
        .filter(|conv| is_valid_session_id(conv));
    let (view_stamp, overview) = read_overview(
        &located.app_data,
        &dir,
        narrator_conv,
        plan.as_ref(),
        &plan_stamp,
        &located.roster.title,
        &parsed_board,
        &members,
    );

    let mut channels = Vec::with_capacity(channel_set.len() + 1);
    channels.push("main".to_owned());
    for channel in channel_set {
        if channel != "main" {
            channels.push(channel);
        }
    }

    let board_fingerprint = format!("{}|{}", located.swarm_id, board_stamp);
    let roster_fingerprint = format!(
        "{}|{}|{}|{}",
        located.swarm_id,
        located.members_stamp,
        board_stamp,
        transcript_stamps.join(",")
    );
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
        channels,
        board_fingerprint: Some(board_fingerprint),
        roster_fingerprint: Some(roster_fingerprint),
    }))
}

fn member_sort_rank(member: &AntigravitySwarmMember) -> (u8, u8) {
    let status_rank = match member.status {
        AntigravitySwarmMemberStatus::Working => 0,
        AntigravitySwarmMemberStatus::Failed => 1,
        AntigravitySwarmMemberStatus::Waiting => 2,
        AntigravitySwarmMemberStatus::Stopped => 3,
        AntigravitySwarmMemberStatus::Killed => 4,
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

    if max_scan_dirs > 0 && is_negative_swarm_cached(roots, session_id) {
        return None;
    }

    // Worker path: scan the most recently updated `members.json` files first so
    // non-swarm conversations never crowd out active swarms and updating
    // `members.json` under `.system_generated/swarm/` is reflected even when
    // `brain/<id>`'s top-level directory mtime is older.
    let mut negative_root_stamps = Vec::with_capacity(roots.len());
    let mut was_capped = false;
    for root in roots {
        let brain_dir = root.join("brain");
        let brain_mtime = dir_mtime(&brain_dir);
        let Ok(entries) = fs::read_dir(&brain_dir) else {
            negative_root_stamps.push(NegativeRootStamp {
                brain_dir,
                brain_mtime,
                swarm_stamps: Vec::new(),
            });
            continue;
        };
        let mut candidates: Vec<(Option<SystemTime>, u64, String, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let file_name = entry.file_name();
                let candidate_id = file_name.to_str()?;
                if candidate_id == session_id || !is_valid_session_id(candidate_id) {
                    return None;
                }
                let members_path = swarm_dir(root, candidate_id).join("members.json");
                let (modified, len) = file_mtime_and_len(&members_path)?;
                Some((modified, len, candidate_id.to_owned(), members_path))
            })
            .collect();
        candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));
        if candidates.len() > max_scan_dirs {
            was_capped = true;
        }
        let swarm_stamps: Vec<(PathBuf, Option<(Option<SystemTime>, u64)>)> = candidates
            .iter()
            .take(max_scan_dirs)
            .map(|(modified, len, _, path)| (path.clone(), Some((*modified, *len))))
            .collect();
        negative_root_stamps.push(NegativeRootStamp {
            brain_dir,
            brain_mtime,
            swarm_stamps,
        });

        for (_, _, candidate_id, members_path) in candidates.into_iter().take(max_scan_dirs) {
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
    if max_scan_dirs > 0 && !was_capped {
        record_negative_swarm_cache(roots, session_id, negative_root_stamps);
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
    channels: BTreeSet<String>,
    done_seq: HashMap<String, u64>,
    brief_seq: HashMap<String, u64>,
    screenshots: Vec<AntigravitySwarmScreenshot>,
}

fn parse_board(text: &str, roster_names: &[String], skipped_prefix: bool) -> ParsedBoard {
    let mut parsed = Vec::new();
    let mut posts_by_agent: HashMap<String, u64> = HashMap::new();
    let mut channels: BTreeSet<String> = BTreeSet::new();
    let mut done_seq: HashMap<String, u64> = HashMap::new();
    let mut brief_seq: HashMap<String, u64> = HashMap::new();
    let mut screenshots: Vec<AntigravitySwarmScreenshot> = Vec::new();
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
        if is_control {
            continue;
        }

        if !raw.agent.is_empty() {
            *posts_by_agent.entry(raw.agent.clone()).or_insert(0) += 1;
        }
        let (mentions, mentions_all) = extract_mentions(&raw.text, roster_names);
        let channel_mentions = extract_channel_mentions(&raw.text);
        for mentioned_channel in &channel_mentions {
            channels.insert(mentioned_channel.clone());
        }

        let normalized_channel = raw
            .channel
            .as_deref()
            .map(str::trim)
            .filter(|channel| !channel.is_empty())
            .map(str::to_owned);
        if let Some(channel) = &normalized_channel {
            channels.insert(channel.clone());
        } else {
            channels.insert("main".to_owned());
        }
        let channel = normalized_channel.filter(|channel| channel != "main");
        let kind = raw.kind.filter(|kind| !kind.is_empty());

        if kind.as_deref() == Some("brief") {
            if mentions_all {
                for name in roster_names {
                    brief_seq
                        .entry(name.clone())
                        .and_modify(|prev| *prev = (*prev).max(seq))
                        .or_insert(seq);
                }
            } else {
                for mentioned in &mentions {
                    brief_seq
                        .entry(mentioned.clone())
                        .and_modify(|prev| *prev = (*prev).max(seq))
                        .or_insert(seq);
                }
            }
        }
        if !raw.agent.is_empty() && is_done_post(&raw.text) {
            done_seq
                .entry(raw.agent.clone())
                .and_modify(|prev| *prev = (*prev).max(seq))
                .or_insert(seq);
        }

        let message_screenshots = extract_screenshots(&raw.text, seq, &raw.agent);
        if screenshots.len() < MAX_SCREENSHOTS {
            let remaining = MAX_SCREENSHOTS - screenshots.len();
            screenshots.extend(message_screenshots.iter().take(remaining).cloned());
        }

        parsed.push(AntigravitySwarmMessage {
            seq,
            agent: raw.agent,
            text: raw.text,
            time: raw.time,
            mentions,
            mentions_all,
            kind,
            channel,
            channel_mentions,
            screenshots: message_screenshots,
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
        channels,
        done_seq,
        brief_seq,
        screenshots,
    }
}

fn collect_roster_channels(roster: &RawRoster, channels: &mut BTreeSet<String>) {
    if let Some(raw_channels) = &roster.channels {
        match raw_channels {
            Value::Array(items) => {
                for item in items {
                    if let Some(name) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        channels.insert(name.to_owned());
                    }
                }
            }
            Value::Object(map) => {
                for key in map.keys() {
                    let trimmed = key.trim();
                    if !trimmed.is_empty() {
                        channels.insert(trimmed.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    for member in roster.members.values() {
        if let Some(member_channels) = &member.channels {
            for channel in member_channels {
                let trimmed = channel.trim();
                if !trimmed.is_empty() {
                    channels.insert(trimmed.to_owned());
                }
            }
        }
    }
}

fn extract_channel_mentions(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut channels = Vec::new();
    let mut offset = 0;
    while let Some(rel) = lower[offset..].find("@#") {
        let at_index = offset + rel;
        let preceded_by_name = lower[..at_index]
            .chars()
            .next_back()
            .is_some_and(is_name_char);
        if !preceded_by_name {
            let after_hash = &lower[at_index + 2..];
            let len: usize = after_hash
                .chars()
                .take_while(|character| is_name_char(*character))
                .map(char::len_utf8)
                .sum();
            if len > 0 {
                let channel = &after_hash[..len];
                if !channels.iter().any(|existing| existing == channel) {
                    channels.push(channel.to_owned());
                }
            }
        }
        offset = at_index + 2;
    }
    channels
}

fn extract_screenshots(text: &str, seq: u64, agent: &str) -> Vec<AntigravitySwarmScreenshot> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(prefix) = trimmed.get(..11) else {
            continue;
        };
        if !prefix.eq_ignore_ascii_case("screenshot:") {
            continue;
        }
        let rest = trimmed[11..].trim();
        if rest.is_empty() {
            continue;
        }
        let (raw_path, raw_caption) = rest.split_once('|').unwrap_or((rest, ""));
        let path = raw_path.trim().trim_matches('`').trim();
        if path.is_empty() {
            continue;
        }
        out.push(AntigravitySwarmScreenshot {
            path: path.to_owned(),
            caption: one_line(raw_caption, 200),
            seq: Some(seq),
            agent: (!agent.is_empty()).then(|| agent.to_owned()),
        });
    }
    out
}

fn is_done_post(text: &str) -> bool {
    let mut rest = text.trim_start();
    while let Some(after_at) = rest.strip_prefix('@') {
        let token_len = after_at
            .find(char::is_whitespace)
            .unwrap_or(after_at.len());
        if token_len == 0 {
            break;
        }
        rest = after_at[token_len..].trim_start();
    }
    rest.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("[done]"))
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
    if raw.desired_state.eq_ignore_ascii_case("STOPPED") {
        return AntigravitySwarmMemberStatus::Stopped;
    }
    if summary.failure.is_some() {
        return AntigravitySwarmMemberStatus::Failed;
    }
    if raw.is_stopped() {
        return AntigravitySwarmMemberStatus::Stopped;
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

    let mut summary = summarize_transcript_text(&text, mtime_ms, now_ms, skipped_prefix);
    summary.file_stamp = file_stamp;
    summary
}

fn summarize_transcript_text(
    text: &str,
    mtime_ms: Option<u64>,
    now_ms: u64,
    skipped_prefix: bool,
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

    let idle = transcript_turn_ended(&lines, skipped_prefix);
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
/// - a final `PLANNER_RESPONSE` with zero tool calls and zero trailing results -> `Some(true)`
/// - a `PLANNER_RESPONSE` with `swarm_wait` at call index `w`, followed by at
///   least `calls.len()` `GENERIC` results where `results[w].status == "DONE"` -> `Some(true)`
/// - when `skipped_prefix` truncated a huge tail of `GENERIC` steps before
///   reaching `PLANNER_RESPONSE`, return `None` so `wake_mode` decides
/// - anything else (including torn trailing JSON) -> still mid-turn (`Some(false)`).
fn transcript_turn_ended(lines: &[&str], skipped_prefix: bool) -> Option<bool> {
    let mut results_rev: Vec<String> = Vec::new();
    for line in lines.iter().rev() {
        let Ok(step) = serde_json::from_str::<Value>(line) else {
            return Some(false);
        };
        let Some(obj) = step.as_object() else {
            return Some(false);
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
            return Some(false);
        }
        let calls = obj
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if calls.is_empty() {
            return Some(results_rev.is_empty());
        }
        let Some(wait_index) = calls
            .iter()
            .position(|call| call.get("name").and_then(Value::as_str) == Some("swarm_wait"))
        else {
            return Some(false);
        };
        results_rev.reverse();
        return Some(
            results_rev.len() >= calls.len()
                && results_rev.get(wait_index).map(String::as_str) == Some("DONE"),
        );
    }
    if skipped_prefix { None } else { Some(false) }
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
struct RawPlan {
    #[serde(default)]
    goal: Option<RawPlanGoal>,
    #[serde(default)]
    tasks: BTreeMap<String, String>,
    #[serde(default)]
    threads: Vec<RawPlanThread>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPlanGoal {
    #[serde(default)]
    title: String,
    #[serde(default)]
    line: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawPlanThread {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    tasks: Vec<String>,
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
    goal: Option<RawPlanGoal>,
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
    #[serde(default)]
    reel: Option<RawViewReel>,
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
struct RawViewRef {
    #[serde(default)]
    member: Option<String>,
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
    #[serde(default)]
    member: Option<String>,
    #[serde(default)]
    refs: Vec<RawViewRef>,
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

#[derive(Debug, Default, Deserialize)]
struct RawViewReel {
    #[serde(default)]
    frames: Vec<RawViewReelFrame>,
}

#[derive(Debug, Default, Deserialize)]
struct RawViewReelFrame {
    #[serde(default)]
    path: String,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    caption: String,
}

fn read_plan(dir: &Path) -> (String, Option<RawPlan>) {
    let path = dir.join("plan.json");
    let Some((stamp, text)) = read_full_bounded(&path, MAX_VIEW_BYTES) else {
        return ("-".to_owned(), None);
    };
    let parsed = serde_json::from_str::<RawPlan>(&text).ok();
    (stamp, parsed)
}

#[allow(clippy::too_many_arguments)]
fn read_overview(
    app_data: &Path,
    dir: &Path,
    narrator_conv: Option<&str>,
    plan: Option<&RawPlan>,
    plan_stamp: &str,
    roster_title: &str,
    parsed_board: &ParsedBoard,
    members: &[AntigravitySwarmMember],
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
    let fallback_view_stamp = loaded.as_ref().map(|(stamp, _)| stamp.clone());
    if let Some((view_stamp, text)) = loaded
        && let Ok(raw) = serde_json::from_str::<RawView>(&text)
    {
        let mut attention: Vec<AntigravitySwarmAttentionItem> = raw
            .attention
            .into_iter()
            .take(MAX_OVERVIEW_ITEMS)
            .filter(|item| !item.title.trim().is_empty())
            .enumerate()
            .map(|(index, item)| {
                let member = item
                    .member
                    .as_deref()
                    .or_else(|| item.refs.iter().find_map(|r| r.member.as_deref()))
                    .map(|name| one_line(name, 64))
                    .filter(|name| !name.is_empty());
                AntigravitySwarmAttentionItem {
                    id: if item.id.trim().is_empty() {
                        format!("att-{index}")
                    } else {
                        one_line(&item.id, 64)
                    },
                    kind: sanitize_attention_kind(&item.kind).to_owned(),
                    severity: sanitize_severity(&item.severity).to_owned(),
                    title: one_line(&item.title, 200),
                    why: one_line(&item.why, 400),
                    member,
                }
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

        let headline = {
            let text = one_line(&raw.headline, 240);
            if !text.is_empty() {
                text
            } else {
                raw.goal
                    .as_ref()
                    .map(|goal| one_line(&goal.title, 240))
                    .unwrap_or_default()
            }
        };
        let goal_line = raw
            .goal
            .as_ref()
            .map(|goal| one_line(&goal.line, 240))
            .filter(|line| !line.is_empty())
            .or_else(|| {
                plan.and_then(|p| p.goal.as_ref())
                    .map(|goal| one_line(&goal.line, 240))
                    .filter(|line| !line.is_empty())
            });
        let reel_screenshots: Vec<AntigravitySwarmScreenshot> = raw
            .reel
            .map(|reel| {
                reel.frames
                    .into_iter()
                    .take(MAX_SCREENSHOTS)
                    .filter_map(|frame| {
                        let path = frame.path.trim().to_owned();
                        if path.is_empty() {
                            return None;
                        }
                        let agent = frame.seq.and_then(|seq| {
                            parsed_board
                                .messages
                                .iter()
                                .find(|msg| msg.seq == seq && !msg.agent.is_empty())
                                .map(|msg| msg.agent.clone())
                        });
                        Some(AntigravitySwarmScreenshot {
                            path,
                            caption: one_line(&frame.caption, 200),
                            seq: frame.seq,
                            agent,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let screenshots = if reel_screenshots.is_empty() {
            parsed_board.screenshots.clone()
        } else {
            reel_screenshots
        };

        let overview = AntigravitySwarmOverview {
            as_of_seq: raw.as_of_seq,
            updated_at: raw
                .updated_at
                .map(|value| one_line(&value, 64))
                .filter(|value| !value.is_empty()),
            headline,
            phase,
            attention,
            workstreams,
            milestones,
            risks,
            feedback_ack,
            source: Some("view".to_owned()),
            goal_line,
            screenshots,
        };
        let stamp = if plan_stamp == "-" {
            view_stamp
        } else {
            format!("{view_stamp}+{plan_stamp}")
        };
        return (stamp, Some(overview));
    }

    if let Some(raw_plan) = plan {
        let headline = raw_plan
            .goal
            .as_ref()
            .map(|goal| one_line(&goal.title, 240))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| one_line(roster_title, 240));
        let goal_line = raw_plan
            .goal
            .as_ref()
            .map(|goal| one_line(&goal.line, 240))
            .filter(|line| !line.is_empty());
        let workstreams: Vec<AntigravitySwarmWorkstream> = raw_plan
            .threads
            .iter()
            .take(MAX_OVERVIEW_ITEMS)
            .filter(|thread| !thread.title.trim().is_empty())
            .enumerate()
            .map(|(index, thread)| {
                let owners: Vec<String> = thread
                    .tasks
                    .iter()
                    .take(16)
                    .map(|owner| one_line(owner, 48))
                    .filter(|owner| !owner.is_empty())
                    .collect();
                let state = derive_thread_state(&owners, members);
                AntigravitySwarmWorkstream {
                    id: if thread.id.trim().is_empty() {
                        format!("t-{index}")
                    } else {
                        one_line(&thread.id, 64)
                    },
                    title: one_line(&thread.title, 160),
                    owners,
                    state,
                    note: one_line(&thread.goal, 320),
                }
            })
            .collect();
        let overview = AntigravitySwarmOverview {
            as_of_seq: parsed_board.head_seq,
            updated_at: None,
            headline,
            phase: None,
            attention: Vec::new(),
            workstreams,
            milestones: Vec::new(),
            risks: Vec::new(),
            feedback_ack: None,
            source: Some("plan".to_owned()),
            goal_line,
            screenshots: parsed_board.screenshots.clone(),
        };
        return (plan_stamp.to_owned(), Some(overview));
    }

    (
        fallback_view_stamp.unwrap_or_else(|| "-".to_owned()),
        None,
    )
}

fn derive_thread_state(owners: &[String], members: &[AntigravitySwarmMember]) -> String {
    if owners.is_empty() {
        return "queued".to_owned();
    }
    let resolved: Vec<Option<&AntigravitySwarmMember>> = owners
        .iter()
        .map(|owner| members.iter().find(|member| member.name == *owner))
        .collect();
    if resolved
        .iter()
        .flatten()
        .any(|member| member.status == AntigravitySwarmMemberStatus::Failed)
    {
        return "failed".to_owned();
    }
    if resolved
        .iter()
        .flatten()
        .any(|member| member.status == AntigravitySwarmMemberStatus::Working)
    {
        return "active".to_owned();
    }
    if resolved.iter().all(|entry| {
        entry.is_some_and(|member| {
            member.has_posted_done || member.status == AntigravitySwarmMemberStatus::Killed
        })
    }) {
        return "done".to_owned();
    }
    if resolved.iter().flatten().any(|member| {
        member.status == AntigravitySwarmMemberStatus::Waiting
            && !member.has_posted_done
            && (member.steps > 0 || member.posts > 0)
    }) {
        return "active".to_owned();
    }
    "queued".to_owned()
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

    #[test]
    fn plan_json_screenshots_stopped_channels_control_and_done_tracking() {
        let root = temp_root();
        let coord_id = "coord-plan-1";
        let worker_done = "work-done-1";
        let worker_active = "work-active-2";
        let worker_stopped = "work-stopped-3";
        let worker_failed = "work-failed-4";
        let worker_reassigned = "work-reassigned-5";

        let swarm_path = swarm_dir(&root, coord_id);
        write_file(
            &swarm_path.join("members.json"),
            &serde_json::json!({
                "title": "Plan Swarm",
                "channels": { "main": {}, "z-extra": {} },
                "members": {
                    "coordinator": {
                        "conversation_id": coord_id,
                        "model": "gemini-4-neon",
                        "role": "coordinator",
                        "wake_mode": "mentions"
                    },
                    "worker-done": {
                        "conversation_id": worker_done,
                        "model": "gemini-4-neon",
                        "role": "worker",
                        "wake_mode": "mentions"
                    },
                    "worker-active": {
                        "conversation_id": worker_active,
                        "model": "gemini-4-carbon",
                        "role": "worker",
                        "wake_mode": "working"
                    },
                    "worker-stopped": {
                        "conversation_id": worker_stopped,
                        "model": "gemini-4-argon",
                        "role": "worker",
                        "phase": "STOPPED",
                        "desired_state": "STOPPED",
                        "wake_mode": "mentions"
                    },
                    "worker-failed": {
                        "conversation_id": worker_failed,
                        "model": "gemini-3.8-flash",
                        "role": "worker",
                        "wake_mode": "working"
                    },
                    "worker-reassigned": {
                        "conversation_id": worker_reassigned,
                        "model": "gemini-4-hydrogen",
                        "role": "worker",
                        "wake_mode": "mentions"
                    }
                }
            })
            .to_string(),
        );
        write_file(
            &swarm_path.join("plan.json"),
            &serde_json::json!({
                "goal": {
                    "title": "Ship native Swarm Overview",
                    "line": "Parse plan.json, channels, screenshots, and stopped state."
                },
                "tasks": {
                    "worker-done": "Parse board screenshots",
                    "worker-active": "Wire plan overview",
                    "worker-stopped": "Idle standby task",
                    "worker-failed": "Investigate quota failure",
                    "worker-reassigned": "Follow-up pass"
                },
                "threads": [
                    { "id": "t-done", "title": "Board parser", "goal": "Extract screenshots and channels", "tasks": ["worker-done"] },
                    { "id": "t-active", "title": "Plan builder", "goal": "Build plan overview", "tasks": ["worker-active"] },
                    { "id": "t-failed", "title": "Failure check", "goal": "Handle worker crash", "tasks": ["worker-failed"] },
                    { "id": "t-queued", "title": "Standby work", "goal": "Wait for resume", "tasks": ["worker-stopped", "worker-reassigned"] }
                ]
            })
            .to_string(),
        );
        write_file(
            &swarm_path.join("board.jsonl"),
            concat!(
                r#"{"seq":1,"agent":"worker-reassigned","text":"@coordinator [done] Wave 1 finished","time":"2026-11-24T10:00:00Z"}"#,
                "\n",
                r#"{"seq":2,"agent":"coordinator","text":"@worker-reassigned Start wave 2","time":"2026-11-24T10:01:00Z","kind":"brief"}"#,
                "\n",
                r#"{"seq":3,"agent":"worker-done","text":"@coordinator [done] Posted shot on @#design\nScreenshot: /tmp/brain/shot-1.png | Added split header","time":"2026-11-24T10:02:00Z","channel":"infra"}"#,
                "\n",
                r#"{"seq":4,"agent":"system","text":"internal nudge","time":"2026-11-24T10:03:00Z","control":{"type":"nudge","target":"worker-active"}}"#
            ),
        );

        write_file(
            &transcript_path(&root, coord_id),
            r#"{"step_index":0,"type":"PLANNER_RESPONSE","tool_calls":[]}"#,
        );
        write_file(
            &transcript_path(&root, worker_done),
            r#"{"step_index":0,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"swarm_wait","args":{}}]}
{"step_index":1,"type":"GENERIC","status":"DONE"}"#,
        );
        write_file(
            &transcript_path(&root, worker_active),
            r#"{"step_index":0,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"read_file","args":{"Path":"plan.json"}}]}
{"step_index":1,"type":"GENERIC","status":"RUNNING"}"#,
        );
        write_file(
            &transcript_path(&root, worker_failed),
            r#"{"step_index":0,"type":"ERROR_MESSAGE","status":"ERROR","error":"rate_limit exceeded"}"#,
        );
        write_file(
            &transcript_path(&root, worker_reassigned),
            r#"{"step_index":0,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"swarm_wait","args":{}}]}
{"step_index":1,"type":"GENERIC","status":"DONE"}"#,
        );

        let snapshot = read_swarm_snapshot_in_roots(&[root.clone()], coord_id, now_unix_ms())
            .expect("read snapshot")
            .expect("snapshot exists");

        // Control message seq 4 is filtered from messages, while head_seq advances.
        assert_eq!(snapshot.head_seq, 4);
        assert_eq!(snapshot.messages.len(), 3);
        assert_eq!(snapshot.channels, vec!["main", "design", "infra", "z-extra"]);
        assert!(snapshot.board_fingerprint.is_some());
        assert!(snapshot.roster_fingerprint.is_some());

        let shot_msg = &snapshot.messages[2];
        assert_eq!(shot_msg.channel.as_deref(), Some("infra"));
        assert_eq!(shot_msg.channel_mentions, vec!["design"]);
        assert_eq!(shot_msg.screenshots.len(), 1);
        assert_eq!(shot_msg.screenshots[0].path, "/tmp/brain/shot-1.png");
        assert_eq!(shot_msg.screenshots[0].caption, "Added split header");
        assert_eq!(shot_msg.screenshots[0].seq, Some(3));
        assert_eq!(shot_msg.screenshots[0].agent.as_deref(), Some("worker-done"));

        let by_name: HashMap<&str, &AntigravitySwarmMember> = snapshot
            .members
            .iter()
            .map(|member| (member.name.as_str(), member))
            .collect();
        assert_eq!(by_name["worker-stopped"].status, AntigravitySwarmMemberStatus::Stopped);
        assert_eq!(by_name["worker-done"].status, AntigravitySwarmMemberStatus::Waiting);
        assert!(by_name["worker-done"].has_posted_done);
        assert_eq!(by_name["worker-done"].task.as_deref(), Some("Parse board screenshots"));
        assert!(!by_name["worker-reassigned"].has_posted_done);

        let overview = snapshot.overview.as_ref().expect("plan.json overview populated");
        assert_eq!(overview.source.as_deref(), Some("plan"));
        assert_eq!(overview.headline, "Ship native Swarm Overview");
        assert_eq!(
            overview.goal_line.as_deref(),
            Some("Parse plan.json, channels, screenshots, and stopped state.")
        );
        assert_eq!(overview.screenshots.len(), 1);
        assert_eq!(overview.workstreams.len(), 4);
        let ws_states: HashMap<&str, &str> = overview
            .workstreams
            .iter()
            .map(|ws| (ws.id.as_str(), ws.state.as_str()))
            .collect();
        assert_eq!(ws_states["t-done"], "done");
        assert_eq!(ws_states["t-active"], "active");
        assert_eq!(ws_states["t-failed"], "failed");
        assert_eq!(ws_states["t-queued"], "active");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn view_json_reel_and_attention_member_parse_cleanly() {
        let root = temp_root();
        let coord_id = "coord-view-reel";
        let swarm_path = swarm_dir(&root, coord_id);
        write_file(
            &swarm_path.join("members.json"),
            &serde_json::json!({
                "title": "Reel Swarm",
                "members": {
                    "coordinator": { "conversation_id": coord_id, "role": "coordinator" },
                    "alpha": { "conversation_id": "alpha-1", "role": "worker" }
                }
            })
            .to_string(),
        );
        write_file(
            &swarm_path.join("board.jsonl"),
            r#"{"seq":5,"agent":"alpha","text":"Screenshot: /tmp/old.png | Fallback ignored when reel exists","time":"2026-11-24T10:00:00Z"}"#,
        );
        write_file(
            &swarm_path.join("view.json"),
            &serde_json::json!({
                "as_of_seq": 5,
                "headline": "Visual polish underway",
                "goal": { "title": "Visual polish underway", "line": "Align cards across themes." },
                "attention": [
                    { "id": "a1", "kind": "review", "severity": "med", "title": "Direct member", "member": "alpha" },
                    { "id": "a2", "kind": "decision", "severity": "high", "title": "Ref member", "refs": [{"member": "coordinator"}] }
                ],
                "reel": {
                    "frames": [
                        { "path": "/tmp/reel-1.png", "seq": 5, "caption": "Card header aligned" }
                    ]
                }
            })
            .to_string(),
        );

        let snapshot = read_swarm_snapshot_in_roots(&[root.clone()], coord_id, now_unix_ms())
            .expect("read snapshot")
            .expect("snapshot exists");
        let overview = snapshot.overview.as_ref().expect("overview present");
        assert_eq!(overview.source.as_deref(), Some("view"));
        assert_eq!(overview.goal_line.as_deref(), Some("Align cards across themes."));
        assert_eq!(overview.attention[0].member.as_deref(), Some("coordinator"));
        assert_eq!(overview.attention[1].member.as_deref(), Some("alpha"));
        assert_eq!(overview.screenshots.len(), 1);
        assert_eq!(overview.screenshots[0].path, "/tmp/reel-1.png");
        assert_eq!(overview.screenshots[0].agent.as_deref(), Some("alpha"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn truncated_transcript_tail_falls_back_to_wake_mode_instead_of_working() {
        let root = temp_root();
        let coord_id = "coord-trunc-1";
        let worker_id = "worker-trunc-2";
        let swarm_path = swarm_dir(&root, coord_id);
        write_file(
            &swarm_path.join("members.json"),
            &serde_json::json!({
                "title": "Truncated Transcript Swarm",
                "members": {
                    "coordinator": { "conversation_id": coord_id, "role": "coordinator", "wake_mode": "mentions" },
                    "alpha": { "conversation_id": worker_id, "role": "worker", "wake_mode": "mentions" }
                }
            })
            .to_string(),
        );
        // Write a >256 KB tail of GENERIC tool output after PLANNER_RESPONSE(swarm_wait)
        // so `read_tail_bounded` skips `PLANNER_RESPONSE` off the front of the window.
        let huge_blob = "x".repeat(MAX_TRANSCRIPT_TAIL_BYTES as usize + 8192);
        let transcript = format!(
            "{{\"step_index\":0,\"type\":\"PLANNER_RESPONSE\",\"tool_calls\":[{{\"name\":\"swarm_wait\",\"args\":{{}}}}]}}\n{{\"step_index\":1,\"type\":\"GENERIC\",\"status\":\"DONE\",\"content\":\"{huge_blob}\"}}\n"
        );
        write_file(&transcript_path(&root, worker_id), &transcript);

        let snapshot = read_swarm_snapshot_in_roots(&[root.clone()], coord_id, now_unix_ms())
            .expect("read snapshot")
            .expect("snapshot exists");
        let alpha = snapshot
            .members
            .iter()
            .find(|m| m.name == "alpha")
            .expect("alpha member present");
        assert_eq!(alpha.status, AntigravitySwarmMemberStatus::Waiting);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn locate_swarm_negative_cache_skips_redundant_scans_and_invalidates_on_change() {
        let root = temp_root();
        let other_swarm_coord = "coord-existing-1";
        let plain_session = format!("plain-session-{}", Uuid::new_v4());
        fs::create_dir_all(root.join("brain").join(&plain_session)).unwrap();
        let other_members = swarm_dir(&root, other_swarm_coord).join("members.json");
        write_file(
            &other_members,
            r#"{"title":"Other","members":{"coordinator":{"conversation_id":"coord-existing-1","role":"coordinator"}}}"#,
        );

        assert!(locate_swarm_with_cap(&[root.clone()], &plain_session, MAX_BRAIN_SCAN_DIRS).is_none());
        assert!(is_negative_swarm_cached(&[root.clone()], &plain_session));

        // While nothing changed, subsequent polls hit the negative cache without rescanning.
        assert!(locate_swarm_with_cap(&[root.clone()], &plain_session, MAX_BRAIN_SCAN_DIRS).is_none());

        // Updating an existing swarm's `members.json` invalidates the negative cache immediately.
        std::thread::sleep(Duration::from_millis(20));
        write_file(
            &other_members,
            &format!(
                r#"{{"title":"Other","members":{{"coordinator":{{"conversation_id":"{other_swarm_coord}","role":"coordinator"}},"helper":{{"conversation_id":"{plain_session}","role":"worker"}}}}}}"#
            ),
        );
        let located = locate_swarm_with_cap(&[root.clone()], &plain_session, MAX_BRAIN_SCAN_DIRS)
            .expect("negative cache must invalidate when members.json changes");
        assert_eq!(located.swarm_id, other_swarm_coord);
        assert_eq!(located.current_member.as_deref(), Some("helper"));

        let _ = fs::remove_dir_all(&root);
    }
}
