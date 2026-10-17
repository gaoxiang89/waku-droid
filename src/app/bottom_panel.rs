use super::*;

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

    /// The panel is nothing but the terminal: `TerminalView` carries its own
    /// toolbar (status, title, working directory), and the window header's
    /// always-visible toggle is the mouse way out. The resize handle along the
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
            && let Some(terminal) = self.bottom_panel_terminal.clone()
        {
            let focus_handle = terminal.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            self.bottom_panel_pending_terminal_focus = false;
        }
        let body = match self.bottom_panel_terminal.clone() {
            Some(terminal) => {
                terminal.update(cx, |terminal, _| {
                    terminal.set_panel_width(width);
                    terminal.set_panel_height(height);
                });
                terminal.into_any_element()
            }
            None => self
                .render_right_panel_empty_message(
                    tr!("right_panel.terminal_unavailable"),
                    tr!("right_panel.terminal_unavailable_description"),
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

    /// Keep exactly one terminal, recreated when the selected workspace moves
    /// to another directory or host — same contract as the right panel's
    /// per-surface terminals.
    pub(super) fn ensure_bottom_panel_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(working_directory) = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf)
        else {
            self.bottom_panel_terminal = None;
            return;
        };
        let matches_project = self
            .bottom_panel_terminal
            .as_ref()
            .is_some_and(|terminal| terminal.read(cx).working_directory() == working_directory);
        if !matches_project {
            // A desktop PTY would interpret a remote daemon's working
            // directory on the wrong machine, so remote windows run the shell
            // on the daemon host.
            let launcher = if self.selected_daemon().is_remote() {
                crate::terminal::TerminalLauncher::Daemon(self.selected_daemon().clone())
            } else {
                crate::terminal::TerminalLauncher::Local
            };
            self.bottom_panel_terminal =
                Some(cx.new(|cx| TerminalView::new(working_directory.clone(), launcher, cx)));
        }
    }
}
