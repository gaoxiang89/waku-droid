//! Native GPUI right-panel surface for Antigravity Swarm Boards.
//!
//! All filesystem reads execute on the daemon host through
//! [`waku_client::WorkspaceOperation::ReadAntigravitySwarm`] on the background
//! executor, guarded by a generation counter so superseded responses cannot
//! overwrite newer state. `render` reads only in-memory snapshots.

use waku_client::antigravity_swarm::{
    AntigravitySwarmAttentionItem, AntigravitySwarmFailureKind, AntigravitySwarmMember,
    AntigravitySwarmMemberStatus, AntigravitySwarmMessage, AntigravitySwarmOverview,
    AntigravitySwarmSnapshot,
};

use super::*;
use crate::ui::ActivationExt as _;

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
    Board,
    Members,
    Overview,
}

impl SwarmPanelTab {
    const ALL: [Self; 3] = [Self::Board, Self::Members, Self::Overview];

    fn step(self, delta: isize) -> Self {
        let index = Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .unwrap_or(0) as isize;
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

pub(super) struct SwarmPanelState {
    active_tab: SwarmPanelTab,
    member_filter: Option<String>,
    follow_tail: Rc<Cell<bool>>,
    show_killed_members: bool,
    /// Per-session cached swarm snapshot (`Some(None)` = probed and no swarm).
    snapshots: HashMap<Uuid, Option<Arc<AntigravitySwarmSnapshot>>>,
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
    board_list_key: Option<(Uuid, String, Option<String>)>,
    board_first_seq: Option<u64>,
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
    tab_board_focus: FocusHandle,
    tab_members_focus: FocusHandle,
    tab_overview_focus: FocusHandle,
    follow_focus: FocusHandle,
    refresh_focus: FocusHandle,
    show_killed_focus: FocusHandle,
    clear_filter_focus: FocusHandle,
    jump_latest_focus: FocusHandle,
    retry_focus: FocusHandle,
    filter_chip_focuses: RefCell<HashMap<String, FocusHandle>>,
    member_row_focuses: RefCell<HashMap<String, FocusHandle>>,
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
            active_tab: SwarmPanelTab::Board,
            member_filter: None,
            follow_tail,
            show_killed_members: false,
            snapshots: HashMap::new(),
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
            board_first_seq: None,
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
            tab_board_focus: cx.focus_handle(),
            tab_members_focus: cx.focus_handle(),
            tab_overview_focus: cx.focus_handle(),
            follow_focus: cx.focus_handle(),
            refresh_focus: cx.focus_handle(),
            show_killed_focus: cx.focus_handle(),
            clear_filter_focus: cx.focus_handle(),
            jump_latest_focus: cx.focus_handle(),
            retry_focus: cx.focus_handle(),
            filter_chip_focuses: RefCell::new(HashMap::new()),
            member_row_focuses: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn remove_session(&mut self, session_id: Uuid) {
        self.snapshots.remove(&session_id);
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
            SwarmPanelTab::Board => &self.tab_board_focus,
            SwarmPanelTab::Members => &self.tab_members_focus,
            SwarmPanelTab::Overview => &self.tab_overview_focus,
        }
    }

    fn filter_chip_focus(&self, key: &str, cx: &mut Context<Waku>) -> FocusHandle {
        let mut map = self.filter_chip_focuses.borrow_mut();
        if let Some(handle) = map.get(key) {
            return handle.clone();
        }
        let handle = cx.focus_handle();
        map.insert(key.to_owned(), handle.clone());
        handle
    }

    fn member_row_focus(&self, key: &str, cx: &mut Context<Waku>) -> FocusHandle {
        let mut map = self.member_row_focuses.borrow_mut();
        if let Some(handle) = map.get(key) {
            return handle.clone();
        }
        let handle = cx.focus_handle();
        map.insert(key.to_owned(), handle.clone());
        handle
    }
}

pub(super) fn antigravity_session_id(session: &AgentSession) -> Option<&str> {
    session
        .all_provider_cursors()
        .find_map(|cursor| match cursor {
            ProviderResumeCursor::Antigravity { session_id, .. } if !session_id.is_empty() => {
                Some(session_id.as_str())
            }
            _ => None,
        })
}

fn swarm_member_counts(snapshot: &AntigravitySwarmSnapshot) -> (usize, usize, usize, usize) {
    let mut working = 0usize;
    let mut waiting = 0usize;
    let mut failed = 0usize;
    let mut killed = 0usize;
    for member in &snapshot.members {
        if member.is_role_worker {
            continue;
        }
        match member.status {
            AntigravitySwarmMemberStatus::Working => working += 1,
            AntigravitySwarmMemberStatus::Waiting => waiting += 1,
            AntigravitySwarmMemberStatus::Failed => failed += 1,
            AntigravitySwarmMemberStatus::Killed => killed += 1,
        }
    }
    (working, waiting, failed, killed)
}

fn filter_swarm_message_indices(
    messages: &[AntigravitySwarmMessage],
    member_filter: Option<&str>,
) -> Vec<usize> {
    let Some(filter) = member_filter else {
        return (0..messages.len()).collect();
    };
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.agent == filter
                || message.mentions_all
                || message.mentions.iter().any(|name| name == filter)
        })
        .map(|(index, _)| index)
        .collect()
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
        Self { full, preview }
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

