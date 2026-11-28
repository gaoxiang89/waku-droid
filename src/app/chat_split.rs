use super::*;

pub(super) const CHAT_SPLIT_DIVIDER_WIDTH: f32 = 1.0;
pub(super) const CHAT_PANE_HEADER_HEIGHT: f32 = 30.0;
/// Narrowest a split pane may get, by dragging or by shrinking the window.
/// Below twice this the split shows only its focused pane.
pub(super) const CHAT_PANE_MIN_WIDTH: f32 = 360.0;
pub(super) const DEFAULT_CHAT_SPLIT_RATIO: f32 = 0.5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChatPaneSide {
    Left,
    Right,
}

impl ChatPaneSide {
    pub(super) const fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }

    pub(super) const fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }

    pub(super) const fn pane_id(self) -> &'static str {
        match self {
            Self::Left => "chat-pane-left",
            Self::Right => "chat-pane-right",
        }
    }

    pub(super) const fn body_id(self) -> &'static str {
        match self {
            Self::Left => "chat-pane-body-left",
            Self::Right => "chat-pane-body-right",
        }
    }

    pub(super) const fn header_id(self) -> &'static str {
        match self {
            Self::Left => "chat-pane-header-left",
            Self::Right => "chat-pane-header-right",
        }
    }

    pub(super) const fn close_id(self) -> &'static str {
        match self {
            Self::Left => "chat-pane-close-left",
            Self::Right => "chat-pane-close-right",
        }
    }

    pub(super) const fn icon(self) -> &'static str {
        match self {
            Self::Left => "icons/panel-left.svg",
            Self::Right => "icons/panel-right.svg",
        }
    }

    pub(super) const fn focus_shortcut(self) -> &'static str {
        match self {
            Self::Left => crate::platform::primary_shortcut("⌥⌘←", "Ctrl+Alt+Left"),
            Self::Right => crate::platform::primary_shortcut("⌥⌘→", "Ctrl+Alt+Right"),
        }
    }

    pub(super) const fn switch_id(self) -> &'static str {
        match self {
            Self::Left => "chat-pane-switch-left",
            Self::Right => "chat-pane-switch-right",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChatPaneRole {
    Focused,
    Unfocused,
}

/// Everything one chat pane owns: its composer, transcript list state, and
/// the per-conversation caches and disclosures its rows read. The focused
/// pane's copy lives on [`Waku::view`]; a split's other pane keeps its own in
/// [`ChatSplit::view`], and [`Waku::with_chat_view`] swaps the two whole.
/// Per-conversation state belongs here, never directly on `Waku`, or it
/// would leak between the two panes.
pub(super) struct ChatViewState {
    pub(super) composer: Entity<ComposerInput>,
    /// Files dropped onto the composer, drawn as chips above the input and
    /// drained into the next submission.
    pub(super) composer_attachments: Vec<ComposerAttachment>,
    pub(super) composer_autocomplete: autocomplete::AutocompleteUi,
    pub(super) message_edit: Option<MessageEdit>,
    /// User expansion overrides keyed by persisted transcript block index.
    pub(super) activities_expanded: HashMap<usize, bool>,
    /// Per-item disclosure overrides. Reasoning starts open while live; tool
    /// details start closed, so the stored bool must preserve either choice.
    pub(super) expanded_activity_items: HashMap<Uuid, bool>,
    /// Tool runs in a running turn that the user unfolded past their recent
    /// tail, keyed by the run's first activity.
    pub(super) unfolded_activity_runs: HashSet<Uuid>,
    /// Settled turns whose folded work the user has reopened.
    pub(super) expanded_turns: HashSet<Uuid>,
    /// Per-response file cards the user expanded beyond their three-file
    /// preview. Runtime-only, like the other transcript disclosures.
    pub(super) expanded_changed_files: HashSet<Uuid>,
    /// Long user messages the user expanded beyond their compact preview.
    pub(super) expanded_user_messages: HashSet<Uuid>,
    /// Stable focus identities for controls inside virtualized transcript and
    /// diff rows. Recreating a handle on every row build would drop keyboard
    /// focus whenever GPUI re-renders the list.
    pub(super) transcript_control_focuses: RefCell<HashMap<String, FocusHandle>>,
    pub(super) transcript_rows: ListState,
    /// Active turns use top alignment so row remeasurement cannot invoke the
    /// bottom-aligned list's implicit pin and displace the sent-message anchor.
    pub(super) anchored_transcript_rows: ListState,
    pub(super) transcript_row_kinds: RefCell<Vec<TranscriptRowKind>>,
    /// Fingerprint of the transcript inputs `transcript_row_kinds` was folded
    /// from, so an unchanged transcript costs nothing on a frame. `None` until
    /// the first fold. See `transcript_rows_fingerprint`.
    pub(super) transcript_row_kinds_fingerprint: Cell<Option<u64>>,
    /// The navigation rail's turn list, shared by `Rc` so a frame hands the
    /// rail a pointer instead of re-extracting every turn's snippets. Rebuilt
    /// by `navigation_turns` when the row-kinds fingerprint moves.
    pub(super) transcript_navigation_turns: RefCell<Rc<Vec<TranscriptNavigationTurn>>>,
    /// The row-kinds fingerprint `transcript_navigation_turns` was built from.
    pub(super) transcript_navigation_turns_fingerprint: Cell<Option<u64>>,
    /// Response-footer copy content and completion time per message index,
    /// rebuilt when the row-kinds fingerprint moves. The row builder asks for
    /// every visible row on every frame, and the underlying turn walk and
    /// answer join are O(session). Footers exist only for settled turns,
    /// whose parts are immutable, and settling moves the fingerprint.
    pub(super) assistant_footer_cache: RefCell<HashMap<usize, (Option<SharedString>, Option<u64>)>>,
    /// The row-kinds fingerprint `assistant_footer_cache` was built under.
    pub(super) assistant_footer_fingerprint: Cell<Option<u64>>,
    /// Provider badge per visible assistant message index in a mixed-agent
    /// session, rebuilt in one linear pass when the row-kinds fingerprint
    /// moves so `transcript_row` stays O(1) per visible row.
    pub(super) assistant_turn_provider_cache: RefCell<HashMap<usize, TurnAgentBadge>>,
    /// The row-kinds fingerprint `assistant_turn_provider_cache` was built under.
    pub(super) assistant_turn_provider_fingerprint: Cell<Option<u64>>,
    /// Selected `(session_id, has_mixed_providers)` last reconciled with the
    /// transcript `ListState`s, so flipping `false <-> true` forces a full
    /// re-measure of rows that gained or lost their provider header.
    pub(super) transcript_mixed_providers_state: Cell<Option<(Uuid, bool)>>,
    /// The response row currently under the pointer. Response footers are
    /// separate virtual-list rows, so GPUI's ancestor-scoped `group_hover`
    /// cannot reveal them when a sibling response row is hovered.
    pub(super) hovered_response_row: Option<(Uuid, TranscriptRowKind)>,
    /// The (session, generation) the latest scheduled prefetch covers.
    pub(super) checkpoint_ref_prefetch: Cell<Option<(Uuid, u64)>>,
    pub(super) transcript_anchor: Cell<Option<TranscriptAnchor>>,
    pub(super) transcript_anchor_end_space: Rc<Cell<Pixels>>,
    pub(super) transcript_anchor_following: Rc<Cell<bool>>,
    /// A wheel scroll has landed and where it came to rest is not classified
    /// yet. The first frame that can measure the tail consumes this and
    /// re-engages following when the reader scrolled back onto it; a frame that
    /// cannot measure the tail leaves it set, so a stream remeasure cannot
    /// swallow the re-engage.
    pub(super) transcript_tail_recheck: Rc<Cell<bool>>,
    pub(super) transcript_is_scrolled: Rc<Cell<bool>>,
    /// Last decided visibility of the scroll-to-tail affordance. The tail's
    /// position is unknowable on the frames a stream commit remeasures it, and
    /// those arrive at commit cadence — deciding "show" from that silence
    /// strobes the button against the frames in between.
    pub(super) transcript_scroll_to_bottom_visible: Cell<bool>,
    /// Whether the transcript's scrollbar thumb was held at the last frame, so
    /// render can notice a drag starting and ending.
    pub(super) transcript_scrollbar_dragging: Cell<bool>,
    pub(super) transcript_layout_width: Cell<Pixels>,
    /// Parsed markdown per assistant message, keeping each response's
    /// incremental parse and flattened blocks alive across frames.
    pub(super) message_markdown: RefCell<HashMap<Uuid, MarkdownView>>,
    /// Stable offsets for capped user bubbles, including across virtualized row rebuilds.
    pub(super) user_message_viewports: RefCell<HashMap<Uuid, UserMessageScrollViewport>>,
    /// Parsed markdown for reasoning activities, keyed by stable activity id.
    pub(super) activity_markdown: RefCell<HashMap<Uuid, MarkdownView>>,
    /// Byte offsets live reasoning peeks render from, slid forward as the
    /// thought grows; see `live_reasoning_window_start`.
    pub(super) reasoning_window_starts: RefCell<HashMap<Uuid, usize>>,
    /// Independent capped viewports for expanded thoughts and command output.
    /// Keeping these stable preserves scroll position through virtualization.
    pub(super) activity_scroll_viewports: RefCell<HashMap<Uuid, ActivityScrollViewport>>,
    /// Positioned, syntax-tokenized diff rows for expanded file-change
    /// activities. Built once when the activity is expanded and dropped when it
    /// collapses or its changes are replaced, so a frame only indexes rows.
    pub(super) activity_diffs: RefCell<HashMap<Uuid, Rc<activity_diff::Diff>>>,
    /// Viewports for those diffs. Separate from `activity_scroll_viewports`
    /// because a failed edit shows both its diff and the error it returned.
    pub(super) activity_diff_viewports: RefCell<HashMap<Uuid, ActivityScrollViewport>>,
    /// Transcript-wide text selection, spanning messages and tool output.
    pub(super) transcript_selection: TranscriptSelection,
    /// Programmatic focus for the transcript canvas. Clicking the transcript
    /// moves focus here so the shared find action can distinguish it from the
    /// right-panel file editor without putting the canvas in the tab order.
    pub(super) transcript_focus: FocusHandle,
    /// Find-in-page state for the selected transcript, created lazily on the
    /// first primary-modifier F press.
    pub(super) transcript_search: Option<transcript_search::TranscriptSearch>,
    pub(super) transcript_scrollbar: Rc<ScrollbarState>,
    /// Every menu site in the app, keyed by a stable id. Handles are created on
    /// first use and live as long as the window.
    pub(super) menus: RefCell<HashMap<SharedString, ContextMenuHandle>>,
    pub(super) navigation_rail: Entity<ConversationNavigationRail>,
    pub(super) navigation_rail_reset_generation: Cell<u64>,
}

impl Waku {
    pub(super) fn exchange_chat_view_fields(&mut self, view: &mut ChatViewState) {
        std::mem::swap(&mut self.view, view);
    }
}

pub(super) fn transcript_list_states() -> (
    ListState,
    ListState,
    Rc<Cell<bool>>,
    Rc<Cell<bool>>,
    Rc<Cell<bool>>,
) {
    // Measure visible rows only, with a generous overdraw — the same shape
    // Zed's own agent chat uses. `measure_all` lays out every row in the
    // session on the first frame and again after any structural splice,
    // which a long transcript cannot afford.
    let transcript_rows = ListState::new(0, ListAlignment::Bottom, px(2048.0));
    let anchored_transcript_rows = ListState::new(0, ListAlignment::Top, px(2048.0));
    let transcript_is_scrolled = Rc::new(Cell::new(false));
    let transcript_anchor_following = Rc::new(Cell::new(false));
    let transcript_tail_recheck = Rc::new(Cell::new(false));
    // A wheel scroll drops tail following and asks the next measured frame
    // whether it landed back on the tail. GPUI re-engages its own tail pin
    // when a bottom-aligned list reaches the end — it represents that end as
    // no logical offset — but a turn renders through the top-aligned
    // anchored list, whose end is an ordinary offset, so only this can.
    transcript_rows.set_scroll_handler({
        let transcript_is_scrolled = transcript_is_scrolled.clone();
        let transcript_anchor_following = transcript_anchor_following.clone();
        let transcript_tail_recheck = transcript_tail_recheck.clone();
        move |event, window, _| {
            transcript_is_scrolled.set(event.is_scrolled);
            transcript_anchor_following.set(false);
            transcript_tail_recheck.set(true);
            window.refresh();
        }
    });
    anchored_transcript_rows.set_scroll_handler({
        let transcript_is_scrolled = transcript_is_scrolled.clone();
        let transcript_anchor_following = transcript_anchor_following.clone();
        let transcript_tail_recheck = transcript_tail_recheck.clone();
        move |event, window, _| {
            transcript_is_scrolled.set(event.is_scrolled);
            transcript_anchor_following.set(false);
            transcript_tail_recheck.set(true);
            window.refresh();
        }
    });
    (
        transcript_rows,
        anchored_transcript_rows,
        transcript_is_scrolled,
        transcript_anchor_following,
        transcript_tail_recheck,
    )
}

impl ChatViewState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Waku>) -> Self {
        let composer = cx.new(|cx| ComposerInput::new(window, cx).padding_x(px(14.0), cx));
        Waku::subscribe_composer_events(&composer, cx);
        let (
            transcript_rows,
            anchored_transcript_rows,
            transcript_is_scrolled,
            transcript_anchor_following,
            transcript_tail_recheck,
        ) = transcript_list_states();
        let navigation_rail = cx.new(|_| ConversationNavigationRail::new());
        let waku = cx.entity().downgrade();
        navigation_rail.update(cx, |rail, _| rail.set_waku(waku));
        Self {
            composer,
            composer_attachments: Vec::new(),
            composer_autocomplete: autocomplete::AutocompleteUi::new(),
            message_edit: None,
            activities_expanded: HashMap::new(),
            expanded_activity_items: HashMap::new(),
            unfolded_activity_runs: HashSet::new(),
            expanded_turns: HashSet::new(),
            expanded_changed_files: HashSet::new(),
            expanded_user_messages: HashSet::new(),
            transcript_control_focuses: RefCell::new(HashMap::new()),
            transcript_rows,
            anchored_transcript_rows,
            transcript_row_kinds: RefCell::new(Vec::new()),
            transcript_row_kinds_fingerprint: Cell::new(None),
            transcript_navigation_turns: RefCell::new(Rc::new(Vec::new())),
            transcript_navigation_turns_fingerprint: Cell::new(None),
            assistant_footer_cache: RefCell::new(HashMap::new()),
            assistant_footer_fingerprint: Cell::new(None),
            assistant_turn_provider_cache: RefCell::new(HashMap::new()),
            assistant_turn_provider_fingerprint: Cell::new(None),
            transcript_mixed_providers_state: Cell::new(None),
            hovered_response_row: None,
            checkpoint_ref_prefetch: Cell::new(None),
            transcript_anchor: Cell::new(None),
            transcript_anchor_end_space: Rc::new(Cell::new(Pixels::ZERO)),
            transcript_anchor_following,
            transcript_tail_recheck,
            transcript_is_scrolled,
            transcript_scroll_to_bottom_visible: Cell::new(false),
            transcript_scrollbar_dragging: Cell::new(false),
            transcript_layout_width: Cell::new(Pixels::ZERO),
            message_markdown: RefCell::new(HashMap::new()),
            user_message_viewports: RefCell::new(HashMap::new()),
            activity_markdown: RefCell::new(HashMap::new()),
            reasoning_window_starts: RefCell::new(HashMap::new()),
            activity_scroll_viewports: RefCell::new(HashMap::new()),
            activity_diffs: RefCell::new(HashMap::new()),
            activity_diff_viewports: RefCell::new(HashMap::new()),
            transcript_selection: TranscriptSelection::default(),
            transcript_focus: cx.focus_handle(),
            transcript_search: None,
            transcript_scrollbar: ScrollbarState::new(),
            menus: RefCell::new(HashMap::new()),
            navigation_rail,
            navigation_rail_reset_generation: Cell::new(0),
        }
    }
}

