//! Native GPUI right-panel surface for Antigravity Swarm Boards.
//!
//! All filesystem reads execute on the daemon host through
//! [`waku_client::WorkspaceOperation::ReadAntigravitySwarm`] on the background
//! executor, guarded by a generation counter so superseded responses cannot
//! overwrite newer state. `render` reads only in-memory snapshots.

use base64::Engine as _;
use waku_client::antigravity_swarm::{
    AntigravitySwarmAttentionItem, AntigravitySwarmFailureKind, AntigravitySwarmMember,
    AntigravitySwarmMemberStatus, AntigravitySwarmMessage, AntigravitySwarmOverview,
    AntigravitySwarmScreenshot, AntigravitySwarmSnapshot,
};

use super::*;
use crate::ui::ActivationExt as _;

const MAX_SWARM_SCREENSHOT_BYTES: u64 = 20 * 1024 * 1024;

const SWARM_PANEL_CONTEXT: &str = "SwarmPanel";
/// Polite background poll cadence while an Antigravity swarm is active or the
/// Swarm panel is visible. Stops completely when hidden or switched away.
const SWARM_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Minimum interval between stream-triggered refreshes so rapid consecutive
/// tool events coalesce cleanly.
const SWARM_ACTIVITY_THROTTLE: Duration = Duration::from_millis(500);