const MEMBER_GROUP_ORDER: [AntigravitySwarmMemberStatus; 4] = [
    AntigravitySwarmMemberStatus::Failed,
    AntigravitySwarmMemberStatus::Working,
    AntigravitySwarmMemberStatus::Waiting,
    AntigravitySwarmMemberStatus::Killed,
];

fn member_display_status(member: &AntigravitySwarmMember) -> AntigravitySwarmMemberStatus {
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
        let token_end = after_at
            .find(char::is_whitespace)
            .unwrap_or(after_at.len());
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

fn short_board_time(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some((_, time_part)) = trimmed.split_once(' ') {
        return extract_hh_mm(time_part).unwrap_or_else(|| time_part.to_owned());
    }
    trimmed.to_owned()
}

fn format_overview_updated_at_with_tz<Tz: chrono::TimeZone>(
    raw: &str,
    tz: &Tz,
) -> Option<String>
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
    Some(short_board_time(trimmed))
}

fn format_overview_updated_at(raw: &str) -> Option<String> {
    format_overview_updated_at_with_tz(raw, &Local)
}

fn overview_meta_label(as_of_seq: u64, updated_at: Option<&str>) -> Option<String> {
    let updated = updated_at.and_then(format_overview_updated_at);
    match (as_of_seq > 0, updated) {
        (true, Some(time)) => Some(format!("{} · {time}", tr!("swarm.as_of_seq", seq = as_of_seq))),
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
        session.provider == ProviderKind::Antigravity
            || self
                .swarm_panel
                .snapshots
                .get(&session.id)
                .and_then(|entry| entry.as_ref())
                .is_some()
    }

    pub(super) fn selected_antigravity_swarm_snapshot(&self) -> Option<Arc<AntigravitySwarmSnapshot>> {
        let session_id = self.state.selected_session?;
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
        self.swarm_panel.last_activity_refresh_at.insert(session_id, now);
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
        self.swarm_panel.member_filter = None;
        self.swarm_panel.board_list_key = None;
        self.swarm_panel.board_first_seq = None;
        self.swarm_panel.members_list_key = None;
        self.swarm_panel.follow_tail.set(true);
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

    pub(super) fn refresh_selected_antigravity_swarm_if_relevant(&mut self, cx: &mut Context<Self>) {
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

    fn is_swarm_surface_visible(&self) -> bool {
        self.right_panel_visible
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
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .filter(|session| session.provider == ProviderKind::Antigravity)
        else {
            return;
        };
        let Some(ag_session_id) = antigravity_session_id(session).map(str::to_owned) else {
            return;
        };
        let config_dir = self.effective_config_dir_for_session(session);

        if auto_reveal {
            self.swarm_panel.auto_surface.queue_auto_reveal(session_id);
        }
        if self.swarm_panel.pending.contains(&session_id) {
            self.swarm_panel.stale.insert(session_id);
            return;
        }

        self.swarm_panel.pending.insert(session_id);
        self.swarm_panel.stale.remove(&session_id);
        self.swarm_panel.generation = self.swarm_panel.generation.wrapping_add(1);
        let generation = self.swarm_panel.generation;
        self.swarm_panel
            .session_generations
            .insert(session_id, generation);

        let workspace = waku_client::WorkspaceClient::for_daemon(
            self.daemon_for_session(session_id).clone(),
        );
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
                this.swarm_panel.pending.remove(&session_id);
                if this.swarm_panel.session_generations.get(&session_id) != Some(&generation) {
                    return;
                }
                let mut changed = false;
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

    pub(super) fn render_header_swarm_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = self.is_swarm_surface_visible();
        let snapshot = self.selected_antigravity_swarm_snapshot();
        let indicator_color = snapshot.as_ref().map(|snap| {
            let (working, _, failed, _) = swarm_member_counts(snap);
            if failed > 0 {
                theme.danger
            } else if working > 0 {
                theme.success
            } else {
                theme.accent
            }
        });
        let tooltip_label = match &snapshot {
            Some(snap) => {
                let (working, waiting, failed, _) = swarm_member_counts(snap);
                if failed > 0 {
                    format!(
                        "{} · {} · {}",
                        tr!("right_panel.swarm"),
                        tr!("swarm.active_count", count = working),
                        tr!("swarm.failed_count", count = failed)
                    )
                } else {
                    format!(
                        "{} · {} · {}",
                        tr!("right_panel.swarm"),
                        tr!("swarm.active_count", count = working),
                        tr!("swarm.waiting_count", count = waiting)
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
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| {
                style
                    .bg(theme.overlay)
                    .border_1()
                    .border_color(theme.accent)
            })
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
        _panel_width: f32,
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

        let snapshot = self
            .swarm_panel
            .snapshots
            .get(&session_id)
            .and_then(|entry| entry.as_ref())
            .cloned();
        let error = self.swarm_panel.errors.get(&session_id).cloned();
        let loading = self.swarm_panel.pending.contains(&session_id) && snapshot.is_none();

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
                if !event.keystroke.modifiers.modified()
                    && event.keystroke.key.as_str() == "escape"
                    && this.swarm_panel.member_filter.is_some()
                {
                    this.swarm_panel.member_filter = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }));

        if let Some(snapshot) = snapshot {
            root = root
                .child(self.render_swarm_header(&snapshot, &theme, cx))
                .child(self.render_swarm_tabs(&snapshot, &theme, cx))
                .child(match self.swarm_panel.active_tab {
                    SwarmPanelTab::Board => self.render_swarm_board_tab(session_id, &snapshot, &theme, cx),
                    SwarmPanelTab::Members => self.render_swarm_members_tab(session_id, &snapshot, &theme, cx),
                    SwarmPanelTab::Overview => self.render_swarm_overview_tab(&snapshot, &theme, cx),
                });
            return root.into_any_element();
        }

        if let Some(error) = error {
            let retry_focus = self.swarm_panel.retry_focus.clone();
            return root
                .child(self.render_swarm_toolbar_empty(&theme, cx))
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
                                .text_size(sp(12.5))
                                .text_color(theme.text)
                                .cursor_default()
                                .focus_visible(|style| style.border_color(theme.accent))
                                .hover(|style| style.bg(theme.overlay_strong))
                                .child(tr!("swarm.retry"))
                                .on_activation(cx, |this, _, cx| {
                                    this.refresh_selected_antigravity_swarm(cx);
                                }),
                        ),
                )
                .into_any_element();
        }

        if loading && has_cursor {
            return root
                .child(self.render_swarm_toolbar_empty(&theme, cx))
                .child(self.render_right_panel_empty_message(
                    tr!("swarm.loading_title"),
                    tr!("swarm.loading_description"),
                    cx,
                ))
                .into_any_element();
        }

        root.child(self.render_swarm_toolbar_empty(&theme, cx))
            .child(self.render_right_panel_empty_message(
                tr!("swarm.empty_title"),
                tr!("swarm.empty_description"),
                cx,
            ))
            .into_any_element()
    }

    fn render_swarm_toolbar_empty(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let refresh_focus = self.swarm_panel.refresh_focus.clone();
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
            .child(
                icon_button("swarm-empty-refresh", "icons/rotate-cw.svg", *theme)
                    .track_focus(&refresh_focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .tooltip(Tooltip::text(tr!("swarm.refresh")))
                    .on_activation(cx, |this, _, cx| {
                        this.refresh_selected_antigravity_swarm(cx);
                    }),
            )
    }

    fn render_swarm_header(
        &self,
        snapshot: &AntigravitySwarmSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let (working, waiting, failed, _) = swarm_member_counts(snapshot);
        let title = if snapshot.title.trim().is_empty() {
            tr!("swarm.title_default")
        } else {
            snapshot.title.clone()
        };

        let mut summary_parts = vec![
            tr!("swarm.active_count", count = working),
            tr!("swarm.waiting_count", count = waiting),
        ];
        if failed > 0 {
            summary_parts.push(tr!("swarm.failed_count", count = failed));
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
        let status_bar = (working + waiting + failed > 0).then(|| {
            let mut bar = div().w_full().h(px(3.0)).flex().gap(px(2.0));
            for (count, color) in [
                (working, theme.success),
                (waiting, theme.warning),
                (failed, theme.danger),
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
                                        if failed > 0 {
                                            theme.danger
                                        } else if working > 0 {
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
                                    ),
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
                                icon_button("swarm-refresh-button", "icons/rotate-cw.svg", *theme)
                                    .track_focus(&refresh_focus)
                                    .tab_index(0)
                                    .tab_stop(true)
                                    .focus_visible(|style| {
                                        style.border_1().border_color(theme.accent)
                                    })
                                    .tooltip(Tooltip::text(tr!("swarm.refresh")))
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
        let member_count = snapshot
            .members
            .iter()
            .filter(|member| !member.killed)
            .count();
        let overview_attention = snapshot.system_attention.len()
            + snapshot
                .overview
                .as_ref()
                .map_or(0, |overview| overview.attention.len());

        let tabs = [
            (
                SwarmPanelTab::Board,
                "swarm-tab-board",
                tr!("swarm.tab_board"),
                Some(snapshot.total_messages as usize),
                false,
            ),
            (
                SwarmPanelTab::Members,
                "swarm-tab-members",
                tr!("swarm.tab_members"),
                Some(member_count),
                false,
            ),
            (
                SwarmPanelTab::Overview,
                "swarm-tab-overview",
                tr!("swarm.tab_overview"),
                (overview_attention > 0).then_some(overview_attention),
                overview_attention > 0,
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

        for (tab, id, label, badge, highlight_badge) in tabs {
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
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_default()
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
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
                    .when_some(badge, |element, count| {
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
                                .text_size(sp(10.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if highlight_badge {
                                    theme.danger
                                } else {
                                    theme.text_tertiary
                                })
                                .child(count.to_string()),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.swarm_panel.active_tab = tab;
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.modifiers.modified() {
                            return;
                        }
                        match event.keystroke.key.as_str() {
                            "enter" | "space" => {
                                this.swarm_panel.active_tab = tab;
                                cx.notify();
                                cx.stop_propagation();
                            }
                            "left" => {
                                let next = tab.step(-1);
                                this.swarm_panel.active_tab = next;
                                window.focus(this.swarm_panel.tab_focus(next), cx);
                                cx.notify();
                                cx.stop_propagation();
                            }
                            "right" => {
                                let next = tab.step(1);
                                this.swarm_panel.active_tab = next;
                                window.focus(this.swarm_panel.tab_focus(next), cx);
                                cx.notify();
                                cx.stop_propagation();
                            }
                            "home" => {
                                let next = SwarmPanelTab::Board;
                                this.swarm_panel.active_tab = next;
                                window.focus(this.swarm_panel.tab_focus(next), cx);
                                cx.notify();
                                cx.stop_propagation();
                            }
                            "end" => {
                                let next = SwarmPanelTab::Overview;
                                this.swarm_panel.active_tab = next;
                                window.focus(this.swarm_panel.tab_focus(next), cx);
                                cx.notify();
                                cx.stop_propagation();
                            }
                            _ => {}
                        }
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
        // Clear stale filter if that member no longer exists on the roster.
        if let Some(filter) = self.swarm_panel.member_filter.as_deref()
            && !snapshot.members.iter().any(|member| member.name == filter)
        {
            self.swarm_panel.member_filter = None;
        }

        let list_key = (
            session_id,
            snapshot.fingerprint.clone(),
            self.swarm_panel.member_filter.clone(),
        );
        if self.swarm_panel.board_list_key.as_ref() != Some(&list_key) {
            self.prune_swarm_message_caches(snapshot);
            let indices = filter_swarm_message_indices(
                &snapshot.messages,
                self.swarm_panel.member_filter.as_deref(),
            );
            let old_len = self.swarm_panel.filtered_message_indices.len();
            let new_len = indices.len();
            let next_first_seq = indices
                .first()
                .and_then(|&idx| snapshot.messages.get(idx))
                .map(|message| message.seq);
            let prev_first_seq = self.swarm_panel.board_first_seq;
            let same_session_and_filter = self.swarm_panel.board_list_key.as_ref().is_some_and(
                |(prev_session, _, prev_filter)| {
                    *prev_session == session_id && *prev_filter == self.swarm_panel.member_filter
                },
            );
            let keep_top = (!self.swarm_panel.follow_tail.get() && same_session_and_filter)
                .then(|| self.swarm_panel.board_list_state.logical_scroll_top());
            self.swarm_panel.filtered_message_indices = Rc::new(indices);
            self.swarm_panel.board_list_key = Some(list_key);
            self.swarm_panel.board_first_seq = next_first_seq;
            if same_session_and_filter
                && !snapshot.messages_truncated
                && prev_first_seq == next_first_seq
                && new_len > old_len
                && old_len > 0
            {
                self.swarm_panel
                    .board_list_state
                    .splice(old_len..old_len, new_len - old_len);
            } else {
                self.swarm_panel.board_list_state.reset(new_len);
            }
            if let Some(top) = keep_top {
                self.swarm_panel.board_list_state.scroll_to(top);
            } else if self.swarm_panel.follow_tail.get() && new_len > 0 {
                self.swarm_panel
                    .board_list_state
                    .scroll_to_reveal_item(new_len - 1);
            }
        }

        let filter_bar = self.render_swarm_member_filter_bar(snapshot, theme, cx);
        let attention_strip = (!snapshot.system_attention.is_empty())
            .then(|| render_swarm_system_attention_strip(&snapshot.system_attention, theme));
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
            let has_filter = self.swarm_panel.member_filter.is_some();
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
                        .child(if has_filter {
                            tr!("swarm.board_empty")
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
            let markdown_cache = Rc::clone(&self.swarm_panel.message_markdown);
            let expanded = Rc::clone(&self.swarm_panel.expanded_messages);
            let toggle_focuses = Rc::clone(&self.swarm_panel.message_toggle_focuses);
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
                .child(md::render::frame_reset(self.swarm_panel.board_selection.clone()))
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
                                let collapsible = entry.preview.is_some();
                                let (view, row_key) = match entry.preview.as_ref() {
                                    Some(preview) if !is_expanded => {
                                        (preview, format!("swarm-post-{seq}-preview"))
                                    }
                                    _ => (&entry.full, format!("swarm-post-{seq}")),
                                };
                                let ctx =
                                    MarkdownCtx::new(row_key, &palette, metrics, selection.clone())
                                        .with_math_enabled(false)
                                        .with_streaming_animation(false)
                                        .with_link_handler(link_handler.clone());
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
                            render_swarm_message_row(
                                message,
                                snapshot_for_list.current_member.as_deref(),
                                body,
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
                        div()
                            .absolute()
                            .right(px(14.0))
                            .bottom(px(12.0))
                            .child(
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
                                        let count =
                                            this.swarm_panel.filtered_message_indices.len();
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

    fn render_swarm_members_tab(
        &mut self,
        session_id: Uuid,
        snapshot: &Arc<AntigravitySwarmSnapshot>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let killed_count = snapshot.members.iter().filter(|member| member.killed).count();
        let show_killed = self.swarm_panel.show_killed_members;
        let members_key = (session_id, snapshot.fingerprint.clone(), show_killed);
        if self.swarm_panel.members_list_key.as_ref() != Some(&members_key) {
            let items = swarm_member_list_items(&snapshot.members, show_killed);
            let len = items.len();
            self.swarm_panel.member_list_items = Rc::new(items);
            self.swarm_panel.members_list_key = Some(members_key);
            self.swarm_panel.members_list_state.reset(len);
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
                        move |row_index, _window, _cx| {
                            match items_for_list.get(row_index) {
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
                            }
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
                theme,
            ));
        }

        if let Some(overview) = snapshot.overview.as_ref() {
            content = content.child(render_swarm_overview_body(overview, theme));
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

fn member_status_color(status: AntigravitySwarmMemberStatus, theme: &Theme) -> Hsla {
    match status {
        AntigravitySwarmMemberStatus::Working => theme.success,
        AntigravitySwarmMemberStatus::Waiting => theme.warning,
        AntigravitySwarmMemberStatus::Failed => theme.danger,
        AntigravitySwarmMemberStatus::Killed => theme.text_ghost,
    }
}

fn member_status_icon(status: AntigravitySwarmMemberStatus) -> &'static str {
    match status {
        AntigravitySwarmMemberStatus::Working => "icons/zap.svg",
        AntigravitySwarmMemberStatus::Waiting => "icons/hourglass.svg",
        AntigravitySwarmMemberStatus::Failed => "icons/alert.svg",
        AntigravitySwarmMemberStatus::Killed => "icons/block.svg",
    }
}

fn member_status_label(status: AntigravitySwarmMemberStatus) -> String {
    match status {
        AntigravitySwarmMemberStatus::Working => tr!("swarm.status_working"),
        AntigravitySwarmMemberStatus::Waiting => tr!("swarm.status_waiting"),
        AntigravitySwarmMemberStatus::Failed => tr!("swarm.status_failed"),
        AntigravitySwarmMemberStatus::Killed => tr!("swarm.status_killed"),
    }
}

fn render_swarm_system_attention_strip(
    items: &[AntigravitySwarmAttentionItem],
    theme: &Theme,
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

fn swarm_badge(label: String, color: Hsla, theme: &Theme) -> Div {
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

fn render_swarm_message_row(
    message: &AntigravitySwarmMessage,
    current_member: Option<&str>,
    body: AnyElement,
    toggle: Option<(bool, FocusHandle)>,
    waku: gpui::WeakEntity<Waku>,
    theme: &Theme,
) -> Div {
    let mentions_me = current_member
        .is_some_and(|me| message.mentions.iter().any(|target| target == me));
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
        let key_waku = waku;
        div()
            .id(SharedString::from(format!("swarm-post-toggle-{seq}")))
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .h(px(22.0))
            .px(px(6.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_default()
            .text_size(sp(11.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_secondary)
            .focus_visible(|style| style.bg(theme.overlay_strong).text_color(theme.text))
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

    let header = div()
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
            row.child(swarm_badge(tr!("swarm.brief_badge"), theme.text_secondary, theme))
        })
        .when_some(message.channel.as_ref(), |row, channel| {
            row.child(swarm_badge(format!("#{channel}"), theme.text_tertiary, theme))
        })
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
                .text_color(theme.text_ghost)
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
    let show_role = !member.role.is_empty() && member.role != "worker" && member.role != member.name;

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
            .id(SharedString::from(format!("swarm-member-row-{}", member.name)))
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
            .on_click(move |_, _, cx| {
                let _ = click_waku.update(cx, |this, cx| {
                    this.swarm_panel.member_filter = Some(member_name_click.clone());
                    this.swarm_panel.active_tab = SwarmPanelTab::Board;
                    this.swarm_panel.follow_tail.set(true);
                    cx.notify();
                });
                cx.stop_propagation();
            })
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    let _ = key_waku.update(cx, |this, cx| {
                        this.swarm_panel.member_filter = Some(member_name_key.clone());
                        this.swarm_panel.active_tab = SwarmPanelTab::Board;
                        this.swarm_panel.follow_tail.set(true);
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
                                    member.role.clone(),
                                    theme.text_tertiary,
                                    theme,
                                ))
                            }),
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
                                        .child(activity.clone()),
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
fn render_swarm_overview_attention(
    heading: &str,
    items: &[AntigravitySwarmAttentionItem],
    theme: &Theme,
) -> Div {
    let mut section = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(sp(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .child(heading.to_owned()),
        );
    for item in items {
        let color = match item.kind.as_str() {
            "failure" | "quota" | "error" => theme.danger,
            "decision" | "review" | "stuck" => theme.warning,
            _ => theme.text_secondary,
        };
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
                .gap(px(3.0))
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
                                .child(item.kind.clone()),
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
                }),
        );
    }
    section
}

fn render_swarm_overview_body(overview: &AntigravitySwarmOverview, theme: &Theme) -> Div {
    let mut column = div().flex().flex_col().gap(px(12.0));

    let phase_label = overview.phase.as_ref().map(|phase| match (phase.index, phase.total) {
        (Some(index), Some(total)) => format!("{} ({index}/{total})", phase.label),
        _ => phase.label.clone(),
    });
    let meta_label = overview_meta_label(overview.as_of_seq, overview.updated_at.as_deref());
    if !overview.headline.is_empty() || phase_label.is_some() || meta_label.is_some() {
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
                .when(phase_label.is_some() || meta_label.is_some(), |card| {
                    card.child(
                        div()
                            .flex()
                            .items_center()
                            .flex_wrap()
                            .gap(px(8.0))
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
                }),
        );
    }

    if !overview.attention.is_empty() {
        column = column.child(render_swarm_overview_attention(
            &tr!("swarm.section_attention"),
            &overview.attention,
            theme,
        ));
    }

    if !overview.workstreams.is_empty() {
        let mut ws_section = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(sp(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_secondary)
                    .child(tr!("swarm.section_workstreams")),
            );
        for ws in &overview.workstreams {
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
                                    .child(ws.state.clone()),
                            ),
                    )
                    .when(!ws.owners.is_empty(), |card| {
                        card.child(
                            div()
                                .text_size(sp(11.5))
                                .text_color(theme.text_tertiary)
                                .child(
                                    ws.owners
                                        .iter()
                                        .map(|owner| format!("@{owner}"))
                                        .collect::<Vec<_>>()
                                        .join(" "),
                                ),
                        )
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

    if !overview.milestones.is_empty() {
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
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
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
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
    fn filters_board_messages_by_sender_mention_or_all() {
        let messages = vec![
            AntigravitySwarmMessage {
                seq: 1,
                agent: "coordinator".to_owned(),
                text: "@alpha start".to_owned(),
                time: "2026-11-23 15:00:00".to_owned(),
                mentions: vec!["alpha".to_owned()],
                mentions_all: false,
                kind: None,
                channel: None,
            },
            AntigravitySwarmMessage {
                seq: 2,
                agent: "beta".to_owned(),
                text: "@coordinator status".to_owned(),
                time: "2026-11-23 15:01:00".to_owned(),
                mentions: vec!["coordinator".to_owned()],
                mentions_all: false,
                kind: None,
                channel: None,
            },
            AntigravitySwarmMessage {
                seq: 3,
                agent: "coordinator".to_owned(),
                text: "@all sync".to_owned(),
                time: "2026-11-23 15:02:00".to_owned(),
                mentions: vec![],
                mentions_all: true,
                kind: None,
                channel: None,
            },
        ];

        assert_eq!(filter_swarm_message_indices(&messages, None), vec![0, 1, 2]);
        assert_eq!(
            filter_swarm_message_indices(&messages, Some("alpha")),
            vec![0, 2]
        );
        assert_eq!(
            filter_swarm_message_indices(&messages, Some("beta")),
            vec![1, 2]
        );
    }

    #[test]
    fn cycles_swarm_panel_tabs_with_arrow_wraparound() {
        assert_eq!(SwarmPanelTab::Board.step(1), SwarmPanelTab::Members);
        assert_eq!(SwarmPanelTab::Members.step(1), SwarmPanelTab::Overview);
        assert_eq!(SwarmPanelTab::Overview.step(1), SwarmPanelTab::Board);
        assert_eq!(SwarmPanelTab::Board.step(-1), SwarmPanelTab::Overview);
    }

    #[test]
    fn extracts_short_clock_time_from_board_timestamp() {
        assert_eq!(short_board_time("2026-11-23 15:24:28"), "15:24");
        assert_eq!(short_board_time("15:24"), "15:24");
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
        assert_eq!(strip_leading_mentions("@verifier", &mentions, false), "@verifier");
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

    fn member(name: &str, status: AntigravitySwarmMemberStatus, killed: bool) -> AntigravitySwarmMember {
        AntigravitySwarmMember {
            name: name.to_owned(),
            conversation_id: name.to_owned(),
            model: "model-a".to_owned(),
            role: "worker".to_owned(),
            is_role_worker: false,
            wake_mode: "mentions".to_owned(),
            status,
            killed,
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
        use AntigravitySwarmMemberStatus::{Failed, Killed, Waiting, Working};
        let members = vec![
            member("coordinator", Waiting, false),
            member("alpha", Working, false),
            member("beta", Failed, false),
            member("gamma", Working, false),
            member("old", Working, true),
        ];
        assert_eq!(
            swarm_member_list_items(&members, false),
            vec![
                SwarmMemberListItem::Group { status: Failed, count: 1 },
                SwarmMemberListItem::Member(2),
                SwarmMemberListItem::Group { status: Working, count: 2 },
                SwarmMemberListItem::Member(1),
                SwarmMemberListItem::Member(3),
                SwarmMemberListItem::Group { status: Waiting, count: 1 },
                SwarmMemberListItem::Member(0),
            ]
        );
        let with_killed = swarm_member_list_items(&members, true);
        assert_eq!(
            &with_killed[with_killed.len() - 2..],
            &[
                SwarmMemberListItem::Group { status: Killed, count: 1 },
                SwarmMemberListItem::Member(4),
            ]
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