pub(super) struct ChatSplit {
    pub(super) session_id: Uuid,
    pub(super) project_id: Option<Uuid>,
    pub(super) focused_side: ChatPaneSide,
    pub(super) view: ChatViewState,
}

pub(super) enum ChatActivation {
    Replace,
    Preserve,
    FreshPane(ChatViewState),
}

/// Left and right pane widths for a chat column split at `ratio`, or `None`
/// when the column cannot fit two panes at [`CHAT_PANE_MIN_WIDTH`].
pub(super) fn chat_split_widths(column: f32, ratio: f32) -> Option<(f32, f32)> {
    let available = column - CHAT_SPLIT_DIVIDER_WIDTH;
    if available < CHAT_PANE_MIN_WIDTH * 2.0 {
        return None;
    }
    let left = (available * sanitize_chat_split_ratio(ratio))
        .clamp(CHAT_PANE_MIN_WIDTH, available - CHAT_PANE_MIN_WIDTH);
    Some((left, available - left))
}

pub(super) fn sanitize_chat_split_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        DEFAULT_CHAT_SPLIT_RATIO
    }
}

impl Waku {
    fn exchange_with_split(&mut self) -> bool {
        let Some(mut split) = self.chat_split.take() else {
            return false;
        };
        let Some(focused_session_id) = self.state.selected_session else {
            self.chat_split = Some(split);
            return false;
        };
        self.exchange_chat_view_fields(&mut split.view);
        let mut other_session_id = Some(split.session_id);
        std::mem::swap(&mut self.state.selected_session, &mut other_session_id);
        split.session_id = focused_session_id;
        std::mem::swap(&mut self.state.selected_project, &mut split.project_id);
        self.chat_split = Some(split);
        true
    }

