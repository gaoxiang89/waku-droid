//! The Connections settings page: WSL distributions and SSH hosts whose
//! daemon the workspace can connect to.
//!
//! Each connection joins the current window's host registry and sidebar.
//! Listing distributions, testing, and connecting all start processes, so
//! they run on the background executor; frames read only the cached state.
//! Saving a connection never contacts the host.

use std::collections::HashSet;

use waku_client::bridge::IncompatibleDaemon;
use waku_client::remote::{
    Connection, ConnectionKind, ConnectionsFile, IncompatibleDaemonError, SshAuthMethod, SshTarget,
};

use gpui::{ElementId, KeyBinding, actions};

use super::*;

actions!(
    waku_connections,
    [ConfirmDaemonReplace, DismissDaemonReplace]
);

const DAEMON_REPLACE_DIALOG_CONTEXT: &str = "DaemonReplaceDialog";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            "secondary-enter",
            ConfirmDaemonReplace,
            Some(DAEMON_REPLACE_DIALOG_CONTEXT),
        ),
        KeyBinding::new(
            "escape",
            DismissDaemonReplace,
            Some(DAEMON_REPLACE_DIALOG_CONTEXT),
        ),
    ]);
}

/// A host whose running daemon speaks another protocol. Connecting requires
/// ending it and its sessions, which only an explicit confirmation may do.
#[derive(Clone)]
pub(super) struct DaemonReplaceConfirm {
    connection: Connection,
    daemon: IncompatibleDaemon,
    /// Focus moves into the dialog on the first frame that shows it.
    focus_pending: bool,
}

pub(super) struct ConnectionsUi {
    pub(super) file: ConnectionsFile,
    /// `None` until the first listing; `Err` carries a displayable reason.
    distributions: Option<Result<Vec<String>, String>>,
    distributions_loading: bool,
    /// Connection ids with a connect or test in flight.
    pub(super) pending: HashSet<String>,
    host_input: Entity<TextInput>,
    identity_input: Entity<TextInput>,
    password_input: Entity<TextInput>,
    proxy_jump_input: Entity<TextInput>,
    editing_id: Option<String>,
    /// Connection ids known to have a password or key passphrase in the
    /// credential store. Populated once at startup and updated on
    /// save/clear/remove so frames never query the keyring synchronously.
    saved_passwords: HashSet<String>,
    replace_confirm: Option<DaemonReplaceConfirm>,
    replace_cancel_focus: FocusHandle,
    replace_confirm_focus: FocusHandle,
}

impl ConnectionsUi {
    pub(super) fn new(window: &mut Window, cx: &mut App) -> Self {
        let file = ConnectionsFile::load()
            .inspect_err(|error| eprintln!("could not read saved connections: {error}"))
            .unwrap_or_default();
        let saved_passwords = file
            .connections
            .iter()
            .filter(|connection| matches!(connection.kind, ConnectionKind::Ssh { .. }))
            .filter(|connection| waku_client::remote::credentials::has_ssh_password(&connection.id))
            .map(|connection| connection.id.clone())
            .collect();
        Self {
            file,
            distributions: None,
            distributions_loading: false,
            pending: HashSet::new(),
            host_input: cx.new(|cx| {
                TextInput::new(window, cx).placeholder(tr!("connections.ssh_host_placeholder"))
            }),
            identity_input: cx.new(|cx| {
                TextInput::new(window, cx).placeholder(tr!("connections.ssh_key_placeholder"))
            }),
            password_input: cx.new(|cx| {
                TextInput::new(window, cx)
                    .masked(true)
                    .placeholder(tr!("connections.ssh_password_placeholder"))
            }),
            proxy_jump_input: cx.new(|cx| {
                TextInput::new(window, cx)
                    .placeholder(tr!("connections.ssh_proxy_jump_placeholder"))
            }),
            editing_id: None,
            saved_passwords,
            replace_confirm: None,
            replace_cancel_focus: cx.focus_handle(),
            replace_confirm_focus: cx.focus_handle(),
        }
    }

    pub(super) fn inputs(&self) -> [&Entity<TextInput>; 4] {
        [
            &self.host_input,
            &self.identity_input,
            &self.password_input,
            &self.proxy_jump_input,
        ]
    }

    fn password_placeholder(&self) -> String {
        if self
            .editing_id
            .as_deref()
            .is_some_and(|id| self.saved_passwords.contains(id))
        {
            tr!("connections.ssh_password_saved_placeholder")
        } else {
            tr!("connections.ssh_password_placeholder")
        }
    }
}

