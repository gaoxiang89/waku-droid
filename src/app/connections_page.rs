//! The Connections settings page: WSL distributions and SSH hosts whose
//! daemon the workspace can connect to.
//!
//! Each connection joins the current window's host registry and sidebar.
//! Listing distributions, testing, and connecting all start processes, so
//! they run on the background executor; frames read only the cached state.
//! Saving a connection never contacts the host.

use std::collections::HashSet;

use waku_client::remote::{Connection, ConnectionKind, ConnectionsFile, SshAuthMethod, SshTarget};

use gpui::ElementId;

use super::*;

pub(super) struct ConnectionsUi {
    pub(super) file: ConnectionsFile,
    /// `None` until the first listing; `Err` carries a displayable reason.
    distributions: Option<Result<Vec<String>, String>>,
    distributions_loading: bool,
    /// Connection ids with a connect or test in flight.
    pub(super) pending: HashSet<String>,
    host_input: Entity<TextInput>,
    identity_input: Entity<TextInput>,
}

impl ConnectionsUi {
    pub(super) fn new(window: &mut Window, cx: &mut App) -> Self {
        let file = ConnectionsFile::load()
            .inspect_err(|error| eprintln!("could not read saved connections: {error}"))
            .unwrap_or_default();
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
        }
    }

    pub(super) fn inputs(&self) -> [&Entity<TextInput>; 2] {
        [&self.host_input, &self.identity_input]
    }
}