    pub(super) fn with_chat_view<R>(
        &mut self,
        role: ChatPaneRole,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        if (role == ChatPaneRole::Unfocused) == self.chat_view_exchanged {
            return Some(f(self));
        }
        if self.chat_split.is_none() || self.state.selected_session.is_none() {
            return None;
        }
        if !self.exchange_with_split() {
            return None;
        }
        self.chat_view_exchanged = !self.chat_view_exchanged;
        let result = f(self);
        let restored = self.exchange_with_split();
        debug_assert!(restored);
        self.chat_view_exchanged = !self.chat_view_exchanged;
        Some(result)
    }

    pub(super) fn current_chat_pane_role(&self) -> ChatPaneRole {
        if self.chat_view_exchanged {
            ChatPaneRole::Unfocused
        } else {
            ChatPaneRole::Focused
        }
    }

    /// The side whose view is installed on `self` right now.
    pub(super) fn current_chat_pane_side(&self) -> ChatPaneSide {
        match self.chat_split.as_ref() {
            Some(split) if self.chat_view_exchanged => split.focused_side.other(),
            Some(split) => split.focused_side,
            None => ChatPaneSide::Left,
        }
    }

    /// Pane widths of the live split, or `None` without one or when the
    /// column is too narrow and the split shows its focused pane alone.
    pub(super) fn chat_split_layout(&self, window: &Window) -> Option<(f32, f32)> {
        self.chat_split.as_ref()?;
        chat_split_widths(self.chat_viewport_width(window), self.chat_split_ratio)
    }

