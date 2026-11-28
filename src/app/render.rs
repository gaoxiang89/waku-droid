use super::*;

fn should_render_empty_state(session: Option<&AgentSession>) -> bool {
    // Turns count as content even before any message exists: a
    // provider-initiated turn (Codex goal continuation) reasons for a while
    // before its first text delta, and the transcript's working indicator —
    // not the new-task greeting — is what represents that state.
    session
        .map(|session| {
            session.detail_loaded && session.messages.is_empty() && session.turns.is_empty()
        })
        .unwrap_or(true)
}

impl Waku {
    pub(super) fn render_panel_resize_handle(
        &self,
        id: &'static str,
        target: PanelResizeTarget,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = self
            .panel_resize_drag
            .is_some_and(|drag| drag.target == target);
        // The right panel's left edge abuts the browser webview, a native view
        // that composites above every base-scene pixel at or beyond the edge.
        // Its bar and hover strip therefore sit entirely left of the edge,
        // where GPUI still owns rendering and input; the other edges keep the
        // conventional straddle.
        let (strip_left, strip_width) = match target {
            PanelResizeTarget::RightPanel => (-7.0, 8.0),
            PanelResizeTarget::Sidebar
            | PanelResizeTarget::FileTree
            | PanelResizeTarget::FilesPanel
            | PanelResizeTarget::GitGraphColumn
            | PanelResizeTarget::ChatSplit => (-5.0, 10.0),
            // The bottom panel's edge is horizontal, so it renders its own
            // handle; this vertical bar is never built for it.
            PanelResizeTarget::BottomPanel => (-5.0, 10.0),
        };
        div()
            .id(id)
            .absolute()
            .top_0()
            .left(px(strip_left))
            .w(px(strip_width))
            .h_full()
            .group("panel-resize-handle")
            .cursor_col_resize()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(5.0))
                    .w(px(2.0))
                    .h_full()
                    .bg(if active {
                        theme.resize_handle
                    } else {
                        gpui::transparent_black()
                    })
                    .group_hover("panel-resize-handle", |element| {
                        element.bg(theme.resize_handle)
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.begin_panel_resize(target, event, window, cx);
                }),
            )
    }
}

/// Panel geometry for the frame being built.
#[derive(Clone, Copy)]
struct PanelFrame {
    /// Width each panel lays its content out at, sliding or not.
    sidebar_content: f32,
    right_panel_content: f32,
    /// Height the bottom panel lays its content out at, sliding or not.
    bottom_panel_content: f32,
    /// Width each panel occupies on screen: the eased slide while one runs.
    sidebar: f32,
    right_panel: f32,
    files_panel: f32,
    /// Height the bottom panel occupies on screen this frame.
    bottom_panel: f32,
    /// Which edge is mid-slide. The clip that keeps a sliding panel inside its
    /// narrowing container also cuts whatever that panel draws outside its own
    /// bounds — the right panel's resize handle sits entirely left of its edge
    /// — so each clip only goes on while its own panel is actually moving.
    sidebar_sliding: bool,
    right_panel_sliding: bool,
    bottom_panel_sliding: bool,
    /// An edge is still moving, so the frame loop has to keep going.
    sliding: bool,
}

/// Advance one panel's slide: the eased width while it runs, the settled
/// target once it is over. Retiring the tween here is what lets a closed
/// panel leave the element tree instead of lingering at zero width, still
/// rebuilding itself on every notify.
fn slide_width(slide: &mut Option<motion::WidthTween>, target: f32) -> f32 {
    match slide.and_then(|slide| slide.width_toward(target)) {
        Some(width) => width,
        None => {
            *slide = None;
            target
        }
    }
}

impl Waku {
    /// An edge is currently animating. While this holds, the pane islands'
    /// root observer stops fanning root notifies out to every island (see
    /// [`WakuPane::bind`]) and lets the cached-view geometry checks decide
    /// which islands a slide tick actually rebuilds.
    pub(super) fn panels_sliding(&self) -> bool {
        self.sidebar_slide.is_some()
            || self.right_panel_slide.is_some()
            || self.bottom_panel_slide.is_some()
    }