pub(super) fn init(_cx: &mut App) {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SwarmPanelTab {
    #[default]
    Hive,
    Board,
    Members,
    Overview,
}

impl SwarmPanelTab {
    const ALL: [Self; 4] = [Self::Hive, Self::Board, Self::Members, Self::Overview];

    fn step(self, delta: isize) -> Self {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0) as isize;
        let next = (index + delta).rem_euclid(Self::ALL.len() as isize) as usize;
        Self::ALL[next]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SwarmSurfaceAction {
    None,
    OpenPanel,
    AttachTab,
}

#[derive(Default)]
struct SwarmAutoSurfaceState {
    /// Tracks whether an auto-reveal was requested for a session's newly
    /// detected swarm across throttled/in-flight refreshes or while another
    /// session was active.
    pending_auto_reveal: HashSet<Uuid>,
    /// Sessions where we already surfaced the Swarm tab on first detection.
    auto_surfaced_sessions: HashSet<Uuid>,
    /// Sessions where the user explicitly closed the Swarm tab.
    dismissed_sessions: HashSet<Uuid>,
}

impl SwarmAutoSurfaceState {
    fn queue_auto_reveal(&mut self, session_id: Uuid) -> bool {
        if !self.auto_surfaced_sessions.contains(&session_id)
            && !self.dismissed_sessions.contains(&session_id)
        {
            self.pending_auto_reveal.insert(session_id);
            true
        } else {
            false
        }
    }

    fn has_pending_reveal(&self, session_id: Uuid) -> bool {
        self.pending_auto_reveal.contains(&session_id)
    }

    fn mark_opened(&mut self, session_id: Uuid) {
        self.dismissed_sessions.remove(&session_id);
        self.pending_auto_reveal.remove(&session_id);
    }

    fn mark_dismissed(&mut self, session_id: Uuid) {
        self.dismissed_sessions.insert(session_id);
        self.pending_auto_reveal.remove(&session_id);
    }

    fn remove_session(&mut self, session_id: Uuid) {
        self.pending_auto_reveal.remove(&session_id);
        self.auto_surfaced_sessions.remove(&session_id);
        self.dismissed_sessions.remove(&session_id);
    }

    /// Resolves what surface action to take when a swarm snapshot is detected.
    ///
    /// Deferred auto-reveal stays queued while `session_id` is not selected so
    /// switching back to that session later still opens the Swarm panel once.
    fn resolve_detected_swarm(
        &mut self,
        session_id: Uuid,
        is_selected: bool,
        auto_reveal: bool,
        right_panel_visible: bool,
        has_swarm_surface: bool,
    ) -> SwarmSurfaceAction {
        if !is_selected {
            if auto_reveal {
                self.queue_auto_reveal(session_id);
            }
            return SwarmSurfaceAction::None;
        }

        let was_pending = self.pending_auto_reveal.remove(&session_id);
        let should_reveal = auto_reveal || was_pending;
        if self.dismissed_sessions.contains(&session_id)
            || self.auto_surfaced_sessions.contains(&session_id)
        {
            return SwarmSurfaceAction::None;
        }
        if should_reveal {
            self.auto_surfaced_sessions.insert(session_id);
            return SwarmSurfaceAction::OpenPanel;
        }
        if right_panel_visible && !has_swarm_surface {
            self.auto_surfaced_sessions.insert(session_id);
            return SwarmSurfaceAction::AttachTab;
        }
        SwarmSurfaceAction::None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SwarmRequestIdentity {
    host: HostKey,
    host_generation: u64,
    account: Option<String>,
    config_dir: Option<String>,
    native_id: String,
    cursor: Option<ProviderResumeCursor>,
}

pub(super) struct SwarmPanelState {
    pub(super) active_tab: SwarmPanelTab,
    pub(super) member_filter: Option<String>,
    pub(super) channel_filter: Option<String>,
    pub(super) follow_tail: Rc<Cell<bool>>,
    pub(super) show_killed_members: bool,
    pub(super) hive: super::swarm_hive::SwarmHiveState,
    /// Per-session cached swarm snapshot (`Some(None)` = probed and no swarm).
    pub(super) snapshots: HashMap<Uuid, Option<Arc<AntigravitySwarmSnapshot>>>,
    snapshot_identities: HashMap<Uuid, SwarmRequestIdentity>,
    request_identities: HashMap<Uuid, SwarmRequestIdentity>,
    errors: HashMap<Uuid, String>,
    pending: HashSet<Uuid>,
    stale: HashSet<Uuid>,
    last_activity_refresh_at: HashMap<Uuid, Instant>,
    /// Tracks sessions that emitted swarm tool activity before a snapshot landed.
    saw_swarm_activity: HashSet<Uuid>,
    /// Coalesces trailing-edge throttle timers per session.
    throttle_scheduled: HashSet<Uuid>,
    /// Monotonic counter per fetch so out-of-order responses are discarded.
    generation: u64,
    session_generations: HashMap<Uuid, u64>,
    /// Monotonic counter controlling the active 2s polling task.
    poll_generation: u64,
    poll_active: bool,
    auto_surface: SwarmAutoSurfaceState,
    /// Virtualized list backing `board.jsonl` messages.
    board_list_state: ListState,
    board_scrollbar: Rc<ScrollbarState>,
    board_list_key: Option<(Uuid, String, Option<String>, Option<String>)>,
    board_message_seqs: Vec<u64>,
    filtered_message_indices: Rc<Vec<usize>>,
    /// Parsed markdown per board post `seq`, valid for `message_markdown_swarm`.
    message_markdown: Rc<RefCell<HashMap<u64, SwarmMessageMarkdown>>>,
    message_markdown_swarm: Option<String>,
    expanded_messages: Rc<RefCell<HashSet<u64>>>,
    message_toggle_focuses: Rc<RefCell<HashMap<u64, FocusHandle>>>,
    pub(super) board_selection: TranscriptSelection,
    /// Virtualized list backing the Members roster, grouped by status.
    members_list_state: ListState,
    members_scrollbar: Rc<ScrollbarState>,
    members_list_key: Option<(Uuid, String, bool)>,
    member_list_items: Rc<Vec<SwarmMemberListItem>>,
    /// Scroll container for Overview.
    overview_scroll: ScrollHandle,
    overview_scrollbar: Rc<ScrollbarState>,
    /// Stable keyboard focus handles.
    pub(super) tab_hive_focus: FocusHandle,
    pub(super) tab_board_focus: FocusHandle,
    pub(super) tab_members_focus: FocusHandle,
    pub(super) tab_overview_focus: FocusHandle,
    follow_focus: FocusHandle,
    refresh_focus: FocusHandle,
    show_killed_focus: FocusHandle,
    clear_filter_focus: FocusHandle,
    jump_latest_focus: FocusHandle,
    retry_focus: FocusHandle,
    more_attention_focus: FocusHandle,
    filter_chip_focuses: RefCell<HashMap<String, FocusHandle>>,
    channel_chip_focuses: RefCell<HashMap<String, FocusHandle>>,
    member_row_focuses: RefCell<HashMap<String, FocusHandle>>,
    screenshot_focuses: RefCell<HashMap<String, FocusHandle>>,
    attention_member_focuses: RefCell<HashMap<String, FocusHandle>>,
}

impl SwarmPanelState {
    pub(super) fn new(cx: &mut Context<Waku>) -> Self {
        let follow_tail = Rc::new(Cell::new(true));
        let board_list_state = ListState::new(0, ListAlignment::Bottom, px(512.0));
        board_list_state.set_scroll_handler({
            let follow_tail = follow_tail.clone();
            move |event, window, _| {
                let next = !event.is_scrolled;
                if follow_tail.get() != next {
                    follow_tail.set(next);
                    window.refresh();
                }
            }
        });
        Self {
            active_tab: SwarmPanelTab::Hive,
            member_filter: None,
            channel_filter: None,
            follow_tail,
            show_killed_members: false,
            hive: super::swarm_hive::SwarmHiveState::new(cx),
            snapshots: HashMap::new(),
            snapshot_identities: HashMap::new(),
            request_identities: HashMap::new(),
            errors: HashMap::new(),
            pending: HashSet::new(),
            stale: HashSet::new(),
            last_activity_refresh_at: HashMap::new(),
            saw_swarm_activity: HashSet::new(),
            throttle_scheduled: HashSet::new(),
            generation: 0,
            session_generations: HashMap::new(),
            poll_generation: 0,
            poll_active: false,
            auto_surface: SwarmAutoSurfaceState::default(),
            board_list_state,
            board_scrollbar: ScrollbarState::new(),
            board_list_key: None,
            board_message_seqs: Vec::new(),
            filtered_message_indices: Rc::new(Vec::new()),
            message_markdown: Rc::new(RefCell::new(HashMap::new())),
            message_markdown_swarm: None,
            expanded_messages: Rc::new(RefCell::new(HashSet::new())),
            message_toggle_focuses: Rc::new(RefCell::new(HashMap::new())),
            board_selection: TranscriptSelection::default(),
            members_list_state: ListState::new(0, ListAlignment::Top, px(256.0)),
            members_scrollbar: ScrollbarState::new(),
            members_list_key: None,
            member_list_items: Rc::new(Vec::new()),
            overview_scroll: ScrollHandle::new(),
            overview_scrollbar: ScrollbarState::new(),
            tab_hive_focus: cx.focus_handle(),
            tab_board_focus: cx.focus_handle(),
            tab_members_focus: cx.focus_handle(),
            tab_overview_focus: cx.focus_handle(),
            follow_focus: cx.focus_handle(),
            refresh_focus: cx.focus_handle(),
            show_killed_focus: cx.focus_handle(),
            clear_filter_focus: cx.focus_handle(),
            jump_latest_focus: cx.focus_handle(),
            retry_focus: cx.focus_handle(),
            more_attention_focus: cx.focus_handle(),
            filter_chip_focuses: RefCell::new(HashMap::new()),
            channel_chip_focuses: RefCell::new(HashMap::new()),
            member_row_focuses: RefCell::new(HashMap::new()),
            screenshot_focuses: RefCell::new(HashMap::new()),
            attention_member_focuses: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn clear_board_selection(&self) {
        self.board_selection.clear();
    }

    fn clear_dynamic_focuses(&self) {
        self.filter_chip_focuses.borrow_mut().clear();
        self.channel_chip_focuses.borrow_mut().clear();
        self.member_row_focuses.borrow_mut().clear();
        self.message_toggle_focuses.borrow_mut().clear();
        self.screenshot_focuses.borrow_mut().clear();
        self.attention_member_focuses.borrow_mut().clear();
    }

    fn prune_dynamic_focuses(&self, snapshot: &AntigravitySwarmSnapshot) {
        let live_members: HashSet<&str> =
            snapshot.members.iter().map(|m| m.name.as_str()).collect();
        self.filter_chip_focuses
            .borrow_mut()
            .retain(|key, _| key == "__all__" || live_members.contains(key.as_str()));
        self.member_row_focuses
            .borrow_mut()
            .retain(|key, _| live_members.contains(key.as_str()));
        let live_channels: HashSet<&str> = snapshot.channels.iter().map(String::as_str).collect();
        self.channel_chip_focuses
            .borrow_mut()
            .retain(|key, _| key == "__all_ch__" || live_channels.contains(key.as_str()));
    }

    pub(super) fn remove_session(&mut self, session_id: Uuid) {
        self.snapshots.remove(&session_id);
        self.snapshot_identities.remove(&session_id);
        self.request_identities.remove(&session_id);
        self.errors.remove(&session_id);
        self.pending.remove(&session_id);
        self.stale.remove(&session_id);
        self.last_activity_refresh_at.remove(&session_id);
        self.saw_swarm_activity.remove(&session_id);
        self.throttle_scheduled.remove(&session_id);
        self.session_generations.remove(&session_id);
        self.auto_surface.remove_session(session_id);
    }

    fn tab_focus(&self, tab: SwarmPanelTab) -> &FocusHandle {
        match tab {
            SwarmPanelTab::Hive => &self.tab_hive_focus,
            SwarmPanelTab::Board => &self.tab_board_focus,
            SwarmPanelTab::Members => &self.tab_members_focus,
            SwarmPanelTab::Overview => &self.tab_overview_focus,
        }
    }

    fn focus_from_map(
        map: &RefCell<HashMap<String, FocusHandle>>,
        key: &str,
        cx: &mut Context<Waku>,
    ) -> FocusHandle {
        let mut map = map.borrow_mut();
        if let Some(handle) = map.get(key) {
            return handle.clone();
        }
        let handle = cx.focus_handle();
        map.insert(key.to_owned(), handle.clone());
        handle
    }

    fn filter_chip_focus(&self, key: &str, cx: &mut Context<Waku>) -> FocusHandle {
        Self::focus_from_map(&self.filter_chip_focuses, key, cx)
    }

    fn channel_chip_focus(&self, key: &str, cx: &mut Context<Waku>) -> FocusHandle {
        Self::focus_from_map(&self.channel_chip_focuses, key, cx)
    }

    fn member_row_focus(&self, key: &str, cx: &mut Context<Waku>) -> FocusHandle {
        Self::focus_from_map(&self.member_row_focuses, key, cx)
    }
}

pub(super) fn antigravity_session_id(session: &AgentSession) -> Option<&str> {
    if session.provider != ProviderKind::Antigravity {
        return None;
    }
    match session.provider_cursor.as_ref() {
        Some(ProviderResumeCursor::Antigravity { session_id, .. }) if !session_id.is_empty() => {
            return Some(session_id.as_str());
        }
        Some(_) => return None,
        None => {}
    }
    if let Some(account_id) = session.account_id.as_deref().filter(|id| !id.is_empty()) {
        session
            .account_cursors
            .iter()
            .find_map(|entry| match &entry.cursor {
                ProviderResumeCursor::Antigravity { session_id, .. }
                    if entry.account_id == account_id && !session_id.is_empty() =>
                {
                    Some(session_id.as_str())
                }
                _ => None,
            })
    } else {
        session
            .provider_cursors
            .iter()
            .find_map(|cursor| match cursor {
                ProviderResumeCursor::Antigravity { session_id, .. } if !session_id.is_empty() => {
                    Some(session_id.as_str())
                }
                _ => None,
            })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SwarmMemberCounts {
    working: usize,
    waiting: usize,
    stopped: usize,
    failed: usize,
    killed: usize,
}

impl SwarmMemberCounts {
    fn live(self) -> usize {
        self.working + self.waiting + self.stopped + self.failed
    }
}

fn swarm_member_counts(snapshot: &AntigravitySwarmSnapshot) -> SwarmMemberCounts {
    let mut counts = SwarmMemberCounts::default();
    for member in &snapshot.members {
        if member.is_role_worker {
            continue;
        }
        match member_display_status(member) {
            AntigravitySwarmMemberStatus::Working => counts.working += 1,
            AntigravitySwarmMemberStatus::Waiting => counts.waiting += 1,
            AntigravitySwarmMemberStatus::Stopped => counts.stopped += 1,
            AntigravitySwarmMemberStatus::Failed => counts.failed += 1,
            AntigravitySwarmMemberStatus::Killed => counts.killed += 1,
        }
    }
    counts
}

fn message_matches_channel(message: &AntigravitySwarmMessage, channel: &str) -> bool {
    if channel == "main" {
        message.channel.as_deref().is_none_or(|c| c == "main")
            || message.channel_mentions.iter().any(|c| c == "main")
    } else {
        message.channel.as_deref() == Some(channel)
            || message.channel_mentions.iter().any(|c| c == channel)
    }
}

fn filter_swarm_message_indices(
    messages: &[AntigravitySwarmMessage],
    member_filter: Option<&str>,
    channel_filter: Option<&str>,
) -> Vec<usize> {
    if member_filter.is_none() && channel_filter.is_none() {
        return (0..messages.len()).collect();
    }
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            let member_ok = member_filter.is_none_or(|filter| {
                message.agent == filter
                    || message.mentions_all
                    || message.mentions.iter().any(|name| name == filter)
            });
            let channel_ok =
                channel_filter.is_none_or(|channel| message_matches_channel(message, channel));
            member_ok && channel_ok
        })
        .map(|(index, _)| index)
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SwarmBoardListDiff {
    Unchanged,
    TailAppend {
        old_len: usize,
        delta: usize,
    },
    Reset {
        new_len: usize,
        anchor_item_ix: Option<usize>,
    },
}

fn compute_board_list_diff(
    same_context: bool,
    prev_seqs: &[u64],
    next_seqs: &[u64],
    scroll_anchor_ix: Option<usize>,
) -> SwarmBoardListDiff {
    if same_context && prev_seqs == next_seqs {
        return SwarmBoardListDiff::Unchanged;
    }
    if same_context
        && !prev_seqs.is_empty()
        && next_seqs.len() > prev_seqs.len()
        && next_seqs.starts_with(prev_seqs)
    {
        return SwarmBoardListDiff::TailAppend {
            old_len: prev_seqs.len(),
            delta: next_seqs.len() - prev_seqs.len(),
        };
    }
    let anchor_item_ix = if same_context {
        scroll_anchor_ix
            .and_then(|ix| prev_seqs.get(ix).copied())
            .and_then(|anchor_seq| {
                next_seqs
                    .iter()
                    .position(|&seq| seq >= anchor_seq)
                    .or_else(|| next_seqs.len().checked_sub(1))
            })
    } else {
        None
    };
    SwarmBoardListDiff::Reset {
        new_len: next_seqs.len(),
        anchor_item_ix,
    }
}

/// Board body scale: one notch under the transcript's compact prose so a
/// narrow side panel still fits a readable line.
const SWARM_BOARD_METRICS: MarkdownMetrics = MarkdownMetrics {
    text_size: 12.5,
    line_height: 18.0,
    code_text_size: 12.0,
    code_line_height: 17.5,
    block_gap: 6.0,
};
/// Posts longer than this many lines or characters start collapsed; coordinator
/// briefs are whole task prompts and otherwise bury every reply under them.
const MESSAGE_COLLAPSE_LINES: usize = 6;
const MESSAGE_COLLAPSE_CHARS: usize = 480;
const MESSAGE_PREVIEW_LINES: usize = 4;
const MESSAGE_PREVIEW_CHARS: usize = 320;

pub(super) struct SwarmMessageMarkdown {
    source: String,
    full: MarkdownView,
    /// Leading excerpt shown while a long post is collapsed.
    preview: Option<MarkdownView>,
}

impl SwarmMessageMarkdown {
    fn new(message: &AntigravitySwarmMessage) -> Self {
        let body = strip_leading_mentions(&message.text, &message.mentions, message.mentions_all);
        let mut full = MarkdownView::new();
        full.set_text(body, false);
        let preview = collapsed_preview(body).map(|excerpt| {
            let mut view = MarkdownView::new();
            view.set_text(&excerpt, false);
            view
        });
        Self {
            source: body.to_owned(),
            full,
            preview,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SwarmMemberListItem {
    Group {
        status: AntigravitySwarmMemberStatus,
        count: usize,
    },
    Member(usize),
}

const MEMBER_GROUP_ORDER: [AntigravitySwarmMemberStatus; 5] = [
    AntigravitySwarmMemberStatus::Failed,
    AntigravitySwarmMemberStatus::Working,
    AntigravitySwarmMemberStatus::Waiting,
    AntigravitySwarmMemberStatus::Stopped,
    AntigravitySwarmMemberStatus::Killed,
];

fn localized_swarm_role(role: &str) -> String {
    match role {
        "coordinator" => tr!("swarm.role_coordinator"),
        "worker" => tr!("swarm.role_worker"),
        "narrator" => tr!("swarm.role_narrator"),
        "monitor" => tr!("swarm.role_monitor"),
        "verifier" => tr!("swarm.role_verifier"),
        "investigator" => tr!("swarm.role_investigator"),
        other => other.to_owned(),
    }
}

fn localized_attention_kind(kind: &str) -> String {
    match kind {
        "decision" => tr!("swarm.attention_decision"),
        "failure" => tr!("swarm.attention_failure"),
        "review" => tr!("swarm.attention_review"),
        "stuck" => tr!("swarm.attention_stuck"),
        "quota" => tr!("swarm.attention_quota"),
        "error" => tr!("swarm.attention_error"),
        "fyi" => tr!("swarm.attention_fyi"),
        other => other.to_owned(),
    }
}

fn localized_workstream_state(state: &str) -> String {
    match state {
        "active" => tr!("swarm.state_active"),
        "blocked" => tr!("swarm.state_blocked"),
        "review" => tr!("swarm.state_review"),
        "done" => tr!("swarm.state_done"),
        "failed" => tr!("swarm.state_failed"),
        "queued" => tr!("swarm.state_queued"),
        other => other.to_owned(),
    }
}

pub(super) fn localized_member_activity(activity: &str) -> String {
    match activity {
        "Waiting for posts" => tr!("swarm.activity_waiting_posts"),
        "Reading the board" => tr!("swarm.activity_reading_board"),
        "Posting to the board" => tr!("swarm.activity_posting_board"),
        other => {
            if let Some(target) = other.strip_prefix("Read ").filter(|s| !s.is_empty()) {
                tr!("swarm.activity_read_file", target = target)
            } else if let Some(target) = other.strip_prefix("Write ").filter(|s| !s.is_empty()) {
                tr!("swarm.activity_write_file", target = target)
            } else if let Some(target) = other.strip_prefix("Edit ").filter(|s| !s.is_empty()) {
                tr!("swarm.activity_edit_file", target = target)
            } else if let Some(target) = other.strip_prefix("Search ").filter(|s| !s.is_empty()) {
                tr!("swarm.activity_search", target = target)
            } else {
                other.to_owned()
            }
        }
    }
}

pub(super) fn member_display_status(
    member: &AntigravitySwarmMember,
) -> AntigravitySwarmMemberStatus {
    if member.killed {
        AntigravitySwarmMemberStatus::Killed
    } else {
        member.status
    }
}

/// Roster rows grouped under status headers, in attention order.
fn swarm_member_list_items(
    members: &[AntigravitySwarmMember],
    show_killed: bool,
) -> Vec<SwarmMemberListItem> {
    let mut items = Vec::with_capacity(members.len() + MEMBER_GROUP_ORDER.len());
    for status in MEMBER_GROUP_ORDER {
        let start = items.len();
        items.push(SwarmMemberListItem::Group { status, count: 0 });
        items.extend(
            members
                .iter()
                .enumerate()
                .filter(|(_, member)| {
                    (show_killed || !member.killed) && member_display_status(member) == status
                })
                .map(|(index, _)| SwarmMemberListItem::Member(index)),
        );
        let count = items.len() - start - 1;
        if count == 0 {
            items.truncate(start);
        } else {
            items[start] = SwarmMemberListItem::Group { status, count };
        }
    }
    items
}

/// The model every live member runs, when they all share one, so the header
/// can state it once instead of every row repeating it.
fn shared_swarm_model(members: &[AntigravitySwarmMember]) -> Option<&str> {
    let mut models = members
        .iter()
        .filter(|member| !member.killed)
        .map(|member| member.model.as_str());
    let first = models.next().filter(|model| !model.is_empty())?;
    models.all(|model| model == first).then_some(first)
}

/// Stable identity color for a member, shared by board avatars, filter chips,
/// and roster rows so one agent reads the same everywhere.
fn agent_color(name: &str, theme: &Theme) -> Hsla {
    const HUES: [f32; 8] = [212.0, 152.0, 38.0, 276.0, 338.0, 188.0, 96.0, 16.0];
    let hash = name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    let hue = HUES[hash as usize % HUES.len()] / 360.0;
    if theme.is_dark {
        gpui::hsla(hue, 0.55, 0.68, 1.0)
    } else {
        gpui::hsla(hue, 0.55, 0.40, 1.0)
    }
}

/// Drops the `@name` tokens a post opens with when the header already lists
/// them as recipients.
fn strip_leading_mentions<'a>(text: &'a str, mentions: &[String], mentions_all: bool) -> &'a str {
    let mut rest = text.trim_start();
    while let Some(after_at) = rest.strip_prefix('@') {
        let token_end = after_at.find(char::is_whitespace).unwrap_or(after_at.len());
        let name = after_at[..token_end].trim_end_matches([',', ':', ';']);
        let listed = !name.is_empty()
            && ((mentions_all && name == "all") || mentions.iter().any(|mention| mention == name));
        if !listed {
            break;
        }
        rest = after_at[token_end..].trim_start();
    }
    if rest.is_empty() { text } else { rest }
}

fn collapsed_preview(body: &str) -> Option<String> {
    if body.lines().count() <= MESSAGE_COLLAPSE_LINES
        && body.chars().count() <= MESSAGE_COLLAPSE_CHARS
    {
        return None;
    }
    let mut excerpt = body
        .lines()
        .take(MESSAGE_PREVIEW_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if let Some((cut, _)) = excerpt.char_indices().nth(MESSAGE_PREVIEW_CHARS) {
        excerpt.truncate(cut);
    }
    Some(format!("{}…", excerpt.trim_end()))
}
fn extract_hh_mm(time_part: &str) -> Option<String> {
    let mut parts = time_part.split(':');
    let hour = parts.next()?;
    let minute = parts.next()?;
    let minute = minute.get(..2)?;
    ((1..=2).contains(&hour.len())
        && hour.bytes().all(|b| b.is_ascii_digit())
        && minute.bytes().all(|b| b.is_ascii_digit()))
    .then(|| format!("{hour}:{minute}"))
}

fn format_swarm_clock_time_with_tz<Tz: chrono::TimeZone>(raw: &str, tz: &Tz) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.with_timezone(tz).format("%H:%M").to_string());
    }
    if let Some((_, time_part)) = trimmed.split_once(' ').or_else(|| trimmed.split_once('T')) {
        return Some(extract_hh_mm(time_part).unwrap_or_else(|| time_part.to_owned()));
    }
    Some(extract_hh_mm(trimmed).unwrap_or_else(|| trimmed.to_owned()))
}

pub(super) fn short_board_time_with_tz<Tz: chrono::TimeZone>(raw: &str, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format_swarm_clock_time_with_tz(raw, tz).unwrap_or_default()
}

pub(super) fn short_board_time(raw: &str) -> String {
    short_board_time_with_tz(raw, &Local)
}

fn format_overview_updated_at_with_tz<Tz: chrono::TimeZone>(raw: &str, tz: &Tz) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    format_swarm_clock_time_with_tz(raw, tz)
}

fn format_overview_updated_at(raw: &str) -> Option<String> {
    format_overview_updated_at_with_tz(raw, &Local)
}

fn overview_meta_label(as_of_seq: u64, updated_at: Option<&str>) -> Option<String> {
    let updated = updated_at.and_then(format_overview_updated_at);
    match (as_of_seq > 0, updated) {
        (true, Some(time)) => Some(format!(
            "{} · {time}",
            tr!("swarm.as_of_seq", seq = as_of_seq)
        )),
        (true, None) => Some(tr!("swarm.as_of_seq", seq = as_of_seq)),
        (false, Some(time)) => Some(time),
        (false, None) => None,
    }
}

fn swarm_has_active_work(snapshot: &AntigravitySwarmSnapshot) -> bool {
    snapshot.members.iter().any(|member| {
        !member.killed
            && matches!(
                member.status,
                AntigravitySwarmMemberStatus::Working | AntigravitySwarmMemberStatus::Waiting
            )
    })
}

impl Waku {
    /// Opens the native Swarm Board right panel for the selected session.
    /// Invoked by `/swarm board` in the composer and the header Swarm button.
    pub(super) fn open_antigravity_swarm_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = self.state.selected_session {
            self.swarm_panel.auto_surface.mark_opened(session_id);
        }
        self.open_right_panel_surface(RightPanelSurface::Swarm, cx);
        self.refresh_selected_antigravity_swarm(cx);
        self.sync_antigravity_swarm_polling(cx);
    }

    pub(super) fn session_is_antigravity(&self, session_id: Uuid) -> bool {
        self.state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(|session| session.provider == ProviderKind::Antigravity)
    }

    pub(super) fn should_show_swarm_header_button(&self) -> bool {
        let Some(session) = self.selected_session() else {
            return false;
        };
        if session.provider != ProviderKind::Antigravity {
            return false;
        }
        true
    }

    fn swarm_request_identity(&self, session_id: Uuid) -> Option<SwarmRequestIdentity> {
        let session = self.state.sessions.iter().find(|s| s.id == session_id)?;
        let native_id = antigravity_session_id(session)?.to_owned();
        let host = self.hosts.ownership.session(session_id);
        Some(SwarmRequestIdentity {
            host_generation: self.hosts.get(&host)?.generation,
            host,
            account: session.account_id.clone(),
            config_dir: self.effective_config_dir_for_session(session),
            native_id,
            cursor: session.provider_cursor.clone(),
        })
    }

    pub(super) fn selected_antigravity_swarm_snapshot(
        &self,
    ) -> Option<Arc<AntigravitySwarmSnapshot>> {
        let session_id = self.state.selected_session?;
        let identity = self.swarm_request_identity(session_id)?;
        if self.swarm_panel.snapshot_identities.get(&session_id) != Some(&identity) {
            return None;
        }
        self.swarm_panel
            .snapshots
            .get(&session_id)
            .and_then(|entry| entry.as_ref())
            .cloned()
    }

    pub(super) fn mark_antigravity_swarm_dismissed(&mut self) {
        if let Some(session_id) = self.state.selected_session {
            self.swarm_panel.auto_surface.mark_dismissed(session_id);
        }
    }

    pub(super) fn on_antigravity_session_connected(
        &mut self,
        session_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        if self.state.selected_session == Some(session_id) {
            self.request_antigravity_swarm_refresh(session_id, false, cx);
            self.sync_antigravity_swarm_polling(cx);
        }
    }

    pub(super) fn on_antigravity_swarm_activity(
        &mut self,
        session_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.swarm_panel.saw_swarm_activity.insert(session_id);
        let should_reveal = self.swarm_panel.auto_surface.queue_auto_reveal(session_id);
        let now = Instant::now();
        if let Some(last) = self.swarm_panel.last_activity_refresh_at.get(&session_id) {
            let elapsed = now.duration_since(*last);
            if elapsed < SWARM_ACTIVITY_THROTTLE {
                self.swarm_panel.stale.insert(session_id);
                if !self.swarm_panel.pending.contains(&session_id)
                    && self.swarm_panel.throttle_scheduled.insert(session_id)
                {
                    let wait = SWARM_ACTIVITY_THROTTLE.saturating_sub(elapsed);
                    cx.spawn(async move |waku, cx| {
                        cx.background_executor().timer(wait).await;
                        let _ = waku.update(cx, |this, cx| {
                            this.swarm_panel.throttle_scheduled.remove(&session_id);
                            if this.swarm_panel.stale.remove(&session_id) {
                                this.swarm_panel
                                    .last_activity_refresh_at
                                    .insert(session_id, Instant::now());
                                let reveal =
                                    this.swarm_panel.auto_surface.has_pending_reveal(session_id);
                                this.request_antigravity_swarm_refresh(session_id, reveal, cx);
                            }
                        });
                    })
                    .detach();
                }
                return;
            }
        }
        self.swarm_panel
            .last_activity_refresh_at
            .insert(session_id, now);
        self.request_antigravity_swarm_refresh(session_id, should_reveal, cx);
        if self.state.selected_session == Some(session_id) {
            self.sync_antigravity_swarm_polling(cx);
        }
    }

    pub(super) fn on_antigravity_swarm_turn_settled(
        &mut self,
        session_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        if self.state.selected_session != Some(session_id) {
            return;
        }
        let has_swarm = self
            .swarm_panel
            .snapshots
            .get(&session_id)
            .and_then(|entry| entry.as_ref())
            .is_some();
        if has_swarm
            || self.is_swarm_surface_visible()
            || self.swarm_panel.saw_swarm_activity.contains(&session_id)
            || self.swarm_panel.stale.contains(&session_id)
        {
            let auto_reveal = self.swarm_panel.auto_surface.has_pending_reveal(session_id);
            self.request_antigravity_swarm_refresh(session_id, auto_reveal, cx);
            self.sync_antigravity_swarm_polling(cx);
        }
    }

    pub(super) fn on_session_activated_for_swarm(
        &mut self,
        session_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.image_preview_generation = self.image_preview_generation.wrapping_add(1);
        self.swarm_panel.member_filter = None;
        self.swarm_panel.channel_filter = None;
        self.swarm_panel.board_list_key = None;
        self.swarm_panel.board_message_seqs.clear();
        self.swarm_panel.members_list_key = None;
        self.swarm_panel.follow_tail.set(true);
        self.swarm_panel.clear_board_selection();
        self.swarm_panel
            .overview_scroll
            .set_offset(gpui::Point::default());
        self.swarm_panel.clear_dynamic_focuses();
        self.swarm_panel.hive.reset_for_session_change();
        let is_antigravity = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(|session| {
                session.provider == ProviderKind::Antigravity
                    && antigravity_session_id(session).is_some()
            });
        if is_antigravity {
            self.request_antigravity_swarm_refresh(session_id, false, cx);
        }
        self.sync_antigravity_swarm_polling(cx);
    }

    pub(super) fn refresh_selected_antigravity_swarm(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.state.selected_session else {
            return;
        };
        self.request_antigravity_swarm_refresh(session_id, false, cx);
    }

    pub(super) fn refresh_selected_antigravity_swarm_if_relevant(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected_session() else {
            return;
        };
        if session.provider == ProviderKind::Antigravity
            && antigravity_session_id(session).is_some()
            && (self.is_swarm_surface_visible()
                || self
                    .swarm_panel
                    .snapshots
                    .get(&session.id)
                    .and_then(|entry| entry.as_ref())
                    .is_some())
        {
            let session_id = session.id;
            self.request_antigravity_swarm_refresh(session_id, false, cx);
        }
        self.sync_antigravity_swarm_polling(cx);
    }

    pub(super) fn is_swarm_surface_visible(&self) -> bool {
        !self.is_bottom_panel_maximized()
            && self.right_panel_visible
            && self.active_right_panel_surface() == Some(&RightPanelSurface::Swarm)
    }

    fn should_poll_antigravity_swarm(&self) -> bool {
        let Some(session) = self.selected_session() else {
            return false;
        };
        if session.provider != ProviderKind::Antigravity {
            return false;
        }
        if antigravity_session_id(session).is_none() {
            return false;
        }
        if self.is_swarm_surface_visible() {
            return true;
        }
        self.swarm_panel
            .snapshots
            .get(&session.id)
            .and_then(|entry| entry.as_ref())
            .is_some_and(|snapshot| swarm_has_active_work(snapshot) || session.is_busy())
    }

    /// Starts or stops the polite ~2s Swarm polling task so zero background
    /// polling runs while hidden or switched away from an Antigravity swarm.
    pub(super) fn sync_antigravity_swarm_polling(&mut self, cx: &mut Context<Self>) {
        if !self.is_swarm_surface_visible() {
            self.swarm_panel.hive.stop_replay();
        }
        let should_poll = self.should_poll_antigravity_swarm();
        if !should_poll {
            if self.swarm_panel.poll_active {
                self.swarm_panel.poll_active = false;
                self.swarm_panel.poll_generation = self.swarm_panel.poll_generation.wrapping_add(1);
            }
            return;
        }
        if self.swarm_panel.poll_active {
            return;
        }
        self.swarm_panel.poll_active = true;
        self.swarm_panel.poll_generation = self.swarm_panel.poll_generation.wrapping_add(1);
        let generation = self.swarm_panel.poll_generation;
        cx.spawn(async move |waku, cx| {
            loop {
                cx.background_executor().timer(SWARM_POLL_INTERVAL).await;
                let keep_going = waku
                    .update(cx, |this, cx| {
                        if this.swarm_panel.poll_generation != generation
                            || !this.should_poll_antigravity_swarm()
                        {
                            if this.swarm_panel.poll_generation == generation {
                                this.swarm_panel.poll_active = false;
                            }
                            return false;
                        }
                        if this.settings_page.is_none() && this.workspace_watch.active {
                            this.refresh_selected_antigravity_swarm(cx);
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    fn request_antigravity_swarm_refresh(
        &mut self,
        session_id: Uuid,
        auto_reveal: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(identity) = self.swarm_request_identity(session_id) else {
            return;
        };
        let ag_session_id = identity.native_id.clone();
        let config_dir = identity.config_dir.clone();
        if self.swarm_panel.request_identities.get(&session_id) != Some(&identity) {
            self.swarm_panel.pending.remove(&session_id);
            self.swarm_panel.snapshots.remove(&session_id);
            self.swarm_panel.snapshot_identities.remove(&session_id);
            self.swarm_panel.errors.remove(&session_id);
            self.swarm_panel.auto_surface.remove_session(session_id);
            self.swarm_panel.board_list_key = None;
            self.swarm_panel.board_message_seqs.clear();
            self.swarm_panel.members_list_key = None;
            self.swarm_panel.hive.reset_for_session_change();
        }
        self.swarm_panel
            .request_identities
            .insert(session_id, identity.clone());

        if auto_reveal {
            self.swarm_panel.auto_surface.queue_auto_reveal(session_id);
        }
        if self.swarm_panel.pending.contains(&session_id) {
            self.swarm_panel.stale.insert(session_id);
            return;
        }

        self.swarm_panel.pending.insert(session_id);
        self.swarm_panel.stale.remove(&session_id);
        let notify_loading = !self.swarm_panel.snapshots.contains_key(&session_id)
            || self.swarm_panel.errors.contains_key(&session_id);
        if notify_loading {
            cx.notify();
        }
        self.swarm_panel.generation = self.swarm_panel.generation.wrapping_add(1);
        let generation = self.swarm_panel.generation;
        self.swarm_panel
            .session_generations
            .insert(session_id, generation);

        let workspace =
            waku_client::WorkspaceClient::for_daemon(self.daemon_for_session(session_id).clone());
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match workspace.request(waku_client::WorkspaceOperation::ReadAntigravitySwarm {
                        session_id: ag_session_id,
                        config_dir,
                    }) {
                        Ok(waku_client::WorkspaceResult::AntigravitySwarm { snapshot }) => {
                            Ok(snapshot.map(Arc::new))
                        }
                        Ok(_) => Err("the daemon returned an invalid swarm response".to_owned()),
                        Err(error) => Err(error.to_string()),
                    }
                })
                .await;

            let _ = waku.update(cx, |this, cx| {
                if this.swarm_panel.session_generations.get(&session_id) != Some(&generation) {
                    return;
                }
                this.swarm_panel.pending.remove(&session_id);
                if this.swarm_request_identity(session_id).as_ref() != Some(&identity) {
                    // An old host/account/cursor cannot populate even errors or
                    // auto-reveal state. Refresh the new identity instead.
                    this.swarm_panel.stale.remove(&session_id);
                    this.request_antigravity_swarm_refresh(session_id, false, cx);
                    return;
                }
                let mut changed = notify_loading;
                let mut detected_now = false;
                match result {
                    Ok(snapshot) => {
                        if this.swarm_panel.errors.remove(&session_id).is_some() {
                            changed = true;
                        }
                        let previous_fp = this
                            .swarm_panel
                            .snapshots
                            .get(&session_id)
                            .and_then(|entry| entry.as_ref())
                            .map(|snap| snap.fingerprint.clone());
                        let next_fp = snapshot.as_ref().map(|snap| snap.fingerprint.clone());
                        let was_known = this.swarm_panel.snapshots.contains_key(&session_id);
                        detected_now = snapshot.is_some();
                        if !was_known || previous_fp != next_fp {
                            this.swarm_panel.snapshots.insert(session_id, snapshot);
                            this.swarm_panel
                                .snapshot_identities
                                .insert(session_id, identity.clone());
                            changed = true;
                        }
                    }
                    Err(error) => {
                        if this.swarm_panel.errors.get(&session_id) != Some(&error) {
                            this.swarm_panel.errors.insert(session_id, error);
                            changed = true;
                        }
                    }
                }

                if detected_now {
                    let is_selected = this.state.selected_session == Some(session_id);
                    let has_swarm_surface = this
                        .right_panel_surfaces
                        .contains(&RightPanelSurface::Swarm);
                    match this.swarm_panel.auto_surface.resolve_detected_swarm(
                        session_id,
                        is_selected,
                        auto_reveal,
                        this.right_panel_visible,
                        has_swarm_surface,
                    ) {
                        SwarmSurfaceAction::OpenPanel => {
                            this.open_right_panel_surface(RightPanelSurface::Swarm, cx);
                        }
                        SwarmSurfaceAction::AttachTab => {
                            this.right_panel_surfaces.push(RightPanelSurface::Swarm);
                            changed = true;
                        }
                        SwarmSurfaceAction::None => {}
                    }
                }

                if this.swarm_panel.stale.remove(&session_id) {
                    let retry_reveal = this.swarm_panel.auto_surface.has_pending_reveal(session_id);
                    this.request_antigravity_swarm_refresh(session_id, retry_reveal, cx);
                }

                if this.state.selected_session == Some(session_id) {
                    this.sync_antigravity_swarm_polling(cx);
                }
                if changed {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn open_swarm_screenshot_preview(
        &mut self,
        raw_path: String,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let image_base = self
            .selected_workspace_path()
            .map(|workspace| md::image_path::ImageBase::directory(&workspace.to_string_lossy()))
            .unwrap_or_default();
        let Some(image_path) = swarm_screenshot_image_path(&raw_path, &image_base) else {
            self.show_toast_with_tone(tr!("attachments.preview_unavailable"), ToastTone::Alert);
            return;
        };
        let Some(host) = self.hosts.get(&self.selected_host_key()) else {
            return;
        };
        let workspace = waku_client::WorkspaceClient::for_daemon(host.daemon.clone());
        let host_key = self.selected_host_key();
        let host_generation = host.generation;
        let session_id = self.state.selected_session;
        let workspace_path = self.selected_workspace_path().map(Path::to_path_buf);
        let request_identity = session_id.and_then(|id| self.swarm_request_identity(id));
        let previous_focus = window.focused(cx);
        self.image_preview_generation = self.image_preview_generation.wrapping_add(1);
        let preview_generation = self.image_preview_generation;
        let window_handle = window.window_handle();
        let display_name = if label.trim().is_empty() {
            screenshot_basename(&raw_path)
        } else {
            label
        };

        cx.spawn(async move |waku, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let (bytes, resolved_name) =
                        match read_swarm_host_file(&workspace, &image_path.path) {
                            Ok(bytes) => (bytes, image_path.path),
                            Err(error) => match image_path.fallback {
                                Some(fallback) => {
                                    (read_swarm_host_file(&workspace, &fallback)?, fallback)
                                }
                                None => return Err(error),
                            },
                        };
                    let format = super::image_preview::image_format_for_name(&resolved_name)
                        .or_else(|| sniff_swarm_image_format(&bytes))
                        .ok_or_else(|| anyhow::anyhow!("{resolved_name} is not an image"))?;
                    Ok::<Arc<gpui::Image>, anyhow::Error>(Arc::new(gpui::Image::from_bytes(
                        format, bytes,
                    )))
                })
                .await;

            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = waku.update(cx, |this, cx| {
                    if this.image_preview_generation != preview_generation
                        || this.state.selected_session != session_id
                        || this.selected_host_key() != host_key
                        || this.hosts.get(&host_key).map(|host| host.generation)
                            != Some(host_generation)
                        || this.selected_workspace_path() != workspace_path.as_deref()
                        || session_id.and_then(|id| this.swarm_request_identity(id))
                            != request_identity
                        || previous_focus
                            .as_ref()
                            .is_some_and(|focus| !focus.is_focused(window))
                        || this.any_overlay_open(cx)
                    {
                        return;
                    }
                    match loaded {
                        Ok(image) => {
                            this.open_image_preview(
                                image,
                                SharedString::from(display_name),
                                window,
                                cx,
                            );
                        }
                        Err(_) => {
                            this.show_toast_with_tone(
                                tr!("attachments.preview_unavailable"),
                                ToastTone::Alert,
                            );
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
    }

    pub(super) fn render_header_swarm_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = self.is_swarm_surface_visible();
        let snapshot = self.selected_antigravity_swarm_snapshot();
        let indicator_color = snapshot.as_ref().map(|snap| {
            let counts = swarm_member_counts(snap);
            if counts.failed > 0 {
                theme.danger
            } else if counts.working > 0 {
                theme.success
            } else {
                theme.accent
            }
        });
        let tooltip_label = match &snapshot {
            Some(snap) => {
                let counts = swarm_member_counts(snap);
                if counts.failed > 0 {
                    format!(
                        "{} · {} · {}",
                        tr!("right_panel.swarm"),
                        tr!("swarm.active_count", count = counts.working),
                        tr!("swarm.failed_count", count = counts.failed)
                    )
                } else if counts.stopped > 0 && counts.working == 0 && counts.waiting == 0 {
                    format!(
                        "{} · {}",
                        tr!("right_panel.swarm"),
                        tr!("swarm.stopped_count", count = counts.stopped)
                    )
                } else {
                    format!(
                        "{} · {} · {}",
                        tr!("right_panel.swarm"),
                        tr!("swarm.active_count", count = counts.working),
                        tr!("swarm.waiting_count", count = counts.waiting)
                    )
                }
            }
            None => tr!("right_panel.swarm"),
        };
        let focus = self.transcript_control_focus("header-swarm-button", cx);
        div()
            .id("header-swarm-button")
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .relative()
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.bg(theme.overlay).border_color(theme.accent))
            .when(active, |element| element.bg(theme.overlay_strong))
            .when(!active, |element| {
                element.hover(|element| element.bg(theme.overlay))
            })
            .active(|element| element.bg(theme.overlay_strong))
            .child(icon(
                RightPanelSurface::Swarm.icon_path(),
                14.0,
                if active {
                    theme.text
                } else {
                    theme.text_tertiary
                },
            ))
            .when_some(indicator_color, |element, color| {
                element.child(
                    div()
                        .id("header-swarm-active-dot")
                        .absolute()
                        .top(px(3.0))
                        .right(px(3.0))
                        .size(px(6.0))
                        .rounded_full()
                        .bg(color),
                )
            })
            .tooltip(Tooltip::text(tooltip_label))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_activation(cx, |this, _, cx| {
                if this.is_swarm_surface_visible() {
                    this.set_right_panel_visible(false, cx);
                } else {
                    this.open_antigravity_swarm_panel(cx);
                }
            })
    }

    pub(super) fn render_swarm_panel(
        &mut self,
        panel_width: f32,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(session) = self.selected_session() else {
            return self
                .render_right_panel_empty_message(
                    tr!("swarm.empty_non_antigravity"),
                    tr!("swarm.empty_non_antigravity_description"),
                    cx,
                )
                .into_any_element();
        };
        let session_id = session.id;
        let is_antigravity = session.provider == ProviderKind::Antigravity;
        let has_cursor = antigravity_session_id(session).is_some();

        let snapshot = self.selected_antigravity_swarm_snapshot();
        let current_identity = self.swarm_request_identity(session_id);
        let error = (current_identity.as_ref()
            == self.swarm_panel.request_identities.get(&session_id))
        .then(|| self.swarm_panel.errors.get(&session_id).cloned())
        .flatten();
        let is_refreshing = self.swarm_panel.pending.contains(&session_id);
        let loading = is_refreshing && snapshot.is_none();

        if !is_antigravity && snapshot.is_none() {
            return self
                .render_right_panel_empty_message(
                    tr!("swarm.empty_non_antigravity"),
                    tr!("swarm.empty_non_antigravity_description"),
                    cx,
                )
                .into_any_element();
        }

        let mut root = div()
            .id("swarm-panel")
            .key_context(SWARM_PANEL_CONTEXT)
            .tab_group()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified() && event.keystroke.key.as_str() == "escape"
                {
                    if this.swarm_panel.active_tab == SwarmPanelTab::Hive {
                        if this.swarm_panel.hive.selected_member.take().is_some() {
                            cx.notify();
                            cx.stop_propagation();
                            return;
                        }
                        if this.swarm_panel.hive.replay_cursor_seq.take().is_some()
                            || this.swarm_panel.hive.replay_playing
                        {
                            this.swarm_panel.hive.stop_replay();
                            cx.notify();
                            cx.stop_propagation();
                            return;
                        }
                    } else if this.swarm_panel.member_filter.take().is_some()
                        || this.swarm_panel.channel_filter.take().is_some()
                    {
                        cx.notify();
                        cx.stop_propagation();
                    }
                }
            }));

        if let Some(snapshot) = snapshot {
            self.swarm_panel.prune_dynamic_focuses(&snapshot);
            let stale_error_banner = error.map(|err| {
                let retry_focus = self.swarm_panel.retry_focus.clone();
                div()
                    .id("swarm-stale-error-banner")
                    .flex_none()
                    .px(px(12.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.warning.opacity(0.14))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(icon("icons/alert.svg", 12.0, theme.warning))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_size(sp(11.5))
                                    .line_height(sp(15.0))
                                    .text_color(theme.text)
                                    .whitespace_normal()
                                    .child(tr!("swarm.stale_error_banner", error = err)),
                            ),
                    )
                    .child(
                        div()
                            .id("swarm-stale-retry-button")
                            .track_focus(&retry_focus)
                            .tab_index(0)
                            .tab_stop(true)
                            .flex_none()
                            .h(px(22.0))
                            .px(px(8.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(theme.border_strong)
                            .bg(theme.raised)
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .cursor_default()
                            .focus_visible(|style| style.border_color(theme.accent))
                            .hover(|style| style.bg(theme.overlay_strong))
                            .text_size(sp(11.0))
                            .text_color(theme.text)
                            .when(is_refreshing, |btn| {
                                btn.child(motion::spin_slow(icon(
                                    "icons/loader-circle.svg",
                                    11.0,
                                    theme.accent,
                                )))
                            })
                            .child(if is_refreshing {
                                tr!("swarm.retrying")
                            } else {
                                tr!("swarm.retry")
                            })
                            .on_activation(cx, |this, _, cx| {
                                this.refresh_selected_antigravity_swarm(cx);
                            }),
                    )
            });

            root = root
                .child(self.render_swarm_header(&snapshot, is_refreshing, &theme, cx))
                .children(stale_error_banner)
                .child(self.render_swarm_tabs(&snapshot, &theme, cx))
                .child(match self.swarm_panel.active_tab {
                    SwarmPanelTab::Hive => {
                        self.render_swarm_hive_tab(session_id, &snapshot, panel_width, &theme, cx)
                    }
                    SwarmPanelTab::Board => {
                        self.render_swarm_board_tab(session_id, &snapshot, &theme, cx)
                    }
                    SwarmPanelTab::Members => {
                        self.render_swarm_members_tab(session_id, &snapshot, &theme, cx)
                    }
                    SwarmPanelTab::Overview => {
                        self.render_swarm_overview_tab(&snapshot, &theme, cx)
                    }
                });
            return root.into_any_element();
        }

        if let Some(error) = error {
            let retry_focus = self.swarm_panel.retry_focus.clone();
            return root
                .child(self.render_swarm_toolbar_empty(is_refreshing, &theme, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(10.0))
                        .px(px(20.0))
                        .pb(px(32.0))
                        .child(icon("icons/alert.svg", 18.0, theme.danger))
                        .child(
                            div()
                                .text_size(sp(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(tr!("swarm.error_title")),
                        )
                        .child(
                            div()
                                .max_w(px(300.0))
                                .text_center()
                                .text_size(sp(12.5))
                                .line_height(sp(17.0))
                                .text_color(theme.text_tertiary)
                                .whitespace_normal()
                                .child(error),
                        )
                        .child(
                            div()
                                .id("swarm-retry-button")
                                .track_focus(&retry_focus)
                                .tab_index(0)
                                .tab_stop(true)
                                .px(px(12.0))
                                .py(px(5.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(theme.border_strong)
                                .bg(theme.raised)
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .text_size(sp(12.5))
                                .text_color(theme.text)
                                .cursor_default()
                                .focus_visible(|style| style.border_color(theme.accent))
                                .hover(|style| style.bg(theme.overlay_strong))
                                .when(is_refreshing, |btn| {
                                    btn.child(motion::spin_slow(icon(
                                        "icons/loader-circle.svg",
                                        12.0,
                                        theme.accent,
                                    )))
                                })
                                .child(if is_refreshing {
                                    tr!("swarm.retrying")
                                } else {
                                    tr!("swarm.retry")
                                })
                                .on_activation(cx, |this, _, cx| {
                                    this.refresh_selected_antigravity_swarm(cx);
                                }),
                        ),
                )
                .into_any_element();
        }

        if loading && has_cursor {
            return root
                .child(self.render_swarm_toolbar_empty(true, &theme, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(8.0))
                        .px(px(24.0))
                        .pb(px(32.0))
                        .child(motion::spin_slow(icon(
                            "icons/loader-circle.svg",
                            20.0,
                            theme.accent,
                        )))
                        .child(
                            div()
                                .text_size(sp(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(tr!("swarm.loading_title")),
                        )
                        .child(
                            div()
                                .max_w(px(280.0))
                                .text_center()
                                .text_size(sp(12.5))
                                .line_height(sp(17.0))
                                .text_color(theme.text_tertiary)
                                .whitespace_normal()
                                .child(tr!("swarm.loading_description")),
                        ),
                )
                .into_any_element();
        }

        root.child(self.render_swarm_toolbar_empty(is_refreshing, &theme, cx))
            .child(self.render_right_panel_empty_message(
                tr!("swarm.empty_title"),
                tr!("swarm.empty_description"),
                cx,
            ))
            .into_any_element()
    }

    fn render_swarm_toolbar_empty(
        &self,
        is_refreshing: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let refresh_focus = self.swarm_panel.refresh_focus.clone();
        let refresh_btn = div()
            .id("swarm-empty-refresh")
            .track_focus(&refresh_focus)
            .tab_index(0)
            .tab_stop(true)
            .size(px(22.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|style| style.bg(theme.overlay))
            .active(|style| style.bg(theme.overlay_strong))
            .child(if is_refreshing {
                motion::spin_slow(icon("icons/loader-circle.svg", 13.0, theme.accent))
            } else {
                icon("icons/rotate-cw.svg", 13.0, theme.text_tertiary).into_any_element()
            })
            .tooltip(Tooltip::text(if is_refreshing {
                tr!("swarm.retrying")
            } else {
                tr!("swarm.refresh")
            }))
            .on_activation(cx, |this, _, cx| {
                this.refresh_selected_antigravity_swarm(cx);
            });

        div()
            .flex_none()
            .h(px(40.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(icon("icons/hexagon.svg", 14.0, theme.text_secondary))
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(tr!("swarm.title_default")),
                    ),
            )
            .child(refresh_btn)
    }

    fn render_swarm_header(
        &self,
        snapshot: &AntigravitySwarmSnapshot,
        is_refreshing: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let counts = swarm_member_counts(snapshot);
        let title = if snapshot.title.trim().is_empty() {
            tr!("swarm.title_default")
        } else {
            snapshot.title.clone()
        };
        let phase_label = snapshot
            .overview
            .as_ref()
            .and_then(|overview| overview.phase.as_ref())
            .map(|phase| match (phase.index, phase.total) {
                (Some(index), Some(total)) => format!("{} ({index}/{total})", phase.label),
                _ => phase.label.clone(),
            });

        let mut summary_parts = vec![
            tr!("swarm.active_count", count = counts.working),
            tr!("swarm.waiting_count", count = counts.waiting),
        ];
        if counts.stopped > 0 {
            summary_parts.push(tr!("swarm.stopped_count", count = counts.stopped));
        }
        if counts.failed > 0 {
            summary_parts.push(tr!("swarm.failed_count", count = counts.failed));
        }
        if snapshot.total_messages > 0 {
            summary_parts.push(tr!("swarm.posts_count", count = snapshot.total_messages));
        }
        if let Some(member) = snapshot.current_member.as_deref() {
            summary_parts.push(tr!("swarm.viewing_as", name = member));
        }
        if let Some(model) = shared_swarm_model(&snapshot.members) {
            summary_parts.push(model.to_owned());
        }

        // Proportional status strip; the summary line above carries the same
        // counts as text, so color is never the only signal.
        let status_bar = (counts.live() > 0).then(|| {
            let mut bar = div().w_full().h(px(3.0)).flex().gap(px(2.0));
            for (count, color) in [
                (counts.working, theme.success),
                (counts.waiting, theme.warning),
                (counts.stopped, theme.text_tertiary),
                (counts.failed, theme.danger),
            ] {
                if count == 0 {
                    continue;
                }
                let mut segment = div()
                    .h_full()
                    .flex_basis(px(0.0))
                    .min_w(px(4.0))
                    .rounded_full()
                    .bg(color.opacity(0.8));
                segment.style().flex_grow = Some(count as f32);
                bar = bar.child(segment);
            }
            bar
        });

        let following = self.swarm_panel.follow_tail.get();
        let follow_focus = self.swarm_panel.follow_focus.clone();
        let refresh_focus = self.swarm_panel.refresh_focus.clone();

        div()
            .flex_none()
            .px(px(12.0))
            .pt(px(9.0))
            .pb(px(9.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .min_w_0()
                                    .child(icon(
                                        "icons/hexagon.svg",
                                        13.0,
                                        if counts.failed > 0 {
                                            theme.danger
                                        } else if counts.working > 0 {
                                            theme.success
                                        } else {
                                            theme.text_secondary
                                        },
                                    ))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .truncate()
                                            .text_size(sp(13.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child(title),
                                    )
                                    .when_some(phase_label, |row, phase| {
                                        row.child(
                                            div()
                                                .flex_none()
                                                .max_w(px(140.0))
                                                .px(px(5.0))
                                                .py(px(1.0))
                                                .rounded(px(4.0))
                                                .bg(theme.overlay_strong)
                                                .truncate()
                                                .text_size(sp(10.5))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(theme.text_secondary)
                                                .child(phase),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(sp(11.5))
                                    .text_color(theme.text_tertiary)
                                    .truncate()
                                    .child(summary_parts.join(" · ")),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .when(self.swarm_panel.active_tab == SwarmPanelTab::Board, |row| {
                                row.child(
                                    div()
                                        .id("swarm-follow-toggle")
                                        .track_focus(&follow_focus)
                                        .tab_index(0)
                                        .tab_stop(true)
                                        .h(px(24.0))
                                        .px(px(8.0))
                                        .rounded(px(6.0))
                                        .border_1()
                                        .border_color(if following {
                                            theme.accent.opacity(0.6)
                                        } else {
                                            theme.border_strong
                                        })
                                        .bg(if following {
                                            theme.overlay_strong
                                        } else {
                                            theme.surface
                                        })
                                        .flex()
                                        .items_center()
                                        .gap(px(4.0))
                                        .cursor_default()
                                        .focus_visible(|style| style.border_color(theme.accent))
                                        .hover(|style| style.bg(theme.overlay))
                                        .child(icon(
                                            if following {
                                                "icons/check.svg"
                                            } else {
                                                "icons/arrow-down.svg"
                                            },
                                            11.0,
                                            if following {
                                                theme.accent
                                            } else {
                                                theme.text_tertiary
                                            },
                                        ))
                                        .child(
                                            div()
                                                .text_size(sp(11.5))
                                                .text_color(if following {
                                                    theme.text
                                                } else {
                                                    theme.text_secondary
                                                })
                                                .child(if following {
                                                    tr!("swarm.following")
                                                } else {
                                                    tr!("swarm.follow")
                                                }),
                                        )
                                        .on_activation(cx, |this, _, cx| {
                                            let next = !this.swarm_panel.follow_tail.get();
                                            this.swarm_panel.follow_tail.set(next);
                                            if next {
                                                let count =
                                                    this.swarm_panel.filtered_message_indices.len();
                                                if count > 0 {
                                                    this.swarm_panel
                                                        .board_list_state
                                                        .scroll_to_reveal_item(count - 1);
                                                }
                                            }
                                            cx.notify();
                                        }),
                                )
                            })
                            .child(
                                div()
                                    .id("swarm-refresh-button")
                                    .track_focus(&refresh_focus)
                                    .tab_index(0)
                                    .tab_stop(true)
                                    .size(px(22.0))
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_default()
                                    .focus_visible(|style| style.border_color(theme.accent))
                                    .hover(|style| style.bg(theme.overlay))
                                    .active(|style| style.bg(theme.overlay_strong))
                                    .child(if is_refreshing {
                                        motion::spin_slow(icon(
                                            "icons/loader-circle.svg",
                                            13.0,
                                            theme.accent,
                                        ))
                                    } else {
                                        icon("icons/rotate-cw.svg", 13.0, theme.text_tertiary)
                                            .into_any_element()
                                    })
                                    .tooltip(Tooltip::text(if is_refreshing {
                                        tr!("swarm.retrying")
                                    } else {
                                        tr!("swarm.refresh")
                                    }))
                                    .on_activation(cx, |this, _, cx| {
                                        this.refresh_selected_antigravity_swarm(cx);
                                    }),
                            ),
                    ),
            )
            .children(status_bar)
    }

    fn render_swarm_tabs(
        &self,
        snapshot: &AntigravitySwarmSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let counts = swarm_member_counts(snapshot);
        let hive_badge = if counts.failed > 0 {
            Some((counts.failed, true, Some("icons/alert.svg")))
        } else if counts.working > 0 {
            Some((counts.working, false, Some("icons/zap.svg")))
        } else {
            None
        };
        let member_count = counts.live();
        let overview_attention = snapshot.system_attention.len()
            + snapshot
                .overview
                .as_ref()
                .map_or(0, |overview| overview.attention.len());

        let tabs = [
            (
                SwarmPanelTab::Hive,
                "swarm-tab-hive",
                tr!("swarm.tab_hive"),
                hive_badge,
            ),
            (
                SwarmPanelTab::Board,
                "swarm-tab-board",
                tr!("swarm.tab_board"),
                Some((snapshot.total_messages as usize, false, None)),
            ),
            (
                SwarmPanelTab::Members,
                "swarm-tab-members",
                tr!("swarm.tab_members"),
                Some((member_count, false, None)),
            ),
            (
                SwarmPanelTab::Overview,
                "swarm-tab-overview",
                tr!("swarm.tab_overview"),
                (overview_attention > 0).then_some((
                    overview_attention,
                    true,
                    Some("icons/alert.svg"),
                )),
            ),
        ];

        let mut bar = div()
            .id("swarm-tabs")
            .flex_none()
            .h(px(34.0))
            .px(px(10.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .gap(px(4.0));

        for (tab, id, label, badge) in tabs {
            let active = self.swarm_panel.active_tab == tab;
            let focus = self.swarm_panel.tab_focus(tab).clone();
            bar = bar.child(
                div()
                    .id(id)
                    .track_focus(&focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .h(px(26.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_default()
                    .focus_visible(|style| style.border_color(theme.accent))
                    .when(active, |element| element.bg(theme.overlay_strong))
                    .when(!active, |element| {
                        element.hover(|style| style.bg(theme.overlay))
                    })
                    .child(
                        div()
                            .text_size(sp(12.0))
                            .font_weight(if active {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if active {
                                theme.text
                            } else {
                                theme.text_secondary
                            })
                            .child(label),
                    )
                    .when_some(badge, |element, (count, highlight_badge, badge_icon)| {
                        let color = if highlight_badge {
                            theme.danger
                        } else if badge_icon.is_some() {
                            theme.success
                        } else {
                            theme.text_tertiary
                        };
                        element.child(
                            div()
                                .px(px(5.0))
                                .py(px(1.0))
                                .rounded(px(4.0))
                                .bg(if highlight_badge {
                                    theme.danger_soft
                                } else {
                                    theme.overlay
                                })
                                .flex()
                                .items_center()
                                .gap(px(3.0))
                                .when_some(badge_icon, |badge_el, icon_path| {
                                    badge_el.child(icon(icon_path, 9.5, color))
                                })
                                .child(
                                    div()
                                        .text_size(sp(10.5))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(color)
                                        .child(count.to_string()),
                                ),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if tab != SwarmPanelTab::Hive {
                            this.swarm_panel.hive.stop_replay();
                        }
                        if tab != SwarmPanelTab::Board {
                            this.swarm_panel.clear_board_selection();
                        }
                        this.swarm_panel.active_tab = tab;
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.modifiers.modified() {
                            return;
                        }
                        let next = match event.keystroke.key.as_str() {
                            "enter" | "space" => tab,
                            "left" => tab.step(-1),
                            "right" => tab.step(1),
                            "home" => SwarmPanelTab::Hive,
                            "end" => SwarmPanelTab::Overview,
                            _ => return,
                        };
                        if next != SwarmPanelTab::Hive {
                            this.swarm_panel.hive.stop_replay();
                        }
                        if next != SwarmPanelTab::Board {
                            this.swarm_panel.clear_board_selection();
                        }
                        this.swarm_panel.active_tab = next;
                        if next != tab {
                            window.focus(this.swarm_panel.tab_focus(next), cx);
                        }
                        cx.notify();
                        cx.stop_propagation();
                    })),
            );
        }

        bar
    }

    fn toggle_swarm_message_expanded(&mut self, seq: u64, cx: &mut Context<Self>) {
        {
            let mut expanded = self.swarm_panel.expanded_messages.borrow_mut();
            if !expanded.remove(&seq) {
                expanded.insert(seq);
            }
        }
        if let Some(snapshot) = self.selected_antigravity_swarm_snapshot()
            && let Some(row) = self
                .swarm_panel
                .filtered_message_indices
                .iter()
                .position(|&index| snapshot.messages.get(index).is_some_and(|m| m.seq == seq))
        {
            self.swarm_panel.board_list_state.splice(row..row + 1, 1);
        }
        cx.notify();
    }

    /// Keeps the per-post markdown cache scoped to the current swarm and its
    /// retained window of posts.
    fn prune_swarm_message_caches(&mut self, snapshot: &AntigravitySwarmSnapshot) {
        let panel = &mut self.swarm_panel;
        if panel.message_markdown_swarm.as_deref() != Some(snapshot.swarm_id.as_str()) {
            panel.message_markdown.borrow_mut().clear();
            panel.expanded_messages.borrow_mut().clear();
            panel.message_toggle_focuses.borrow_mut().clear();
            panel.message_markdown_swarm = Some(snapshot.swarm_id.clone());
            return;
        }
        let live = snapshot
            .messages
            .iter()
            .map(|message| message.seq)
            .collect::<HashSet<_>>();
        panel
            .message_markdown
            .borrow_mut()
            .retain(|seq, _| live.contains(seq));
        panel
            .expanded_messages
            .borrow_mut()
            .retain(|seq| live.contains(seq));
        panel
            .message_toggle_focuses
            .borrow_mut()
            .retain(|seq, _| live.contains(seq));
    }

    fn render_swarm_board_tab(
        &mut self,
        session_id: Uuid,
        snapshot: &Arc<AntigravitySwarmSnapshot>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        // Clear stale filters if that member or channel no longer exists.
        if let Some(filter) = self.swarm_panel.member_filter.as_deref()
            && !snapshot.members.iter().any(|member| member.name == filter)
        {
            self.swarm_panel.member_filter = None;
        }
        if let Some(channel) = self.swarm_panel.channel_filter.as_deref()
            && !snapshot.channels.iter().any(|item| item == channel)
        {
            self.swarm_panel.channel_filter = None;
        }

        let board_fp = snapshot
            .board_fingerprint
            .as_deref()
            .unwrap_or(&snapshot.fingerprint)
            .to_owned();
        let list_key = (
            session_id,
            board_fp,
            self.swarm_panel.member_filter.clone(),
            self.swarm_panel.channel_filter.clone(),
        );
        if self.swarm_panel.board_list_key.as_ref() != Some(&list_key) {
            self.prune_swarm_message_caches(snapshot);
            let indices = filter_swarm_message_indices(
                &snapshot.messages,
                self.swarm_panel.member_filter.as_deref(),
                self.swarm_panel.channel_filter.as_deref(),
            );
            let next_seqs: Vec<u64> = indices
                .iter()
                .filter_map(|&idx| snapshot.messages.get(idx).map(|m| m.seq))
                .collect();
            let same_context = self.swarm_panel.board_list_key.as_ref().is_some_and(
                |(prev_session, _, prev_member, prev_channel)| {
                    *prev_session == session_id
                        && *prev_member == self.swarm_panel.member_filter
                        && *prev_channel == self.swarm_panel.channel_filter
                },
            );
            let following = self.swarm_panel.follow_tail.get();
            let keep_top = (!following && same_context)
                .then(|| self.swarm_panel.board_list_state.logical_scroll_top());
            let diff = compute_board_list_diff(
                same_context,
                &self.swarm_panel.board_message_seqs,
                &next_seqs,
                keep_top.as_ref().map(|top| top.item_ix),
            );
            self.swarm_panel.filtered_message_indices = Rc::new(indices);
            self.swarm_panel.board_list_key = Some(list_key);
            self.swarm_panel.board_message_seqs = next_seqs;

            match diff {
                SwarmBoardListDiff::Unchanged => {}
                SwarmBoardListDiff::TailAppend { old_len, delta } => {
                    self.swarm_panel
                        .board_list_state
                        .splice(old_len..old_len, delta);
                    if let Some(top) = keep_top {
                        self.swarm_panel.board_list_state.scroll_to(top);
                    } else if following && old_len + delta > 0 {
                        self.swarm_panel
                            .board_list_state
                            .scroll_to_reveal_item(old_len + delta - 1);
                    }
                }
                SwarmBoardListDiff::Reset {
                    new_len,
                    anchor_item_ix,
                } => {
                    self.swarm_panel.board_list_state.reset(new_len);
                    if let Some(top) = keep_top {
                        let restored = gpui::ListOffset {
                            item_ix: anchor_item_ix
                                .unwrap_or(top.item_ix.min(new_len.saturating_sub(1))),
                            offset_in_item: top.offset_in_item,
                        };
                        self.swarm_panel.board_list_state.scroll_to(restored);
                    } else if following && new_len > 0 {
                        self.swarm_panel
                            .board_list_state
                            .scroll_to_reveal_item(new_len - 1);
                    }
                }
            }
        }

        let filter_bar = self.render_swarm_member_filter_bar(snapshot, theme, cx);
        let channel_bar = (snapshot.channels.len() > 1)
            .then(|| self.render_swarm_channel_filter_bar(snapshot, theme, cx));
        let attention_strip = (!snapshot.system_attention.is_empty()).then(|| {
            render_swarm_system_attention_strip(
                &snapshot.system_attention,
                self.swarm_panel.more_attention_focus.clone(),
                theme,
                cx,
            )
        });
        let truncated_banner = snapshot.messages_truncated.then(|| {
            div()
                .flex_none()
                .px(px(12.0))
                .py(px(5.0))
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.inset)
                .text_size(sp(11.5))
                .text_color(theme.text_tertiary)
                .child(tr!(
                    "swarm.truncated_banner",
                    shown = snapshot.messages.len(),
                    total = snapshot.total_messages
                ))
        });

        let filtered_count = self.swarm_panel.filtered_message_indices.len();
        let following = self.swarm_panel.follow_tail.get();

        let feed = if filtered_count == 0 {
            let has_filter = self.swarm_panel.member_filter.is_some()
                || self.swarm_panel.channel_filter.is_some();
            let clear_focus = self.swarm_panel.clear_filter_focus.clone();
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .px(px(20.0))
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .text_center()
                        .whitespace_normal()
                        .child(if has_filter {
                            tr!("swarm.board_empty")
                        } else if snapshot.messages_truncated && snapshot.total_messages > 0 {
                            tr!("swarm.board_no_posts_truncated")
                        } else {
                            tr!("swarm.board_no_posts")
                        }),
                )
                .when(has_filter, |container| {
                    container.child(
                        div()
                            .id("swarm-empty-clear-filter")
                            .track_focus(&clear_focus)
                            .tab_index(0)
                            .tab_stop(true)
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border_strong)
                            .bg(theme.raised)
                            .text_size(sp(12.0))
                            .text_color(theme.text)
                            .cursor_default()
                            .focus_visible(|style| style.border_color(theme.accent))
                            .hover(|style| style.bg(theme.overlay_strong))
                            .child(tr!("swarm.clear_filter"))
                            .on_activation(cx, |this, _, cx| {
                                this.swarm_panel.member_filter = None;
                                this.swarm_panel.channel_filter = None;
                                cx.notify();
                            }),
                    )
                })
        } else {
            let snapshot_for_list = Arc::clone(snapshot);
            let indices_for_list = Rc::clone(&self.swarm_panel.filtered_message_indices);
            let theme_copy = *theme;
            let palette = MarkdownPalette::from_theme(theme);
            let metrics = self.scaled_markdown_metrics(SWARM_BOARD_METRICS);
            let selection = self.swarm_panel.board_selection.clone();
            let link_handler = self.markdown_link_handler.clone();
            let image_base = self
                .selected_workspace_path()
                .map(|workspace| md::image_path::ImageBase::directory(&workspace.to_string_lossy()))
                .unwrap_or_default();
            let image_loader = self.markdown_image_loader(&self.selected_host_key());
            let screenshot_focuses = self.swarm_panel.screenshot_focuses.clone();
            let markdown_cache = Rc::clone(&self.swarm_panel.message_markdown);
            let expanded = Rc::clone(&self.swarm_panel.expanded_messages);
            let toggle_focuses = Rc::clone(&self.swarm_panel.message_toggle_focuses);
            let active_channel_filter = self.swarm_panel.channel_filter.clone();
            let waku = cx.entity().downgrade();
            let jump_focus = self.swarm_panel.jump_latest_focus.clone();
            let selection_input = {
                let selection = self.swarm_panel.board_selection.clone();
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| md::render::install_selection_input(window, &selection),
                )
                .absolute()
                .w(px(0.0))
                .h(px(0.0))
            };

            div()
                .flex_1()
                .min_h_0()
                .relative()
                // Painted before the posts, so the frame's selection registry
                // holds exactly the text elements this frame put on screen.
                .child(md::render::frame_reset(
                    self.swarm_panel.board_selection.clone(),
                ))
                .child(
                    list(
                        self.swarm_panel.board_list_state.clone(),
                        move |row_index, _window, cx| {
                            let Some(&message_index) = indices_for_list.get(row_index) else {
                                return div().into_any_element();
                            };
                            let Some(message) = snapshot_for_list.messages.get(message_index)
                            else {
                                return div().into_any_element();
                            };
                            let seq = message.seq;
                            let is_expanded = expanded.borrow().contains(&seq);
                            let (body, collapsible) = {
                                let mut cache = markdown_cache.borrow_mut();
                                let entry = cache
                                    .entry(seq)
                                    .or_insert_with(|| SwarmMessageMarkdown::new(message));
                                let body = strip_leading_mentions(
                                    &message.text,
                                    &message.mentions,
                                    message.mentions_all,
                                );
                                if entry.source != body {
                                    *entry = SwarmMessageMarkdown::new(message);
                                }
                                let collapsible = entry.preview.is_some();
                                let (view, row_key) = match entry.preview.as_ref() {
                                    Some(preview) if !is_expanded => {
                                        (preview, format!("swarm-post-{seq}-preview"))
                                    }
                                    _ => (&entry.full, format!("swarm-post-{seq}")),
                                };
                                let mut ctx =
                                    MarkdownCtx::new(row_key, &palette, metrics, selection.clone())
                                        .with_math_enabled(false)
                                        .with_streaming_animation(false)
                                        .with_link_handler(link_handler.clone());
                                if let Some(loader) = image_loader.as_ref() {
                                    ctx = ctx.with_images(image_base.clone(), Rc::clone(loader));
                                }
                                let body = md::render::markdown(view, &ctx).unwrap_or_else(|| {
                                    md::render::plain_text(
                                        message.text.clone(),
                                        md::render::SANS_FAMILY,
                                        FontWeight::NORMAL,
                                        theme_copy.text,
                                        &ctx,
                                    )
                                });
                                (body, collapsible)
                            };
                            let toggle = collapsible.then(|| {
                                let focus = toggle_focuses
                                    .borrow_mut()
                                    .entry(seq)
                                    .or_insert_with(|| cx.focus_handle())
                                    .clone();
                                (is_expanded, focus)
                            });
                            let screenshots = (!message.screenshots.is_empty()).then(|| {
                                render_swarm_screenshot_strip(
                                    &format!("msg-{seq}"),
                                    &message.screenshots,
                                    &image_base,
                                    image_loader.as_ref(),
                                    &screenshot_focuses,
                                    &waku,
                                    &theme_copy,
                                    cx,
                                )
                            });
                            let mut channel_chips: Vec<(String, FocusHandle, bool)> = Vec::new();
                            if let Some(channel) = message.channel.as_deref() {
                                let key = format!("msg-{seq}-ch-{channel}");
                                let focus =
                                    channel_chip_focuses_from(&screenshot_focuses, &key, cx);
                                let active = active_channel_filter.as_deref() == Some(channel);
                                channel_chips.push((channel.to_owned(), focus, active));
                            }
                            for mentioned in &message.channel_mentions {
                                if message.channel.as_deref() == Some(mentioned.as_str()) {
                                    continue;
                                }
                                let key = format!("msg-{seq}-chm-{mentioned}");
                                let focus =
                                    channel_chip_focuses_from(&screenshot_focuses, &key, cx);
                                let active =
                                    active_channel_filter.as_deref() == Some(mentioned.as_str());
                                channel_chips.push((mentioned.clone(), focus, active));
                            }
                            render_swarm_message_row(
                                message,
                                snapshot_for_list.current_member.as_deref(),
                                body,
                                screenshots,
                                channel_chips,
                                toggle,
                                waku.clone(),
                                &theme_copy,
                            )
                            .into_any_element()
                        },
                    )
                    .size_full(),
                )
                .child(selection_input)
                .child(scrollbar::vertical(
                    &self.swarm_panel.board_list_state,
                    &self.swarm_panel.board_scrollbar,
                ))
                .when(!following, |container| {
                    container.child(
                        div().absolute().right(px(14.0)).bottom(px(12.0)).child(
                            div()
                                .id("swarm-jump-latest")
                                .track_focus(&jump_focus)
                                .tab_index(0)
                                .tab_stop(true)
                                .h(px(26.0))
                                .px(px(10.0))
                                .rounded(px(13.0))
                                .border_1()
                                .border_color(theme.border_strong)
                                .bg(theme.raised)
                                .shadow_md()
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .cursor_default()
                                .focus_visible(|style| style.border_color(theme.accent))
                                .hover(|style| style.bg(theme.overlay_strong))
                                .child(icon("icons/arrow-down.svg", 11.0, theme.text))
                                .child(
                                    div()
                                        .text_size(sp(11.5))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(tr!("swarm.jump_latest")),
                                )
                                .on_activation(cx, |this, _, cx| {
                                    this.swarm_panel.follow_tail.set(true);
                                    let count = this.swarm_panel.filtered_message_indices.len();
                                    if count > 0 {
                                        this.swarm_panel
                                            .board_list_state
                                            .scroll_to_reveal_item(count - 1);
                                    }
                                    cx.notify();
                                }),
                        ),
                    )
                })
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(filter_bar)
            .children(channel_bar)
            .children(attention_strip)
            .children(truncated_banner)
            .child(feed)
    }

    fn render_swarm_member_filter_bar(
        &self,
        snapshot: &AntigravitySwarmSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let active_filter = self.swarm_panel.member_filter.as_deref();
        let all_focus = self.swarm_panel.filter_chip_focus("__all__", cx);
        let mut chips = div()
            .id("swarm-member-filter-bar")
            .flex_none()
            .px(px(10.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .gap(px(5.0))
            .overflow_x_scroll();

        let all_selected = active_filter.is_none();
        chips = chips.child(
            div()
                .id("swarm-filter-all")
                .track_focus(&all_focus)
                .tab_index(0)
                .tab_stop(true)
                .h(px(22.0))
                .px(px(8.0))
                .flex_none()
                .rounded(px(11.0))
                .border_1()
                .border_color(if all_selected {
                    theme.accent.opacity(0.65)
                } else {
                    theme.border_strong
                })
                .bg(if all_selected {
                    theme.overlay_strong
                } else {
                    theme.surface
                })
                .flex()
                .items_center()
                .cursor_default()
                .focus_visible(|style| style.border_color(theme.accent))
                .hover(|style| style.bg(theme.overlay))
                .text_size(sp(11.5))
                .text_color(if all_selected {
                    theme.text
                } else {
                    theme.text_secondary
                })
                .child(tr!("swarm.all_members"))
                .on_activation(cx, |this, _, cx| {
                    this.swarm_panel.member_filter = None;
                    cx.notify();
                }),
        );

        for member in snapshot.members.iter().filter(|member| !member.killed) {
            let name = member.name.clone();
            let selected = active_filter == Some(name.as_str());
            let focus = self.swarm_panel.filter_chip_focus(&name, cx);
            let dot_color = agent_color(&name, theme);
            let toggle_name = name.clone();
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("swarm-filter-chip-{name}")))
                    .track_focus(&focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .h(px(22.0))
                    .px(px(8.0))
                    .flex_none()
                    .rounded(px(11.0))
                    .border_1()
                    .border_color(if selected {
                        theme.accent.opacity(0.75)
                    } else {
                        theme.border_strong
                    })
                    .bg(if selected {
                        theme.overlay_strong
                    } else {
                        theme.surface
                    })
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_default()
                    .focus_visible(|style| style.border_color(theme.accent))
                    .hover(|style| style.bg(theme.overlay))
                    .child(div().size(px(6.0)).flex_none().rounded_full().bg(dot_color))
                    .child(
                        div()
                            .text_size(sp(11.5))
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_secondary
                            })
                            .child(name),
                    )
                    .on_activation(cx, move |this, _, cx| {
                        if this.swarm_panel.member_filter.as_deref() == Some(toggle_name.as_str()) {
                            this.swarm_panel.member_filter = None;
                        } else {
                            this.swarm_panel.member_filter = Some(toggle_name.clone());
                        }
                        cx.notify();
                    })
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && event.keystroke.key.as_str() == "escape"
                            && this.swarm_panel.member_filter.is_some()
                        {
                            this.swarm_panel.member_filter = None;
                            cx.notify();
                            cx.stop_propagation();
                        }
                    })),
            );
        }

        chips
    }

    fn render_swarm_channel_filter_bar(
        &self,
        snapshot: &AntigravitySwarmSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let active_channel = self.swarm_panel.channel_filter.as_deref();
        let all_focus = self.swarm_panel.channel_chip_focus("__all_ch__", cx);
        let mut bar = div()
            .id("swarm-channel-filter-bar")
            .flex_none()
            .px(px(10.0))
            .py(px(5.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .gap(px(5.0))
            .overflow_x_scroll();

        let all_selected = active_channel.is_none();
        bar = bar.child(
            div()
                .id("swarm-channel-all")
                .track_focus(&all_focus)
                .tab_index(0)
                .tab_stop(true)
                .h(px(20.0))
                .px(px(7.0))
                .flex_none()
                .rounded(px(5.0))
                .border_1()
                .border_color(if all_selected {
                    theme.accent.opacity(0.65)
                } else {
                    gpui::transparent_black()
                })
                .bg(if all_selected {
                    theme.overlay_strong
                } else {
                    theme.overlay.opacity(0.6)
                })
                .flex()
                .items_center()
                .cursor_default()
                .focus_visible(|style| style.border_color(theme.accent))
                .hover(|style| style.bg(theme.overlay))
                .text_size(sp(11.0))
                .text_color(if all_selected {
                    theme.text
                } else {
                    theme.text_secondary
                })
                .child(tr!("swarm.all_channels"))
                .on_activation(cx, |this, _, cx| {
                    this.swarm_panel.channel_filter = None;
                    cx.notify();
                }),
        );

        for channel in &snapshot.channels {
            let selected = active_channel == Some(channel.as_str());
            let focus = self.swarm_panel.channel_chip_focus(channel, cx);
            let toggle_channel = channel.clone();
            bar = bar.child(
                div()
                    .id(SharedString::from(format!("swarm-channel-chip-{channel}")))
                    .track_focus(&focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .h(px(20.0))
                    .px(px(7.0))
                    .flex_none()
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(if selected {
                        theme.accent.opacity(0.75)
                    } else {
                        gpui::transparent_black()
                    })
                    .bg(if selected {
                        theme.overlay_strong
                    } else {
                        theme.overlay.opacity(0.6)
                    })
                    .flex()
                    .items_center()
                    .cursor_default()
                    .focus_visible(|style| style.border_color(theme.accent))
                    .hover(|style| style.bg(theme.overlay))
                    .text_size(sp(11.0))
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_secondary
                    })
                    .child(format!("#{channel}"))
                    .on_activation(cx, move |this, _, cx| {
                        if this.swarm_panel.channel_filter.as_deref()
                            == Some(toggle_channel.as_str())
                        {
                            this.swarm_panel.channel_filter = None;
                        } else {
                            this.swarm_panel.channel_filter = Some(toggle_channel.clone());
                        }
                        cx.notify();
                    })
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && event.keystroke.key.as_str() == "escape"
                            && this.swarm_panel.channel_filter.is_some()
                        {
                            this.swarm_panel.channel_filter = None;
                            cx.notify();
                            cx.stop_propagation();
                        }
                    })),
            );
        }

        bar
    }

    fn render_swarm_members_tab(
        &mut self,
        session_id: Uuid,
        snapshot: &Arc<AntigravitySwarmSnapshot>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let killed_count = snapshot
            .members
            .iter()
            .filter(|member| member.killed)
            .count();
        let show_killed = self.swarm_panel.show_killed_members;
        let roster_fp = snapshot
            .roster_fingerprint
            .as_deref()
            .unwrap_or(&snapshot.fingerprint)
            .to_owned();
        let members_key = (session_id, roster_fp, show_killed);
        if self.swarm_panel.members_list_key.as_ref() != Some(&members_key) {
            let same_mode = self.swarm_panel.members_list_key.as_ref().is_some_and(
                |(prev_session, _, prev_killed)| {
                    *prev_session == session_id && *prev_killed == show_killed
                },
            );
            let items = swarm_member_list_items(&snapshot.members, show_killed);
            let items_changed = !same_mode || *self.swarm_panel.member_list_items != items;
            self.swarm_panel.members_list_key = Some(members_key);
            if items_changed {
                let prev_top =
                    same_mode.then(|| self.swarm_panel.members_list_state.logical_scroll_top());
                let len = items.len();
                self.swarm_panel.member_list_items = Rc::new(items);
                self.swarm_panel.members_list_state.reset(len);
                if let Some(top) = prev_top
                    && len > 0
                {
                    self.swarm_panel
                        .members_list_state
                        .scroll_to(gpui::ListOffset {
                            item_ix: top.item_ix.min(len - 1),
                            offset_in_item: top.offset_in_item,
                        });
                }
            }
        }

        let toolbar = (killed_count > 0).then(|| {
            let focus = self.swarm_panel.show_killed_focus.clone();
            div()
                .flex_none()
                .px(px(12.0))
                .py(px(6.0))
                .border_b_1()
                .border_color(theme.border)
                .flex()
                .items_center()
                .justify_end()
                .child(
                    div()
                        .id("swarm-show-killed-toggle")
                        .track_focus(&focus)
                        .tab_index(0)
                        .tab_stop(true)
                        .h(px(24.0))
                        .px(px(8.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme.border_strong)
                        .bg(if show_killed {
                            theme.overlay_strong
                        } else {
                            theme.surface
                        })
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .cursor_default()
                        .focus_visible(|style| style.border_color(theme.accent))
                        .hover(|style| style.bg(theme.overlay))
                        .child(icon(
                            if show_killed {
                                "icons/eye.svg"
                            } else {
                                "icons/eye-off.svg"
                            },
                            11.0,
                            theme.text_secondary,
                        ))
                        .child(
                            div()
                                .text_size(sp(11.5))
                                .text_color(theme.text_secondary)
                                .child(if show_killed {
                                    tr!("swarm.hide_killed", count = killed_count)
                                } else {
                                    tr!("swarm.show_killed", count = killed_count)
                                }),
                        )
                        .on_activation(cx, |this, _, cx| {
                            this.swarm_panel.show_killed_members =
                                !this.swarm_panel.show_killed_members;
                            cx.notify();
                        }),
                )
        });

        let body = if self.swarm_panel.member_list_items.is_empty() {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(20.0))
                .text_size(sp(12.5))
                .text_color(theme.text_tertiary)
                .child(tr!("swarm.members_empty"))
        } else {
            let snapshot_for_list = Arc::clone(snapshot);
            let items_for_list = Rc::clone(&self.swarm_panel.member_list_items);
            let row_focuses = Rc::new(
                items_for_list
                    .iter()
                    .map(|item| match item {
                        SwarmMemberListItem::Member(index) => snapshot
                            .members
                            .get(*index)
                            .map(|member| self.swarm_panel.member_row_focus(&member.name, cx)),
                        SwarmMemberListItem::Group { .. } => None,
                    })
                    .collect::<Vec<_>>(),
            );
            let show_model = shared_swarm_model(&snapshot.members).is_none();
            let active_filter = self.swarm_panel.member_filter.clone();
            let waku = cx.entity().downgrade();
            let theme_copy = *theme;
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    list(
                        self.swarm_panel.members_list_state.clone(),
                        move |row_index, _window, _cx| match items_for_list.get(row_index) {
                            Some(SwarmMemberListItem::Group { status, count }) => {
                                render_swarm_member_group_header(*status, *count, &theme_copy)
                                    .into_any_element()
                            }
                            Some(SwarmMemberListItem::Member(member_index)) => {
                                let (Some(member), Some(Some(focus))) = (
                                    snapshot_for_list.members.get(*member_index),
                                    row_focuses.get(row_index).cloned(),
                                ) else {
                                    return div().into_any_element();
                                };
                                render_swarm_member_row(
                                    member,
                                    snapshot_for_list.current_member.as_deref()
                                        == Some(member.name.as_str()),
                                    active_filter.as_deref() == Some(member.name.as_str()),
                                    show_model,
                                    focus,
                                    waku.clone(),
                                    &theme_copy,
                                )
                                .into_any_element()
                            }
                            None => div().into_any_element(),
                        },
                    )
                    .size_full(),
                )
                .child(scrollbar::vertical(
                    &self.swarm_panel.members_list_state,
                    &self.swarm_panel.members_scrollbar,
                ))
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .children(toolbar)
            .child(body)
    }

    fn render_swarm_overview_tab(
        &self,
        snapshot: &AntigravitySwarmSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        if snapshot.overview.is_none() && snapshot.system_attention.is_empty() {
            return self.render_right_panel_empty_message(
                tr!("swarm.overview_empty_title"),
                tr!("swarm.overview_empty_description"),
                cx,
            );
        }

        let image_base = self
            .selected_workspace_path()
            .map(|workspace| md::image_path::ImageBase::directory(&workspace.to_string_lossy()))
            .unwrap_or_default();
        let image_loader = self.markdown_image_loader(&self.selected_host_key());
        let waku = cx.entity().downgrade();

        let mut content = div()
            .id("swarm-overview-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.swarm_panel.overview_scroll)
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(12.0));

        if !snapshot.system_attention.is_empty() {
            content = content.child(render_swarm_overview_attention(
                &tr!("swarm.section_attention"),
                &snapshot.system_attention,
                &self.swarm_panel.attention_member_focuses,
                &waku,
                theme,
                cx,
            ));
        }

        if let Some(overview) = snapshot.overview.as_ref() {
            content = content.child(render_swarm_overview_body(
                overview,
                &image_base,
                image_loader.as_ref(),
                &self.swarm_panel.screenshot_focuses,
                &self.swarm_panel.attention_member_focuses,
                &waku,
                theme,
                cx,
            ));
        }

        div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(content)
            .child(scrollbar::vertical(
                &self.swarm_panel.overview_scroll,
                &self.swarm_panel.overview_scrollbar,
            ))
    }
}

pub(super) fn member_status_color(status: AntigravitySwarmMemberStatus, theme: &Theme) -> Hsla {
    match status {
        AntigravitySwarmMemberStatus::Working => theme.success,
        AntigravitySwarmMemberStatus::Waiting => theme.warning,
        AntigravitySwarmMemberStatus::Stopped => theme.text_tertiary,
        AntigravitySwarmMemberStatus::Failed => theme.danger,
        AntigravitySwarmMemberStatus::Killed => theme.text_ghost,
    }
}

pub(super) fn member_status_icon(status: AntigravitySwarmMemberStatus) -> &'static str {
    match status {
        AntigravitySwarmMemberStatus::Working => "icons/zap.svg",
        AntigravitySwarmMemberStatus::Waiting => "icons/hourglass.svg",
        AntigravitySwarmMemberStatus::Stopped => "icons/stop.svg",
        AntigravitySwarmMemberStatus::Failed => "icons/alert.svg",
        AntigravitySwarmMemberStatus::Killed => "icons/block.svg",
    }
}

pub(super) fn member_status_label(status: AntigravitySwarmMemberStatus) -> String {
    match status {
        AntigravitySwarmMemberStatus::Working => tr!("swarm.status_working"),
        AntigravitySwarmMemberStatus::Waiting => tr!("swarm.status_waiting"),
        AntigravitySwarmMemberStatus::Stopped => tr!("swarm.status_stopped"),
        AntigravitySwarmMemberStatus::Failed => tr!("swarm.status_failed"),
        AntigravitySwarmMemberStatus::Killed => tr!("swarm.status_killed"),
    }
}

fn channel_chip_focuses_from(
    focuses: &RefCell<HashMap<String, FocusHandle>>,
    key: &str,
    cx: &mut App,
) -> FocusHandle {
    let mut map = focuses.borrow_mut();
    if let Some(handle) = map.get(key) {
        return handle.clone();
    }
    let handle = cx.focus_handle();
    map.insert(key.to_owned(), handle.clone());
    handle
}

fn render_swarm_system_attention_strip(
    items: &[AntigravitySwarmAttentionItem],
    more_focus: FocusHandle,
    theme: &Theme,
    cx: &mut Context<Waku>,
) -> Div {
    let mut strip = div()
        .flex_none()
        .px(px(12.0))
        .py(px(7.0))
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.danger_soft)
        .flex()
        .flex_col()
        .gap(px(4.0));
    for item in items.iter().take(3) {
        let color = if item.kind == "stuck" {
            theme.warning
        } else {
            theme.danger
        };
        strip = strip.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .min_w_0()
                .child(icon("icons/alert.svg", 12.0, color))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(sp(11.5))
                        .text_color(theme.text)
                        .child(item.title.clone()),
                ),
        );
    }
    if items.len() > 3 {
        let extra = items.len() - 3;
        strip = strip.child(
            div().flex().child(
                div()
                    .id("swarm-attention-more-badge")
                    .track_focus(&more_focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .px(px(6.0))
                    .py(px(1.5))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .bg(theme.overlay_strong)
                    .cursor_default()
                    .focus_visible(|style| style.border_color(theme.accent))
                    .hover(|style| style.bg(theme.raised))
                    .text_size(sp(11.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!("swarm.more_attention", count = extra))
                    .on_activation(cx, |this, window, cx| {
                        this.swarm_panel.clear_board_selection();
                        this.swarm_panel.active_tab = SwarmPanelTab::Overview;
                        window.focus(&this.swarm_panel.tab_overview_focus, cx);
                        cx.notify();
                    }),
            ),
        );
    }
    strip
}

/// Board screenshot fields are literal filesystem names, not Markdown URLs.
/// Escape URL metacharacters before reusing host-native path resolution.
fn swarm_screenshot_image_path(
    raw: &str,
    base: &md::image_path::ImageBase,
) -> Option<md::image_path::ImagePath> {
    let escaped = raw
        .replace('%', "%25")
        .replace('#', "%23")
        .replace('?', "%3F");
    match md::image_path::classify(&escaped, base) {
        md::image_path::ImageTarget::Path(path) => Some(path),
        _ => None,
    }
}

fn screenshot_basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
}

fn read_swarm_host_file(
    workspace: &waku_client::WorkspaceClient,
    path: &str,
) -> anyhow::Result<Vec<u8>> {
    let (root, name) = md::image_path::split_parent(path)
        .ok_or_else(|| anyhow::anyhow!("not a file path: {path}"))?;
    let mut bytes = Vec::new();
    loop {
        let response = workspace.request(waku_client::WorkspaceOperation::ReadFileChunk {
            root: std::path::PathBuf::from(root),
            relative_path: std::path::PathBuf::from(name),
            offset: bytes.len() as u64,
            length: MAX_SWARM_SCREENSHOT_BYTES,
        })?;
        let waku_client::WorkspaceResult::FileChunk {
            bytes: chunk,
            total_bytes,
        } = response
        else {
            anyhow::bail!("the daemon returned an invalid file response");
        };
        if total_bytes > MAX_SWARM_SCREENSHOT_BYTES {
            anyhow::bail!(
                "{path} is larger than {} MB",
                MAX_SWARM_SCREENSHOT_BYTES / 1024 / 1024
            );
        }
        let chunk = base64::engine::general_purpose::STANDARD.decode(chunk.trim())?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 >= total_bytes {
            break;
        }
    }
    Ok(bytes)
}

fn sniff_swarm_image_format(bytes: &[u8]) -> Option<gpui::ImageFormat> {
    if bytes.starts_with(b"\x89PNG") {
        Some(gpui::ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(gpui::ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF8") {
        Some(gpui::ImageFormat::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(gpui::ImageFormat::Webp)
    } else if bytes.starts_with(b"BM") {
        Some(gpui::ImageFormat::Bmp)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn render_swarm_screenshot_strip(
    scope: &str,
    screenshots: &[AntigravitySwarmScreenshot],
    image_base: &md::image_path::ImageBase,
    image_loader: Option<&md::render::ImageLoader>,
    focuses: &RefCell<HashMap<String, FocusHandle>>,
    waku: &gpui::WeakEntity<Waku>,
    theme: &Theme,
    cx: &mut App,
) -> Div {
    let mut strip = div().mt(px(4.0)).flex().flex_wrap().gap(px(8.0));
    for (index, shot) in screenshots.iter().enumerate() {
        let key = format!("swarm-shot-{scope}-{index}");
        let focus = channel_chip_focuses_from(focuses, &key, cx);
        let fallback_name = screenshot_basename(&shot.path);
        let display_caption = if shot.caption.trim().is_empty() {
            fallback_name.clone()
        } else {
            shot.caption.clone()
        };
        let meta = match (shot.agent.as_deref(), shot.seq) {
            (Some(agent), Some(seq)) => Some(format!("@{agent} · #{seq}")),
            (Some(agent), None) => Some(format!("@{agent}")),
            (None, Some(seq)) => Some(format!("#{seq}")),
            (None, None) => None,
        };
        let source = swarm_screenshot_image_path(&shot.path, image_base)
            .and_then(|path| image_loader.map(|load| load(&path)));
        let click_waku = waku.clone();
        let key_waku = waku.clone();
        let click_path = shot.path.clone();
        let key_path = shot.path.clone();
        let click_label = display_caption.clone();
        let key_label = display_caption.clone();
        let theme_copy = *theme;

        let thumb = if let Some(source) = source {
            let err_name = fallback_name.clone();
            gpui::img(source)
                .size_full()
                .object_fit(gpui::ObjectFit::Cover)
                .rounded_t(px(6.0))
                .with_fallback(move || {
                    div()
                        .size_full()
                        .px(px(8.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(4.0))
                        .child(icon(
                            "icons/file-types/image.svg",
                            16.0,
                            theme_copy.text_tertiary,
                        ))
                        .child(
                            div()
                                .max_w_full()
                                .truncate()
                                .text_size(sp(10.5))
                                .text_color(theme_copy.text_tertiary)
                                .child(err_name.clone()),
                        )
                        .into_any_element()
                })
                .into_any_element()
        } else {
            div()
                .size_full()
                .px(px(8.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .child(icon(
                    "icons/file-types/image.svg",
                    16.0,
                    theme.text_tertiary,
                ))
                .child(
                    div()
                        .max_w_full()
                        .truncate()
                        .text_size(sp(10.5))
                        .text_color(theme.text_tertiary)
                        .child(fallback_name),
                )
                .into_any_element()
        };

        strip = strip.child(
            div()
                .id(SharedString::from(key))
                .track_focus(&focus)
                .tab_index(0)
                .tab_stop(true)
                .w(px(148.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.inset)
                .overflow_hidden()
                .flex()
                .flex_col()
                .cursor_default()
                .focus_visible(|style| style.border_color(theme.accent))
                .hover(|style| style.bg(theme.raised))
                .tooltip(Tooltip::text(shot.path.clone()))
                .child(
                    div()
                        .w_full()
                        .h(px(84.0))
                        .rounded_t(px(6.0))
                        .overflow_hidden()
                        .bg(theme.surface)
                        .child(thumb),
                )
                .child(
                    div()
                        .px(px(7.0))
                        .py(px(5.0))
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(sp(11.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(display_caption),
                        )
                        .when_some(meta, |col, text| {
                            col.child(
                                div()
                                    .truncate()
                                    .text_size(sp(10.5))
                                    .text_color(theme.text_tertiary)
                                    .child(text),
                            )
                        }),
                )
                .on_click(move |_, window, cx| {
                    let _ = click_waku.update(cx, |this, cx| {
                        this.open_swarm_screenshot_preview(
                            click_path.clone(),
                            click_label.clone(),
                            window,
                            cx,
                        );
                    });
                    cx.stop_propagation();
                })
                .on_key_down(move |event: &KeyDownEvent, window, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        let _ = key_waku.update(cx, |this, cx| {
                            this.open_swarm_screenshot_preview(
                                key_path.clone(),
                                key_label.clone(),
                                window,
                                cx,
                            );
                        });
                        cx.stop_propagation();
                    }
                }),
        );
    }
    strip
}

fn swarm_avatar(name: &str, size: f32, theme: &Theme) -> Div {
    let color = agent_color(name, theme);
    let initial = name
        .chars()
        .find(|ch| ch.is_alphanumeric())
        .map(|ch| ch.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".to_owned());
    div()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .bg(color.opacity(0.18))
        .flex()
        .items_center()
        .justify_center()
        .text_size(sp(size * 0.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(initial)
}

pub(super) fn swarm_badge(label: String, color: Hsla, theme: &Theme) -> Div {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(theme.overlay_strong)
        .text_size(sp(10.5))
        .text_color(color)
        .child(label)
}

#[allow(clippy::too_many_arguments)]
fn render_swarm_message_row(
    message: &AntigravitySwarmMessage,
    current_member: Option<&str>,
    body: AnyElement,
    screenshots: Option<Div>,
    channel_chips: Vec<(String, FocusHandle, bool)>,
    toggle: Option<(bool, FocusHandle)>,
    waku: gpui::WeakEntity<Waku>,
    theme: &Theme,
) -> Div {
    let mentions_me =
        current_member.is_some_and(|me| message.mentions.iter().any(|target| target == me));
    let is_from_me = current_member == Some(message.agent.as_str());
    let mut recipients = message
        .mentions
        .iter()
        .map(|name| format!("@{name}"))
        .collect::<Vec<_>>();
    if message.mentions_all {
        recipients.push("@all".to_owned());
    }
    let recipients_label = (!recipients.is_empty()).then(|| format!("→ {}", recipients.join(" ")));
    let time_label = if message.time.is_empty() {
        format!("#{}", message.seq)
    } else {
        format!("#{} · {}", message.seq, short_board_time(&message.time))
    };
    let seq = message.seq;

    let toggle_button = toggle.map(|(expanded, focus)| {
        let click_waku = waku.clone();
        let key_waku = waku.clone();
        div()
            .id(SharedString::from(format!("swarm-post-toggle-{seq}")))
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .h(px(22.0))
            .px(px(6.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_default()
            .text_size(sp(11.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_secondary)
            .focus_visible(|style| {
                style
                    .bg(theme.overlay_strong)
                    .border_color(theme.accent)
                    .text_color(theme.text)
            })
            .hover(|style| style.bg(theme.overlay).text_color(theme.text))
            .child(icon(
                if expanded {
                    "icons/chevron-up.svg"
                } else {
                    "icons/chevron-down.svg"
                },
                11.0,
                theme.text_tertiary,
            ))
            .child(if expanded {
                tr!("swarm.show_less")
            } else {
                tr!("swarm.show_more")
            })
            .on_click(move |_, _, cx| {
                let _ = click_waku.update(cx, |this, cx| {
                    this.toggle_swarm_message_expanded(seq, cx);
                });
                cx.stop_propagation();
            })
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    let _ = key_waku.update(cx, |this, cx| {
                        this.toggle_swarm_message_expanded(seq, cx);
                    });
                    cx.stop_propagation();
                }
            })
    });

    let mut header = div()
        .h(px(20.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .min_w_0()
        .child(
            div()
                .flex_none()
                .text_size(sp(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(message.agent.clone()),
        )
        .when(is_from_me, |row| {
            row.child(swarm_badge(tr!("swarm.you"), theme.accent, theme))
        })
        .when(message.kind.as_deref() == Some("brief"), |row| {
            row.child(swarm_badge(
                tr!("swarm.brief_badge"),
                theme.text_secondary,
                theme,
            ))
        });

    for (idx, (channel, focus, active)) in channel_chips.into_iter().enumerate() {
        let click_waku = waku.clone();
        let key_waku = waku.clone();
        let ch_click = channel.clone();
        let ch_key = channel.clone();
        header = header.child(
            div()
                .id(SharedString::from(format!(
                    "swarm-msg-{seq}-ch-{idx}-{channel}"
                )))
                .track_focus(&focus)
                .tab_index(0)
                .tab_stop(true)
                .flex_none()
                .px(px(5.0))
                .py(px(1.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(if active {
                    theme.accent.opacity(0.65)
                } else {
                    gpui::transparent_black()
                })
                .bg(theme.overlay_strong)
                .cursor_default()
                .focus_visible(|style| style.border_color(theme.accent))
                .hover(|style| style.bg(theme.raised))
                .text_size(sp(10.5))
                .text_color(if active {
                    theme.accent
                } else {
                    theme.text_tertiary
                })
                .child(format!("#{channel}"))
                .on_click(move |_, _, cx| {
                    let _ = click_waku.update(cx, |this, cx| {
                        if this.swarm_panel.channel_filter.as_deref() == Some(ch_click.as_str()) {
                            this.swarm_panel.channel_filter = None;
                        } else {
                            this.swarm_panel.channel_filter = Some(ch_click.clone());
                        }
                        cx.notify();
                    });
                    cx.stop_propagation();
                })
                .on_key_down(move |event: &KeyDownEvent, _, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        let _ = key_waku.update(cx, |this, cx| {
                            if this.swarm_panel.channel_filter.as_deref() == Some(ch_key.as_str()) {
                                this.swarm_panel.channel_filter = None;
                            } else {
                                this.swarm_panel.channel_filter = Some(ch_key.clone());
                            }
                            cx.notify();
                        });
                        cx.stop_propagation();
                    }
                }),
        );
    }

    header = header
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(sp(11.5))
                .text_color(if mentions_me {
                    theme.accent
                } else {
                    theme.text_tertiary
                })
                .children(recipients_label),
        )
        .child(
            div()
                .flex_none()
                .text_size(sp(11.0))
                .text_color(theme.text_tertiary)
                .child(time_label),
        );

    div()
        .relative()
        .px(px(12.0))
        .py(px(9.0))
        .border_b_1()
        .border_color(theme.border)
        .when(mentions_me, |row| {
            row.bg(theme.overlay).child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(8.0))
                    .bottom(px(8.0))
                    .w(px(2.0))
                    .rounded_full()
                    .bg(theme.accent),
            )
        })
        .flex()
        .items_start()
        .gap(px(9.0))
        .child(swarm_avatar(&message.agent, 22.0, theme))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(header)
                .child(div().min_w_0().text_color(theme.text).child(body))
                .children(screenshots)
                .children(toggle_button.map(|button| div().flex().child(button))),
        )
}

fn render_swarm_member_group_header(
    status: AntigravitySwarmMemberStatus,
    count: usize,
    theme: &Theme,
) -> Div {
    div()
        .px(px(16.0))
        .pt(px(12.0))
        .pb(px(4.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(icon(
            member_status_icon(status),
            11.0,
            member_status_color(status, theme),
        ))
        .child(
            div()
                .text_size(sp(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .child(member_status_label(status)),
        )
        .child(
            div()
                .text_size(sp(11.0))
                .text_color(theme.text_tertiary)
                .child(count.to_string()),
        )
}

fn render_swarm_member_row(
    member: &AntigravitySwarmMember,
    is_you: bool,
    is_filtered: bool,
    show_model: bool,
    focus: FocusHandle,
    waku: gpui::WeakEntity<Waku>,
    theme: &Theme,
) -> Div {
    let member_name_click = member.name.clone();
    let member_name_key = member.name.clone();
    let click_waku = waku.clone();
    let key_waku = waku;
    let show_role =
        !member.role.is_empty() && member.role != "worker" && member.role != member.name;

    let mut meta_parts = vec![
        tr!("swarm.steps_count", count = member.steps),
        tr!("swarm.posts_count", count = member.posts),
    ];
    if member.unread > 0 {
        meta_parts.push(tr!("swarm.unread_count", count = member.unread));
    }
    if member_display_status(member) == AntigravitySwarmMemberStatus::Waiting {
        if member.wake_mode == "mentions" {
            meta_parts.push(tr!("swarm.wake_mentions"));
        } else if member.wake_mode == "anything" {
            meta_parts.push(tr!("swarm.wake_anything"));
        }
    }
    if show_model && !member.model.is_empty() {
        meta_parts.push(member.model.clone());
    }

    div().px(px(8.0)).py(px(1.0)).child(
        div()
            .id(SharedString::from(format!(
                "swarm-member-row-{}",
                member.name
            )))
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .px(px(8.0))
            .py(px(7.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .when(is_filtered, |row| row.bg(theme.overlay_strong))
            .cursor_default()
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|style| style.bg(theme.overlay))
            .on_click(move |_, window, cx| {
                let _ = click_waku.update(cx, |this, cx| {
                    this.swarm_panel.member_filter = Some(member_name_click.clone());
                    this.swarm_panel.clear_board_selection();
                    this.swarm_panel.active_tab = SwarmPanelTab::Board;
                    this.swarm_panel.follow_tail.set(true);
                    window.focus(&this.swarm_panel.tab_board_focus, cx);
                    cx.notify();
                });
                cx.stop_propagation();
            })
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    let _ = key_waku.update(cx, |this, cx| {
                        this.swarm_panel.member_filter = Some(member_name_key.clone());
                        this.swarm_panel.clear_board_selection();
                        this.swarm_panel.active_tab = SwarmPanelTab::Board;
                        this.swarm_panel.follow_tail.set(true);
                        window.focus(&this.swarm_panel.tab_board_focus, cx);
                        cx.notify();
                    });
                    cx.stop_propagation();
                }
            })
            .when(member.killed, |row| row.opacity(0.6))
            .flex()
            .items_start()
            .gap(px(9.0))
            .child(swarm_avatar(&member.name, 22.0, theme))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .h(px(20.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .min_w_0()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(member.name.clone()),
                            )
                            .when(is_you, |row| {
                                row.child(swarm_badge(tr!("swarm.you"), theme.accent, theme))
                            })
                            .when(show_role, |row| {
                                row.child(swarm_badge(
                                    localized_swarm_role(&member.role),
                                    theme.text_tertiary,
                                    theme,
                                ))
                            })
                            .when(member.has_posted_done && !member.killed, |row| {
                                row.child(swarm_badge(
                                    tr!("swarm.done_badge"),
                                    theme.success,
                                    theme,
                                ))
                            }),
                    )
                    .when_some(
                        member.task.as_ref().filter(|t| !t.trim().is_empty()),
                        |column, task| {
                            column.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(sp(11.5))
                                    .text_color(theme.text_secondary)
                                    .child(task.clone()),
                            )
                        },
                    )
                    .when_some(member.activity.as_ref(), |column, activity| {
                        let activity_icon = if member.activity_kind.as_deref() == Some("thinking") {
                            "icons/sparkle.svg"
                        } else {
                            "icons/wrench.svg"
                        };
                        column.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .min_w_0()
                                .child(icon(activity_icon, 11.0, theme.text_tertiary))
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .truncate()
                                        .text_size(sp(12.0))
                                        .text_color(theme.text_secondary)
                                        .child(localized_member_activity(activity)),
                                ),
                        )
                    })
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(sp(11.0))
                            .text_color(theme.text_tertiary)
                            .child(meta_parts.join(" · ")),
                    )
                    .when_some(member.failure.as_ref(), |column, failure| {
                        let badge = match failure.kind {
                            AntigravitySwarmFailureKind::Quota => tr!("swarm.failure_quota"),
                            AntigravitySwarmFailureKind::Error => tr!("swarm.failure_error"),
                        };
                        column.child(
                            div()
                                .mt(px(3.0))
                                .px(px(7.0))
                                .py(px(5.0))
                                .rounded(px(6.0))
                                .bg(theme.danger_soft)
                                .flex()
                                .items_start()
                                .gap(px(6.0))
                                .min_w_0()
                                .child(icon("icons/alert.svg", 12.0, theme.danger))
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .whitespace_normal()
                                        .text_size(sp(11.5))
                                        .line_height(sp(15.5))
                                        .text_color(theme.text)
                                        .child(format!("{badge}: {}", failure.message)),
                                ),
                        )
                    }),
            ),
    )
}

fn render_swarm_member_jump_pill(
    scope: &str,
    member_name: String,
    focuses: &RefCell<HashMap<String, FocusHandle>>,
    waku: &gpui::WeakEntity<Waku>,
    theme: &Theme,
    cx: &mut App,
) -> Stateful<Div> {
    let key = format!("swarm-jump-{scope}-{member_name}");
    let focus = channel_chip_focuses_from(focuses, &key, cx);
    let click_waku = waku.clone();
    let key_waku = waku.clone();
    let click_name = member_name.clone();
    let key_name = member_name.clone();
    div()
        .id(SharedString::from(key))
        .track_focus(&focus)
        .tab_index(0)
        .tab_stop(true)
        .px(px(6.0))
        .py(px(1.5))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::transparent_black())
        .bg(theme.overlay)
        .cursor_default()
        .focus_visible(|style| style.border_color(theme.accent))
        .hover(|style| style.bg(theme.overlay_strong))
        .text_size(sp(11.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(agent_color(&member_name, theme))
        .child(format!("@{member_name}"))
        .on_click(move |_, window, cx| {
            let _ = click_waku.update(cx, |this, cx| {
                this.swarm_panel.member_filter = Some(click_name.clone());
                this.swarm_panel.clear_board_selection();
                this.swarm_panel.active_tab = SwarmPanelTab::Board;
                this.swarm_panel.follow_tail.set(true);
                window.focus(&this.swarm_panel.tab_board_focus, cx);
                cx.notify();
            });
            cx.stop_propagation();
        })
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                let _ = key_waku.update(cx, |this, cx| {
                    this.swarm_panel.member_filter = Some(key_name.clone());
                    this.swarm_panel.clear_board_selection();
                    this.swarm_panel.active_tab = SwarmPanelTab::Board;
                    this.swarm_panel.follow_tail.set(true);
                    window.focus(&this.swarm_panel.tab_board_focus, cx);
                    cx.notify();
                });
                cx.stop_propagation();
            }
        })
}

fn render_swarm_overview_attention(
    heading: &str,
    items: &[AntigravitySwarmAttentionItem],
    member_focuses: &RefCell<HashMap<String, FocusHandle>>,
    waku: &gpui::WeakEntity<Waku>,
    theme: &Theme,
    cx: &mut App,
) -> Div {
    let mut section = div().flex().flex_col().gap(px(6.0)).child(
        div()
            .text_size(sp(11.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text_secondary)
            .child(heading.to_owned()),
    );
    for (idx, item) in items.iter().enumerate() {
        let color = match item.kind.as_str() {
            "failure" | "quota" | "error" => theme.danger,
            "decision" | "review" | "stuck" => theme.warning,
            _ => theme.text_secondary,
        };
        let member_pill = item.member.as_ref().map(|member_name| {
            render_swarm_member_jump_pill(
                &format!("att-{idx}-{}", item.id),
                member_name.clone(),
                member_focuses,
                waku,
                theme,
                cx,
            )
        });
        section = section.child(
            div()
                .px(px(10.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(color.opacity(0.45))
                .bg(theme.composer)
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(icon("icons/alert.svg", 12.0, color))
                        .child(
                            div()
                                .px(px(5.0))
                                .py(px(1.0))
                                .rounded(px(4.0))
                                .bg(theme.overlay)
                                .text_size(sp(10.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(color)
                                .child(localized_attention_kind(&item.kind)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .whitespace_normal()
                                .child(item.title.clone()),
                        ),
                )
                .when(!item.why.is_empty(), |card| {
                    card.child(
                        div()
                            .text_size(sp(12.0))
                            .line_height(sp(16.5))
                            .text_color(theme.text_secondary)
                            .whitespace_normal()
                            .child(item.why.clone()),
                    )
                })
                .when_some(member_pill, |card, pill| {
                    card.child(div().flex().flex_wrap().gap(px(4.0)).child(pill))
                }),
        );
    }
    section
}

#[allow(clippy::too_many_arguments)]
fn render_swarm_overview_body(
    overview: &AntigravitySwarmOverview,
    image_base: &md::image_path::ImageBase,
    image_loader: Option<&md::render::ImageLoader>,
    screenshot_focuses: &RefCell<HashMap<String, FocusHandle>>,
    member_focuses: &RefCell<HashMap<String, FocusHandle>>,
    waku: &gpui::WeakEntity<Waku>,
    theme: &Theme,
    cx: &mut App,
) -> Div {
    let mut column = div().flex().flex_col().gap(px(12.0));
    let is_plan_source = overview.source.as_deref() == Some("plan");

    let phase_label = overview
        .phase
        .as_ref()
        .map(|phase| match (phase.index, phase.total) {
            (Some(index), Some(total)) => format!("{} ({index}/{total})", phase.label),
            _ => phase.label.clone(),
        });
    let meta_label = overview_meta_label(overview.as_of_seq, overview.updated_at.as_deref());
    let has_goal_line = overview
        .goal_line
        .as_ref()
        .is_some_and(|line| !line.trim().is_empty());
    if !overview.headline.is_empty()
        || has_goal_line
        || is_plan_source
        || phase_label.is_some()
        || meta_label.is_some()
    {
        column = column.child(
            div()
                .px(px(10.0))
                .py(px(9.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.composer)
                .flex()
                .flex_col()
                .gap(px(5.0))
                .when(!overview.headline.is_empty(), |card| {
                    card.child(
                        div()
                            .text_size(sp(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .whitespace_normal()
                            .child(overview.headline.clone()),
                    )
                })
                .when_some(
                    overview
                        .goal_line
                        .as_ref()
                        .filter(|line| !line.trim().is_empty()),
                    |card, line| {
                        card.child(
                            div()
                                .text_size(sp(12.0))
                                .line_height(sp(16.5))
                                .text_color(theme.text_secondary)
                                .whitespace_normal()
                                .child(line.clone()),
                        )
                    },
                )
                .when(
                    is_plan_source || phase_label.is_some() || meta_label.is_some(),
                    |card| {
                        card.child(
                            div()
                                .flex()
                                .items_center()
                                .flex_wrap()
                                .gap(px(8.0))
                                .when(is_plan_source, |row| {
                                    row.child(swarm_badge(
                                        tr!("swarm.plan_overview"),
                                        theme.accent,
                                        theme,
                                    ))
                                })
                                .when_some(phase_label, |row, label| {
                                    row.child(
                                        div()
                                            .px(px(6.0))
                                            .py(px(1.0))
                                            .rounded(px(4.0))
                                            .bg(theme.overlay)
                                            .text_size(sp(11.0))
                                            .text_color(theme.text_secondary)
                                            .child(label),
                                    )
                                })
                                .when_some(meta_label, |row, label| {
                                    row.child(
                                        div()
                                            .text_size(sp(11.0))
                                            .text_color(theme.text_tertiary)
                                            .child(label),
                                    )
                                }),
                        )
                    },
                ),
        );
    }

    if !overview.attention.is_empty() {
        column = column.child(render_swarm_overview_attention(
            &tr!("swarm.section_attention"),
            &overview.attention,
            member_focuses,
            waku,
            theme,
            cx,
        ));
    }

    if !overview.workstreams.is_empty() {
        let heading = if is_plan_source {
            tr!("swarm.section_threads")
        } else {
            tr!("swarm.section_workstreams")
        };
        let mut ws_section = div().flex().flex_col().gap(px(6.0)).child(
            div()
                .text_size(sp(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .child(heading),
        );
        for (ws_idx, ws) in overview.workstreams.iter().enumerate() {
            let state_color = match ws.state.as_str() {
                "active" => theme.success,
                "blocked" | "failed" => theme.danger,
                "review" => theme.warning,
                "done" => theme.accent,
                _ => theme.text_tertiary,
            };
            ws_section = ws_section.child(
                div()
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.composer)
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_size(sp(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .whitespace_normal()
                                    .child(ws.title.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .px(px(6.0))
                                    .py(px(1.0))
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(state_color.opacity(0.5))
                                    .text_size(sp(10.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(state_color)
                                    .child(localized_workstream_state(&ws.state)),
                            ),
                    )
                    .when(!ws.owners.is_empty(), |card| {
                        let mut owner_row = div().flex().items_center().flex_wrap().gap(px(4.0));
                        for owner in &ws.owners {
                            owner_row = owner_row.child(render_swarm_member_jump_pill(
                                &format!("ws-{ws_idx}-{}", ws.id),
                                owner.clone(),
                                member_focuses,
                                waku,
                                theme,
                                cx,
                            ));
                        }
                        card.child(owner_row)
                    })
                    .when(!ws.note.is_empty(), |card| {
                        card.child(
                            div()
                                .text_size(sp(12.0))
                                .line_height(sp(16.0))
                                .text_color(theme.text_secondary)
                                .whitespace_normal()
                                .child(ws.note.clone()),
                        )
                    }),
            );
        }
        column = column.child(ws_section);
    }

    if !overview.screenshots.is_empty() {
        column = column.child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(sp(11.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_secondary)
                        .child(tr!("swarm.section_screenshots")),
                )
                .child(render_swarm_screenshot_strip(
                    "overview",
                    &overview.screenshots,
                    image_base,
                    image_loader,
                    screenshot_focuses,
                    waku,
                    theme,
                    cx,
                )),
        );
    }

    if !overview.milestones.is_empty() {
        let mut section = div().flex().flex_col().gap(px(4.0)).child(
            div()
                .text_size(sp(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .child(tr!("swarm.section_milestones")),
        );
        for item in &overview.milestones {
            section = section.child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(6.0))
                    .child(icon("icons/check.svg", 12.0, theme.success))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(sp(12.0))
                            .text_color(theme.text)
                            .whitespace_normal()
                            .child(item.text.clone()),
                    ),
            );
        }
        column = column.child(section);
    }

    if !overview.risks.is_empty() {
        let mut section = div().flex().flex_col().gap(px(4.0)).child(
            div()
                .text_size(sp(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .child(tr!("swarm.section_risks")),
        );
        for item in &overview.risks {
            section = section.child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(6.0))
                    .child(icon("icons/alert.svg", 12.0, theme.warning))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(sp(12.0))
                            .text_color(theme.text)
                            .whitespace_normal()
                            .child(item.text.clone()),
                    ),
            );
        }
        column = column.child(section);
    }

    if let Some(ack) = overview.feedback_ack.as_ref() {
        column = column.child(
            div()
                .px(px(9.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .bg(theme.overlay)
                .text_size(sp(11.5))
                .text_color(theme.text_tertiary)
                .whitespace_normal()
                .child(ack.clone()),
        );
    }

    column
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_board_messages_by_sender_mention_and_channel() {
        let mut msg1 = AntigravitySwarmMessage {
            seq: 1,
            agent: "coordinator".to_owned(),
            text: "@alpha start".to_owned(),
            time: "2026-11-23 15:00:00".to_owned(),
            mentions: vec!["alpha".to_owned()],
            mentions_all: false,
            kind: None,
            channel: None,
            channel_mentions: vec![],
            screenshots: vec![],
        };
        let msg2 = AntigravitySwarmMessage {
            seq: 2,
            agent: "beta".to_owned(),
            text: "@coordinator status #design".to_owned(),
            time: "2026-11-23 15:01:00".to_owned(),
            mentions: vec!["coordinator".to_owned()],
            mentions_all: false,
            kind: None,
            channel: None,
            channel_mentions: vec!["design".to_owned()],
            screenshots: vec![],
        };
        let msg3 = AntigravitySwarmMessage {
            seq: 3,
            agent: "coordinator".to_owned(),
            text: "@all sync".to_owned(),
            time: "2026-11-23 15:02:00".to_owned(),
            mentions: vec![],
            mentions_all: true,
            kind: None,
            channel: Some("design".to_owned()),
            channel_mentions: vec![],
            screenshots: vec![],
        };
        msg1.channel = None;
        let messages = vec![msg1, msg2, msg3];

        assert_eq!(
            filter_swarm_message_indices(&messages, None, None),
            vec![0, 1, 2]
        );
        assert_eq!(
            filter_swarm_message_indices(&messages, Some("alpha"), None),
            vec![0, 2]
        );
        assert_eq!(
            filter_swarm_message_indices(&messages, Some("beta"), None),
            vec![1, 2]
        );
        assert_eq!(
            filter_swarm_message_indices(&messages, None, Some("main")),
            vec![0, 1]
        );
        assert_eq!(
            filter_swarm_message_indices(&messages, None, Some("design")),
            vec![1, 2]
        );
        assert_eq!(
            filter_swarm_message_indices(&messages, Some("beta"), Some("design")),
            vec![1, 2]
        );
    }

    #[test]
    fn board_list_diff_avoids_reset_when_unchanged_and_preserves_anchor_on_slide() {
        assert_eq!(
            compute_board_list_diff(true, &[10, 11, 12], &[10, 11, 12], Some(1)),
            SwarmBoardListDiff::Unchanged
        );
        assert_eq!(
            compute_board_list_diff(true, &[10, 11, 12], &[10, 11, 12, 13, 14], Some(1)),
            SwarmBoardListDiff::TailAppend {
                old_len: 3,
                delta: 2,
            }
        );
        // Truncated sliding window shifted off seq 10 & 11; item at index 2 (seq 12) shifts to index 0.
        assert_eq!(
            compute_board_list_diff(true, &[10, 11, 12, 13], &[12, 13, 14, 15], Some(2)),
            SwarmBoardListDiff::Reset {
                new_len: 4,
                anchor_item_ix: Some(0),
            }
        );
    }

    #[test]
    fn cycles_swarm_panel_tabs_with_arrow_wraparound() {
        assert_eq!(SwarmPanelTab::Hive.step(1), SwarmPanelTab::Board);
        assert_eq!(SwarmPanelTab::Board.step(1), SwarmPanelTab::Members);
        assert_eq!(SwarmPanelTab::Members.step(1), SwarmPanelTab::Overview);
        assert_eq!(SwarmPanelTab::Overview.step(1), SwarmPanelTab::Hive);
        assert_eq!(SwarmPanelTab::Hive.step(-1), SwarmPanelTab::Overview);
    }

    #[test]
    fn extracts_short_clock_time_from_board_timestamp() {
        let utc = chrono::FixedOffset::east_opt(0).unwrap();
        let plus_eight = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        assert_eq!(short_board_time("2026-11-23 15:24:28"), "15:24");
        assert_eq!(short_board_time("15:24"), "15:24");
        assert_eq!(
            short_board_time_with_tz("2026-11-23T15:24:28Z", &utc),
            "15:24"
        );
        assert_eq!(
            short_board_time_with_tz("2026-11-23T15:24:28Z", &plus_eight),
            "23:24"
        );
    }

    #[test]
    fn strips_only_leading_mentions_already_listed_as_recipients() {
        let mentions = vec!["cli-catalog".to_owned(), "verifier".to_owned()];
        assert_eq!(
            strip_leading_mentions("@cli-catalog You are an investigator", &mentions, false),
            "You are an investigator"
        );
        assert_eq!(
            strip_leading_mentions("@verifier, @cli-catalog: [done] ok", &mentions, false),
            "[done] ok"
        );
        assert_eq!(
            strip_leading_mentions("@monitor ping @verifier", &mentions, false),
            "@monitor ping @verifier"
        );
        assert_eq!(strip_leading_mentions("@all sync", &[], true), "sync");
        assert_eq!(
            strip_leading_mentions("@verifier", &mentions, false),
            "@verifier"
        );
    }

    #[test]
    fn collapses_only_long_posts_into_a_short_excerpt() {
        assert_eq!(collapsed_preview("short reply"), None);
        let long = (1..=10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = collapsed_preview(&long).expect("long post collapses");
        assert_eq!(preview, "line 1\nline 2\nline 3\nline 4…");
        let wide = "字".repeat(MESSAGE_COLLAPSE_CHARS + 1);
        let preview = collapsed_preview(&wide).expect("wide post collapses");
        assert_eq!(preview.chars().count(), MESSAGE_PREVIEW_CHARS + 1);
    }

    fn member(
        name: &str,
        status: AntigravitySwarmMemberStatus,
        killed: bool,
    ) -> AntigravitySwarmMember {
        AntigravitySwarmMember {
            name: name.to_owned(),
            conversation_id: name.to_owned(),
            model: "model-a".to_owned(),
            role: "worker".to_owned(),
            is_role_worker: false,
            wake_mode: "mentions".to_owned(),
            status,
            killed,
            has_posted_done: false,
            task: None,
            last_read_seq: 0,
            unread: 0,
            posts: 0,
            steps: 0,
            activity: None,
            activity_kind: None,
            failure: None,
            transcript_mtime_ms: None,
        }
    }

    #[test]
    fn groups_roster_by_status_in_attention_order() {
        use AntigravitySwarmMemberStatus::{Failed, Killed, Stopped, Waiting, Working};
        let members = vec![
            member("coordinator", Waiting, false),
            member("alpha", Working, false),
            member("beta", Failed, false),
            member("gamma", Working, false),
            member("paused", Stopped, false),
            member("old", Working, true),
        ];
        assert_eq!(
            swarm_member_list_items(&members, false),
            vec![
                SwarmMemberListItem::Group {
                    status: Failed,
                    count: 1
                },
                SwarmMemberListItem::Member(2),
                SwarmMemberListItem::Group {
                    status: Working,
                    count: 2
                },
                SwarmMemberListItem::Member(1),
                SwarmMemberListItem::Member(3),
                SwarmMemberListItem::Group {
                    status: Waiting,
                    count: 1
                },
                SwarmMemberListItem::Member(0),
                SwarmMemberListItem::Group {
                    status: Stopped,
                    count: 1
                },
                SwarmMemberListItem::Member(4),
            ]
        );
        let with_killed = swarm_member_list_items(&members, true);
        assert_eq!(
            &with_killed[with_killed.len() - 2..],
            &[
                SwarmMemberListItem::Group {
                    status: Killed,
                    count: 1
                },
                SwarmMemberListItem::Member(5),
            ]
        );
    }

    #[test]
    fn antigravity_session_id_scopes_to_provider_and_active_account() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Antigravity);
        session.account_id = Some("acct-b".to_owned());
        session
            .account_cursors
            .push(waku_protocol::model::ParkedAccountCursor {
                account_id: "acct-a".to_owned(),
                cursor: ProviderResumeCursor::Antigravity {
                    session_id: "ag-a".to_owned(),
                    fork_context: None,
                },
            });
        assert_eq!(antigravity_session_id(&session), None);

        session
            .account_cursors
            .push(waku_protocol::model::ParkedAccountCursor {
                account_id: "acct-b".to_owned(),
                cursor: ProviderResumeCursor::Antigravity {
                    session_id: "ag-b".to_owned(),
                    fork_context: None,
                },
            });
        assert_eq!(antigravity_session_id(&session), Some("ag-b"));

        session.switch_provider(ProviderKind::Codex);
        assert_eq!(antigravity_session_id(&session), None);
    }

    #[test]
    fn swarm_member_counts_exclude_role_workers_and_include_stopped() {
        use AntigravitySwarmMemberStatus::{Failed, Stopped, Waiting, Working};
        let mut narrator = member("narrator", Working, false);
        narrator.is_role_worker = true;
        let snapshot = AntigravitySwarmSnapshot {
            swarm_id: "sw".to_owned(),
            current_member: None,
            title: "T".to_owned(),
            head_seq: 0,
            total_messages: 0,
            messages_truncated: false,
            channels: vec![],
            members: vec![
                member("coordinator", Waiting, false),
                member("a", Working, false),
                member("b", Stopped, false),
                member("c", Failed, false),
                member("d", Working, true),
                narrator,
            ],
            messages: vec![],
            overview: None,
            system_attention: vec![],
            fingerprint: "fp".to_owned(),
            board_fingerprint: None,
            roster_fingerprint: None,
        };
        assert_eq!(
            swarm_member_counts(&snapshot),
            SwarmMemberCounts {
                working: 1,
                waiting: 1,
                stopped: 1,
                failed: 1,
                killed: 1,
            }
        );
    }

    #[test]
    fn hoists_model_only_when_every_live_member_shares_it() {
        use AntigravitySwarmMemberStatus::Working;
        let mut members = vec![member("a", Working, false), member("b", Working, false)];
        assert_eq!(shared_swarm_model(&members), Some("model-a"));
        members.push(member("c", Working, true));
        members[2].model = "model-b".to_owned();
        assert_eq!(shared_swarm_model(&members), Some("model-a"));
        members[1].model = "model-b".to_owned();
        assert_eq!(shared_swarm_model(&members), None);
    }

    #[test]
    fn preserves_pending_auto_reveal_until_session_is_selected() {
        let mut state = SwarmAutoSurfaceState::default();
        let session_id = Uuid::new_v4();

        assert!(state.queue_auto_reveal(session_id));
        assert!(state.has_pending_reveal(session_id));

        // Detection while viewing another session must keep pending_auto_reveal intact.
        assert_eq!(
            state.resolve_detected_swarm(session_id, false, true, false, false),
            SwarmSurfaceAction::None
        );
        assert!(state.has_pending_reveal(session_id));

        // Switching back triggers a refresh with auto_reveal = false, which consumes pending_auto_reveal.
        assert_eq!(
            state.resolve_detected_swarm(session_id, true, false, false, false),
            SwarmSurfaceAction::OpenPanel
        );
        assert!(!state.has_pending_reveal(session_id));

        // Once surfaced, further activity neither re-queues nor re-opens the panel.
        assert!(!state.queue_auto_reveal(session_id));
        assert_eq!(
            state.resolve_detected_swarm(session_id, true, true, true, true),
            SwarmSurfaceAction::None
        );

        // Dismissed sessions clear pending state and do not auto-surface.
        let dismissed_id = Uuid::new_v4();
        assert!(state.queue_auto_reveal(dismissed_id));
        state.mark_dismissed(dismissed_id);
        assert!(!state.has_pending_reveal(dismissed_id));
        assert!(!state.queue_auto_reveal(dismissed_id));
        assert_eq!(
            state.resolve_detected_swarm(dismissed_id, true, true, false, false),
            SwarmSurfaceAction::None
        );

        // Without auto-reveal, an already-open right panel still attaches the Swarm tab once.
        let attach_id = Uuid::new_v4();
        assert_eq!(
            state.resolve_detected_swarm(attach_id, true, false, true, false),
            SwarmSurfaceAction::AttachTab
        );

        // Detecting a swarm passively while the right panel is closed must NOT
        // prevent a later activity-driven auto-reveal from opening the panel once.
        let passive_id = Uuid::new_v4();
        assert_eq!(
            state.resolve_detected_swarm(passive_id, true, false, false, false),
            SwarmSurfaceAction::None
        );
        assert!(state.queue_auto_reveal(passive_id));
        assert_eq!(
            state.resolve_detected_swarm(passive_id, true, true, false, false),
            SwarmSurfaceAction::OpenPanel
        );
    }

    #[test]
    fn formats_overview_updated_at_and_meta_label() {
        let utc = chrono::FixedOffset::east_opt(0).unwrap();
        let plus_eight = chrono::FixedOffset::east_opt(8 * 3600).unwrap();

        assert_eq!(
            format_overview_updated_at_with_tz("2026-11-23T15:30:00Z", &utc).as_deref(),
            Some("15:30")
        );
        assert_eq!(
            format_overview_updated_at_with_tz("2026-11-23T15:30:00Z", &plus_eight).as_deref(),
            Some("23:30")
        );
        assert_eq!(
            format_overview_updated_at_with_tz("2026-11-23 15:30:00", &utc).as_deref(),
            Some("15:30")
        );
        assert_eq!(
            format_overview_updated_at_with_tz("2026-11-23T15:30:00", &utc).as_deref(),
            Some("15:30")
        );
        assert_eq!(
            format_overview_updated_at_with_tz("15:30", &utc).as_deref(),
            Some("15:30")
        );
        assert_eq!(format_overview_updated_at_with_tz("   ", &utc), None);

        assert_eq!(
            overview_meta_label(12, Some("2026-11-23 15:30:00")),
            Some(format!("{} · 15:30", tr!("swarm.as_of_seq", seq = 12)))
        );
        assert_eq!(
            overview_meta_label(0, Some("2026-11-23 15:30:00")),
            Some("15:30".to_owned())
        );
        assert_eq!(overview_meta_label(0, Some("  ")), None);
        assert_eq!(overview_meta_label(0, None), None);
    }
}
