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
    /// Tracks whether an auto-reveal was requested for a session's newly
    /// detected swarm across throttled/in-flight refreshes.
    pending_auto_reveal: HashSet<Uuid>,
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
    /// Sessions where we already surfaced the Swarm tab on first detection.
    auto_surfaced_sessions: HashSet<Uuid>,
    /// Sessions where the user explicitly closed the Swarm tab.
    dismissed_sessions: HashSet<Uuid>,
    /// Virtualized list backing `board.jsonl` messages.
    board_list_state: ListState,
    board_scrollbar: Rc<ScrollbarState>,
    board_list_key: Option<(Uuid, String, Option<String>)>,
    board_first_seq: Option<u64>,
    filtered_message_indices: Rc<Vec<usize>>,
    /// Virtualized list backing the Members roster.
    members_list_state: ListState,
    members_scrollbar: Rc<ScrollbarState>,
    members_list_key: Option<(Uuid, String, bool)>,
    visible_member_indices: Rc<Vec<usize>>,
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
            pending_auto_reveal: HashSet::new(),
            saw_swarm_activity: HashSet::new(),
            throttle_scheduled: HashSet::new(),
            generation: 0,
            session_generations: HashMap::new(),
            poll_generation: 0,
            poll_active: false,
            auto_surfaced_sessions: HashSet::new(),
            dismissed_sessions: HashSet::new(),
            board_list_state,
            board_scrollbar: ScrollbarState::new(),
            board_list_key: None,
            board_first_seq: None,
            filtered_message_indices: Rc::new(Vec::new()),
            members_list_state: ListState::new(0, ListAlignment::Top, px(256.0)),
            members_scrollbar: ScrollbarState::new(),
            members_list_key: None,
            visible_member_indices: Rc::new(Vec::new()),
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
        self.pending_auto_reveal.remove(&session_id);
        self.saw_swarm_activity.remove(&session_id);
        self.throttle_scheduled.remove(&session_id);
        self.session_generations.remove(&session_id);
        self.auto_surfaced_sessions.remove(&session_id);
        self.dismissed_sessions.remove(&session_id);
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
            ProviderResumeCursor::Antigravity { session_id } if !session_id.is_empty() => {
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

fn visible_swarm_member_indices(
    members: &[AntigravitySwarmMember],
    show_killed: bool,
) -> Vec<usize> {
    members
        .iter()
        .enumerate()
        .filter(|(_, member)| show_killed || !member.killed)
        .map(|(index, _)| index)
        .collect()
}

fn short_board_time(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some((_, time_part)) = trimmed.split_once(' ') {
        let mut parts = time_part.split(':');
        if let (Some(hour), Some(minute)) = (parts.next(), parts.next()) {
            return format!("{hour}:{minute}");
        }
        return time_part.to_owned();
    }
    trimmed.to_owned()
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
            self.swarm_panel.dismissed_sessions.remove(&session_id);
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
            self.swarm_panel.dismissed_sessions.insert(session_id);
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
        if !self.swarm_panel.auto_surfaced_sessions.contains(&session_id)
            && !self.swarm_panel.dismissed_sessions.contains(&session_id)
        {
            self.swarm_panel.pending_auto_reveal.insert(session_id);
        }
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
                                    this.swarm_panel.pending_auto_reveal.contains(&session_id);
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
        self.request_antigravity_swarm_refresh(session_id, true, cx);
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
            let auto_reveal = self.swarm_panel.pending_auto_reveal.contains(&session_id);
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
        let Some(ag_session_id) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .filter(|session| session.provider == ProviderKind::Antigravity)
            .and_then(antigravity_session_id)
            .map(str::to_owned)
        else {
            return;
        };

        if auto_reveal {
            self.swarm_panel.pending_auto_reveal.insert(session_id);
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
                    let was_pending = this.swarm_panel.pending_auto_reveal.remove(&session_id);
                    let should_reveal = auto_reveal || was_pending;
                    if this.state.selected_session == Some(session_id)
                        && !this.swarm_panel.dismissed_sessions.contains(&session_id)
                        && this.swarm_panel.auto_surfaced_sessions.insert(session_id)
                    {
                        if should_reveal {
                            this.open_right_panel_surface(RightPanelSurface::Swarm, cx);
                        } else if this.right_panel_visible
                            && !this
                                .right_panel_surfaces
                                .contains(&RightPanelSurface::Swarm)
                        {
                            this.right_panel_surfaces.push(RightPanelSurface::Swarm);
                            changed = true;
                        }
                    }
                }

                if this.swarm_panel.stale.remove(&session_id) {
                    let retry_reveal = this.swarm_panel.pending_auto_reveal.contains(&session_id);
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
        if snapshot.head_seq > 0 {
            summary_parts.push(format!("#{}", snapshot.head_seq));
        }

        let following = self.swarm_panel.follow_tail.get();
        let follow_focus = self.swarm_panel.follow_focus.clone();
        let refresh_focus = self.swarm_panel.refresh_focus.clone();

        div()
            .flex_none()
            .px(px(12.0))
            .py(px(9.0))
            .border_b_1()
            .border_color(theme.border)
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
                            )
                            .when_some(snapshot.current_member.as_ref(), |row, member| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .px(px(6.0))
                                        .py(px(1.0))
                                        .rounded(px(4.0))
                                        .bg(theme.overlay)
                                        .border_1()
                                        .border_color(theme.border_strong)
                                        .text_size(sp(11.0))
                                        .text_color(theme.text_secondary)
                                        .child(format!("@{member}")),
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
                                        let count = this.swarm_panel.filtered_message_indices.len();
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
                            .focus_visible(|style| style.border_1().border_color(theme.accent))
                            .tooltip(Tooltip::text(tr!("swarm.refresh")))
                            .on_activation(cx, |this, _, cx| {
                                this.refresh_selected_antigravity_swarm(cx);
                            }),
                    ),
            )
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
            let jump_focus = self.swarm_panel.jump_latest_focus.clone();

            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    list(
                        self.swarm_panel.board_list_state.clone(),
                        move |row_index, _window, _cx| {
                            let Some(&message_index) = indices_for_list.get(row_index) else {
                                return div().into_any_element();
                            };
                            let Some(message) = snapshot_for_list.messages.get(message_index)
                            else {
                                return div().into_any_element();
                            };
                            render_swarm_message_row(
                                message,
                                snapshot_for_list.current_member.as_deref(),
                                &theme_copy,
                            )
                            .into_any_element()
                        },
                    )
                    .size_full(),
                )
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
            let dot_color = member_status_color(member.status, theme);
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
            let indices = visible_swarm_member_indices(&snapshot.members, show_killed);
            let len = indices.len();
            self.swarm_panel.visible_member_indices = Rc::new(indices);
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

        let body = if self.swarm_panel.visible_member_indices.is_empty() {
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
            let indices_for_list = Rc::clone(&self.swarm_panel.visible_member_indices);
            let row_focuses = Rc::new(
                indices_for_list
                    .iter()
                    .filter_map(|&member_index| snapshot.members.get(member_index))
                    .map(|member| self.swarm_panel.member_row_focus(&member.name, cx))
                    .collect::<Vec<_>>(),
            );
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
                            let Some(&member_index) = indices_for_list.get(row_index) else {
                                return div().into_any_element();
                            };
                            let Some(member) = snapshot_for_list.members.get(member_index) else {
                                return div().into_any_element();
                            };
                            let Some(focus) = row_focuses.get(row_index).cloned() else {
                                return div().into_any_element();
                            };
                            render_swarm_member_row(
                                member,
                                snapshot_for_list.current_member.as_deref()
                                    == Some(member.name.as_str()),
                                active_filter.as_deref() == Some(member.name.as_str()),
                                focus,
                                waku.clone(),
                                &theme_copy,
                            )
                            .into_any_element()
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

fn render_swarm_message_row(
    message: &AntigravitySwarmMessage,
    current_member: Option<&str>,
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

    div()
        .px(px(10.0))
        .py(px(4.0))
        .child(
            div()
                .px(px(10.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(if mentions_me {
                    theme.accent.opacity(0.55)
                } else {
                    theme.border
                })
                .bg(if mentions_me {
                    theme.overlay
                } else {
                    theme.composer
                })
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .min_w_0()
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(sp(12.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(if is_from_me {
                                            theme.accent
                                        } else {
                                            theme.text
                                        })
                                        .child(message.agent.clone()),
                                )
                                .when(message.kind.as_deref() == Some("brief"), |row| {
                                    row.child(
                                        div()
                                            .flex_none()
                                            .px(px(5.0))
                                            .py(px(1.0))
                                            .rounded(px(4.0))
                                            .bg(theme.overlay)
                                            .text_size(sp(10.5))
                                            .text_color(theme.text_tertiary)
                                            .child(tr!("swarm.brief_badge")),
                                    )
                                })
                                .when_some(message.channel.as_ref(), |row, channel| {
                                    row.child(
                                        div()
                                            .flex_none()
                                            .px(px(5.0))
                                            .py(px(1.0))
                                            .rounded(px(4.0))
                                            .bg(theme.overlay)
                                            .text_size(sp(10.5))
                                            .text_color(theme.text_tertiary)
                                            .child(format!("#{channel}")),
                                    )
                                })
                                .when_some(recipients_label, |row, label| {
                                    row.child(
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
                                            .child(label),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(sp(11.0))
                                .text_color(theme.text_ghost)
                                .child(time_label),
                        ),
                )
                .child(
                    div()
                        .min_w_0()
                        .whitespace_normal()
                        .text_size(sp(12.5))
                        .line_height(sp(17.5))
                        .text_color(theme.text)
                        .child(message.text.clone()),
                ),
        )
}

fn render_swarm_member_row(
    member: &AntigravitySwarmMember,
    is_you: bool,
    is_filtered: bool,
    focus: FocusHandle,
    waku: gpui::WeakEntity<Waku>,
    theme: &Theme,
) -> Div {
    let status_color = member_status_color(member.status, theme);
    let status_icon = member_status_icon(member.status);
    let status_label = member_status_label(member.status);
    let member_name_click = member.name.clone();
    let member_name_key = member.name.clone();
    let click_waku = waku.clone();
    let key_waku = waku;

    let mut meta_parts = vec![
        tr!("swarm.steps_count", count = member.steps),
        tr!("swarm.posts_count", count = member.posts),
    ];
    if member.unread > 0 {
        meta_parts.push(tr!("swarm.unread_count", count = member.unread));
    }
    if member.status == AntigravitySwarmMemberStatus::Waiting {
        if member.wake_mode == "mentions" {
            meta_parts.push(tr!("swarm.wake_mentions"));
        } else if member.wake_mode == "anything" {
            meta_parts.push(tr!("swarm.wake_anything"));
        }
    }

    div()
        .px(px(10.0))
        .py(px(4.0))
        .child(
            div()
                .id(SharedString::from(format!("swarm-member-row-{}", member.name)))
                .track_focus(&focus)
                .tab_index(0)
                .tab_stop(true)
                .px(px(10.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(if member.failure.is_some() {
                    theme.danger.opacity(0.55)
                } else if is_filtered {
                    theme.accent.opacity(0.75)
                } else {
                    theme.border
                })
                .bg(theme.composer)
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
                .when(member.killed, |card| card.opacity(0.65))
                .flex()
                .flex_col()
                .gap(px(5.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(6.0))
                        .min_w_0()
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(icon(status_icon, 12.0, status_color))
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
                                    row.child(
                                        div()
                                            .flex_none()
                                            .px(px(5.0))
                                            .py(px(1.0))
                                            .rounded(px(4.0))
                                            .bg(theme.overlay_strong)
                                            .text_size(sp(10.5))
                                            .text_color(theme.accent)
                                            .child(tr!("swarm.you")),
                                    )
                                })
                                .when(!member.role.is_empty() && member.role != "worker", |row| {
                                    row.child(
                                        div()
                                            .flex_none()
                                            .px(px(5.0))
                                            .py(px(1.0))
                                            .rounded(px(4.0))
                                            .bg(theme.overlay)
                                            .text_size(sp(10.5))
                                            .text_color(theme.text_tertiary)
                                            .child(member.role.clone()),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .px(px(6.0))
                                .py(px(2.0))
                                .rounded(px(5.0))
                                .border_1()
                                .border_color(status_color.opacity(0.5))
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .child(div().size(px(6.0)).rounded_full().bg(status_color))
                                .child(
                                    div()
                                        .text_size(sp(11.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(status_color)
                                        .child(status_label),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .min_w_0()
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(sp(11.5))
                                .text_color(theme.text_tertiary)
                                .child(meta_parts.join(" · ")),
                        )
                        .when(!member.model.is_empty(), |row| {
                            row.child(
                                div()
                                    .flex_none()
                                    .max_w(px(140.0))
                                    .truncate()
                                    .text_size(sp(11.0))
                                    .text_color(theme.text_ghost)
                                    .child(member.model.clone()),
                            )
                        }),
                )
                .when_some(member.activity.as_ref(), |card, activity| {
                    let act_icon = if member.activity_kind.as_deref() == Some("thinking") {
                        "icons/sparkle.svg"
                    } else {
                        "icons/wrench.svg"
                    };
                    card.child(
                        div()
                            .px(px(7.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .bg(theme.overlay)
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .min_w_0()
                            .child(icon(act_icon, 11.0, theme.text_secondary))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_size(sp(11.5))
                                    .text_color(theme.text_secondary)
                                    .child(activity.clone()),
                            ),
                    )
                })
                .when_some(member.failure.as_ref(), |card, failure| {
                    let badge = match failure.kind {
                        AntigravitySwarmFailureKind::Quota => tr!("swarm.failure_quota"),
                        AntigravitySwarmFailureKind::Error => tr!("swarm.failure_error"),
                    };
                    card.child(
                        div()
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
    if !overview.headline.is_empty() || phase_label.is_some() || overview.as_of_seq > 0 {
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
                .child(
                    div()
                        .flex()
                        .items_center()
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
                        .when(overview.as_of_seq > 0, |row| {
                            row.child(
                                div()
                                    .text_size(sp(11.0))
                                    .text_color(theme.text_tertiary)
                                    .child(tr!("swarm.as_of_seq", seq = overview.as_of_seq)),
                            )
                        }),
                ),
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
}