    /// Settle both panel slides for this frame and publish the widths the
    /// pane islands — which render later, during layout — have to agree with.
    fn settle_panel_slides(&mut self, window: &Window) -> PanelFrame {
        let was_sliding = self.panels_sliding();
        if self.settings_page.is_some() {
            // Settings covers the workspace, so there is no edge on screen to
            // move. Retire the slide rather than animate a layout nobody can
            // see; reopening the workspace finds the panels where they belong.
            self.sidebar_slide = None;
            self.right_panel_slide = None;
            self.bottom_panel_slide = None;
        }
        let (sidebar_content, right_panel_content) = self.effective_panel_widths(window);
        let sidebar = slide_width(
            &mut self.sidebar_slide,
            if self.sidebar_visible {
                sidebar_content
            } else {
                0.0
            },
        );
        let right_panel = slide_width(
            &mut self.right_panel_slide,
            if self.right_panel_visible {
                right_panel_content
            } else {
                0.0
            },
        );
        let bottom_panel_content = self.effective_bottom_panel_height(window);
        if self.is_bottom_panel_maximized() {
            self.bottom_panel_slide = None;
        }
        let bottom_panel = slide_width(
            &mut self.bottom_panel_slide,
            if self.bottom_panel_visible {
                bottom_panel_content
            } else {
                0.0
            },
        );
        self.sidebar_rendered_width = sidebar;
        self.right_panel_rendered_width = right_panel;
        self.bottom_panel_rendered_height = bottom_panel;
        let sliding = self.panels_sliding();
        if was_sliding && !sliding {
            // The observer gate held root-state fan-out away from any island
            // the slide left geometry-stable. One ungated notify now that
            // the slide is over rebuilds every island once, so whatever
            // root state changed during those 200ms lands the next frame.
            let root = window.current_view();
            window.on_next_frame(move |_, cx| cx.notify(root));
        }
        PanelFrame {
            sidebar_content,
            right_panel_content,
            bottom_panel_content,
            sidebar,
            right_panel,
            files_panel: self.effective_files_panel_width(window),
            bottom_panel,
            sidebar_sliding: self.sidebar_slide.is_some(),
            right_panel_sliding: self.right_panel_slide.is_some(),
            bottom_panel_sliding: self.bottom_panel_slide.is_some(),
            sliding,
        }
    }

    /// Width left for the chat column once the panels take theirs — the
    /// widths they are painted at this frame, so a transcript measured
    /// mid-slide matches the column it is laid out in.
    pub(super) fn chat_viewport_width(&self, window: &Window) -> f32 {
        f32::from(window.viewport_size().width)
            - self.sidebar_rendered_width
            - self.right_panel_rendered_width
            - self.effective_files_panel_width(window)
    }

    pub(super) fn chat_pane_width(&self, window: &Window, side: chat_split::ChatPaneSide) -> f32 {
        self.chat_pane_column_width(self.chat_viewport_width(window), side)
    }