/// Parse the SSH form. A bare word is a `Host` alias from `~/.ssh/config`;
/// `user@host:port` is spelled out. A key path switches to key-only auth; an
/// empty key path with a saved/entered password switches to password auth; an
/// optional ProxyJump routes the connection through `-J`.
fn ssh_target_from_fields(
    host: &str,
    identity: &str,
    proxy_jump: &str,
    has_password: bool,
) -> anyhow::Result<SshTarget> {
    let host = host.trim();
    let identity = identity.trim();
    let identity = identity
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(identity)
        .trim();
    let proxy_jump = proxy_jump.trim();
    if host.is_empty() {
        anyhow::bail!("{}", tr!("connections.ssh_host_required"));
    }
    let (user, rest) = match host.rsplit_once('@') {
        Some((user, rest)) => (Some(user.to_owned()), rest),
        None => (None, host),
    };
    let (hostname, port) = if let Some(after_open) = rest.strip_prefix('[') {
        let Some((bracketed, trailing)) = after_open.split_once(']') else {
            anyhow::bail!("{}", tr!("connections.ssh_port_invalid"));
        };
        let bracketed = bracketed.trim();
        if bracketed.is_empty() {
            anyhow::bail!("{}", tr!("connections.ssh_host_required"));
        }
        let port = if trailing.is_empty() {
            None
        } else if let Some(port) = trailing.strip_prefix(':') {
            Some(
                port.parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("{}", tr!("connections.ssh_port_invalid")))?,
            )
        } else {
            anyhow::bail!("{}", tr!("connections.ssh_port_invalid"));
        };
        (bracketed.to_owned(), port)
    } else {
        match rest.rsplit_once(':') {
            Some((hostname, port)) if !hostname.contains(':') => {
                let port = port
                    .parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("{}", tr!("connections.ssh_port_invalid")))?;
                (hostname.to_owned(), Some(port))
            }
            _ => (rest.to_owned(), None),
        }
    };
    let auth_method = if !identity.is_empty() {
        SshAuthMethod::Key
    } else if has_password {
        SshAuthMethod::Password
    } else {
        SshAuthMethod::Agent
    };
    let target =
        if user.is_none() && port.is_none() && identity.is_empty() && proxy_jump.is_empty() {
            SshTarget::ConfigHost {
                host: hostname,
                auth_method,
            }
        } else {
            SshTarget::Direct {
                hostname,
                port,
                user,
                identity_file: (!identity.is_empty()).then(|| identity.to_owned()),
                auth_method,
                proxy_jump: (!proxy_jump.is_empty()).then(|| proxy_jump.to_owned()),
            }
        };
    // Reuse the transport's own validation so the form rejects exactly what
    // ssh would be handed.
    waku_client::remote::ssh::base_args(&target)?;
    Ok(target)
}

fn format_ssh_target_host_field(target: &SshTarget) -> String {
    match target {
        SshTarget::ConfigHost { host, .. } => host.clone(),
        SshTarget::Direct {
            hostname,
            user,
            port,
            ..
        } => {
            let mut label = String::new();
            if let Some(user) = user {
                label.push_str(user);
                label.push('@');
            }
            if hostname.contains(':') {
                label.push('[');
                label.push_str(hostname);
                label.push(']');
            } else {
                label.push_str(hostname);
            }
            if let Some(port) = port {
                label.push(':');
                label.push_str(&port.to_string());
            }
            label
        }
    }
}

