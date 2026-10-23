use super::right_panel::{
    PanelTabs, TAB_SCROLL_FADE_WIDTH, TabScrollFadeSide, tab_scroll_fade, tab_scroll_reveal_guard,
};
use super::*;

/// Height of the tab strip that owns the panel's chrome — the same 34 px the
/// terminal's built-in toolbar occupied, so the grid keeps every pixel it had.
const BOTTOM_PANEL_TAB_STRIP_HEIGHT: f32 = 34.0;

impl Waku {
    /// The header toggle. Mirrors the right panel's toggle in size and
    /// treatment so the two read as a pair. Wears the terminal glyph and the
    /// panel's open state, and stays up whether the panel is open or closed —
    /// the mouse way both in and out.
    pub(super) fn render_bottom_panel_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = self.bottom_panel_visible;
        let focus = self.transcript_control_focus("toggle-bottom-panel", cx);
        div()
            .id("toggle-bottom-panel")
            .track_focus(&focus)
            .tab_index(0)
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
                "icons/terminal.svg",
                14.0,
                if active {
                    theme.text
                } else {
                    theme.text_tertiary
                },
            ))
            .tooltip(Tooltip::with_shortcut(
                tr!("bottom_panel.toggle"),
                crate::platform::primary_shortcut("⌘J", "Ctrl+J"),
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.set_bottom_panel_visible(!this.bottom_panel_visible, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.set_bottom_panel_visible(!this.bottom_panel_visible, cx);
                    cx.stop_propagation();
                }
            }))
    }

    /// One terminal tab behind a 34 px strip that replaces the terminal's own
    /// toolbar; the strip carries what the toolbar did (status, title) per
    /// tab. A tab's terminals fill the panel together as a split tree that
    /// the split shortcuts grow and the close shortcut shrinks. Terminals
    /// belong to the session they were opened in — switching sessions swaps
    /// the panel's tabs and visibility, and only closing a tab (or deleting
    /// the session) ends the process. The resize handle along the top edge
    /// rides along as an overlay.
    pub(super) fn render_bottom_panel(
        &mut self,
        height: f32,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        self.ensure_bottom_panel_terminal(cx);
        if self.bottom_panel_pending_terminal_focus
            && let Some(terminal) = self.active_bottom_panel_terminal()
        {
            let focus_handle = terminal.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            self.bottom_panel_pending_terminal_focus = false;
        }
        let body = match self.active_bottom_panel_group_index() {
            Some(group_index) => {
                let content_height = (height - BOTTOM_PANEL_TAB_STRIP_HEIGHT).max(0.0);
                let member = &self.bottom_panel_terminal_groups[group_index].root;
                let splits = self.render_bottom_panel_split(
                    group_index,
                    member,
                    1.0,
                    width,
                    content_height,
                    &[],
                    cx,
                );
                div()
                    .id("bottom-panel-body")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(self.render_bottom_panel_tab_strip(cx))
                    .child(splits)
                    .into_any_element()
            }
            None => self
                .render_right_panel_empty_message(
                    tr!("right_panel.terminal_unavailable"),
                    tr!("terminal.no_workspace_description"),
                    cx,
                )
                .into_any_element(),
        };
        div()
            .id("bottom-panel")
            .w_full()
            .h(px(height))
            .flex_none()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(Theme::current(cx).border_strong)
            .bg(Theme::current(cx).surface)
            .relative()
            .child(body)
            .child(self.render_bottom_panel_resize_handle(cx))
    }

    /// The active tab's split tree. Pane wrappers carry the split and close
    /// actions so they run against the focused pane — the key chords only
    /// reach the keymap from a bottom-panel terminal, and the action rides
    /// the same focus path back down.
    #[allow(clippy::too_many_arguments)]
    fn render_bottom_panel_split(
        &self,
        group_index: usize,
        member: &SplitMember,
        flex: f32,
        width: f32,
        height: f32,
        path: &[usize],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match member {
            SplitMember::Pane(terminal) => {
                // The mirrored box this pane will occupy; the terminal sizes
                // its grid from it, so it must match the flexbox arithmetic
                // below (grow weights, zero basis, flex-none dividers).
                terminal.update(cx, |terminal, _| {
                    terminal.set_panel_width(width);
                    terminal.set_panel_height(height);
                });
                let split_right_pane = terminal.clone();
                let split_down_pane = terminal.clone();
                let close_pane = terminal.clone();
                let resize_left_pane = terminal.clone();
                let resize_right_pane = terminal.clone();
                let resize_up_pane = terminal.clone();
                let resize_down_pane = terminal.clone();
                div()
                    .id(SharedString::from(format!(
                        "bottom-panel-pane-{:?}",
                        terminal.entity_id()
                    )))
                    .relative()
                    .overflow_hidden()
                    .flex_grow(flex)
                    .flex_basis(px(0.0))
                    .min_w_0()
                    .min_h_0()
                    .child(terminal.clone())
                    .on_action(cx.listener(move |this, _: &SplitTerminalRight, _, cx| {
                        this.split_bottom_panel_pane(&split_right_pane, Axis::Horizontal, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &SplitTerminalDown, _, cx| {
                        this.split_bottom_panel_pane(&split_down_pane, Axis::Vertical, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &CloseTerminalPane, _, cx| {
                        this.close_bottom_panel_pane(&close_pane, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &ResizeTerminalPaneLeft, _, cx| {
                        this.resize_bottom_panel_pane(&resize_left_pane, ResizeDirection::Left, cx);
                    }))
                    .on_action(
                        cx.listener(move |this, _: &ResizeTerminalPaneRight, _, cx| {
                            this.resize_bottom_panel_pane(
                                &resize_right_pane,
                                ResizeDirection::Right,
                                cx,
                            );
                        }),
                    )
                    .on_action(cx.listener(move |this, _: &ResizeTerminalPaneUp, _, cx| {
                        this.resize_bottom_panel_pane(&resize_up_pane, ResizeDirection::Up, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &ResizeTerminalPaneDown, _, cx| {
                        this.resize_bottom_panel_pane(&resize_down_pane, ResizeDirection::Down, cx);
                    }))
                    .into_any_element()
            }
            SplitMember::Axis(axis) => {
                let (child_sizes, space_per_flex) = axis_child_sizes(
                    match axis.axis {
                        Axis::Horizontal => width,
                        Axis::Vertical => height,
                    },
                    &axis.flexes,
                );
                let mut container = div()
                    .flex()
                    .when(axis.axis == Axis::Horizontal, |element| element.flex_row())
                    .when(axis.axis == Axis::Vertical, |element| element.flex_col())
                    .relative()
                    .overflow_hidden()
                    .flex_grow(flex)
                    .flex_basis(px(0.0))
                    .min_w_0()
                    .min_h_0();
                for (index, child) in axis.members.iter().enumerate() {
                    let child_width = match axis.axis {
                        Axis::Horizontal => child_sizes[index],
                        Axis::Vertical => width,
                    };
                    let child_height = match axis.axis {
                        Axis::Vertical => child_sizes[index],
                        Axis::Horizontal => height,
                    };
                    let mut child_path = path.to_vec();
                    child_path.push(index);
                    if index > 0 {
                        container = container.child(self.render_bottom_panel_divider(
                            group_index,
                            path.to_vec(),
                            index - 1,
                            axis.axis,
                            child_sizes[index - 1],
                            child_sizes[index],
                            space_per_flex,
                            cx,
                        ));
                    }
                    container = container.child(self.render_bottom_panel_split(
                        group_index,
                        child,
                        axis.flexes.get(index).copied().unwrap_or(1.0),
                        child_width,
                        child_height,
                        &child_path,
                        cx,
                    ));
                }
                container.into_any_element()
            }
        }
    }

    /// The draggable seam between two panes: a flex-none strip wider than
    /// its 1 px paint so the grab area is humane.
    #[allow(clippy::too_many_arguments)]
    fn render_bottom_panel_divider(
        &self,
        group_index: usize,
        path: Vec<usize>,
        divider_after: usize,
        axis: Axis,
        first_size: f32,
        second_size: f32,
        space_per_flex: f32,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let horizontal = axis == Axis::Horizontal;
        let (line, drag) = if horizontal {
            (
                div()
                    .absolute()
                    .left(px((SPLIT_DIVIDER_SIZE - 1.0) / 2.0))
                    .top_0()
                    .bottom_0()
                    .w(px(1.0))
                    .bg(theme.border_strong),
                div().w(px(SPLIT_DIVIDER_SIZE)).h_full().cursor_col_resize(),
            )
        } else {
            (
                div()
                    .absolute()
                    .top(px((SPLIT_DIVIDER_SIZE - 1.0) / 2.0))
                    .left_0()
                    .right_0()
                    .h(px(1.0))
                    .bg(theme.border_strong),
                div().h(px(SPLIT_DIVIDER_SIZE)).w_full().cursor_row_resize(),
            )
        };
        drag.id(SharedString::from(format!(
            "bottom-panel-divider-{group_index}-{}",
            path.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("-")
        )))
        .relative()
        .flex_none()
        .child(line)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.split_divider_drag = Some(SplitDividerDrag::new(
                    group_index,
                    path.clone(),
                    divider_after,
                    axis,
                    f32::from(event.position.x),
                    f32::from(event.position.y),
                    first_size,
                    second_size,
                    space_per_flex,
                ));
                cx.stop_propagation();
                cx.notify();
            }),
        )
    }

    /// The tab strip: one tab per group (status dot, label, pane count,
    /// close) plus the new-terminal and split buttons, laid out like the
    /// right panel's tab row — scrollable with edge fades when tabs overflow.
    fn render_bottom_panel_tab_strip(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = self.bottom_panel_active_group;
        let mut tabs = div()
            .id("bottom-panel-tabs")
            .h_full()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .gap(px(4.0))
            .overflow_x_scroll()
            .track_scroll(&self.bottom_panel_tabs_scroll_handle);
        for (index, group) in self.bottom_panel_terminal_groups.iter().enumerate() {
            let is_active = active == Some(index);
            let Some(reference) = group.active_or_first() else {
                continue;
            };
            let terminal = reference.read(cx);
            let exited = terminal.exited();
            let pane_count = group.pane_count();
            let mut label = bottom_panel_tab_label(terminal.title(), terminal.working_directory());
            if pane_count > 1 {
                label.push_str(&format!(" · {pane_count}"));
            }
            let close_weak = cx.entity().downgrade();
            let activate_weak = cx.entity().downgrade();
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("bottom-panel-tab-{index}")))
                    .h(px(26.0))
                    .min_w(px(90.0))
                    .max_w(px(160.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_default()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .when(is_active, |element| element.bg(theme.overlay_strong))
                    .when(!is_active, |element| {
                        element.hover(|element| element.bg(theme.overlay))
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "bottom-panel-tab-status-{index}"
                            )))
                            .size(px(6.0))
                            .flex_none()
                            .rounded_full()
                            .bg(if exited { theme.danger } else { theme.accent })
                            .when(exited, |element| {
                                element.tooltip(|window, cx| {
                                    Tooltip::new(tr!("terminal.process_exited")).build(window, cx)
                                })
                            }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(sp(12.5))
                            .text_color(if is_active {
                                theme.text
                            } else {
                                theme.text_secondary
                            })
                            .child(SharedString::from(label.clone())),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "close-bottom-panel-tab-{index}"
                            )))
                            .w(px(16.0))
                            .h(px(16.0))
                            .rounded(px(4.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(|element| element.bg(theme.overlay_strong))
                            .child(icon("icons/x.svg", 10.0, theme.text_tertiary))
                            .tooltip(move |window, cx| {
                                Tooltip::new(tr!("bottom_panel.close_tab", title = label))
                                    .build(window, cx)
                            })
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                let _ = close_weak.update(cx, |this, cx| {
                                    this.close_bottom_panel_group(index, cx);
                                });
                            }),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = activate_weak.update(cx, |this, cx| {
                            this.activate_bottom_panel_group(index, cx);
                        });
                    }),
            );
        }
        tabs = tabs.child(div().w(px(TAB_SCROLL_FADE_WIDTH)).h(px(1.0)).flex_none());

        let new_terminal_weak = cx.entity().downgrade();
        let split_weak = cx.entity().downgrade();
        let split_pane = self.active_bottom_panel_terminal();
        div()
            .id("bottom-panel-tab-strip")
            .h(px(BOTTOM_PANEL_TAB_STRIP_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(6.0))
            .pr(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(
                div()
                    .relative()
                    .h_full()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .child(tabs)
                    .when_some(
                        self.bottom_panel_pending_tab_reveal,
                        |element, tab_index| {
                            element.child(tab_scroll_reveal_guard(
                                self.bottom_panel_tabs_scroll_handle.clone(),
                                tab_index,
                                cx.entity().downgrade(),
                                PanelTabs::BottomPanel,
                            ))
                        },
                    )
                    .child(tab_scroll_fade(
                        self.bottom_panel_tabs_scroll_handle.clone(),
                        TabScrollFadeSide::Left,
                        theme.surface,
                    ))
                    .child(tab_scroll_fade(
                        self.bottom_panel_tabs_scroll_handle.clone(),
                        TabScrollFadeSide::Right,
                        theme.surface,
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        icon_button("new-bottom-panel-terminal", "icons/plus.svg", theme)
                            .tooltip(Tooltip::with_shortcut(
                                tr!("bottom_panel.new_terminal"),
                                crate::platform::primary_shortcut("⇧⌘J", "Ctrl+Shift+J"),
                            ))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                let _ = new_terminal_weak.update(cx, |this, cx| {
                                    this.open_bottom_panel_terminal(cx);
                                });
                            }),
                    )
                    .when_some(split_pane, |element, pane| {
                        element.child(
                            icon_button("split-bottom-panel-terminal", "icons/split.svg", theme)
                                .tooltip(Tooltip::with_shortcut(
                                    tr!("bottom_panel.split_right"),
                                    crate::platform::primary_shortcut("⇧⌘D", "Ctrl+Shift+D"),
                                ))
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    let _ = split_weak.update(cx, |this, cx| {
                                        this.split_bottom_panel_pane(&pane, Axis::Horizontal, cx);
                                    });
                                }),
                        )
                    }),
            )
    }

    /// The bar along the panel's top edge. It straddles the edge the way the
    /// sidebar's does; the composer above is pure GPUI, so no native-view
    /// inset is needed.
    fn render_bottom_panel_resize_handle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = self
            .panel_resize_drag
            .is_some_and(|drag| drag.target == PanelResizeTarget::BottomPanel);
        div()
            .id("bottom-panel-resize-handle")
            .absolute()
            .top(px(-5.0))
            .left_0()
            .w_full()
            .h(px(10.0))
            .group("bottom-panel-resize-handle")
            .cursor_row_resize()
            .child(
                div()
                    .absolute()
                    .top(px(5.0))
                    .left_0()
                    .w_full()
                    .h(px(2.0))
                    .bg(if active {
                        theme.resize_handle
                    } else {
                        gpui::transparent_black()
                    })
                    .group_hover("bottom-panel-resize-handle", |element| {
                        element.bg(theme.resize_handle)
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.begin_panel_resize(PanelResizeTarget::BottomPanel, event, window, cx);
                }),
            )
    }

    fn active_bottom_panel_group_index(&self) -> Option<usize> {
        self.bottom_panel_active_group
            .filter(|index| *index < self.bottom_panel_terminal_groups.len())
    }

    fn active_bottom_panel_terminal(&self) -> Option<Entity<TerminalView>> {
        self.bottom_panel_active_group
            .and_then(|index| self.bottom_panel_terminal_groups.get(index))
            .and_then(SplitGroup::active_or_first)
    }

    /// Show the panel with a fresh terminal — the `+` button, shortcut, and
    /// palette all funnel here. With no terminals open this just bootstraps
    /// the first one; with no workspace selected the panel opens empty.
    pub(super) fn open_bottom_panel_terminal(&mut self, cx: &mut Context<Self>) {
        if self.bottom_panel_terminal_groups.is_empty() {
            self.ensure_bottom_panel_terminal(cx);
        } else if let Some(working_directory) = self.selected_workspace_path() {
            let working_directory = working_directory.to_path_buf();
            let mut group =
                SplitGroup::single(self.new_bottom_panel_terminal(&working_directory, cx));
            if let Some(first) = group.active_or_first() {
                group.set_active(first.entity_id());
            }
            self.bottom_panel_terminal_groups.push(group);
            self.bottom_panel_active_group = Some(self.bottom_panel_terminal_groups.len() - 1);
        }
        if let Some(active) = self.bottom_panel_active_group {
            self.reveal_bottom_panel_tab(active);
            self.bottom_panel_pending_terminal_focus = true;
        }
        self.set_bottom_panel_visible(true, cx);
        cx.notify();
    }

    /// Create the panel's first terminal when none exists. Existing terminals
    /// are never recreated or dropped here: they belong to the session they
    /// were opened in and die only when their tab closes.
    pub(super) fn ensure_bottom_panel_terminal(&mut self, cx: &mut Context<Self>) {
        if !self.bottom_panel_terminal_groups.is_empty() {
            return;
        }
        let Some(working_directory) = self.selected_workspace_path() else {
            return;
        };
        let working_directory = working_directory.to_path_buf();
        let mut group = SplitGroup::single(self.new_bottom_panel_terminal(&working_directory, cx));
        if let Some(first) = group.active_or_first() {
            group.set_active(first.entity_id());
        }
        self.bottom_panel_terminal_groups.push(group);
        self.bottom_panel_active_group = Some(0);
    }

    fn new_bottom_panel_terminal(
        &self,
        working_directory: &Path,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        // A desktop PTY would interpret a remote daemon's working directory on
        // the wrong machine, so remote windows run the shell on the daemon
        // host.
        let launcher = if self.selected_daemon().is_remote() {
            crate::terminal::TerminalLauncher::Daemon(self.selected_daemon().clone())
        } else {
            crate::terminal::TerminalLauncher::Local
        };
        let terminal = cx.new(|cx| {
            TerminalView::new(working_directory.to_path_buf(), launcher, cx)
                .with_bottom_panel_tabs()
        });
        Self::show_terminal_notices(&terminal, cx);
        terminal
    }

    /// A terminal reports what a paste could not do; surface it as a toast.
    pub(super) fn show_terminal_notices(terminal: &Entity<TerminalView>, cx: &mut Context<Self>) {
        cx.subscribe(
            terminal,
            |this, _, notice: &crate::terminal::TerminalNotice, cx| {
                this.show_toast(notice.0.clone());
                cx.notify();
            },
        )
        .detach();
    }

    /// Park the selected workspace's panel state, the bottom panel's half of
    /// what `store_selected_right_panel_state` saves. Terminal entities move
    /// into the map, so their processes keep running off screen.
    pub(super) fn store_selected_bottom_panel_state(&mut self) {
        let Some(scope) = self
            .state
            .selected_session
            .and_then(|id| self.panel_scope_for_session(id))
        else {
            return;
        };
        let state = self.take_active_bottom_panel_state();
        self.bottom_panel_workspace_states.insert(scope, state);
    }

    /// Swap in the newly selected workspace's panel state. A workspace never
    /// seen before opens closed, exactly as the right panel does.
    pub(super) fn restore_bottom_panel_state(&mut self, session_id: Uuid) {
        let state = self
            .panel_scope_for_session(session_id)
            .map(|scope| {
                BottomPanelWorkspaceState::take_or_closed(
                    &mut self.bottom_panel_workspace_states,
                    &scope,
                )
            })
            .unwrap_or_else(|| BottomPanelWorkspaceState::empty(false));
        self.replace_active_bottom_panel_state(state);
        // The persisted layout flag follows whichever workspace is on screen,
        // so relaunching restores the panel the way it was left.
        self.state.bottom_panel_visible = self.bottom_panel_visible;
        if self.bottom_panel_visible && self.bottom_panel_active_group.is_some() {
            self.bottom_panel_pending_terminal_focus = true;
        }
    }

    /// Drop a deleted session's share of the bottom panel. Terminal groups
    /// belong to the workspace, so they outlive the session while any other
    /// session still opens into it; only the workspace's last session takes
    /// the PTYs down.
    pub(super) fn remove_bottom_panel_session_state(&mut self, session_id: Uuid) {
        let Some(scope) = self.panel_scope_for_session(session_id) else {
            return;
        };
        let workspace_still_open = self.state.sessions.iter().any(|session| {
            session.id != session_id
                && self.panel_scope_for_session(session.id).as_ref() == Some(&scope)
        });
        let removed = if self.state.selected_session == Some(session_id) {
            let removed = self.take_active_bottom_panel_state();
            self.replace_active_bottom_panel_state(BottomPanelWorkspaceState::empty(false));
            Some(removed)
        } else {
            self.bottom_panel_workspace_states.remove(&scope)
        };
        if workspace_still_open {
            if let Some(removed) = removed {
                self.bottom_panel_workspace_states.insert(scope, removed);
            }
        }
        // A workspace with no sessions left drops its groups, which is what
        // shuts the PTYs down.
    }

    fn take_active_bottom_panel_state(&mut self) -> BottomPanelWorkspaceState {
        BottomPanelWorkspaceState {
            visible: self.bottom_panel_visible,
            groups: std::mem::take(&mut self.bottom_panel_terminal_groups),
            active_group: self.bottom_panel_active_group.take(),
            tabs_scroll_handle: std::mem::replace(
                &mut self.bottom_panel_tabs_scroll_handle,
                ScrollHandle::new(),
            ),
            pending_tab_reveal: self.bottom_panel_pending_tab_reveal.take(),
        }
    }

    fn replace_active_bottom_panel_state(&mut self, state: BottomPanelWorkspaceState) {
        self.bottom_panel_visible = state.visible;
        self.bottom_panel_terminal_groups = state.groups;
        self.bottom_panel_active_group = state.active_group;
        self.bottom_panel_tabs_scroll_handle = state.tabs_scroll_handle;
        self.bottom_panel_pending_tab_reveal = state.pending_tab_reveal;
        self.bottom_panel_pending_terminal_focus = false;
    }

    fn activate_bottom_panel_group(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.bottom_panel_terminal_groups.len() {
            return;
        }
        self.bottom_panel_active_group = Some(index);
        self.reveal_bottom_panel_tab(index);
        self.bottom_panel_pending_terminal_focus = true;
        cx.notify();
    }

    fn reveal_bottom_panel_tab(&mut self, index: usize) {
        self.bottom_panel_pending_tab_reveal = Some(index);
        self.bottom_panel_tabs_scroll_handle.scroll_to_item(index);
    }

    /// Split one pane in two. The new pane takes the focus so consecutive
    /// splits stack where the user is looking.
    pub(super) fn split_bottom_panel_pane(
        &mut self,
        pane: &Entity<TerminalView>,
        axis: Axis,
        cx: &mut Context<Self>,
    ) {
        let Some(group_index) = self
            .bottom_panel_terminal_groups
            .iter()
            .position(|group| group.contains(pane.entity_id()))
        else {
            return;
        };
        if self.bottom_panel_terminal_groups[group_index].pane_count() >= MAX_SPLIT_PANES {
            return;
        }
        let Some(working_directory) = self.selected_workspace_path() else {
            return;
        };
        let working_directory = working_directory.to_path_buf();
        let new_pane = self.new_bottom_panel_terminal(&working_directory, cx);
        let group = &mut self.bottom_panel_terminal_groups[group_index];
        if !group.split_pane(pane.entity_id(), new_pane, axis) {
            return;
        }
        self.bottom_panel_active_group = Some(group_index);
        self.reveal_bottom_panel_tab(group_index);
        self.bottom_panel_pending_terminal_focus = true;
        cx.notify();
    }

    /// Close one pane. Closing the last pane of a tab closes the tab; closing
    /// the last tab hides the panel, the way the right panel does.
    pub(super) fn close_bottom_panel_pane(
        &mut self,
        pane: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let Some(group_index) = self
            .bottom_panel_terminal_groups
            .iter()
            .position(|group| group.contains(pane.entity_id()))
        else {
            return;
        };
        if self.bottom_panel_terminal_groups[group_index].remove_pane(pane.entity_id()) {
            self.bottom_panel_pending_terminal_focus = true;
            cx.notify();
            return;
        }
        self.close_bottom_panel_group(group_index, cx);
    }

    /// Resize one split pane along the chosen direction.
    pub(super) fn resize_bottom_panel_pane(
        &mut self,
        pane: &Entity<TerminalView>,
        direction: ResizeDirection,
        cx: &mut Context<Self>,
    ) {
        let Some(group_index) = self
            .bottom_panel_terminal_groups
            .iter()
            .position(|group| group.contains(pane.entity_id()))
        else {
            return;
        };
        if self.bottom_panel_terminal_groups[group_index].resize_pane(pane.entity_id(), direction) {
            cx.notify();
        }
    }

    /// Close one terminal tab, together with every pane it still shows.
    /// Dropping the entities is what shuts the PTYs down, locally and on a
    /// remote daemon alike.
    pub(super) fn close_bottom_panel_group(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.bottom_panel_terminal_groups.len() {
            return;
        }
        self.bottom_panel_terminal_groups.remove(index);
        self.bottom_panel_active_group = active_index_after_close(
            index,
            self.bottom_panel_active_group,
            self.bottom_panel_terminal_groups.len(),
        );
        match self.bottom_panel_active_group {
            Some(active) => {
                self.reveal_bottom_panel_tab(active);
                self.bottom_panel_pending_terminal_focus = true;
            }
            None => {
                self.bottom_panel_pending_tab_reveal = None;
                self.bottom_panel_pending_terminal_focus = false;
                self.set_bottom_panel_visible(false, cx);
            }
        }
        cx.notify();
    }

    /// Dragging a divider re-weighs its two neighbors; the mirrored layout
    /// math turns the pointer delta into flex weights.
    pub(super) fn split_divider_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.split_divider_drag.clone() else {
            return;
        };
        let resized = self
            .bottom_panel_terminal_groups
            .get_mut(drag.group_index)
            .is_some_and(|group| {
                group.resize_divider(
                    &drag,
                    f32::from(event.position.x),
                    f32::from(event.position.y),
                )
            });
        if !resized {
            self.split_divider_drag = None;
        }
        cx.notify();
    }

    pub(super) fn finish_split_divider_drag(
        &mut self,
        event: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Left && self.split_divider_drag.take().is_some() {
            cx.notify();
        }
    }

    /// Cycle tabs for ctrl+pageup/pagedown, wrapping around the ends.
    pub(super) fn cycle_bottom_panel_terminal(&mut self, delta: i32, cx: &mut Context<Self>) {
        let len = self.bottom_panel_terminal_groups.len();
        if len == 0 {
            return;
        }
        let active = self.bottom_panel_active_group.unwrap_or(0) as i32;
        let next = (active + delta).rem_euclid(len as i32) as usize;
        self.activate_bottom_panel_group(next, cx);
    }

    pub(super) fn new_terminal_action(
        &mut self,
        _: &NewTerminal,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_bottom_panel_terminal(cx);
    }

    pub(super) fn next_terminal_action(
        &mut self,
        _: &NextTerminal,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_bottom_panel_terminal(1, cx);
    }

    pub(super) fn previous_terminal_action(
        &mut self,
        _: &PreviousTerminal,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_bottom_panel_terminal(-1, cx);
    }
}