    /// [`WakuPane`] delegate for the sidebar island.
    pub(super) fn sidebar_pane_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (sidebar_width, _) = self.effective_panel_widths(window);
        self.render_sidebar(sidebar_width, window, cx)
            .into_any_element()
    }

    /// [`WakuPane`] delegate for the primary transcript island.
    pub(super) fn transcript_pane_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.chat_pane_transcript(chat_split::ChatPaneSide::Left, window, cx)
    }

    /// [`WakuPane`] delegate for the split transcript island.
    pub(super) fn split_transcript_pane_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.chat_pane_transcript(chat_split::ChatPaneSide::Right, window, cx)
    }

    fn chat_pane_transcript(
        &mut self,
        side: chat_split::ChatPaneSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let role = self.chat_role_for_side(side);
        let pane_width = self.chat_pane_width(window, side);
        self.with_chat_view(role, |this| {
            // The transcript's own element sizes itself with `flex_1`, which only
            // stretches inside a flex parent. A cached pane lays its content out
            // as a root, so give it that parent here or its height collapses to
            // the zero flex basis.
            div()
                .size_full()
                .flex()
                .flex_col()
                .min_h_0()
                .child(this.render_transcript(window, pane_width, role, cx))
                .into_any_element()
        })
        .unwrap_or_else(|| div().into_any_element())
    }

    /// One chat pane. `width` pins the left pane of a side-by-side split;
    /// `hidden_other` marks a split collapsed to this pane alone.
    fn render_chat_pane(
        &mut self,
        side: chat_split::ChatPaneSide,
        width: Option<f32>,
        hidden_other: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let role = self.chat_role_for_side(side);
        let split = self.chat_split.is_some();
        let island = match side {
            chat_split::ChatPaneSide::Left => self.transcript_pane.clone(),
            chat_split::ChatPaneSide::Right => self.split_transcript_pane.clone(),
        };
        let Some((transcript_or_empty, permission, queued_and_composer, session_id)) =
            self.with_chat_view(role, |this| {
                let empty = should_render_empty_state(this.selected_session());
                let transcript_or_empty = if empty {
                    this.render_empty_state(cx).into_any_element()
                } else {
                    island
                        .cached(StyleRefinement::default().flex_1().min_h(px(0.0)).w_full())
                        .into_any_element()
                };
                let permission = this.render_permission(window, cx);
                let queued_and_composer = this.selected_project().is_some().then(|| {
                    (
                        this.render_queued_messages(cx),
                        this.render_composer(window, cx),
                    )
                });
                (
                    transcript_or_empty,
                    permission,
                    queued_and_composer,
                    this.state.selected_session,
                )
            })
        else {
            return div().into_any_element();
        };
        let focused = role == chat_split::ChatPaneRole::Focused;
        let mut body = div()
            .id(side.body_id())
            .track_focus(&self.chat_pane_focus[side.index()])
            .tab_stop(false)
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            .child(transcript_or_empty)
            .children(permission);
        if let Some((queued, composer)) = queued_and_composer {
            body = body.children(queued).child(composer);
        }
        if split && !focused {
            let other = side.other();
            body = body.capture_any_mouse_down(cx.listener(
                move |this, _: &MouseDownEvent, window, cx| {
                    let should_move_composer_focus =
                        this.chat_pane_has_focus(other, window, cx) || window.focused(cx).is_none();
                    this.focus_chat_pane(side, cx);
                    if should_move_composer_focus {
                        let weak = cx.entity().downgrade();
                        window.defer(cx, move |window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                let still_focused = this
                                    .chat_split
                                    .as_ref()
                                    .is_some_and(|split| split.focused_side == side);
                                if still_focused
                                    && (this.chat_pane_has_focus(other, window, cx)
                                        || window.focused(cx).is_none())
                                {
                                    let focus = this.composer_focus(cx);
                                    window.focus(&focus, cx);
                                }
                            });
                        });
                    }
                },
            ));
        }
        let mut outer = div()
            .id(side.pane_id())
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col();
        outer = match width {
            Some(width) => outer.flex_none().w(px(width)),
            None => outer.flex_1(),
        };
        if split {
            let show_project = self
                .chat_split
                .as_ref()
                .is_some_and(|split| split.project_id != self.state.selected_project);
            let meta = self.chat_pane_header_meta(session_id, show_project);
            let hidden_other = hidden_other.then(|| {
                let other_session = self.chat_split.as_ref().map(|split| split.session_id);
                self.chat_pane_header_meta(other_session, show_project)
            });
            outer = outer.child(self.render_chat_pane_header(
                side,
                focused,
                meta,
                hidden_other,
                cx,
            ));
        }
        outer.child(body).into_any_element()
    }

    /// [`WakuPane`] delegate for the right-panel island.
    pub(super) fn right_panel_pane_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (_, right_panel_width) = self.effective_panel_widths(window);
        self.render_right_panel(right_panel_width, window, cx)
            .into_any_element()
    }

    pub(super) fn files_panel_pane_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_files_panel(self.effective_files_panel_width(window), cx)
            .into_any_element()
    }

    fn maximized_bottom_panel_size(&self, window: &Window) -> (f32, f32) {
        let viewport = window.viewport_size();
        let (horizontal_inset, vertical_inset) = match window.window_decorations() {
            gpui::Decorations::Client { tiling } => {
                let edge = 11.0;
                let horizontal =
                    if !tiling.left { edge } else { 0.0 } + if !tiling.right { edge } else { 0.0 };
                let vertical =
                    if !tiling.top { edge } else { 0.0 } + if !tiling.bottom { edge } else { 0.0 };
                (horizontal, vertical)
            }
            gpui::Decorations::Server => (0.0, 0.0),
        };
        let width = (f32::from(viewport.width) - horizontal_inset).max(0.0);
        let height = (f32::from(viewport.height) - vertical_inset - WINDOW_HEADER_HEIGHT).max(0.0);
        (width, height)
    }

    pub(super) fn bottom_panel_pane_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (width, height) = if self.is_bottom_panel_maximized() {
            self.maximized_bottom_panel_size(window)
        } else {
            (
                self.chat_viewport_width(window),
                self.effective_bottom_panel_height(window),
            )
        };
        self.render_bottom_panel(height, width, window, cx)
            .into_any_element()
    }

    /// Measure live frame rate by counting renders over a sliding one-second
    /// window and keep requesting animation frames so the counter stays current.
    fn tick_fps(&mut self, window: &Window) {
        let now = Instant::now();
        self.fps_frame_count = self.fps_frame_count.saturating_add(1);
        if now.duration_since(self.fps_last_frame) >= Duration::from_secs(1) {
            self.fps_value = self.fps_frame_count as u32;
            self.fps_frame_count = 0;
            self.fps_last_frame = now;
        }
        window.request_animation_frame();
    }
}