impl Waku {
    /// Start listing WSL distributions the first time the page opens.
    pub(super) fn ensure_wsl_distributions(&mut self, force: bool, cx: &mut Context<Self>) {
        if !cfg!(windows) || self.connections.distributions_loading {
            return;
        }
        if self.connections.distributions.is_some() && !force {
            return;
        }
        self.connections.distributions_loading = true;
        let list = cx
            .background_executor()
            .spawn(async { waku_client::remote::wsl::list_distributions() });
        cx.spawn(async move |this, cx| {
            let result = list.await.map_err(|error| format!("{error:#}"));
            let _ = this.update(cx, |this, cx| {
                this.connections.distributions_loading = false;
                this.connections.distributions = Some(result);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save_connections(&mut self) {
        if let Err(error) = self.connections.file.save() {
            self.show_toast(tr!("connections.save_failed", error = error.to_string()));
        }
    }

    fn open_wsl_connection(&mut self, distro: String, cx: &mut Context<Self>) {
        let connection = self
            .connections
            .file
            .get(&format!("wsl:{distro}"))
            .cloned()
            .unwrap_or_else(|| Connection::wsl(&distro));
        self.connections.file.upsert(connection.clone());
        self.save_connections();
        self.open_connection(connection, cx);
    }

    /// Connect a host in this window. The first connection to a host may
    /// install the daemon, so the button reports progress until it resolves.
    pub(super) fn open_connection(&mut self, connection: Connection, cx: &mut Context<Self>) {
        self.open_connection_with(connection, false, cx);
    }

    /// `replace_incompatible` ends a host daemon that speaks another protocol
    /// and its sessions; only [`Self::confirm_daemon_replace`] passes `true`.
    fn open_connection_with(
        &mut self,
        connection: Connection,
        replace_incompatible: bool,
        cx: &mut Context<Self>,
    ) {
        if self
            .hosts
            .get(&waku_client::hosts::HostKey::Connection(
                connection.id.clone(),
            ))
            .is_some()
        {
            return;
        }
        if !self.connections.pending.insert(connection.id.clone()) {
            return;
        }
        let id = connection.id.clone();
        let name = connection.name.clone();
        let host_id = id.clone();
        let host_name = name.clone();
        let retry = connection.clone();
        let task = cx.background_executor().spawn(async move {
            let daemon = waku_client::DaemonSupervisor::connect_remote_with(
                connection,
                replace_incompatible,
            )?;
            let host = waku_client::hosts::Host::new(
                waku_client::hosts::HostKey::Connection(host_id),
                host_name,
                daemon,
            );
            let snapshot = runtime::load_remote_task_state(&host.daemon.client())?;
            let drafts = host.drafts.load().unwrap_or_default();
            Ok::<_, anyhow::Error>((host, drafts, snapshot))
        });

        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.connections.pending.remove(&id);
                match result {
                    Ok((host, drafts, snapshot)) => {
                        let key = host.key.clone();
                        this.composer_drafts
                            .new_sessions
                            .extend(drafts.new_sessions);
                        this.composer_drafts.sessions.extend(drafts.sessions);
                        host.store.mark_task_state_loaded();
                        this.hosts.hosts.push(host);
                        this.sync_host_provider_settings(&key);
                        this.apply_remote_task_state(key.clone(), snapshot, cx);
                        this.refresh_host_catalog(key.clone(), None, true);
                        this.start_host_task_state_sync(key);
                    }
                    Err(error) => {
                        if let Some(incompatible) = IncompatibleDaemonError::find(&error)
                            && !replace_incompatible
                        {
                            this.connections.replace_confirm = Some(DaemonReplaceConfirm {
                                connection: retry,
                                daemon: incompatible.0.clone(),
                                focus_pending: true,
                            });
                        } else {
                            this.show_toast(tr!(
                                "connections.connect_failed",
                                name = name.as_str(),
                                error = format!("{error:#}")
                            ));
                        }
                    }
                }
                if this.connections.pending.is_empty() && this.pending_host_session.is_some() {
                    this.pending_host_session = None;
                    this.state.selected_session = None;
                    this.state.selected_project = None;
                    if let Some(id) = this
                        .state
                        .sessions
                        .iter()
                        .max_by_key(|session| session.updated_at)
                        .map(|session| session.id)
                    {
                        this.select_session(id, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn dismiss_daemon_replace(&mut self, cx: &mut Context<Self>) {
        if self.connections.replace_confirm.take().is_some() {
            cx.notify();
        }
    }

    fn confirm_daemon_replace(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.connections.replace_confirm.take() else {
            return;
        };
        self.open_connection_with(dialog.connection, true, cx);
    }

    pub(super) fn render_daemon_replace_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.connections.replace_confirm.as_mut()?;
        if std::mem::take(&mut dialog.focus_pending) {
            let focus = self.connections.replace_cancel_focus.clone();
            window.defer(cx, move |window, cx| window.focus(&focus, cx));
        }
        let dialog = dialog.clone();
        let theme = Theme::current(cx);
        let cancel = div()
            .id("daemon-replace-cancel")
            .track_focus(&self.connections.replace_cancel_focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .h(px(32.0))
            .px(px(14.0))
            .rounded(px(8.0))
            .flex()
            .items_center()
            .cursor_default()
            .text_size(sp(13.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_secondary)
            .bg(theme.overlay)
            .hover(|element| element.bg(theme.overlay_strong))
            .child(tr_cow!("common.cancel"))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.dismiss_daemon_replace(cx);
                    cx.stop_propagation();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| this.dismiss_daemon_replace(cx)));
        let confirm = div()
            .id("daemon-replace-confirm")
            .track_focus(&self.connections.replace_confirm_focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.danger))
            .h(px(32.0))
            .px(px(14.0))
            .rounded(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .text_size(sp(13.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.danger)
            .bg(theme.danger_soft)
            .hover(|element| element.bg(theme.danger.opacity(0.16)))
            .child(tr_cow!("connections.replace_daemon_confirm"))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.confirm_daemon_replace(cx);
                    cx.stop_propagation();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| this.confirm_daemon_replace(cx)));

        let processes = if dialog.daemon.processes.is_empty() {
            div()
                .text_color(theme.text_secondary)
                .child(tr!("connections.replace_daemon_no_processes"))
                .into_any_element()
        } else {
            div()
                .id("daemon-replace-processes")
                .max_h(px(200.0))
                .overflow_y_scroll()
                .rounded(px(8.0))
                .bg(theme.overlay)
                .p(px(8.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .font_family(crate::md::render::MONO_FAMILY)
                .text_size(sp(11.5))
                .children(dialog.daemon.processes.iter().map(|process| {
                    div()
                        .pl(px(12.0 * process.depth.min(6) as f32))
                        .flex()
                        .gap(px(8.0))
                        .min_w_0()
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.text_secondary)
                                .child(process.pid.to_string()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme.text)
                                .child(process.command.clone()),
                        )
                }))
                .when(dialog.daemon.omitted > 0, |list| {
                    list.child(div().text_color(theme.text_secondary).child(tr!(
                        "connections.replace_daemon_more",
                        count = dialog.daemon.omitted.to_string()
                    )))
                })
                .into_any_element()
        };

        let card = div()
            .id("daemon-replace-dialog-card")
            .key_context(DAEMON_REPLACE_DIALOG_CONTEXT)
            .on_action(cx.listener(|this, _: &ConfirmDaemonReplace, _, cx| {
                this.confirm_daemon_replace(cx);
            }))
            .on_action(cx.listener(|this, _: &DismissDaemonReplace, _, cx| {
                this.dismiss_daemon_replace(cx);
            }))
            .tab_group()
            .tab_stop(false)
            .w_full()
            .max_w(px(480.0))
            .overflow_hidden()
            .rounded(px(18.0))
            .bg(theme.composer)
            .shadow_xl()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
                    .child(icon("icons/alert.svg", 15.0, theme.danger))
                    .child(tr!(
                        "connections.replace_daemon_title",
                        name = dialog.connection.name.as_str()
                    )),
            )
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .text_size(sp(12.5))
                    .line_height(sp(17.0))
                    .text_color(theme.text_secondary)
                    .whitespace_normal()
                    .child(tr!(
                        "connections.replace_daemon_message",
                        old = dialog.daemon.protocol_version.to_string(),
                        new = waku_client::PROTOCOL_VERSION.to_string(),
                        pid = dialog.daemon.pid.to_string()
                    ))
                    .child(processes),
            )
            .child(div().mx(px(16.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .p(px(12.0))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(cancel)
                    .child(confirm),
            );
        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        };
        let layer = div()
            .id("daemon-replace-dialog-layer")
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
                cx.listener(|this, _, _, cx| this.dismiss_daemon_replace(cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(4).into_any_element())
    }

    fn test_connection(&mut self, connection: Connection, cx: &mut Context<Self>) {
        if !self.connections.pending.insert(connection.id.clone()) {
            return;
        }
        let id = connection.id.clone();
        let name = connection.name.clone();
        let test = cx
            .background_executor()
            .spawn(async move { waku_client::remote::test_connection(&connection) });
        cx.spawn(async move |this, cx| {
            let result = test.await;
            let _ = this.update(cx, |this, cx| {
                this.connections.pending.remove(&id);
                match result {
                    Ok(_) => this.show_success_toast(tr!(
                        "connections.test_succeeded",
                        name = name.as_str()
                    )),
                    Err(error) => this.show_toast(tr!(
                        "connections.connect_failed",
                        name = name.as_str(),
                        error = format!("{error:#}")
                    )),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn refresh_connections_localized_text(&mut self, cx: &mut Context<Self>) {
        self.connections.host_input.update(cx, |input, cx| {
            input.set_placeholder(tr!("connections.ssh_host_placeholder"), cx);
        });
        self.connections.identity_input.update(cx, |input, cx| {
            input.set_placeholder(tr!("connections.ssh_key_placeholder"), cx);
        });
        let password_placeholder = self.connections.password_placeholder();
        self.connections.password_input.update(cx, |input, cx| {
            input.set_placeholder(password_placeholder, cx);
        });
        self.connections.proxy_jump_input.update(cx, |input, cx| {
            input.set_placeholder(tr!("connections.ssh_proxy_jump_placeholder"), cx);
        });
    }

    fn reconnect_connection(&mut self, connection: Connection, cx: &mut Context<Self>) {
        let key = waku_client::hosts::HostKey::Connection(connection.id.clone());
        self.disconnect_host(&key, cx);
        self.open_connection(connection, cx);
    }

    fn browse_ssh_key(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("connections.ssh_key_placeholder").into()),
        });
        cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        let display = path.to_string_lossy().into_owned();
                        this.connections
                            .identity_input
                            .update(cx, |input, cx| input.set_content(display, cx));
                        cx.notify();
                    });
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                eprintln!("could not open SSH key file prompt: {error:#}");
            }
            Err(error) => {
                eprintln!("SSH key file prompt cancelled: {error}");
            }
        })
        .detach();
    }

    fn begin_edit_ssh_connection(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(target) = self
            .connections
            .file
            .get(&id)
            .and_then(|connection| match &connection.kind {
                ConnectionKind::Ssh { target } => Some(target.clone()),
                _ => None,
            })
        else {
            return;
        };
        let host_text = format_ssh_target_host_field(&target);
        let (identity_text, proxy_jump_text) = match &target {
            SshTarget::ConfigHost { .. } => (String::new(), String::new()),
            SshTarget::Direct {
                identity_file,
                proxy_jump,
                ..
            } => (
                identity_file.clone().unwrap_or_default(),
                proxy_jump.clone().unwrap_or_default(),
            ),
        };
        self.connections.editing_id = Some(id);
        self.connections
            .host_input
            .update(cx, |input, cx| input.set_content(host_text, cx));
        self.connections
            .identity_input
            .update(cx, |input, cx| input.set_content(identity_text, cx));
        self.connections
            .password_input
            .update(cx, |input, cx| input.set_content(String::new(), cx));
        self.connections
            .proxy_jump_input
            .update(cx, |input, cx| input.set_content(proxy_jump_text, cx));
        self.refresh_connections_localized_text(cx);
        cx.notify();
    }

    fn clear_ssh_form(&mut self, cx: &mut Context<Self>) {
        self.connections.editing_id = None;
        for input in self.connections.inputs() {
            input.update(cx, |input, cx| input.set_content(String::new(), cx));
        }
        self.refresh_connections_localized_text(cx);
    }

    fn cancel_edit_ssh_connection(&mut self, cx: &mut Context<Self>) {
        self.clear_ssh_form(cx);
        cx.notify();
    }

    fn clear_ssh_password(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.connections.editing_id.clone() else {
            return;
        };
        if let Err(error) = waku_client::remote::credentials::delete_ssh_password(&id) {
            let message = format!("{error:#}");
            eprintln!("could not delete SSH password for {id}: {message}");
            self.show_toast(message);
            return;
        }
        self.connections.saved_passwords.remove(&id);
        self.connections
            .password_input
            .update(cx, |input, cx| input.set_content(String::new(), cx));
        if let Some(existing) = self
            .connections
            .file
            .connections
            .iter_mut()
            .find(|connection| connection.id == id)
            && let ConnectionKind::Ssh { target } = &mut existing.kind
        {
            match target {
                SshTarget::ConfigHost { auth_method, .. }
                | SshTarget::Direct { auth_method, .. }
                    if *auth_method == SshAuthMethod::Password =>
                {
                    *auth_method = SshAuthMethod::Agent;
                    self.save_connections();
                }
                _ => {}
            }
        }
        self.refresh_connections_localized_text(cx);
        cx.notify();
    }

    pub(super) fn add_ssh_connection(&mut self, cx: &mut Context<Self>) {
        let host = self.connections.host_input.read(cx).content().to_owned();
        let identity = self
            .connections
            .identity_input
            .read(cx)
            .content()
            .to_owned();
        let password = self
            .connections
            .password_input
            .read(cx)
            .content()
            .to_owned();
        let proxy_jump = self
            .connections
            .proxy_jump_input
            .read(cx)
            .content()
            .to_owned();

        let editing_target = self.connections.editing_id.as_ref().and_then(|id| {
            self.connections
                .file
                .get(id)
                .and_then(|connection| match &connection.kind {
                    ConnectionKind::Ssh { target } => Some((id.clone(), target.clone())),
                    _ => None,
                })
        });
        let editing_id = editing_target.as_ref().map(|(id, _)| id.clone());
        let keep_saved_password = password.is_empty()
            && editing_target.as_ref().is_some_and(|(id, existing)| {
                self.connections.saved_passwords.contains(id)
                    && existing.auth_method() == SshAuthMethod::Password
            });
        let has_password = !password.is_empty() || keep_saved_password;

        let target = match ssh_target_from_fields(&host, &identity, &proxy_jump, has_password) {
            Ok(target) => target,
            Err(error) => {
                self.show_toast(format!("{error:#}"));
                return;
            }
        };
        if self.connections.file.connections.iter().any(|existing| {
            Some(existing.id.as_str()) != editing_id.as_deref()
                && matches!(&existing.kind, ConnectionKind::Ssh { target: saved } if *saved == target)
        }) {
            self.show_toast(tr!("connections.ssh_duplicate"));
            return;
        }

        let mut connection = Connection::ssh(target.clone());
        if let Some(id) = editing_id {
            connection.id = id;
        }

        if !password.is_empty() {
            if let Err(error) =
                waku_client::remote::credentials::set_ssh_password(&connection.id, &password)
            {
                let message = format!("{error:#}");
                eprintln!(
                    "could not save SSH password for {}: {message}",
                    connection.id
                );
                self.show_toast(message);
                return;
            }
            self.connections
                .saved_passwords
                .insert(connection.id.clone());
        } else if matches!(target.auth_method(), SshAuthMethod::Agent)
            && self.connections.saved_passwords.contains(&connection.id)
        {
            if let Err(error) =
                waku_client::remote::credentials::delete_ssh_password(&connection.id)
            {
                let message = format!("{error:#}");
                eprintln!(
                    "could not delete SSH password for {}: {message}",
                    connection.id
                );
                self.show_toast(message);
                return;
            }
            self.connections.saved_passwords.remove(&connection.id);
        }

        self.connections.file.upsert(connection);
        self.save_connections();
        self.clear_ssh_form(cx);
        cx.notify();
    }

    pub(super) fn remove_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        self.disconnect_host(&waku_client::hosts::HostKey::Connection(id.to_owned()), cx);
        if self.connections.editing_id.as_deref() == Some(id) {
            self.clear_ssh_form(cx);
        }
        if let Some(removed) = self.connections.file.remove(id) {
            if matches!(removed.kind, ConnectionKind::Ssh { .. }) {
                if let Err(error) = waku_client::remote::credentials::delete_ssh_password(id) {
                    let message = format!("{error:#}");
                    eprintln!("could not delete SSH password for {id}: {message}");
                    self.show_toast(message);
                } else {
                    self.connections.saved_passwords.remove(id);
                }
            }
            self.save_connections();
        }
        cx.notify();
    }

    pub(super) fn render_connections_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let mut connected = settings_card(theme)
            .child(card_title(theme, tr!("connections.connected_hosts")))
            .child(card_description(theme, tr!("connections.description")));
        for host in &self.hosts.hosts {
            let is_local = host.key == waku_client::hosts::HostKey::Local;
            let name = if is_local {
                tr!("connections.local")
            } else {
                host.name.clone()
            };
            let reconnecting = self.host_disconnected.contains(&host.key);
            let chip = status_chip(
                theme,
                if reconnecting {
                    tr!("connections.reconnecting")
                } else {
                    tr!("connections.connected")
                },
                if reconnecting {
                    theme.warning
                } else {
                    theme.text_secondary
                },
            );
            let mut actions = div().flex().items_center().gap(px(6.0)).child(chip);
            if !is_local {
                let host_id = match &host.key {
                    waku_client::hosts::HostKey::Connection(id) => id.as_str(),
                    waku_client::hosts::HostKey::Local => "local",
                };
                if reconnecting
                    && let waku_client::hosts::HostKey::Connection(id) = &host.key
                    && let Some(connection) = self
                        .connections
                        .file
                        .get(id)
                        .cloned()
                        .or_else(|| id.strip_prefix("wsl:").map(Connection::wsl))
                {
                    let pending = self.connections.pending.contains(id);
                    actions = actions.child(action_button(
                        SharedString::from(format!("connected-reconnect-{host_id}")),
                        if pending {
                            tr!("connections.connecting")
                        } else {
                            tr!("connections.reconnect")
                        },
                        pending,
                        theme,
                        cx,
                        move |this, cx| this.reconnect_connection(connection.clone(), cx),
                    ));
                }
                let disconnect_key = host.key.clone();
                actions = actions.child(action_button(
                    SharedString::from(format!("connected-disconnect-{host_id}")),
                    tr!("connections.disconnect"),
                    false,
                    theme,
                    cx,
                    move |this, cx| this.disconnect_host(&disconnect_key, cx),
                ));
            }
            connected = connected.child(connection_row(
                theme,
                if is_local {
                    "icons/laptop.svg"
                } else {
                    "icons/server.svg"
                },
                name,
                None,
                actions,
            ));
        }
        let mut column = div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(connected);

        if cfg!(windows) {
            column = column.child(self.render_wsl_card(cx));
        }
        column.child(self.render_ssh_card(cx)).into_any_element()
    }

    fn render_wsl_card(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let refresh = action_button(
            "wsl-refresh",
            tr!("connections.refresh"),
            self.connections.distributions_loading,
            theme,
            cx,
            |this, cx| this.ensure_wsl_distributions(true, cx),
        );
        let mut card = settings_card(theme).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(card_title(theme, tr!("connections.wsl_title")))
                .child(refresh),
        );
        match &self.connections.distributions {
            None => {
                card = card.child(card_description(theme, tr!("connections.wsl_loading")));
            }
            Some(Err(error)) => {
                card = card.child(card_description(
                    theme,
                    tr!("connections.wsl_unavailable", error = error.as_str()),
                ));
            }
            Some(Ok(distributions)) if distributions.is_empty() => {
                card = card.child(card_description(theme, tr!("connections.wsl_empty")));
            }
            Some(Ok(distributions)) => {
                for distro in distributions {
                    let id = format!("wsl:{distro}");
                    let host_key = waku_client::hosts::HostKey::Connection(id.clone());
                    let pending = self.connections.pending.contains(&id);
                    let connected = self.hosts.get(&host_key).is_some();
                    let reconnecting = connected && self.host_disconnected.contains(&host_key);
                    let open_distro = distro.clone();
                    let mut actions = div().flex().items_center().gap(px(6.0));
                    if connected && reconnecting {
                        let reconnect = self
                            .connections
                            .file
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(|| Connection::wsl(distro));
                        actions = actions.child(action_button(
                            SharedString::from(format!("wsl-reconnect-{distro}")),
                            if pending {
                                tr!("connections.connecting")
                            } else {
                                tr!("connections.reconnect")
                            },
                            pending,
                            theme,
                            cx,
                            move |this, cx| this.reconnect_connection(reconnect.clone(), cx),
                        ));
                    } else {
                        actions = actions.child(action_button(
                            SharedString::from(format!("wsl-open-{distro}")),
                            if pending {
                                tr!("connections.connecting")
                            } else if connected {
                                tr!("connections.connected")
                            } else {
                                tr!("connections.open")
                            },
                            pending || connected,
                            theme,
                            cx,
                            move |this, cx| this.open_wsl_connection(open_distro.clone(), cx),
                        ));
                    }
                    if connected {
                        let disconnect_key = host_key.clone();
                        actions = actions.child(action_button(
                            SharedString::from(format!("wsl-disconnect-{distro}")),
                            tr!("connections.disconnect"),
                            false,
                            theme,
                            cx,
                            move |this, cx| this.disconnect_host(&disconnect_key, cx),
                        ));
                    }
                    card = card.child(connection_row(
                        theme,
                        "icons/terminal-square.svg",
                        distro.clone(),
                        reconnecting.then(|| tr!("connections.reconnecting")),
                        actions,
                    ));
                }
            }
        }
        card
    }

    fn render_ssh_card(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let mut card = settings_card(theme)
            .child(card_title(theme, tr!("connections.ssh_title")))
            .child(card_description(theme, tr!("connections.ssh_description")));

        let saved = self
            .connections
            .file
            .connections
            .iter()
            .filter_map(|connection| match &connection.kind {
                ConnectionKind::Ssh { target } => Some((connection, target)),
                _ => None,
            });
        for (connection, target) in saved {
            let host_key = waku_client::hosts::HostKey::Connection(connection.id.clone());
            let pending = self.connections.pending.contains(&connection.id);
            let connected = self.hosts.get(&host_key).is_some();
            let reconnecting = connected && self.host_disconnected.contains(&host_key);
            let open = connection.clone();
            let test = connection.clone();
            let edit_id = connection.id.clone();
            let remove_id = connection.id.clone();
            let mut detail_parts = vec![target.display_host()];
            match target.auth_method() {
                SshAuthMethod::Agent => {}
                SshAuthMethod::Key => detail_parts.push(tr!("connections.auth_key")),
                SshAuthMethod::Password => detail_parts.push(tr!("connections.auth_password")),
            }
            if let SshTarget::Direct {
                proxy_jump: Some(jump),
                ..
            } = target
            {
                detail_parts.push(format!("-J {jump}"));
            }
            if reconnecting {
                detail_parts.push(tr!("connections.reconnecting"));
            }
            let detail = detail_parts.join(" · ");

            let mut actions = div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(action_button(
                    SharedString::from(format!("ssh-test-{}", connection.id)),
                    tr!("connections.test"),
                    pending,
                    theme,
                    cx,
                    move |this, cx| this.test_connection(test.clone(), cx),
                ));
            if connected && reconnecting {
                let reconnect = connection.clone();
                actions = actions.child(action_button(
                    SharedString::from(format!("ssh-reconnect-{}", connection.id)),
                    if pending {
                        tr!("connections.connecting")
                    } else {
                        tr!("connections.reconnect")
                    },
                    pending,
                    theme,
                    cx,
                    move |this, cx| this.reconnect_connection(reconnect.clone(), cx),
                ));
            } else {
                actions = actions.child(action_button(
                    SharedString::from(format!("ssh-open-{}", connection.id)),
                    if pending {
                        tr!("connections.connecting")
                    } else if connected {
                        tr!("connections.connected")
                    } else {
                        tr!("connections.open")
                    },
                    pending || connected,
                    theme,
                    cx,
                    move |this, cx| this.open_connection(open.clone(), cx),
                ));
            }
            if connected {
                let disconnect_key = host_key.clone();
                actions = actions.child(action_button(
                    SharedString::from(format!("ssh-disconnect-{}", connection.id)),
                    tr!("connections.disconnect"),
                    false,
                    theme,
                    cx,
                    move |this, cx| this.disconnect_host(&disconnect_key, cx),
                ));
            }
            actions = actions
                .child(action_button(
                    SharedString::from(format!("ssh-edit-{}", connection.id)),
                    tr!("connections.edit"),
                    pending,
                    theme,
                    cx,
                    move |this, cx| this.begin_edit_ssh_connection(edit_id.clone(), cx),
                ))
                .child(action_button(
                    SharedString::from(format!("ssh-remove-{}", connection.id)),
                    tr!("connections.remove"),
                    pending,
                    theme,
                    cx,
                    move |this, cx| this.remove_connection(&remove_id, cx),
                ));

            card = card.child(connection_row(
                theme,
                "icons/globe.svg",
                connection.name.clone(),
                Some(detail),
                actions,
            ));
        }

        let editing_id = self.connections.editing_id.as_deref();
        let has_saved_password =
            editing_id.is_some_and(|id| self.connections.saved_passwords.contains(id));

        let mut form_actions = div().flex().items_center().gap(px(6.0));
        if editing_id.is_some() {
            if has_saved_password {
                form_actions = form_actions.child(action_button(
                    "ssh-clear-password",
                    tr!("connections.clear_password"),
                    false,
                    theme,
                    cx,
                    |this, cx| this.clear_ssh_password(cx),
                ));
            }
            form_actions = form_actions
                .child(action_button(
                    "ssh-cancel-edit",
                    tr!("common.cancel"),
                    false,
                    theme,
                    cx,
                    |this, cx| this.cancel_edit_ssh_connection(cx),
                ))
                .child(action_button(
                    "ssh-save",
                    tr!("connections.save"),
                    false,
                    theme,
                    cx,
                    |this, cx| this.add_ssh_connection(cx),
                ));
        } else {
            form_actions = form_actions.child(action_button(
                "ssh-add",
                tr!("connections.add"),
                false,
                theme,
                cx,
                |this, cx| this.add_ssh_connection(cx),
            ));
        }

        card.child(
            div()
                .mt(px(14.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().flex_1().min_w_0().child(TextField::new(
                            "ssh-host-field",
                            self.connections.host_input.clone(),
                        )))
                        .child(div().w(px(220.0)).flex_none().child(TextField::new(
                            "ssh-key-field",
                            self.connections.identity_input.clone(),
                        )))
                        .child(action_button(
                            "ssh-browse-key",
                            tr!("connections.browse_key"),
                            false,
                            theme,
                            cx,
                            |this, cx| this.browse_ssh_key(cx),
                        )),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().flex_1().min_w_0().child(TextField::new(
                            "ssh-password-field",
                            self.connections.password_input.clone(),
                        )))
                        .child(div().w(px(220.0)).flex_none().child(TextField::new(
                            "ssh-proxy-jump-field",
                            self.connections.proxy_jump_input.clone(),
                        )))
                        .child(form_actions),
                ),
        )
    }
}

fn settings_card(theme: Theme) -> Div {
    div()
        .w_full()
        .px(px(20.0))
        .py(px(15.0))
        .rounded(px(13.0))
        .bg(theme.raised)
}

fn card_title(theme: Theme, title: String) -> Div {
    div()
        .text_size(sp(13.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(title)
}

fn card_description(theme: Theme, text: String) -> Div {
    div()
        .mt(px(5.0))
        .min_w_0()
        .whitespace_normal()
        .text_size(sp(12.5))
        .line_height(sp(18.0))
        .text_color(theme.text_secondary)
        .child(text)
}

fn status_chip(theme: Theme, label: String, color: Hsla) -> Div {
    div()
        .px(px(6.0))
        .py(px(2.0))
        .rounded_full()
        .text_size(sp(12.0))
        .text_color(color)
        .bg(theme.overlay)
        .child(label)
}

fn connection_row(
    theme: Theme,
    icon_path: &'static str,
    title: String,
    detail: Option<String>,
    actions: Div,
) -> Div {
    div()
        .mt(px(10.0))
        .min_h(px(36.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(icon(icon_path, 14.0, theme.text_tertiary))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(sp(13.0))
                        .text_color(theme.text)
                        .truncate()
                        .child(title),
                )
                .children(detail.map(|detail| {
                    div()
                        .text_size(sp(12.0))
                        .text_color(theme.text_tertiary)
                        .truncate()
                        .child(detail)
                })),
        )
        .child(actions)
}

/// A bordered text button, operable by mouse and by `enter`/`space` when
/// focused.
fn action_button(
    id: impl Into<ElementId>,
    label: String,
    disabled: bool,
    theme: Theme,
    cx: &mut Context<Waku>,
    on_press: impl Fn(&mut Waku, &mut Context<Waku>) + Clone + 'static,
) -> Stateful<Div> {
    let on_key = on_press.clone();
    div()
        .id(id)
        .h(px(27.0))
        .px(px(10.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(theme.border_strong)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .cursor_default()
        .text_size(sp(12.5))
        .text_color(theme.text_secondary)
        .opacity(if disabled { 0.55 } else { 1.0 })
        .focus_visible(|style| style.border_color(theme.accent))
        .when(!disabled, |element| {
            element
                .tab_index(0)
                .hover(|element| element.bg(theme.overlay))
                .active(|element| element.bg(theme.overlay_strong))
                .on_click(cx.listener(move |this, _, _, cx| on_press(this, cx)))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        on_key(this, cx);
                        cx.stop_propagation();
                    }
                }))
        })
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_form_distinguishes_aliases_from_explicit_targets() {
        assert_eq!(
            ssh_target_from_fields(" devbox ", "", "", false).unwrap(),
            SshTarget::ConfigHost {
                host: "devbox".into(),
                auth_method: SshAuthMethod::Agent,
            }
        );
        assert_eq!(
            ssh_target_from_fields(" devbox ", "", "", true).unwrap(),
            SshTarget::ConfigHost {
                host: "devbox".into(),
                auth_method: SshAuthMethod::Password,
            }
        );
        assert_eq!(
            ssh_target_from_fields("deploy@build.example.com:2222", "", "", false).unwrap(),
            SshTarget::Direct {
                hostname: "build.example.com".into(),
                port: Some(2222),
                user: Some("deploy".into()),
                identity_file: None,
                auth_method: SshAuthMethod::Agent,
                proxy_jump: None,
            }
        );
        assert_eq!(
            ssh_target_from_fields("deploy@build.example.com:2222", "", "bastion", true).unwrap(),
            SshTarget::Direct {
                hostname: "build.example.com".into(),
                port: Some(2222),
                user: Some("deploy".into()),
                identity_file: None,
                auth_method: SshAuthMethod::Password,
                proxy_jump: Some("bastion".into()),
            }
        );
        assert_eq!(
            ssh_target_from_fields("devbox", "~/.ssh/id_ed25519", "jump@bastion:2200", true)
                .unwrap(),
            SshTarget::Direct {
                hostname: "devbox".into(),
                port: None,
                user: None,
                identity_file: Some("~/.ssh/id_ed25519".into()),
                auth_method: SshAuthMethod::Key,
                proxy_jump: Some("jump@bastion:2200".into()),
            }
        );
        assert!(ssh_target_from_fields("", "", "", false).is_err());
        assert!(ssh_target_from_fields("host:notaport", "", "", false).is_err());
        assert!(ssh_target_from_fields("-oProxyCommand=x", "", "", false).is_err());
        assert!(ssh_target_from_fields("devbox", "", "-oProxyCommand=x", false).is_err());

        assert_eq!(
            ssh_target_from_fields("[::1]:2222", "", "", false).unwrap(),
            SshTarget::Direct {
                hostname: "::1".into(),
                port: Some(2222),
                user: None,
                identity_file: None,
                auth_method: SshAuthMethod::Agent,
                proxy_jump: None,
            }
        );
        let ipv6_target = ssh_target_from_fields(
            "admin@[2001:db8::1]:2200",
            r#""C:\Users\me\.ssh\id_ed25519""#,
            "",
            false,
        )
        .unwrap();
        assert_eq!(
            ipv6_target,
            SshTarget::Direct {
                hostname: "2001:db8::1".into(),
                port: Some(2200),
                user: Some("admin".into()),
                identity_file: Some(r"C:\Users\me\.ssh\id_ed25519".into()),
                auth_method: SshAuthMethod::Key,
                proxy_jump: None,
            }
        );
        assert_eq!(
            format_ssh_target_host_field(&ipv6_target),
            "admin@[2001:db8::1]:2200"
        );
        assert_eq!(
            ssh_target_from_fields("[2001:db8::1]", "", "", false).unwrap(),
            SshTarget::ConfigHost {
                host: "2001:db8::1".into(),
                auth_method: SshAuthMethod::Agent,
            }
        );
        assert!(ssh_target_from_fields("[::1", "", "", false).is_err());
        assert!(ssh_target_from_fields("[]", "", "", false).is_err());
    }
}