/// Where the active index lands after closing tab `closed` from a list that
/// now holds `remaining` tabs — same fixup the right panel applies to its
/// surfaces.
fn active_index_after_close(
    closed: usize,
    active: Option<usize>,
    remaining: usize,
) -> Option<usize> {
    if remaining == 0 {
        return None;
    }
    Some(match active {
        Some(active) if active > closed => active - 1,
        Some(active) if active == closed => closed.saturating_sub(1).min(remaining - 1),
        Some(active) => active.min(remaining - 1),
        None => 0,
    })
}

/// The shell's OSC title once it sets one; the working directory's basename
/// until then — the part that tells several terminals of one workspace apart.
fn bottom_panel_tab_label(title: &str, working_directory: &Path) -> String {
    let title = title.trim();
    if !title.is_empty() && title != tr!("right_panel.terminal") {
        return single_line_label(title);
    }
    let directory = working_directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if directory.is_empty() {
        tr!("right_panel.terminal")
    } else {
        single_line_label(directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_index_survives_closing_any_tab() {
        // Closing a tab after the active one pulls the active index in.
        assert_eq!(active_index_after_close(0, Some(2), 2), Some(1));
        // Closing the active tab steps back to the previous one, the way the
        // right panel does; at the start of the row it stays on position 0.
        assert_eq!(active_index_after_close(1, Some(1), 2), Some(0));
        assert_eq!(active_index_after_close(2, Some(2), 2), Some(1));
        assert_eq!(active_index_after_close(0, Some(0), 2), Some(0));
        // Closing the only tab empties the panel.
        assert_eq!(active_index_after_close(0, Some(0), 0), None);
        // No active tab degenerates to the first one.
        assert_eq!(active_index_after_close(1, None, 2), Some(0));
    }

    #[test]
    fn take_or_closed_returns_stored_state_or_closed() {
        let scope = PanelScope {
            host: HostKey::Local,
            workspace: PathBuf::from("/repos/waku"),
        };
        let other_scope = PanelScope {
            host: HostKey::Local,
            workspace: PathBuf::from("/repos/other"),
        };
        let mut states = HashMap::new();
        let mut state = BottomPanelWorkspaceState::empty(true);
        state.active_group = Some(0);
        states.insert(scope.clone(), state);

        // A workspace with no stored panel state opens closed, exactly as the
        // right panel treats one never visited before.
        let missing = BottomPanelWorkspaceState::take_or_closed(&mut states, &other_scope);
        assert!(!missing.visible);
        assert_eq!(missing.active_group, None);

        // Taking hands the stored state over once, so a workspace opened twice
        // without being switched away from in between reads closed the second
        // time — the map, not the flag, is the source of truth. Every session
        // of the workspace reads the same entry.
        let restored = BottomPanelWorkspaceState::take_or_closed(&mut states, &scope);
        assert!(restored.visible);
        assert_eq!(restored.active_group, Some(0));
        let again = BottomPanelWorkspaceState::take_or_closed(&mut states, &scope);
        assert!(!again.visible);
        assert!(again.groups.is_empty());
    }

    #[test]
    fn tab_label_prefers_title_then_directory() {
        // A shell-set OSC title wins.
        assert_eq!(
            bottom_panel_tab_label("cargo build", Path::new("/proj/waku")),
            "cargo build"
        );
        // Without one, the workspace's basename tells tabs apart.
        assert_eq!(
            bottom_panel_tab_label("Terminal", Path::new("/proj/waku")),
            "waku"
        );
        assert_eq!(bottom_panel_tab_label("", Path::new("/proj/waku")), "waku");
        // A directory without a basename falls back to the generic label.
        let fallback = bottom_panel_tab_label("", Path::new("/"));
        assert_eq!(fallback, tr!("right_panel.terminal"));
    }
}
