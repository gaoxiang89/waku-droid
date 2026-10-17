//! Folder picker for windows bound to a WSL or SSH daemon.
//!
//! The native dialog browses this machine, but a remote window's projects
//! live on the daemon host, so this picker lists directories through the
//! daemon's `BrowseDirectory` operation instead. Listing is RPC, so it runs
//! on the background executor and a generation counter discards results
//! from superseded navigations.

use std::path::PathBuf;

use super::*;

pub(super) struct RemoteFolderPicker {
    host: waku_client::hosts::HostKey,
    path: Option<PathBuf>,
    parent: Option<PathBuf>,
    home: Option<PathBuf>,
    directories: Vec<(String, PathBuf)>,
    loading: bool,
    error: Option<String>,
    generation: u64,
    focus: FocusHandle,
    /// Focus moves to the picker on its first frame; opening it can happen
    /// from an action handler that has no window.
    focus_pending: bool,
    scroll: ScrollHandle,
}

impl Waku {
    pub(super) fn open_remote_folder_picker(
        &mut self,
        host: waku_client::hosts::HostKey,
        cx: &mut Context<Self>,
    ) {
        let focus = cx.focus_handle();
        self.remote_folder_picker = Some(RemoteFolderPicker {
            host,
            path: None,
            parent: None,
            home: None,
            directories: Vec::new(),
            loading: false,
            error: None,
            generation: 0,
            focus,
            focus_pending: true,
            scroll: ScrollHandle::new(),
        });
        self.remote_folder_path_input
            .update(cx, |input, cx| input.set_content(String::new(), cx));
        self.browse_remote_folder(None, cx);
    }

