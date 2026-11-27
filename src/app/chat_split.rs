use super::*;

pub(super) const CHAT_SPLIT_DIVIDER_WIDTH: f32 = 1.0;
pub(super) const CHAT_PANE_HEADER_HEIGHT: f32 = 30.0;

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChatPaneRole {
    Focused,
    Unfocused,
}

macro_rules! chat_view_state {
    ($($field:ident : $ty:ty),* $(,)?) => {
        pub(super) struct ChatViewState {
            $(pub(super) $field: $ty,)*
        }

        impl Waku {
            pub(super) fn exchange_chat_view_fields(&mut self, view: &mut ChatViewState) {
                $(std::mem::swap(&mut self.$field, &mut view.$field);)*
            }
        }
    };
}

chat_view_state! {
    composer: Entity<ComposerInput>,
    composer_attachments: Vec<ComposerAttachment>,
    composer_autocomplete: autocomplete::AutocompleteUi,
    message_edit: Option<MessageEdit>,
    activities_expanded: HashMap<usize, bool>,
    expanded_activity_items: HashMap<Uuid, bool>,
    unfolded_activity_runs: HashSet<Uuid>,
    expanded_turns: HashSet<Uuid>,
    expanded_changed_files: HashSet<Uuid>,
    expanded_user_messages: HashSet<Uuid>,
    transcript_control_focuses: RefCell<HashMap<String, FocusHandle>>,
    transcript_rows: ListState,
    anchored_transcript_rows: ListState,
    transcript_row_kinds: RefCell<Vec<TranscriptRowKind>>,
    transcript_row_kinds_fingerprint: Cell<Option<u64>>,
    transcript_navigation_turns: RefCell<Rc<Vec<TranscriptNavigationTurn>>>,
    transcript_navigation_turns_fingerprint: Cell<Option<u64>>,
    assistant_footer_cache: RefCell<HashMap<usize, (Option<SharedString>, Option<u64>)>>,
    assistant_footer_fingerprint: Cell<Option<u64>>,
    assistant_turn_provider_cache: RefCell<HashMap<usize, TurnAgentBadge>>,
    assistant_turn_provider_fingerprint: Cell<Option<u64>>,
    transcript_mixed_providers_state: Cell<Option<(Uuid, bool)>>,
    hovered_response_row: Option<(Uuid, TranscriptRowKind)>,
    checkpoint_ref_prefetch: Cell<Option<(Uuid, u64)>>,
    transcript_anchor: Cell<Option<TranscriptAnchor>>,
    transcript_anchor_end_space: Rc<Cell<Pixels>>,
    transcript_anchor_following: Rc<Cell<bool>>,
    transcript_tail_recheck: Rc<Cell<bool>>,
    transcript_is_scrolled: Rc<Cell<bool>>,
    transcript_scroll_to_bottom_visible: Cell<bool>,
    transcript_scrollbar_dragging: Cell<bool>,
    transcript_layout_width: Cell<Pixels>,
    message_markdown: RefCell<HashMap<Uuid, MarkdownView>>,
    user_message_viewports: RefCell<HashMap<Uuid, UserMessageScrollViewport>>,
    activity_markdown: RefCell<HashMap<Uuid, MarkdownView>>,
    reasoning_window_starts: RefCell<HashMap<Uuid, usize>>,
    activity_scroll_viewports: RefCell<HashMap<Uuid, ActivityScrollViewport>>,
    activity_diffs: RefCell<HashMap<Uuid, Rc<activity_diff::Diff>>>,
    activity_diff_viewports: RefCell<HashMap<Uuid, ActivityScrollViewport>>,
    transcript_selection: TranscriptSelection,
    transcript_focus: FocusHandle,
    transcript_search: Option<transcript_search::TranscriptSearch>,
    transcript_scrollbar: Rc<ScrollbarState>,
    menus: RefCell<HashMap<SharedString, ContextMenuHandle>>,
    navigation_rail: Entity<ConversationNavigationRail>,
    navigation_rail_reset_generation: Cell<u64>,
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

pub(super) fn split_chat_pane_width(column: f32, split: bool) -> f32 {
    if split {
        ((column - CHAT_SPLIT_DIVIDER_WIDTH) * 0.5).max(1.0)
    } else {
        column
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

        div()
            .id(side.header_id())
            .relative()
            .h(px(CHAT_PANE_HEADER_HEIGHT))
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(12.0))
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
                element.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.focus_chat_pane(side, cx);
                    }),
                )
            })
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
    fn split_chat_pane_width_halves_column_minus_divider() {
        assert_eq!(split_chat_pane_width(900.0, false), 900.0);
        assert_eq!(split_chat_pane_width(901.0, true), 450.0);
        assert_eq!(split_chat_pane_width(0.0, true), 1.0);
    }
}