/// Parse the SSH form. A bare word is a `Host` alias from `~/.ssh/config`;
/// `user@host:port` is spelled out. A key path switches to key-only auth.
fn ssh_target_from_fields(host: &str, identity: &str) -> anyhow::Result<SshTarget> {
    let host = host.trim();
    let identity = identity.trim();
    if host.is_empty() {
        anyhow::bail!("{}", tr!("connections.ssh_host_required"));
    }
    let (user, rest) = match host.rsplit_once('@') {
        Some((user, rest)) => (Some(user.to_owned()), rest),
        None => (None, host),
    };
    let (hostname, port) = match rest.rsplit_once(':') {
        Some((hostname, port)) if !hostname.contains(':') => {
            let port = port
                .parse::<u16>()
                .map_err(|_| anyhow::anyhow!("{}", tr!("connections.ssh_port_invalid")))?;
            (hostname.to_owned(), Some(port))
        }
        _ => (rest.to_owned(), None),
    };
    let target = if user.is_none() && port.is_none() && identity.is_empty() {
        SshTarget::ConfigHost {
            host: hostname,
            auth_method: SshAuthMethod::Agent,
        }
    } else {
        SshTarget::Direct {
            hostname,
            port,
            user,
            identity_file: (!identity.is_empty()).then(|| identity.to_owned()),
            auth_method: if identity.is_empty() {
                SshAuthMethod::Agent
            } else {
                SshAuthMethod::Key
            },
            proxy_jump: None,
        }
    };
    // Reuse the transport's own validation so the form rejects exactly what
    // ssh would be handed.
    waku_client::remote::ssh::base_args(&target)?;
    Ok(target)
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
        let task = cx.background_executor().spawn(async move {
            let daemon = waku_client::DaemonSupervisor::connect_remote(connection)?;
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
                    Err(error) => this.show_toast(tr!(
                        "connections.connect_failed",
                        name = name.as_str(),
                        error = format!("{error:#}")
                    )),
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
                        error = error.to_string()
                    )),
                }
                cx.notify();
            });
        })
        .detach();
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
        let target = match ssh_target_from_fields(&host, &identity) {
            Ok(target) => target,
            Err(error) => {
                self.show_toast(format!("{error:#}"));
                return;
            }
        };
        if self.connections.file.connections.iter().any(|existing| {
            matches!(&existing.kind, ConnectionKind::Ssh { target: saved } if *saved == target)
        }) {
            self.show_toast(tr!("connections.ssh_duplicate"));
            return;
        }
        let connection = Connection::ssh(target);
        self.connections.file.upsert(connection);
        self.save_connections();
        self.connections
            .host_input
            .update(cx, |input, cx| input.set_content(String::new(), cx));
        self.connections
            .identity_input
            .update(cx, |input, cx| input.set_content(String::new(), cx));
        cx.notify();
    }

    pub(super) fn remove_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        self.disconnect_host(&waku_client::hosts::HostKey::Connection(id.to_owned()), cx);
        if let Some(removed) = self.connections.file.remove(id) {
            if matches!(removed.kind, ConnectionKind::Ssh { .. }) {
                let _ = waku_client::remote::credentials::delete_ssh_password(id);
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
            let name = if host.key == waku_client::hosts::HostKey::Local {
                tr!("connections.local")
            } else {
                host.name.clone()
            };
            connected = connected.child(connection_row(
                theme,
                if host.key == waku_client::hosts::HostKey::Local {
                    "icons/laptop.svg"
                } else {
                    "icons/server.svg"
                },
                name,
                None,
                status_chip(
                    theme,
                    if self.host_disconnected.contains(&host.key) {
                        tr!("connections.reconnecting")
                    } else {
                        tr!("connections.connected")
                    },
                    if self.host_disconnected.contains(&host.key) {
                        theme.warning
                    } else {
                        theme.text_secondary
                    },
                ),
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
                    let pending = self.connections.pending.contains(&id);
                    let connected = self
                        .hosts
                        .get(&waku_client::hosts::HostKey::Connection(id.clone()))
                        .is_some();
                    let open_distro = distro.clone();
                    card = card.child(connection_row(
                        theme,
                        "icons/terminal-square.svg",
                        distro.clone(),
                        None,
                        div().flex().gap(px(6.0)).child(action_button(
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
                        )),
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
            let pending = self.connections.pending.contains(&connection.id);
            let connected = self
                .hosts
                .get(&waku_client::hosts::HostKey::Connection(
                    connection.id.clone(),
                ))
                .is_some();
            let open = connection.clone();
            let test = connection.clone();
            let remove_id = connection.id.clone();
            let detail = match target.auth_method() {
                SshAuthMethod::Agent => target.display_host(),
                SshAuthMethod::Key => {
                    format!(
                        "{} · {}",
                        target.display_host(),
                        tr!("connections.auth_key")
                    )
                }
                SshAuthMethod::Password => format!(
                    "{} · {}",
                    target.display_host(),
                    tr!("connections.auth_password")
                ),
            };
            card = card.child(connection_row(
                theme,
                "icons/globe.svg",
                connection.name.clone(),
                Some(detail),
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(action_button(
                        SharedString::from(format!("ssh-test-{}", connection.id)),
                        tr!("connections.test"),
                        pending,
                        theme,
                        cx,
                        move |this, cx| this.test_connection(test.clone(), cx),
                    ))
                    .child(action_button(
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
                    ))
                    .child(action_button(
                        SharedString::from(format!("ssh-remove-{}", connection.id)),
                        tr!("connections.remove"),
                        pending,
                        theme,
                        cx,
                        move |this, cx| this.remove_connection(&remove_id, cx),
                    )),
            ));
        }

        card.child(
            div()
                .mt(px(14.0))
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
                    "ssh-add",
                    tr!("connections.add"),
                    false,
                    theme,
                    cx,
                    |this, cx| this.add_ssh_connection(cx),
                )),
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
        .tab_index(0)
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
            ssh_target_from_fields(" devbox ", "").unwrap(),
            SshTarget::ConfigHost {
                host: "devbox".into(),
                auth_method: SshAuthMethod::Agent,
            }
        );
        assert_eq!(
            ssh_target_from_fields("deploy@build.example.com:2222", "").unwrap(),
            SshTarget::Direct {
                hostname: "build.example.com".into(),
                port: Some(2222),
                user: Some("deploy".into()),
                identity_file: None,
                auth_method: SshAuthMethod::Agent,
                proxy_jump: None,
            }
        );
        assert!(matches!(
            ssh_target_from_fields("devbox", "~/.ssh/id_ed25519").unwrap(),
            SshTarget::Direct {
                auth_method: SshAuthMethod::Key,
                ..
            }
        ));
        assert!(ssh_target_from_fields("", "").is_err());
        assert!(ssh_target_from_fields("host:notaport", "").is_err());
        assert!(ssh_target_from_fields("-oProxyCommand=x", "").is_err());
    }
}
