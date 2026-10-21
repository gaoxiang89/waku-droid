use super::right_panel::{
    PanelTabs, TAB_SCROLL_FADE_WIDTH, TabScrollFadeSide, tab_scroll_fade, tab_scroll_reveal_guard,
};
use super::*;

/// Height of the tab strip that owns the panel's chrome — the same 34 px the
/// terminal's built-in toolbar occupied, so the grid keeps every pixel it had.
const BOTTOM_PANEL_TAB_STRIP_HEIGHT: f32 = 34.0;

impl Waku {
    /// The header toggle. Mirrors the right panel's toggle in size and
    /// treatment so the two read as a pair.
    pub(super) fn render_bottom_panel_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id("toggle-bottom-panel")
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .child(icon("icons/panel-bottom.svg", 14.0, theme.text_tertiary))
            .tooltip(|window, cx| Tooltip::new(tr!("bottom_panel.toggle")).build(window, cx))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.set_bottom_panel_visible(!this.bottom_panel_visible, cx);
            }))
    }

    /// One terminal per tab behind a 34 px strip that replaces the terminal's
    /// own toolbar; the strip carries what the toolbar did (status, title) per
    /// tab. Terminals belong to the session they were opened in — switching
    /// sessions swaps the panel's tabs and visibility, and only closing a tab
    /// (or deleting the session) ends the process. The resize handle along the
    /// top edge rides along as an overlay.
    pub(super) fn render_bottom_panel(
        &mut self,
        height: f32,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        self.ensure_bottom_panel_terminal(cx);
        if self.bottom_panel_pending_terminal_focus
            && let Some(terminal) = self.active_bottom_panel_terminal().cloned()
        {
            let focus_handle = terminal.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            self.bottom_panel_pending_terminal_focus = false;
        }
        let body = match self.active_bottom_panel_terminal().cloned() {
            Some(terminal) => {
                terminal.update(cx, |terminal, _| {
                    terminal.set_panel_width(width);
                    terminal
                        .set_panel_height((height - BOTTOM_PANEL_TAB_STRIP_HEIGHT).max(0.0));
                });
                div()
                    .id("bottom-panel-body")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(self.render_bottom_panel_tab_strip(cx))
                    .child(terminal)
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

    /// The tab strip: one tab per terminal (status dot, label, close) plus the
    /// new-terminal button, laid out like the right panel's tab row —
    /// scrollable with edge fades when tabs overflow.
    fn render_bottom_panel_tab_strip(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = self.bottom_panel_active_terminal;
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
        for (index, terminal) in self.bottom_panel_terminals.iter().enumerate() {
            let is_active = active == Some(index);
            let terminal = terminal.read(cx);
            let exited = terminal.exited();
            let label = bottom_panel_tab_label(terminal.title(), terminal.working_directory());
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
                            .bg(if exited {
                                theme.danger
                            } else {
                                theme.accent
                            })
                            .when(exited, |element| {
                                element.tooltip(|window, cx| {
                                    Tooltip::new(tr!("terminal.process_exited"))
                                        .build(window, cx)
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
                                    this.close_bottom_panel_terminal(index, cx);
                                });
                            }),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = activate_weak.update(cx, |this, cx| {
                            this.activate_bottom_panel_terminal(index, cx);
                        });
                    }),
            );
        }
        tabs = tabs.child(div().w(px(TAB_SCROLL_FADE_WIDTH)).h(px(1.0)).flex_none());

        let new_terminal_weak = cx.entity().downgrade();
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
                    .when_some(self.bottom_panel_pending_tab_reveal, |element, tab_index| {
                        element.child(tab_scroll_reveal_guard(
                            self.bottom_panel_tabs_scroll_handle.clone(),
                            tab_index,
                            cx.entity().downgrade(),
                            PanelTabs::BottomPanel,
                        ))
                    })
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
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        icon_button("new-bottom-panel-terminal", "icons/plus.svg", theme)
                            .tooltip(|window, cx| {
                                Tooltip::new(tr!("bottom_panel.new_terminal"))
                                    .build(window, cx)
                            })
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                let _ = new_terminal_weak.update(cx, |this, cx| {
                                    this.open_bottom_panel_terminal(cx);
                                });
                            }),
                    ),
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

    fn active_bottom_panel_terminal(&self) -> Option<&Entity<TerminalView>> {
        self.bottom_panel_active_terminal
            .and_then(|index| self.bottom_panel_terminals.get(index))
    }

    /// Show the panel with a fresh terminal — the `+` button, shortcut, and
    /// palette all funnel here. With no terminals open this just bootstraps
    /// the first one; with no workspace selected the panel opens empty.
    pub(super) fn open_bottom_panel_terminal(&mut self, cx: &mut Context<Self>) {
        if self.bottom_panel_terminals.is_empty() {
            self.ensure_bottom_panel_terminal(cx);
        } else if let Some(working_directory) = self.selected_workspace_path() {
            let working_directory = working_directory.to_path_buf();
            self.bottom_panel_terminals
                .push(self.new_bottom_panel_terminal(&working_directory, cx));
            self.bottom_panel_active_terminal = Some(self.bottom_panel_terminals.len() - 1);
        }
        if let Some(active) = self.bottom_panel_active_terminal {
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
        if !self.bottom_panel_terminals.is_empty() {
            return;
        }
        let Some(working_directory) = self.selected_workspace_path() else {
            return;
        };
        let working_directory = working_directory.to_path_buf();
        self.bottom_panel_terminals
            .push(self.new_bottom_panel_terminal(&working_directory, cx));
        self.bottom_panel_active_terminal = Some(0);
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

    /// Park the selected session's panel state, the bottom panel's half of
    /// what `store_selected_right_panel_state` saves. Terminal entities move
    /// into the map, so their processes keep running off screen.
    pub(super) fn store_selected_bottom_panel_state(&mut self) {
        let Some(session_id) = self.state.selected_session else {
            return;
        };
        let state = self.take_active_bottom_panel_state();
        self.bottom_panel_session_states.insert(session_id, state);
    }

    /// Swap in the newly selected session's panel state. A session never
    /// seen before opens closed, exactly as the right panel does.
    pub(super) fn restore_bottom_panel_state(&mut self, session_id: Uuid) {
        let state = BottomPanelSessionState::take_or_closed(
            &mut self.bottom_panel_session_states,
            session_id,
        );
        self.replace_active_bottom_panel_state(state);
        // The persisted layout flag follows whichever session is on screen, so
        // relaunching restores the panel the way this session left it.
        self.state.bottom_panel_visible = self.bottom_panel_visible;
        if self.bottom_panel_visible && self.bottom_panel_active_terminal.is_some() {
            self.bottom_panel_pending_terminal_focus = true;
        }
    }

    /// Drop a session's stored panel state, together with its terminals when
    /// it is the selected one — the same split the right panel applies to its
    /// surfaces. Dropping the entities is what shuts the PTYs down.
    pub(super) fn remove_bottom_panel_session_state(&mut self, session_id: Uuid) {
        let removed = if self.state.selected_session == Some(session_id) {
            let removed = self.take_active_bottom_panel_state();
            self.replace_active_bottom_panel_state(BottomPanelSessionState::empty(false));
            removed
        } else {
            self.bottom_panel_session_states
                .remove(&session_id)
                .unwrap_or_else(|| BottomPanelSessionState::empty(false))
        };
        drop(removed);
    }

    fn take_active_bottom_panel_state(&mut self) -> BottomPanelSessionState {
        BottomPanelSessionState {
            visible: self.bottom_panel_visible,
            terminals: std::mem::take(&mut self.bottom_panel_terminals),
            active_terminal: self.bottom_panel_active_terminal.take(),
            tabs_scroll_handle: std::mem::replace(
                &mut self.bottom_panel_tabs_scroll_handle,
                ScrollHandle::new(),
            ),
            pending_tab_reveal: self.bottom_panel_pending_tab_reveal.take(),
        }
    }

    fn replace_active_bottom_panel_state(&mut self, state: BottomPanelSessionState) {
        self.bottom_panel_visible = state.visible;
        self.bottom_panel_terminals = state.terminals;
        self.bottom_panel_active_terminal = state.active_terminal;
        self.bottom_panel_tabs_scroll_handle = state.tabs_scroll_handle;
        self.bottom_panel_pending_tab_reveal = state.pending_tab_reveal;
        self.bottom_panel_pending_terminal_focus = false;
    }

    fn activate_bottom_panel_terminal(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.bottom_panel_terminals.len() {
            return;
        }
        self.bottom_panel_active_terminal = Some(index);
        self.reveal_bottom_panel_tab(index);
        self.bottom_panel_pending_terminal_focus = true;
        cx.notify();
    }

    fn reveal_bottom_panel_tab(&mut self, index: usize) {
        self.bottom_panel_pending_tab_reveal = Some(index);
        self.bottom_panel_tabs_scroll_handle.scroll_to_item(index);
    }

    /// Close one terminal's tab. Dropping the entity is what shuts the PTY
    /// down, locally and on a remote daemon alike. Closing the last tab hides
    /// the panel, the way the right panel does.
    pub(super) fn close_bottom_panel_terminal(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.bottom_panel_terminals.len() {
            return;
        }
        self.bottom_panel_terminals.remove(index);
        self.bottom_panel_active_terminal = active_index_after_close(
            index,
            self.bottom_panel_active_terminal,
            self.bottom_panel_terminals.len(),
        );
        match self.bottom_panel_active_terminal {
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

    /// Cycle tabs for ctrl+pageup/pagedown, wrapping around the ends.
    pub(super) fn cycle_bottom_panel_terminal(&mut self, delta: i32, cx: &mut Context<Self>) {
        let len = self.bottom_panel_terminals.len();
        if len == 0 {
            return;
        }
        let active = self.bottom_panel_active_terminal.unwrap_or(0) as i32;
        let next = (active + delta).rem_euclid(len as i32) as usize;
        self.activate_bottom_panel_terminal(next, cx);
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
        let session = Uuid::new_v4();
        let other_session = Uuid::new_v4();
        let mut states = HashMap::new();
        let mut state = BottomPanelSessionState::empty(true);
        state.active_terminal = Some(0);
        states.insert(session, state);

        // A session with no stored panel state opens closed, exactly as the
        // right panel treats one never visited before.
        let missing = BottomPanelSessionState::take_or_closed(&mut states, other_session);
        assert!(!missing.visible);
        assert_eq!(missing.active_terminal, None);

        // Taking hands the stored state over once, so a session opened twice
        // without being switched away from in between reads closed the second
        // time — the map, not the flag, is the source of truth.
        let restored = BottomPanelSessionState::take_or_closed(&mut states, session);
        assert!(restored.visible);
        assert_eq!(restored.active_terminal, Some(0));
        let again = BottomPanelSessionState::take_or_closed(&mut states, session);
        assert!(!again.visible);
        assert!(again.terminals.is_empty());
    }

    #[test]
    fn tab_label_prefers_title_then_directory() {
        // A shell-set OSC title wins.
        assert_eq!(
            bottom_panel_tab_label("cargo build", Path::new("/proj/waku")),
            "cargo build"
        );
        // Without one, the workspace's basename tells tabs apart.
        assert_eq!(bottom_panel_tab_label("Terminal", Path::new("/proj/waku")), "waku");
        assert_eq!(bottom_panel_tab_label("", Path::new("/proj/waku")), "waku");
        // A directory without a basename falls back to the generic label.
        let fallback = bottom_panel_tab_label("", Path::new("/"));
        assert_eq!(fallback, tr!("right_panel.terminal"));
    }
}
