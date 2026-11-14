//! Folder picker for windows bound to a WSL or SSH daemon.
//!
//! The native dialog browses this machine, but a remote window's projects
//! live on the daemon host, so this picker lists directories through the
//! daemon's `BrowseDirectory` operation instead. Listing is RPC, so it runs
//! on the background executor and a generation counter discards results
//! from superseded navigations.

use std::path::{Path, PathBuf};

use super::*;

pub(super) struct RemoteFolderPicker {
    host: waku_client::hosts::HostKey,
    path: Option<PathBuf>,
    parent: Option<PathBuf>,
    home: Option<PathBuf>,
    directories: Vec<(String, PathBuf)>,
    row_focuses: Vec<FocusHandle>,
    loading: bool,
    error: Option<String>,
    generation: u64,
    focus: FocusHandle,
    /// Focus moves to the picker on its first frame; opening it can happen
    /// from an action handler that has no window.
    focus_pending: bool,
    refocus_after_load: bool,
    focus_row_pending: Option<usize>,
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
            row_focuses: Vec::new(),
            loading: false,
            error: None,
            generation: 0,
            focus,
            focus_pending: true,
            refocus_after_load: false,
            focus_row_pending: None,
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

    fn navigate_from_folder_row(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(picker) = self.remote_folder_picker.as_mut() {
            picker.refocus_after_load = true;
        }
        self.browse_remote_folder(Some(path), cx);
    }

    fn focus_folder_row(&self, index: usize, window: &mut Window, cx: &mut App) {
        let Some(picker) = self.remote_folder_picker.as_ref() else {
            return;
        };
        if let Some(handle) = picker.row_focuses.get(index) {
            window.focus(handle, cx);
            picker.scroll.scroll_to_item(index);
        }
    }

    fn browse_remote_folder(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(host) = self
            .remote_folder_picker
            .as_ref()
            .map(|picker| picker.host.clone())
        else {
            return;
        };
        let Some(owner) = self.hosts.get(&host) else {
            if let Some(picker) = self.remote_folder_picker.as_mut() {
                picker.loading = false;
                picker.error = Some(tr!("connections.reconnecting"));
                picker.row_focuses.clear();
                picker.focus_row_pending = None;
                if std::mem::take(&mut picker.refocus_after_load) {
                    picker.focus_pending = true;
                }
            }
            cx.notify();
            return;
        };
        let workspace = waku_client::WorkspaceClient::for_daemon(owner.daemon.clone());
        self.remote_folder_picker_generation = self.remote_folder_picker_generation.wrapping_add(1);
        let generation = self.remote_folder_picker_generation;
        let Some(picker) = self.remote_folder_picker.as_mut() else {
            return;
        };
        picker.generation = generation;
        picker.loading = true;
        picker.error = None;
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
                let refocus = std::mem::take(&mut picker.refocus_after_load);
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
                        let row_count = usize::from(parent.is_some()) + directories.len();
                        let shown = path.to_string_lossy().into_owned();
                        picker.path = Some(path);
                        picker.parent = parent;
                        picker.home = Some(home);
                        picker.directories = directories;
                        picker.row_focuses = (0..row_count).map(|_| cx.focus_handle()).collect();
                        picker.focus_row_pending =
                            (refocus && !picker.row_focuses.is_empty()).then_some(0);
                        if refocus && picker.row_focuses.is_empty() {
                            picker.focus_pending = true;
                        }
                        picker.scroll.set_offset(gpui::Point::default());
                        this.remote_folder_path_input
                            .update(cx, |input, cx| input.set_content(shown, cx));
                    }
                    Ok(_) => {
                        picker.error = Some(tr!("remote_folder.invalid_response"));
                        picker.focus_row_pending = None;
                        if refocus {
                            picker.focus_pending = true;
                        }
                    }
                    Err(error) => {
                        picker.error = Some(format!("{error:#}"));
                        picker.focus_row_pending = None;
                        if refocus {
                            picker.focus_pending = true;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Navigate to the path typed in the field, expanding a leading `~`
    /// against the daemon host's home directory or resolving relative paths
    /// against the currently opened directory.
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
        let path =
            resolve_typed_remote_path(&typed, picker.path.as_deref(), picker.home.as_deref());
        self.browse_remote_folder(Some(path), cx);
    }

    fn confirm_remote_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self
            .remote_folder_path_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        if let Some(picker) = self.remote_folder_picker.as_ref() {
            let current = picker
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().trim().to_owned());
            if !typed.is_empty() && current.as_deref() != Some(typed.as_str()) {
                self.browse_typed_remote_folder(cx);
                return;
            }
        }
        let Some(path) = self
            .remote_folder_picker
            .as_ref()
            .filter(|picker| can_confirm_folder_path(picker.path.as_deref(), picker.loading))
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
        if let Some(picker) = self.remote_folder_picker.as_mut() {
            if std::mem::take(&mut picker.focus_pending) {
                window.focus(&picker.focus, cx);
            } else if let Some(row_index) = picker.focus_row_pending.take()
                && let Some(handle) = picker.row_focuses.get(row_index)
            {
                window.focus(handle, cx);
                picker.scroll.scroll_to_item(row_index);
            }
        }
        let picker = self.remote_folder_picker.as_ref()?;
        let theme = Theme::current(cx);
        let host = self
            .hosts
            .get(&picker.host)
            .map(|host| host.name.clone())
            .unwrap_or_else(|| tr!("connections.local"));