    pub(super) fn close_remote_folder_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remote_folder_picker = None;
        let composer_focus = self.composer_focus(cx);
        window.focus(&composer_focus, cx);
        cx.notify();
    }

    fn browse_remote_folder(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(picker) = self.remote_folder_picker.as_mut() else {
            return;
        };
        self.remote_folder_picker_generation = self.remote_folder_picker_generation.wrapping_add(1);
        picker.generation = self.remote_folder_picker_generation;
        picker.loading = true;
        picker.error = None;
        let generation = picker.generation;
        let host = picker.host.clone();
        let Some(owner) = self.hosts.get(&host) else {
            return;
        };
        let workspace = waku_client::WorkspaceClient::for_daemon(owner.daemon.clone());
        let listing = cx.background_executor().spawn(async move {
            workspace.request(waku_client::WorkspaceOperation::BrowseDirectory { path })
        });
        cx.spawn(async move |this, cx| {
            let result = listing.await;
            let _ = this.update(cx, |this, cx| {
                let Some(picker) = this.remote_folder_picker.as_mut() else {
                    return;
                };
                if picker.generation != generation || picker.host != host {
                    return;
                }
                picker.loading = false;
                match result {
                    Ok(waku_client::WorkspaceResult::Directory {
                        path,
                        parent,
                        home,
                        entries,
                        ..
                    }) => {
                        let mut directories = entries
                            .into_iter()
                            .filter(|entry| entry.is_dir)
                            .map(|entry| (entry.name, entry.absolute_path))
                            .collect::<Vec<_>>();
                        directories
                            .sort_by_key(|(name, _)| (name.starts_with('.'), name.to_lowercase()));
                        let shown = path.to_string_lossy().into_owned();
                        picker.path = Some(path);
                        picker.parent = parent;
                        picker.home = Some(home);
                        picker.directories = directories;
                        picker.scroll.set_offset(gpui::Point::default());
                        this.remote_folder_path_input
                            .update(cx, |input, cx| input.set_content(shown, cx));
                    }
                    Ok(_) => picker.error = Some(tr!("remote_folder.invalid_response")),
                    Err(error) => picker.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Navigate to the path typed in the field, expanding a leading `~`
    /// against the daemon host's home directory.
    pub(super) fn browse_typed_remote_folder(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self.remote_folder_picker.as_ref() else {
            return;
        };
        let typed = self
            .remote_folder_path_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        if typed.is_empty() {
            return;
        }
        let path = match (typed.strip_prefix('~'), picker.home.as_ref()) {
            (Some(rest), Some(home)) => home.join(rest.trim_start_matches(['/', '\\'])),
            _ => PathBuf::from(typed),
        };
        self.browse_remote_folder(Some(path), cx);
    }

    fn confirm_remote_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self
            .remote_folder_picker
            .as_ref()
            .filter(|picker| !picker.loading)
            .and_then(|picker| picker.path.clone())
        else {
            return;
        };
        let host = self.remote_folder_picker.as_ref().unwrap().host.clone();
        self.close_remote_folder_picker(window, cx);
        self.add_project_path_on_host(path, host, cx);
    }

    pub(super) fn render_remote_folder_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(picker) = self.remote_folder_picker.as_mut()
            && std::mem::take(&mut picker.focus_pending)
        {
            window.focus(&picker.focus, cx);
        }
        let picker = self.remote_folder_picker.as_ref()?;
        let theme = Theme::current(cx);
        let host = self
            .hosts
            .get(&picker.host)
            .map(|host| host.name.clone())
            .unwrap_or_else(|| tr!("connections.local"));

        let mut list = div()
            .id("remote-folder-list")
            .track_scroll(&picker.scroll)
            .overflow_y_scroll()
            .h(px(300.0))
            .flex()
            .flex_col()
            .px(px(8.0))
            .py(px(6.0));
        if let Some(parent) = picker.parent.clone() {
            list = list.child(folder_row(
                "remote-folder-parent".into(),
                "icons/arrow-up.svg",
                "..".to_owned(),
                theme,
                cx,
                move |this, cx| this.browse_remote_folder(Some(parent.clone()), cx),
            ));
        }
        for (index, (name, path)) in picker.directories.iter().enumerate() {
            let path = path.clone();
            list = list.child(folder_row(
                SharedString::from(format!("remote-folder-{index}")),
                "icons/folder.svg",
                name.clone(),
                theme,
                cx,
                move |this, cx| this.browse_remote_folder(Some(path.clone()), cx),
            ));
        }
        if let Some(error) = &picker.error {
            list = list.child(
                div()
                    .px(px(8.0))
                    .py(px(10.0))
                    .whitespace_normal()
                    .text_size(sp(12.5))
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        } else if picker.loading && picker.directories.is_empty() {
            list = list.child(
                div()
                    .px(px(8.0))
                    .py(px(10.0))
                    .text_size(sp(12.5))
                    .text_color(theme.text_tertiary)
                    .child(tr!("remote_folder.loading")),
            );
        }

        let can_confirm = !picker.loading && picker.path.is_some();
        let card = div()
            .id("remote-folder-card")
            .track_focus(&picker.focus)
            .tab_group()
            .w_full()
            .max_w(px(560.0))
            .overflow_hidden()
            .rounded(px(18.0))
            .bg(theme.composer)
            .shadow_xl()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_remote_folder_picker(window, cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "enter" && event.keystroke.modifiers.secondary() {
                    this.confirm_remote_folder(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .h(px(48.0))
                    .px(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .text_size(sp(14.0))
                    .text_color(theme.text)
                    .child(icon("icons/folder-open.svg", 15.0, theme.text))
                    .child(
                        div()
                            .truncate()
                            .child(tr!("remote_folder.title", host = host.as_str())),
                    ),
            )
            .child(div().px(px(16.0)).pb(px(8.0)).child(TextField::new(
                "remote-folder-path-field",
                self.remote_folder_path_input.clone(),
            )))
            .child(div().mx(px(8.0)).h(px(1.0)).bg(theme.border))
            .child(list)
            .child(div().mx(px(8.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .px(px(16.0))
                    .py(px(12.0))
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(dialog_button(
                        "remote-folder-cancel",
                        tr!("remote_folder.cancel"),
                        true,
                        theme,
                        cx,
                        |this, window, cx| this.close_remote_folder_picker(window, cx),
                    ))
                    .child(dialog_button(
                        "remote-folder-confirm",
                        tr!("remote_folder.add"),
                        can_confirm,
                        theme,
                        cx,
                        |this, window, cx| this.confirm_remote_folder(window, cx),
                    )),
            );
        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        };
        let layer = div()
            .id("remote-folder-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(scrim)
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_remote_folder_picker(window, cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(4).into_any_element())
    }
}

fn folder_row(
    id: SharedString,
    icon_path: &'static str,
    label: String,
    theme: Theme,
    cx: &mut Context<Waku>,
    on_open: impl Fn(&mut Waku, &mut Context<Waku>) + Clone + 'static,
) -> Stateful<Div> {
    let on_key = on_open.clone();
    div()
        .id(id)
        .tab_index(0)
        .h(px(28.0))
        .px(px(8.0))
        .flex_none()
        .rounded(px(7.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .cursor_default()
        .text_size(sp(13.0))
        .text_color(theme.text)
        .hover(|row| row.bg(theme.overlay))
        .active(|row| row.bg(theme.overlay_strong))
        .focus_visible(|row| row.bg(theme.overlay))
        .child(icon(icon_path, 13.0, theme.text_tertiary))
        .child(div().truncate().child(label))
        .on_click(cx.listener(move |this, _, _, cx| on_open(this, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                on_key(this, cx);
                cx.stop_propagation();
            }
        }))
}

fn dialog_button(
    id: &'static str,
    label: String,
    enabled: bool,
    theme: Theme,
    cx: &mut Context<Waku>,
    on_press: impl Fn(&mut Waku, &mut Window, &mut Context<Waku>) + Clone + 'static,
) -> Stateful<Div> {
    let on_key = on_press.clone();
    div()
        .id(id)
        .tab_index(0)
        .h(px(30.0))
        .px(px(12.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border_strong)
        .flex()
        .items_center()
        .cursor_default()
        .text_size(sp(13.0))
        .text_color(theme.text)
        .opacity(if enabled { 1.0 } else { 0.55 })
        .focus_visible(|button| button.border_color(theme.accent))
        .when(enabled, |button| {
            button
                .hover(|button| button.bg(theme.overlay))
                .active(|button| button.bg(theme.overlay_strong))
                .on_click(cx.listener(move |this, _, window, cx| on_press(this, window, cx)))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        on_key(this, window, cx);
                        cx.stop_propagation();
                    }
                }))
        })
        .child(label)
}