    /// Width `side` lays out at within a chat column of `column` pixels.
    pub(super) fn chat_pane_column_width(&self, column: f32, side: ChatPaneSide) -> f32 {
        if self.chat_split.is_none() {
            return column;
        }
        match (chat_split_widths(column, self.chat_split_ratio), side) {
            (Some((left, _)), ChatPaneSide::Left) => left,
            (Some((_, right)), ChatPaneSide::Right) => right,
            (None, _) => column,
        }
    }

    pub(super) fn chat_role_for_side(&self, side: ChatPaneSide) -> ChatPaneRole {
        match self.chat_split.as_ref() {
            Some(split) if split.focused_side != side => ChatPaneRole::Unfocused,
            Some(_) => ChatPaneRole::Focused,
            None => match side {
                ChatPaneSide::Left => ChatPaneRole::Focused,
                ChatPaneSide::Right => ChatPaneRole::Unfocused,
            },
        }
    }

    pub(super) fn focus_chat_pane(&mut self, side: ChatPaneSide, cx: &mut Context<Self>) {
        if self.chat_view_exchanged {
            return;
        }
        let Some(split) = self.chat_split.as_ref() else {
            return;
        };
        if split.focused_side == side {
            return;
        }
        let target_session_id = split.session_id;
        if !self.state.sessions.iter().any(|session| {
            session.id == target_session_id && self.state.project_is_open(session.project_id)
        }) {
            return;
        }
        self.pending_session_activation = None;
        self.activate_session_with(target_session_id, ChatActivation::Preserve, cx);
    }