        let row_count = usize::from(picker.parent.is_some()) + picker.directories.len();
        let parent_for_rows = picker.parent.clone();

        let mut list = div()
            .id("remote-folder-list")
            .track_scroll(&picker.scroll)
            .overflow_y_scroll()
            .h(px(300.0))
            .flex()
            .flex_col()
            .px(px(8.0))
            .py(px(6.0));
        let mut next_row_index = 0;
        if let Some(parent) = picker.parent.clone() {
            let row_index = next_row_index;
            next_row_index += 1;
            list = list.child(folder_row(
                "remote-folder-parent".into(),
                "icons/arrow-up.svg",
                "..".to_owned(),
                row_index,
                row_count,
                picker.row_focuses.get(row_index).cloned(),
                parent_for_rows.clone(),
                theme,
                cx,
                move |this, cx| this.navigate_from_folder_row(parent.clone(), cx),
            ));
        }
        for (index, (name, path)) in picker.directories.iter().enumerate() {
            let row_index = next_row_index;
            next_row_index += 1;
            let path = path.clone();
            list = list.child(folder_row(
                SharedString::from(format!("remote-folder-{index}")),
                "icons/folder.svg",
                name.clone(),
                row_index,
                row_count,
                picker.row_focuses.get(row_index).cloned(),
                parent_for_rows.clone(),
                theme,
                cx,
                move |this, cx| this.navigate_from_folder_row(path.clone(), cx),
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
        } else if !picker.loading && picker.error.is_none() && picker.directories.is_empty() {
            list = list.child(
                div()
                    .px(px(8.0))
                    .py(px(10.0))
                    .text_size(sp(12.5))
                    .text_color(theme.text_tertiary)
                    .child(tr!("remote_folder.empty")),
            );
        }

        let can_confirm = can_confirm_folder_path(picker.path.as_deref(), picker.loading);
        let can_browse = !picker.loading;
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
                } else if !event.keystroke.modifiers.modified()
                    && !this.remote_folder_path_input.read(cx).focus().is_focused(window)
                    && let Some(picker) = this.remote_folder_picker.as_ref()
                    && !picker.row_focuses.is_empty()
                {
                    match event.keystroke.key.as_str() {
                        "down" | "home" => {
                            this.focus_folder_row(0, window, cx);
                            cx.stop_propagation();
                        }
                        "end" => {
                            let last = picker.row_focuses.len().saturating_sub(1);
                            this.focus_folder_row(last, window, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
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
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().min_w_0().child(TextField::new(
                        "remote-folder-path-field",
                        self.remote_folder_path_input.clone(),
                    )))
                    .child(dialog_button(
                        "remote-folder-go",
                        tr!("remote_folder.go"),
                        can_browse,
                        theme,
                        cx,
                        |this, _, cx| this.browse_typed_remote_folder(cx),
                    )),
            )
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

fn resolve_typed_remote_path(typed: &str, current: Option<&Path>, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if typed == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = typed
            .strip_prefix("~/")
            .or_else(|| typed.strip_prefix(r"~\"))
        {
            return home_relative_path(home, rest);
        }
    }
    if is_absolute_remote_path(typed) || typed.starts_with('~') {
        return PathBuf::from(typed);
    }
    if let Some(current) = current {
        return join_remote_relative_path(current, typed);
    }
    PathBuf::from(typed)
}

fn is_absolute_remote_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with('\\')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn join_remote_relative_path(current: &Path, relative: &str) -> PathBuf {
    let current_text = current.to_string_lossy();
    let separator = remote_path_separator(&current_text);
    let (prefix, tail) = split_remote_prefix(&current_text, separator);
    let mut parts: Vec<String> = tail
        .split(separator)
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    for segment in relative.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_owned()),
        }
    }
    let mut joined = prefix.to_owned();
    let sep_str = separator.to_string();
    if prefix.is_empty() {
        joined.push_str(&parts.join(&sep_str));
    } else if prefix == "/" || prefix.ends_with(separator) {
        joined.push_str(&parts.join(&sep_str));
    } else if !parts.is_empty() {
        joined.push(separator);
        joined.push_str(&parts.join(&sep_str));
    } else if separator == '\\' && prefix.len() == 2 && prefix.as_bytes()[1] == b':' {
        joined.push('\\');
    }
    PathBuf::from(joined)
}

fn split_remote_prefix(path: &str, separator: char) -> (&str, &str) {
    let bytes = path.as_bytes();
    if path.starts_with('/') {
        return ("/", &path[1..]);
    }
    if separator == '\\' {
        if let Some(after_unc) = path.strip_prefix(r"\\") {
            let mut slashes = after_unc.match_indices('\\');
            let share_end = slashes
                .nth(1)
                .map(|(index, _)| 2 + index)
                .unwrap_or(path.len());
            return (&path[..share_end], path[share_end..].trim_start_matches('\\'));
        }
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return (&path[..2], path[2..].trim_start_matches(['\\', '/']));
        }
    }
    ("", path)
}

/// Join a `~/` suffix against the daemon host's home directory using that
/// host's separator. The home path, not the desktop platform, decides: a
/// remote Windows home may be drive-qualified or UNC, while a POSIX home is
/// rooted at `/` even if one of its directory names contains a backslash.
fn home_relative_path(home: &Path, rest: &str) -> PathBuf {
    let rest = rest.trim_start_matches(['/', '\\']);
    if rest.is_empty() {
        return home.to_path_buf();
    }
    let home_text = home.to_string_lossy();
    let separator = remote_path_separator(&home_text);
    let base = home_text.trim_end_matches(['/', '\\']);
    let normalized_rest = rest.replace(['/', '\\'], &separator.to_string());
    PathBuf::from(format!("{base}{separator}{normalized_rest}"))
}

fn remote_path_separator(home: &str) -> char {
    let bytes = home.as_bytes();
    if home.starts_with('/') {
        '/'
    } else if home.starts_with(r"\\")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        '\\'
    } else {
        std::path::MAIN_SEPARATOR
    }
}