impl Render for Waku {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.composer_focus_pending) {
            let focus = self.composer_focus(cx);
            window.focus(&focus, cx);
        }
        // Panel geometry first: the browser sync right below reads whether a
        // panel is mid-slide, and settling here rather than at the point of
        // use keeps the pane islands and the transcript on one set of widths.
        let panels = self.settle_panel_slides(window);
        if panels.sliding {
            // The manual drive for the width tweens — the same scheduling
            // `with_animation` would do, minus its element-id keying.
            window.request_animation_frame();
        }
        // Before anything can early-return (the settings page below), settle
        // whether each native browser webview belongs on screen this frame —
        // it floats above everything GPUI paints.
        self.sync_browser_webviews(cx);
        if self.fps_counter_visible {
            self.tick_fps(window);
        }
        let image_preview = self.render_image_preview(cx);
        let task_switcher = self.render_task_switcher(window, cx);
        if self.settings_page.is_some() {
            let command_palette = self.render_command_palette(window, cx);
            let commit_dialog = self.render_commit_dialog(cx);
            let goal_dialog = self.render_goal_dialog(window, cx);
            let right_panel_delete_confirm = self.render_right_panel_delete_confirm(cx);
            let right_panel_upload_confirm = self.render_right_panel_upload_confirm(cx);
            let session_delete_dialog = self.render_session_delete_dialog(cx);
            let daemon_replace_dialog = self.render_daemon_replace_dialog(window, cx);
            let git_graph_dialog = self.render_git_graph_dialog(cx);
            let remote_folder_picker = self.render_remote_folder_picker(window, cx);
            let toast = self.render_notifications(cx);
            let content = div()
                .relative()
                .size_full()
                .on_action(cx.listener(Self::toggle_command_palette_action))
                .on_action(cx.listener(Self::open_resume_picker_action))
                .on_action(cx.listener(Self::switch_task_forward_action))
                .on_action(cx.listener(Self::switch_task_backward_action))
                .on_action(cx.listener(Self::select_first_task_action))
                .on_action(cx.listener(Self::select_last_task_action))
                .on_action(cx.listener(Self::confirm_task_switch_action))
                .on_action(cx.listener(Self::cancel_task_switch_action))
                .on_modifiers_changed(cx.listener(Self::task_switcher_modifiers_changed))
                .child(self.render_settings(window, cx))
                .children(toast)
                .children(command_palette)
                .children(commit_dialog)
                .children(goal_dialog)
                .children(right_panel_delete_confirm)
                .children(right_panel_upload_confirm)
                .children(session_delete_dialog)
                .children(daemon_replace_dialog)
                .children(git_graph_dialog)
                .children(remote_folder_picker)
                .children(image_preview)
                .children(task_switcher)
                .into_any_element();
            return self.render_window_frame(content, window, cx);
        }
        // Re-armed every frame this window shows time labels; parks while
        // settings covers them and while the window isn't drawing at all.
        self.schedule_time_label_wake(cx);

        self.reconcile_chat_split();
        let theme = Theme::current(cx);
        let split_focus = self.chat_split.as_ref().map(|split| split.focused_side);
        let chat_area = match (split_focus, self.chat_split_layout(window)) {
            (Some(_), Some((left_width, _))) => div()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .flex_row()
                .child(self.render_chat_pane(
                    chat_split::ChatPaneSide::Left,
                    Some(left_width),
                    false,
                    window,
                    cx,
                ))
                .child(
                    div()
                        .w(px(chat_split::CHAT_SPLIT_DIVIDER_WIDTH))
                        .h_full()
                        .flex_none()
                        .bg(theme.border),
                )
                .child(self.render_chat_pane(
                    chat_split::ChatPaneSide::Right,
                    None,
                    false,
                    window,
                    cx,
                ))
                // Last, so its hit strip sits above both panes it straddles.
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(left_width))
                        .w(px(chat_split::CHAT_SPLIT_DIVIDER_WIDTH))
                        .child(
                            self.render_panel_resize_handle(
                                "chat-split-resize",
                                PanelResizeTarget::ChatSplit,
                                cx,
                            )
                            .track_focus(&self.chat_split_resize_focus)
                            .tab_index(0)
                            .focus_visible(|style| style.bg(theme.accent.opacity(0.18)))
                            .when(self.chat_split_resize_focus.is_focused(window), |handle| {
                                handle.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .left(px(5.0))
                                        .w(px(2.0))
                                        .h_full()
                                        .bg(theme.accent),
                                )
                            })
                            .tooltip(Tooltip::text(tr!("chat_split.resize")))
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                                if event.keystroke.modifiers.modified() {
                                    return;
                                }
                                let column = this.chat_viewport_width(window);
                                match event.keystroke.key.as_str() {
                                    "left" => {
                                        if let Some(ratio) = chat_split::step_chat_split_ratio(
                                            column,
                                            this.chat_split_ratio,
                                            -0.05,
                                        ) {
                                            this.chat_split_ratio = ratio;
                                            this.persist_panel_layout();
                                            cx.notify();
                                        }
                                        cx.stop_propagation();
                                    }
                                    "right" => {
                                        if let Some(ratio) = chat_split::step_chat_split_ratio(
                                            column,
                                            this.chat_split_ratio,
                                            0.05,
                                        ) {
                                            this.chat_split_ratio = ratio;
                                            this.persist_panel_layout();
                                            cx.notify();
                                        }
                                        cx.stop_propagation();
                                    }
                                    "home" => {
                                        if let Some(ratio) = chat_split::step_chat_split_ratio(
                                            column,
                                            this.chat_split_ratio,
                                            -1.0,
                                        ) {
                                            this.chat_split_ratio = ratio;
                                            this.persist_panel_layout();
                                            cx.notify();
                                        }
                                        cx.stop_propagation();
                                    }
                                    "end" => {
                                        if let Some(ratio) = chat_split::step_chat_split_ratio(
                                            column,
                                            this.chat_split_ratio,
                                            1.0,
                                        ) {
                                            this.chat_split_ratio = ratio;
                                            this.persist_panel_layout();
                                            cx.notify();
                                        }
                                        cx.stop_propagation();
                                    }
                                    "enter" | "space" => {
                                        this.reset_chat_split_ratio(cx);
                                        cx.stop_propagation();
                                    }
                                    _ => {}
                                }
                            })),
                        ),
                )
                .into_any_element(),
            (Some(focused_side), None) => {
                self.render_chat_pane(focused_side, None, true, window, cx)
            }
            (None, _) => {
                self.render_chat_pane(chat_split::ChatPaneSide::Left, None, false, window, cx)
            }
        };
        let computer_use = self.render_computer_use_overlay(window, cx);
        let command_palette = self.render_command_palette(window, cx);
        let commit_dialog = self.render_commit_dialog(cx);
        let goal_dialog = self.render_goal_dialog(window, cx);
        let right_panel_delete_confirm = self.render_right_panel_delete_confirm(cx);
        let right_panel_upload_confirm = self.render_right_panel_upload_confirm(cx);
        let session_delete_dialog = self.render_session_delete_dialog(cx);
        let daemon_replace_dialog = self.render_daemon_replace_dialog(window, cx);
        let git_graph_dialog = self.render_git_graph_dialog(cx);
        let remote_folder_picker = self.render_remote_folder_picker(window, cx);
        let toast = self.render_notifications(cx);
        let bottom_panel_maximized = self.is_bottom_panel_maximized();
        let (center_toast, maximized_toast) = if bottom_panel_maximized {
            (None, toast)
        } else {
            (toast, None)
        };
        let content = div()
            .key_context(if bottom_panel_maximized {
                "Waku BottomPanelMaximized"
            } else {
                "Waku"
            })
            .on_action(cx.listener(Self::close_window_or_right_panel_tab_action))
            .on_action(cx.listener(Self::new_session_action))
            .on_action(cx.listener(Self::new_project_action))
            .on_action(cx.listener(Self::open_settings_action))
            .on_action(cx.listener(Self::toggle_sidebar_action))
            .on_action(cx.listener(Self::toggle_right_panel_action))
            .on_action(cx.listener(Self::toggle_bottom_panel_action))
            .on_action(cx.listener(Self::toggle_bottom_panel_maximize_action))
            .on_action(cx.listener(Self::toggle_chat_split_action))
            .on_action(cx.listener(Self::focus_left_chat_pane_action))
            .on_action(cx.listener(Self::focus_right_chat_pane_action))
            .on_action(cx.listener(Self::swap_chat_panes_action))
            .on_action(cx.listener(Self::close_chat_pane_action))
            .on_action(cx.listener(Self::toggle_files_surface_action))
            .on_action(cx.listener(Self::toggle_git_graph_surface_action))
            .on_action(cx.listener(Self::new_terminal_action))
            .on_action(cx.listener(Self::next_terminal_action))
            .on_action(cx.listener(Self::previous_terminal_action))
            .on_action(cx.listener(Self::split_terminal_right_action))
            .on_action(cx.listener(Self::split_terminal_down_action))
            .on_action(cx.listener(Self::close_terminal_pane_action))
            .on_action(cx.listener(Self::resize_terminal_pane_left_action))
            .on_action(cx.listener(Self::resize_terminal_pane_right_action))
            .on_action(cx.listener(Self::resize_terminal_pane_up_action))
            .on_action(cx.listener(Self::resize_terminal_pane_down_action))
            .on_action(cx.listener(Self::focus_terminal_pane_left_action))
            .on_action(cx.listener(Self::focus_terminal_pane_right_action))
            .on_action(cx.listener(Self::focus_terminal_pane_up_action))
            .on_action(cx.listener(Self::focus_terminal_pane_down_action))
            .on_action(cx.listener(Self::toggle_command_palette_action))
            .on_action(cx.listener(Self::open_resume_picker_action))
            .on_action(cx.listener(Self::toggle_fps_counter_action))
            .on_action(cx.listener(Self::navigate_back_action))
            .on_action(cx.listener(Self::navigate_forward_action))
            .on_action(cx.listener(Self::switch_task_forward_action))
            .on_action(cx.listener(Self::switch_task_backward_action))
            .on_action(cx.listener(Self::select_first_task_action))
            .on_action(cx.listener(Self::select_last_task_action))
            .on_action(cx.listener(Self::confirm_task_switch_action))
            .on_action(cx.listener(Self::cancel_task_switch_action))
            .on_action(cx.listener(Self::focus_composer_action))
            .on_action(cx.listener(Self::toggle_model_picker_action))
            .on_action(cx.listener(Self::toggle_usage_panel_action))
            .on_action(cx.listener(Self::save_right_panel_file_action))
            .on_action(cx.listener(Self::cancel_turn_action))
            .on_action(cx.listener(Self::copy_selection_action))
            .on_action(cx.listener(Self::open_find_action))
            .on_action(cx.listener(Self::open_find_replace_action))
            .on_action(cx.listener(Self::close_find_action))
            .on_action(cx.listener(Self::find_next_action))
            .on_action(cx.listener(Self::find_previous_action))
            .on_action(cx.listener(Self::toggle_find_case_action))
            .on_action(cx.listener(Self::toggle_find_whole_word_action))
            .on_action(cx.listener(Self::toggle_find_regex_action))
            .on_action(cx.listener(Self::replace_all_matches_action))
            .on_modifiers_changed(cx.listener(Self::task_switcher_modifiers_changed))
            .capture_any_mouse_down(cx.listener(Self::navigation_mouse_down))
            .on_mouse_move(cx.listener(Self::resize_panel_mouse_move))
            .on_mouse_move(cx.listener(Self::split_divider_mouse_move))
            .capture_any_mouse_up(cx.listener(Self::finish_panel_resize))
            .capture_any_mouse_up(cx.listener(Self::finish_split_divider_drag))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .text_color(theme.text)
            .font_family(".SystemUIFont")
            .child(self.render_header(window, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .relative()
                    .flex()
                    // Both panels slide through a container that narrows while their
                    // content keeps its full width and is clipped: the sidebar list
                    // and the right panel's surfaces never reflow on the way in or
                    // out, and their bounds stay put so only the clip moves.
                    .when(panels.sidebar > 0.0, |root| {
                        root.child(
                            div()
                                .h_full()
                                .flex_none()
                                .w(px(panels.sidebar))
                                .when(panels.sidebar_sliding, |element| element.overflow_hidden())
                                .child(
                                    self.sidebar_pane.clone().cached(
                                        StyleRefinement::default()
                                            .w(px(panels.sidebar_content))
                                            .h_full()
                                            .flex_none(),
                                    ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .bg(theme.surface)
                            .when(panels.sidebar > 0.0, |element| {
                                element.border_l_1().border_color(theme.sidebar_border)
                            })
                            .child(chat_area)
                            .when(self.selected_project().is_some(), |element| {
                                element
                                    // The bottom panel sits between the composer and
                                    // the workspace footer. It slides through the same
                                    // narrowing-container trick as the side panels,
                                    // flipped: the content stays pinned to the column's
                                    // bottom edge and only the clip moves.
                                    .when(
                                        !bottom_panel_maximized && panels.bottom_panel > 0.0,
                                        |element| {
                                            element.child(
                                            div()
                                                .w_full()
                                                .flex_none()
                                                .h(px(panels.bottom_panel))
                                                .relative()
                                                .when(panels.bottom_panel_sliding, |element| {
                                                    element.overflow_hidden()
                                                })
                                                .child(
                                                    self.bottom_panel_pane.clone().cached(
                                                        StyleRefinement::default()
                                                            .absolute()
                                                            .bottom_0()
                                                            .left_0()
                                                            .w_full()
                                                            .h(px(panels.bottom_panel_content)),
                                                    ),
                                                ),
                                            )
                                        },
                                    )
                                    .child(self.render_workspace_footer(cx))
                            })
                            .relative()
                            .children(center_toast)
                            .when(self.sidebar_visible, |element| {
                                element.child(self.render_panel_resize_handle(
                                    "sidebar-resize-handle",
                                    PanelResizeTarget::Sidebar,
                                    cx,
                                ))
                            }),
                    )
                    .when(panels.right_panel > 0.0, |root| {
                        root.child(
                            div()
                                .h_full()
                                .flex_none()
                                .w(px(panels.right_panel))
                                .flex()
                                .relative()
                                .when(panels.right_panel_sliding, |element| {
                                    element.overflow_hidden()
                                })
                                // Pinned to the window's right edge, so the panel is
                                // uncovered from that edge inward rather than dragged
                                // across the screen.
                                .child(
                                    self.right_panel_pane.clone().cached(
                                        StyleRefinement::default()
                                            .absolute()
                                            .top_0()
                                            .right_0()
                                            .w(px(panels.right_panel_content))
                                            .h_full(),
                                    ),
                                ),
                        )
                    })
                    .when(panels.files_panel > 0.0, |root| {
                        root.child(
                            self.files_panel_pane.clone().cached(
                                StyleRefinement::default()
                                    .w(px(panels.files_panel))
                                    .h_full()
                                    .flex_none(),
                            ),
                        )
                    })
                    .when(bottom_panel_maximized, |root| {
                        root.child(
                            div()
                                .id("bottom-panel-maximized-layer")
                                .occlude()
                                .absolute()
                                .inset_0()
                                .overflow_hidden()
                                .bg(theme.surface)
                                .child(
                                    self.bottom_panel_pane
                                        .clone()
                                        .cached(StyleRefinement::default().size_full()),
                                ),
                        )
                    })
                    .children(maximized_toast),
            )
            .children(computer_use)
            .children(command_palette)
            .children(commit_dialog)
            .children(goal_dialog)
            .children(right_panel_delete_confirm)
            .children(right_panel_upload_confirm)
            .children(session_delete_dialog)
            .children(daemon_replace_dialog)
            .children(git_graph_dialog)
            .children(remote_folder_picker)
            .children(image_preview)
            .children(task_switcher)
            .into_any_element();

        self.render_window_frame(content, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unloaded_history_never_renders_the_new_task_prompt() {
        let mut stored = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        stored.detail_loaded = false;

        assert!(!should_render_empty_state(Some(&stored)));

        let draft = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        assert!(should_render_empty_state(Some(&draft)));
        assert!(should_render_empty_state(None));
    }
}

impl Waku {
    /// Arm the dismiss timer and build the floating notification layer — the
    /// toast plus one live card per streaming download. Every full-window
    /// surface (workspace and settings alike) must include this, or a toast
    /// raised there stays invisible until the user navigates away.
    fn render_notifications(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.start_toast_dismiss_timer(cx);
        let toast = self
            .toast
            .as_ref()
            .map(|toast| (toast.message.clone(), toast.tone, toast.id));
        let downloads: Vec<(u64, TransferKind, String, u64, u64)> = self
            .downloads
            .iter()
            .map(|download| {
                (
                    download.id,
                    download.kind,
                    download.name.clone(),
                    download.written,
                    download.total_bytes,
                )
            })
            .collect();
        if toast.is_none() && downloads.is_empty() {
            return None;
        }
        let mut column = div()
            .absolute()
            .left_0()
            .top(px(56.0))
            .w_full()
            .px(px(20.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0));
        if let Some((message, tone, generation)) = toast {
            column = column.child(self.render_toast(message, tone, generation, cx));
        }
        for (id, kind, name, written, total_bytes) in downloads {
            column =
                column.child(self.render_download_card(id, kind, name, written, total_bytes, cx));
        }
        Some(column.into_any_element())
    }

    fn render_toast(
        &self,
        message: String,
        tone: ToastTone,
        generation: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = Theme::current(cx);
        let (status_icon, status_color) = match tone {
            ToastTone::Alert => ("icons/alert.svg", theme.danger),
            ToastTone::Success => ("icons/check.svg", theme.success),
        };
        let palette = MarkdownPalette::from_theme(&theme);
        let text_ctx = MarkdownCtx::new(
            format!("toast-{generation}"),
            &palette,
            self.scaled_markdown_metrics(MarkdownMetrics::COMPACT),
            self.toast_selection.clone(),
        );
        let message = md::render::plain_text(
            message,
            md::render::SANS_FAMILY,
            FontWeight::NORMAL,
            theme.text,
            &text_ctx,
        );
        let dismiss = div()
            .id(SharedString::from(format!("dismiss-toast-{generation}")))
            .tab_index(0)
            .size(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!("common.dismiss_notification")))
            .child(icon("icons/x.svg", 12.0, theme.text_tertiary))
            .on_click(cx.listener(|this, _, _, cx| {
                this.hide_toast();
                cx.notify();
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space" | "escape") {
                    this.hide_toast();
                    cx.notify();
                    cx.stop_propagation();
                }
            }));

        div()
            .id(SharedString::from(format!("toast-{generation}")))
            .occlude()
            .max_w(px(560.0))
            .min_w_0()
            .px(px(10.0))
            .py(px(7.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.raised)
            .shadow_lg()
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(sp(12.5))
            .line_height(sp(16.0))
            .text_color(theme.text)
            .on_hover(cx.listener(|this, hovering: &bool, _, cx| {
                this.set_toast_hovered(*hovering, cx);
            }))
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(md::render::frame_reset(self.toast_selection.clone()))
            .child(icon(status_icon, 14.0, status_color))
            .child(div().flex_1().min_w_0().whitespace_normal().child(message))
            .child(dismiss)
            .child(self.toast_selection_input())
            // The notification column keeps this top-centered just beneath
            // Waku's 48px header. GPUI's animation path honors the system
            // reduce-motion preference and resolves immediately.
            .with_animation(
                SharedString::from(format!("toast-enter-{generation}")),
                Animation::new(TOAST_ANIMATION_DURATION).with_easing(ease_out_quint()),
                |element, delta| {
                    element
                        .top(px(48.0 + 8.0 * delta))
                        .opacity(0.4 + 0.6 * delta)
                },
            )
    }

    /// One live card in the notification column for a file transfer: name,
    /// progress share, and a cancel button that stops the transfer at its next
    /// chunk. The moving bar is decorative; the percent label carries the same
    /// meaning without it.
    fn render_download_card(
        &self,
        id: u64,
        kind: TransferKind,
        name: String,
        written: u64,
        total_bytes: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = Theme::current(cx);
        let (icon_path, card_prefix, cancel_prefix) = match kind {
            TransferKind::Download => ("icons/download.svg", "download-card", "cancel-download"),
            TransferKind::Upload => ("icons/cloud-upload.svg", "upload-card", "cancel-upload"),
        };
        let fraction = if total_bytes > 0 {
            (written as f32 / total_bytes as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let percent = (fraction * 100.0).round() as u32;
        let cancel = div()
            .id(SharedString::from(format!("{cancel_prefix}-{id}")))
            .tab_index(0)
            .size(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!("common.cancel")))
            .child(icon("icons/x.svg", 12.0, theme.text_tertiary))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.cancel_download(id, cx);
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space" | "escape") {
                    this.cancel_download(id, cx);
                    cx.notify();
                    cx.stop_propagation();
                }
            }));

        div()
            .id(SharedString::from(format!("{card_prefix}-{id}")))
            .occlude()
            .min_w(px(360.0))
            .max_w(px(560.0))
            .px(px(10.0))
            .py(px(7.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.raised)
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .text_size(sp(12.5))
            .line_height(sp(16.0))
            .text_color(theme.text)
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w_0()
                    .child(icon(icon_path, 14.0, theme.accent))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(name)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(format!("{percent}%"))),
                    )
                    .child(cancel),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .h(px(3.0))
                            .rounded_full()
                            .bg(theme.overlay_strong)
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(theme.accent)
                                    .w(gpui::relative(fraction)),
                            ),
                    )
                    .child(div().flex_none().text_color(theme.text_tertiary).child(
                        SharedString::from(format!(
                            "{}/{}",
                            super::skills_page::format_bytes(written),
                            super::skills_page::format_bytes(total_bytes)
                        )),
                    )),
            )
    }
}