    /// Keyboard and command entry point: move to `side` and put the caret
    /// in its composer.
    pub(super) fn focus_chat_pane_from_keyboard(
        &mut self,
        side: ChatPaneSide,
        cx: &mut Context<Self>,
    ) {
        let Some(split) = self.chat_split.as_ref() else {
            return;
        };
        if split.focused_side != side {
            self.focus_chat_pane(side, cx);
        }
        self.composer_focus_pending = true;
        cx.notify();
    }

    pub(super) fn focus_other_chat_pane(&mut self, cx: &mut Context<Self>) {
        if let Some(side) = self.chat_split.as_ref().map(|split| split.focused_side.other()) {
            self.focus_chat_pane_from_keyboard(side, cx);
        }
    }

    pub(super) fn focus_left_chat_pane_action(
        &mut self,
        _: &FocusLeftChatPane,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_chat_pane_from_keyboard(ChatPaneSide::Left, cx);
    }

    pub(super) fn focus_right_chat_pane_action(
        &mut self,
        _: &FocusRightChatPane,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_chat_pane_from_keyboard(ChatPaneSide::Right, cx);
    }

    pub(super) fn reset_chat_split_ratio(&mut self, cx: &mut Context<Self>) {
        self.chat_split_ratio = DEFAULT_CHAT_SPLIT_RATIO;
        self.persist_panel_layout();
        cx.notify();
    }