#[allow(clippy::too_many_arguments)]
fn folder_row(
    id: SharedString,
    icon_path: &'static str,
    label: String,
    row_index: usize,
    row_count: usize,
    focus: Option<FocusHandle>,
    parent: Option<PathBuf>,
    theme: Theme,
    cx: &mut Context<Waku>,
    on_open: impl Fn(&mut Waku, &mut Context<Waku>) + Clone + 'static,
) -> Stateful<Div> {
    let on_key = on_open.clone();
    div()
        .id(id)
        .when_some(focus, |row, focus| row.track_focus(&focus).tab_index(0))
        .h(px(28.0))
        .px(px(8.0))
        .flex_none()
        .rounded(px(7.0))
        .border_1()
        .border_color(gpui::transparent_black())
        .flex()
        .items_center()
        .gap(px(8.0))
        .cursor_default()
        .text_size(sp(13.0))
        .text_color(theme.text)
        .hover(|row| row.bg(theme.overlay))
        .active(|row| row.bg(theme.overlay_strong))
        .focus_visible(|row| row.bg(theme.overlay).border_1().border_color(theme.accent))
        .child(icon(icon_path, 13.0, theme.text_tertiary))
        .child(div().truncate().child(label))
        .on_click(cx.listener(move |this, _, _, cx| on_open(this, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if event.keystroke.modifiers.modified() {
                return;
            }
            match event.keystroke.key.as_str() {
                "enter" | "space" => {
                    on_key(this, cx);
                    cx.stop_propagation();
                }
                "up" => {
                    this.focus_folder_row(row_index.saturating_sub(1), window, cx);
                    cx.stop_propagation();
                }
                "down" => {
                    let next = (row_index + 1).min(row_count.saturating_sub(1));
                    this.focus_folder_row(next, window, cx);
                    cx.stop_propagation();
                }
                "home" => {
                    this.focus_folder_row(0, window, cx);
                    cx.stop_propagation();
                }
                "end" => {
                    this.focus_folder_row(row_count.saturating_sub(1), window, cx);
                    cx.stop_propagation();
                }
                "backspace" => {
                    if let Some(parent) = parent.clone() {
                        this.navigate_from_folder_row(parent, cx);
                        cx.stop_propagation();
                    }
                }
                _ => {}
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
                .tab_index(0)
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

fn can_confirm_folder_path(path: Option<&Path>, loading: bool) -> bool {
    !loading && path.is_some_and(|path| !path.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion_uses_the_hosts_separator() {
        assert_eq!(
            home_relative_path(Path::new(r"C:\Users\me"), ""),
            PathBuf::from(r"C:\Users\me")
        );
        assert_eq!(
            home_relative_path(Path::new(r"C:\Users\me"), "/src/app"),
            PathBuf::from(r"C:\Users\me\src\app")
        );
        assert_eq!(
            home_relative_path(Path::new(r"\\server\share\me"), "src"),
            PathBuf::from(r"\\server\share\me\src")
        );
        assert_eq!(
            home_relative_path(Path::new("/home/me"), r"\src\app"),
            PathBuf::from("/home/me/src/app")
        );
        // A backslash in a POSIX directory name is data, not a separator.
        assert_eq!(
            home_relative_path(Path::new(r"/home/me/odd\name"), "x"),
            PathBuf::from(r"/home/me/odd\name/x")
        );
    }

    #[test]
    fn typed_remote_path_handles_tilde_users_and_relative_paths() {
        let posix_home = Path::new("/home/me");
        let posix_cur = Path::new("/home/me/work");
        assert_eq!(
            resolve_typed_remote_path("~", Some(posix_cur), Some(posix_home)),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            resolve_typed_remote_path("~/repos/waku", Some(posix_cur), Some(posix_home)),
            PathBuf::from("/home/me/repos/waku")
        );
        // `~user` must never expand under the current user's home.
        assert_eq!(
            resolve_typed_remote_path("~otheruser/foo", Some(posix_cur), Some(posix_home)),
            PathBuf::from("~otheruser/foo")
        );
        assert_eq!(
            resolve_typed_remote_path("sub/dir", Some(posix_cur), Some(posix_home)),
            PathBuf::from("/home/me/work/sub/dir")
        );
        assert_eq!(
            resolve_typed_remote_path("../other", Some(posix_cur), Some(posix_home)),
            PathBuf::from("/home/me/other")
        );
        assert_eq!(
            resolve_typed_remote_path("/var/log", Some(posix_cur), Some(posix_home)),
            PathBuf::from("/var/log")
        );

        let win_home = Path::new(r"C:\Users\me");
        let win_cur = Path::new(r"C:\Users\me\work");
        assert_eq!(
            resolve_typed_remote_path(r"~\repos\waku", Some(win_cur), Some(win_home)),
            PathBuf::from(r"C:\Users\me\repos\waku")
        );
        assert_eq!(
            resolve_typed_remote_path("./src/app", Some(win_cur), Some(win_home)),
            PathBuf::from(r"C:\Users\me\work\src\app")
        );
        assert_eq!(
            resolve_typed_remote_path(r"D:\code", Some(win_cur), Some(win_home)),
            PathBuf::from(r"D:\code")
        );
        assert_eq!(
            resolve_typed_remote_path(r"\\server\share\proj", Some(win_cur), Some(win_home)),
            PathBuf::from(r"\\server\share\proj")
        );
    }

    #[test]
    fn windows_drive_root_list_cannot_be_confirmed_as_workspace() {
        assert!(!can_confirm_folder_path(None, false));
        assert!(!can_confirm_folder_path(Some(Path::new("")), false));
        assert!(can_confirm_folder_path(Some(Path::new(r"C:\")), false));
        assert!(!can_confirm_folder_path(Some(Path::new(r"C:\")), true));
    }
}