    pub(super) fn focus_chat_pane_owning_composer(
        &mut self,
        composer: &Entity<ComposerInput>,
        cx: &mut Context<Self>,
    ) {
        if self.chat_view_exchanged {
            return;
        }
        let Some(split) = self.chat_split.as_ref() else {
            return;
        };
        if &split.view.composer == composer {
            let side = split.focused_side.other();
            self.focus_chat_pane(side, cx);
        }
    }

    pub(super) fn open_in_split(
        &mut self,
        session_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.chat_view_exchanged {
            return;
        }
        let Some((project_id, detail_loaded)) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| (session.project_id, session.detail_loaded))
        else {
            return;
        };
        if !self.state.project_is_open(project_id) {
            return;
        }
        if let Some(split) = self.chat_split.as_ref() {
            if split.session_id == session_id {
                let other = split.focused_side.other();
                self.focus_chat_pane(other, cx);
            } else if self.state.selected_session == Some(session_id) {
                // Already focused; nothing to do.
            } else {
                let other = split.focused_side.other();
                self.focus_chat_pane(other, cx);
                self.select_session(session_id, cx);
            }
            return;
        }
        if self.state.selected_session.is_none() || self.state.selected_session == Some(session_id) {
            self.select_session(session_id, cx);
            return;
        }
        let view = ChatViewState::new(window, cx);
        self.expand_sidebar_path_to_session(session_id);
        self.reveal_sidebar_session(session_id);
        if !detail_loaded {
            self.pending_split_open = Some((session_id, view));
            self.ensure_session_loaded(session_id, cx);
            cx.notify();
            return;
        }
        self.open_session_in_split(session_id, view, cx);
    }

    pub(super) fn open_session_in_split(
        &mut self,
        session_id: Uuid,
        view: ChatViewState,
        cx: &mut Context<Self>,
    ) {
        if self.chat_view_exchanged || self.chat_split.is_some() {
            return;
        }
        let Some(selected) = self.state.selected_session else {
            return;
        };
        if selected == session_id {
            return;
        }
        if !self.state.sessions.iter().any(|session| {
            session.id == session_id && self.state.project_is_open(session.project_id)
        }) {
            return;
        }
        self.pending_session_activation = None;
        self.activate_session_with(session_id, ChatActivation::FreshPane(view), cx);
        self.composer_focus_pending = true;
        cx.notify();
    }

    pub(super) fn discard_unfocused_chat_pane(&mut self, cx: &mut Context<Self>) {
        if self.chat_view_exchanged || self.chat_split.is_none() {
            return;
        }
        let _ = self.with_chat_view(ChatPaneRole::Unfocused, |this| {
            this.capture_and_save_current_composer_draft(cx);
        });
        self.chat_split = None;
        self.composer_focus_pending = true;
        cx.notify();
    }

    pub(super) fn close_chat_pane(&mut self, side: ChatPaneSide, cx: &mut Context<Self>) {
        if self.chat_view_exchanged {
            return;
        }
        let Some(split) = self.chat_split.as_ref() else {
            return;
        };
        if split.focused_side != side {
            self.discard_unfocused_chat_pane(cx);
            return;
        }
        let other = side.other();
        self.focus_chat_pane(other, cx);
        if self
            .chat_split
            .as_ref()
            .is_some_and(|split| split.focused_side != side)
        {
            self.discard_unfocused_chat_pane(cx);
        }
    }

    pub(super) fn toggle_chat_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_bottom_panel_maximized() {
            self.set_bottom_panel_maximized(false, cx);
            if self.chat_split.is_some() {
                return;
            }
        } else if self.chat_split.is_some() {
            self.discard_unfocused_chat_pane(cx);
            return;
        }
        let Some(project_id) = self.state.selected_project else {
            return;
        };
        if self
            .selected_session()
            .is_none_or(|session| !session.has_started())
        {
            self.show_toast(tr!("chat_split.already_new_task"));
            cx.notify();
            return;
        }
        let id = self.ensure_draft_session_for(project_id, self.state.last_provider);
        self.open_in_split(id, window, cx);
    }

    pub(super) fn toggle_chat_split_action(
        &mut self,
        _: &ToggleChatSplit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_chat_split(window, cx);
    }

    pub(super) fn reconcile_chat_split(&mut self) {
        if self.chat_view_exchanged {
            return;
        }
        let Some(split) = self.chat_split.as_ref() else {
            return;
        };
        let valid = self.state.selected_session != Some(split.session_id)
            && self.state.sessions.iter().any(|session| {
                session.id == split.session_id
                    && !session.archived
                    && self.state.project_is_open(session.project_id)
            });
        if !valid {
            self.chat_split = None;
        }
    }

    pub(super) fn render_chat_split_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = !self.is_bottom_panel_maximized() && self.chat_split.is_some();
        let focus = self.transcript_control_focus("toggle-chat-split", cx);
        div()
            .id("toggle-chat-split")
            .track_focus(&focus)
            .tab_index(0)
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
                "icons/split.svg",
                14.0,
                if active {
                    theme.text
                } else {
                    theme.text_tertiary
                },
            ))
            .tooltip(Tooltip::with_shortcut(
                tr!("chat_split.toggle"),
                crate::platform::primary_shortcut("⌘\\", "Ctrl+\\"),
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_chat_split(window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.toggle_chat_split(window, cx);
                    cx.stop_propagation();
                }
            }))
    }

    pub(super) fn render_chat_pane_header(
        &self,
        side: ChatPaneSide,
        focused: bool,
        title: String,
        hidden_other_title: Option<String>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let close_focus = self.chat_pane_close_focus[side.index()].clone();
        let close_button = div()
            .id(side.close_id())
            .track_focus(&close_focus)
            .tab_index(0)
            .w(px(22.0))
            .h(px(22.0))
            .flex_none()
            .rounded(px(5.0))
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
            .hover(|style| style.bg(theme.overlay))
            .active(|style| style.bg(theme.overlay_strong))
            .child(icon("icons/x.svg", 12.0, theme.text_tertiary))
            .tooltip(Tooltip::text(tr!("chat_split.close_pane")))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.close_chat_pane(side, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.close_chat_pane(side, cx);
                    cx.stop_propagation();
                }
            }));

        // Shown while the column is too narrow for both panes: the hidden
        // pane stays one click or keystroke away instead of vanishing.
        let other = side.other();
        let switch_chip = hidden_other_title.map(|other_title| {
            let focus = self.chat_pane_header_focus[other.index()].clone();
            div()
                .id(side.switch_id())
                .track_focus(&focus)
                .tab_index(0)
                .flex_none()
                .max_w(px(220.0))
                .h(px(22.0))
                .px(px(7.0))
                .rounded(px(5.0))
                .border_1()
                .border_color(theme.border)
                .flex()
                .items_center()
                .gap(px(5.0))
                .cursor_default()
                .focus_visible(|style| style.bg(theme.overlay).border_color(theme.accent))
                .hover(|style| style.bg(theme.overlay))
                .active(|style| style.bg(theme.overlay_strong))
                .child(icon(other.icon(), 12.0, theme.text_tertiary))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(sp(12.0))
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(other_title)),
                )
                .tooltip(Tooltip::with_shortcut(
                    tr!("chat_split.switch_pane"),
                    other.focus_shortcut(),
                ))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.focus_chat_pane_from_keyboard(other, cx);
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        this.focus_chat_pane_from_keyboard(other, cx);
                        cx.stop_propagation();
                    }
                }))
        });

        let header_focus = self.chat_pane_header_focus[side.index()].clone();
        div()
            .id(side.header_id())
            .relative()
            .h(px(CHAT_PANE_HEADER_HEIGHT))
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(10.0))
            .pr(px(6.0))
            .border_b_1()
            .border_color(theme.border)
            .when(focused, |element| {
                element.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(2.0))
                        .bg(theme.accent),
                )
            })
            .when(!focused, |element| {
                element
                    .track_focus(&header_focus)
                    .tab_index(0)
                    .focus_visible(|style| style.bg(theme.overlay))
                    .tooltip(Tooltip::with_shortcut(
                        tr!("chat_split.focus_pane"),
                        side.focus_shortcut(),
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.focus_chat_pane(side, cx);
                        }),
                    )
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            this.focus_chat_pane_from_keyboard(side, cx);
                            cx.stop_propagation();
                        }
                    }))
            })
            .child(icon(
                side.icon(),
                12.0,
                if focused {
                    theme.accent
                } else {
                    theme.text_tertiary
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(sp(12.5))
                    .when(focused, |element| {
                        element.text_color(theme.text).font_weight(FontWeight::MEDIUM)
                    })
                    .when(!focused, |element| element.text_color(theme.text_tertiary))
                    .child(SharedString::from(title)),
            )
            .children(switch_chip)
            .child(close_button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_pane_side_other_round_trips() {
        assert_eq!(ChatPaneSide::Left.other(), ChatPaneSide::Right);
        assert_eq!(ChatPaneSide::Right.other(), ChatPaneSide::Left);
        assert_eq!(ChatPaneSide::Left.index(), 0);
        assert_eq!(ChatPaneSide::Right.index(), 1);
    }

    #[test]
    fn chat_split_widths_follow_ratio_within_minimums() {
        assert_eq!(chat_split_widths(901.0, 0.5), Some((450.0, 450.0)));
        assert_eq!(chat_split_widths(1001.0, 0.7), Some((640.0, 360.0)));
        assert_eq!(chat_split_widths(1001.0, 0.0), Some((360.0, 640.0)));
        assert_eq!(chat_split_widths(1001.0, f32::NAN), Some((500.0, 500.0)));
    }

    #[test]
    fn chat_split_widths_collapse_below_two_minimum_panes() {
        assert_eq!(chat_split_widths(720.0, 0.5), None);
        assert_eq!(chat_split_widths(721.0, 0.5), Some((360.0, 360.0)));
        assert_eq!(chat_split_widths(0.0, 0.5), None);
    }
}
