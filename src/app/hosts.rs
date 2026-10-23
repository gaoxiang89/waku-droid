use super::sidebar::{SIDEBAR_HOST_DIVIDER_GAP, SIDEBAR_HOST_ROW_HEIGHT};
use super::*;
use waku_client::hosts::HostKey;

pub(super) struct HostCatalog {
    probes: Vec<ProviderProbe>,
    pending: HashSet<ProviderKind>,
    discovered: HashSet<ProviderKind>,
}

impl Waku {
    pub(super) fn provider_probes_for_host(&self, key: &HostKey) -> &[ProviderProbe] {
        if *key == HostKey::Local {
            &self.probes
        } else {
            self.host_catalogs
                .get(key)
                .map(|catalog| catalog.probes.as_slice())
                .unwrap_or(&self.probes)
        }
    }

    pub(super) fn provider_probes(&self) -> &[ProviderProbe] {
        self.provider_probes_for_host(&self.selected_host_key())
    }

    pub(super) fn provider_probe_for_session(
        &self,
        session: &AgentSession,
    ) -> Option<&ProviderProbe> {
        self.provider_probes_for_host(&self.hosts.ownership.session(session.id))
            .iter()
            .find(|probe| probe.provider == session.provider)
    }

    pub(super) fn pending_model_discoveries(&self) -> &HashSet<ProviderKind> {
        self.host_catalogs
            .get(&self.selected_host_key())
            .map(|catalog| &catalog.pending)
            .unwrap_or(&self.provider_model_discoveries_pending)
    }

    /// Whether a provider probe for this host is still in flight, for the
    /// Providers page's checking state.
    pub(super) fn host_catalog_pending(&self, key: &HostKey) -> bool {
        self.host_catalogs
            .get(key)
            .is_some_and(|catalog| !catalog.pending.is_empty())
    }

    /// Switch the Providers page to another host. The pending binary override
    /// edit commits to the host being left before the move; if a row stays
    /// expanded, its input reloads the new host's override.
    pub(super) fn set_provider_settings_host(&mut self, key: HostKey, cx: &mut Context<Self>) {
        if key == self.provider_settings_host || self.hosts.get(&key).is_none() {
            return;
        }
        self.apply_provider_path_override(cx);
        self.provider_settings_host = key.clone();
        if key != HostKey::Local {
            self.sync_host_provider_settings(&key);
        }
        if let Some(provider) = self.expanded_provider_settings {
            let override_value = self
                .provider_override_for_host(&key, provider)
                .cloned()
                .unwrap_or_default();
            self.provider_path_input
                .update(cx, |input, cx| input.set_content(override_value, cx));
        }
        cx.notify();
    }

    /// Providers switched off for new sessions on this host. Local is the
    /// desktop preference; a remote host reads the snapshot of its daemon.
    pub(super) fn disabled_providers_for(&self, key: &HostKey) -> &[ProviderKind] {
        if *key == HostKey::Local {
            &self.state.disabled_providers
        } else {
            self.host_disabled_providers
                .get(key)
                .map(Vec::as_slice)
                .unwrap_or(&[])
        }
    }

    /// The override shown for a provider on the Providers page: the desktop's
    /// own setting for Local, the host snapshot for everyone else.
    pub(super) fn provider_override_for_host(
        &self,
        key: &HostKey,
        provider: ProviderKind,
    ) -> Option<&String> {
        if *key == HostKey::Local {
            self.state.provider_binary_overrides.get(&provider)
        } else {
            self.host_provider_overrides
                .get(key)
                .and_then(|overrides| overrides.get(&provider))
        }
    }

    /// Refresh the render snapshot of a remote host's provider settings from
    /// its daemon. Runs in click handlers and on connect, never in render.
    pub(super) fn sync_host_provider_settings(&mut self, key: &HostKey) {
        if let Some(host) = self.hosts.get(key) {
            let settings = host.daemon.settings();
            self.host_provider_overrides
                .insert(key.clone(), settings.provider_binary_overrides);
            self.host_disabled_providers
                .insert(key.clone(), settings.disabled_providers);
        }
    }

    /// Persist whether a provider may back new sessions on a remote host.
    /// Returns false when the daemon rejected the write.
    pub(super) fn set_remote_provider_enabled(
        &mut self,
        key: &HostKey,
        provider: ProviderKind,
        enabled: bool,
    ) -> bool {
        let Some(host) = self.hosts.get(key) else {
            return false;
        };
        let mut settings = host.daemon.settings();
        if enabled {
            settings.disabled_providers.retain(|kind| *kind != provider);
        } else if !settings.disabled_providers.contains(&provider) {
            settings.disabled_providers.push(provider);
        }
        let name = host.name.clone();
        if let Err(error) = host.daemon.update_settings(settings) {
            self.show_toast(tr!(
                "errors.save_remote_settings",
                name = name.as_str(),
                error = error.to_string()
            ));
            return false;
        }
        self.sync_host_provider_settings(key);
        true
    }

    /// Commit a binary override edit against a remote host's daemon settings:
    /// empty means detect from PATH there. The write persists on that host,
    /// and a forced re-probe picks the new path up.
    pub(super) fn apply_host_provider_override(
        &mut self,
        key: &HostKey,
        provider: ProviderKind,
        text: &str,
    ) {
        let Some(host) = self.hosts.get(key) else {
            return;
        };
        let current = self
            .host_provider_overrides
            .get(key)
            .and_then(|overrides| overrides.get(&provider))
            .cloned();
        let changed = match current.as_deref() {
            Some(value) => value != text,
            None => !text.is_empty(),
        };
        if !changed {
            return;
        }
        let mut settings = host.daemon.settings();
        if text.is_empty() {
            settings.provider_binary_overrides.remove(&provider);
        } else {
            settings
                .provider_binary_overrides
                .insert(provider, text.to_owned());
        }
        if let Err(error) = host.daemon.update_settings(settings) {
            self.show_toast(tr!(
                "errors.save_remote_settings",
                name = host.name.as_str(),
                error = error.to_string()
            ));
            return;
        }
        self.sync_host_provider_settings(key);
        self.refresh_host_catalog(key.clone(), Some(provider), true);
    }

    pub(super) fn refresh_host_catalog(
        &mut self,
        key: HostKey,
        provider: Option<ProviderKind>,
        force: bool,
    ) {
        if key == HostKey::Local {
            return;
        }
        let Some(host) = self.hosts.get(&key) else {
            return;
        };
        let daemon = host.daemon.clone();
        let generation = host.generation;
        let disabled = self
            .disabled_providers_for(&key)
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let catalog = self
            .host_catalogs
            .entry(key.clone())
            .or_insert_with(|| HostCatalog {
                probes: self
                    .probes
                    .iter()
                    .cloned()
                    .map(|mut probe| {
                        probe.installed = false;
                        probe.path = None;
                        probe
                    })
                    .collect(),
                pending: HashSet::new(),
                discovered: HashSet::new(),
            });
        let providers = provider
            .map(|provider| vec![provider])
            .unwrap_or_else(|| ProviderKind::ALL.to_vec());
        let providers = providers
            .into_iter()
            .filter(|provider| {
                !disabled.contains(provider)
                    && !catalog.pending.contains(provider)
                    && (force || !catalog.discovered.contains(provider))
            })
            .collect::<Vec<_>>();
        if providers.is_empty() {
            return;
        }
        catalog.pending.extend(providers.iter().copied());
        let tx = self.host_catalog_tx.clone();
        let wake = self.event_wake_tx.clone();
        let failure_key = key.clone();
        if std::thread::Builder::new()
            .name("waku-host-models".into())
            .spawn(move || {
                let settings = daemon.settings();
                for provider in providers {
                    let result = daemon.client().request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::ProbeProvider {
                            provider,
                            binary_override: settings
                                .provider_binary_overrides
                                .get(&provider)
                                .cloned(),
                            discover_models: true,
                            probe_version: false,
                        },
                    );
                    let probe = match result {
                        Ok(waku_client::ResponsePayload::ProviderProbe { probe, .. }) => {
                            Some(probe)
                        }
                        _ => None,
                    };
                    if tx.send((key.clone(), generation, provider, probe)).is_err() {
                        return;
                    }
                    signal_event_pump(&wake);
                }
            })
            .is_err()
        {
            if let Some(catalog) = self.host_catalogs.get_mut(&failure_key) {
                catalog.pending.clear();
            }
        }
    }

    pub(super) fn drain_host_catalog_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok((key, generation, provider, probe)) = self.host_catalog_events.try_recv() {
            if !self
                .hosts
                .get(&key)
                .is_some_and(|host| host.generation == generation)
            {
                continue;
            }
            let Some(catalog) = self.host_catalogs.get_mut(&key) else {
                continue;
            };
            catalog.pending.remove(&provider);
            if let Some(probe) = probe {
                catalog.discovered.insert(provider);
                if let Some(existing) = catalog
                    .probes
                    .iter_mut()
                    .find(|candidate| candidate.provider == provider)
                {
                    *existing = probe;
                } else {
                    catalog.probes.push(probe);
                }
            }
            changed = true;
        }
        changed
    }
    pub(super) fn render_sidebar_host(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(host) = self.hosts.hosts.get(index) else {
            return div().into_any_element();
        };
        let theme = Theme::current(cx);
        let key = host.key.clone();
        let collapsed = self.collapsed_hosts.contains(&key);
        let disconnected = self.host_disconnected.contains(&key);
        let name = if key == HostKey::Local {
            tr!("connections.local")
        } else {
            host.name.clone()
        };
        let focuses = self
            .host_focuses
            .borrow_mut()
            .entry(key.clone())
            .or_insert_with(|| [cx.focus_handle(), cx.focus_handle()])
            .clone();
        // Only remote hosts have a menu: disconnect and remove target a saved
        // connection, which the local host is not.
        // Row actions stay out of the way until the row is hovered or focused.
        let row_group = SharedString::from(format!("sidebar-host-row-{index}"));
        let options = if let HostKey::Connection(id) = &key {
            let menu = self.menu_handle(format!("host-menu-{index}"), cx);
            let menu_open = menu.is_open();
            let weak = cx.entity().downgrade();
            let menu_key = key.clone();
            let menu_id = id.clone();
            Some(dropdown_menu(
                div()
                    .id(SharedString::from(format!("host-options-{index}")))
                    .size(px(26.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .opacity(if menu_open { 1.0 } else { 0.0 })
                    .group_hover(row_group.clone(), |style| style.opacity(1.0))
                    .hover(|style| style.bg(theme.overlay))
                    .focus_visible(|style| style.opacity(1.0).border_1().border_color(theme.accent))
                    .child(icon("icons/ellipsis.svg", 14.0, theme.text_secondary)),
                SharedString::from(format!("host-options-menu-{index}")),
                &menu,
                MenuAlign::BelowRight,
                move |_| {
                    let disconnect = weak.clone();
                    let disconnect_key = menu_key.clone();
                    let remove = weak.clone();
                    let id = menu_id.clone();
                    vec![
                        MenuItem::new(tr!("connections.disconnect"), move |_, cx| {
                            let _ = disconnect
                                .update(cx, |this, cx| this.disconnect_host(&disconnect_key, cx));
                        }),
                        MenuItem::new(tr!("connections.remove"), move |_, cx| {
                            let _ = remove.update(cx, |this, cx| this.remove_connection(&id, cx));
                        }),
                    ]
                },
            ))
        } else {
            None
        };
        let add_key = key.clone();
        let add_key_down = key.clone();
        let add = div()
            .id(SharedString::from(format!("host-add-{index}")))
            .track_focus(&focuses[1])
            .tab_index(0)
            .tab_stop(true)
            .size(px(26.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .justify_center()
            .opacity(0.0)
            .group_hover(row_group.clone(), |style| style.opacity(1.0))
            .hover(|style| style.bg(theme.overlay))
            .focus_visible(|style| style.opacity(1.0).border_1().border_color(theme.accent))
            .tooltip(Tooltip::text(tr!("project.add_project")))
            .child(icon("icons/plus.svg", 14.0, theme.text_secondary))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.add_project_on_host(add_key.clone(), cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    this.add_project_on_host(add_key_down.clone(), cx);
                }
            }));
        let click_key = key.clone();
        let row = div()
            .id(SharedString::from(format!("sidebar-host-{index}")))
            .group(row_group)
            .track_focus(&focuses[0])
            .tab_group()
            .tab_index(0)
            .tab_stop(true)
            .h(px(SIDEBAR_HOST_ROW_HEIGHT))
            .w_full()
            .px(px(4.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|style| style.bg(theme.sidebar_item_background))
            .child(icon(
                if collapsed {
                    "icons/chevron-right.svg"
                } else {
                    "icons/chevron-down.svg"
                },
                12.0,
                theme.text_tertiary,
            ))
            .child(icon(
                if key == HostKey::Local {
                    "icons/laptop.svg"
                } else {
                    "icons/server.svg"
                },
                14.0,
                theme.text_tertiary,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(sp(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_tertiary)
                    .truncate()
                    .child(name),
            )
            .when(disconnected, |row| {
                row.child(
                    div()
                        .text_size(sp(11.0))
                        .text_color(theme.warning)
                        .child(tr!("connections.reconnecting")),
                )
            })
            .child(add)
            .when_some(options, |row, options| row.child(options))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_hosts.remove(&click_key) {
                    this.collapsed_hosts.insert(click_key.clone());
                }
                cx.notify();
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "enter" | "space" => {
                        if !this.collapsed_hosts.remove(&key) {
                            this.collapsed_hosts.insert(key.clone());
                        }
                    }
                    "left" => {
                        this.collapsed_hosts.insert(key.clone());
                    }
                    "right" => {
                        this.collapsed_hosts.remove(&key);
                    }
                    _ => return,
                }
                cx.stop_propagation();
                cx.notify();
            }));
        if index == 0 {
            return row.into_any_element();
        }
        // Later hosts sit below a hairline so each machine reads as its own
        // section rather than another workspace.
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(SIDEBAR_HOST_DIVIDER_GAP))
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .child(div().w_full().h(px(1.0)).bg(theme.border)),
            )
            .child(row)
            .into_any_element()
    }
    pub(super) fn reconnect_saved_hosts(&mut self, cx: &mut Context<Self>) {
        let mut connections = self.connections.file.connections.clone();
        if let Some(connection) = crate::daemon::startup_connection()
            && !connections.iter().any(|saved| saved.id == connection.id)
        {
            self.connections.file.upsert(connection.clone());
            connections.push(connection);
        }
        for connection in connections
            .into_iter()
            .filter(|connection| !connection.is_local())
        {
            self.open_connection(connection, cx);
        }
        if self.connections.pending.is_empty() && self.pending_host_session.take().is_some() {
            self.state.selected_session = None;
            self.state.selected_project = None;
            if let Some(id) = self.state.sessions.first().map(|session| session.id) {
                self.select_session(id, cx);
            }
        }
    }

    pub(super) fn clear_host_workspace_caches(&mut self) {
        self.branch_snapshots.clear();
        self.clear_git_graph_cache();
        self.slash_commands.clear();
        self.mention_files.clear();
        self.working_trees.clear();
        self.slash_command_index = Rc::new(Vec::new());
        self.slash_command_index_key = None;
        self.mention_file_index = Rc::new(Vec::new());
        self.mention_file_index_path = None;
    }

    pub(super) fn disconnect_host(&mut self, key: &HostKey, cx: &mut Context<Self>) {
        if *key == HostKey::Local || self.hosts.get(key).is_none() {
            return;
        }
        self.capture_and_save_current_composer_draft(cx);
        self.save();
        self.composer_draft_save_generation = self.composer_draft_save_generation.saturating_add(1);
        let generation = self.composer_draft_save_generation;
        let saves = self.hosts.draft_saves(&self.composer_drafts);
        cx.background_executor()
            .spawn(async move {
                for (store, drafts) in saves {
                    if let Err(error) = store.save(drafts, generation) {
                        eprintln!("could not save host drafts before disconnect: {error}");
                    }
                }
            })
            .detach();
        // Detach from the host without sending an empty replacement snapshot
        // or stopping its running agents. Reconnecting loads its catalog again.
        self.host_sync_stops.remove(key);
        let sessions = self
            .state
            .sessions
            .iter()
            .filter(|session| self.hosts.ownership.session(session.id) == *key)
            .map(|session| session.id)
            .collect::<HashSet<_>>();
        let projects = self
            .state
            .projects
            .iter()
            .filter(|project| self.hosts.ownership.project(project.id) == *key)
            .map(|project| project.id)
            .collect::<HashSet<_>>();
        for id in &sessions {
            self.runtimes.remove(id);
            self.clear_user_input_fields(*id);
            self.runtime_attach_pending.remove(id);
            self.session_hydrations.remove(id);
            self.response_fork_preparations.remove(id);
            self.submission_preparations.remove(id);
            self.pending_goal_operations.remove(id);
            self.goal_runtime_starts.remove(id);
            self.runtime_attach_misses.remove(id);
            // Panel cleanup reads the session's background-work keys, so it
            // runs before that registry is dropped.
            self.remove_right_panel_session_state(*id);
            self.remove_bottom_panel_session_state(*id);
            self.background_work.remove(id);
            self.task_switcher.remove(*id);
        }
        self.state
            .sessions
            .retain(|session| !sessions.contains(&session.id));
        self.state
            .projects
            .retain(|project| !projects.contains(&project.id));
        self.composer_drafts
            .sessions
            .retain(|id, _| !sessions.contains(id));
        self.composer_drafts
            .new_sessions
            .retain(|id, _| !projects.contains(id));
        self.hosts
            .ownership
            .sessions
            .retain(|_, owner| owner != key);
        self.hosts
            .ownership
            .projects
            .retain(|_, owner| owner != key);
        self.hosts.hosts.retain(|host| &host.key != key);
        self.host_disconnected.remove(key);
        self.host_catalogs.remove(key);
        self.host_provider_overrides.remove(key);
        self.host_disabled_providers.remove(key);
        self.plan_usage_pending.retain(|(owner, _)| owner != key);
        self.plan_usage_checked_at
            .retain(|(owner, _), _| owner != key);
        self.plan_usage.retain(|(owner, _), _| owner != key);
        self.plan_usage_error.retain(|(owner, _), _| owner != key);
        self.plan_usage_unconfigured
            .retain(|(owner, _)| owner != key);
        self.plan_usage_stale.retain(|(owner, _)| owner != key);
        self.host_focuses.borrow_mut().remove(key);
        self.remote_images
            .borrow_mut()
            .retain(|(owner, _, _), _| owner != key);
        self.pending_queue_drains
            .retain(|id| !sessions.contains(id));
        self.collapsed_hosts.remove(key);
        if self
            .state
            .selected_session
            .is_some_and(|id| sessions.contains(&id))
        {
            self.clear_host_workspace_caches();
            self.state.selected_session = None;
            self.state.selected_project = None;
            self.restore_selected_composer_draft(cx);
            self.pending_session_activation = None;
            if let Some(id) = self.state.sessions.first().map(|session| session.id) {
                self.select_session(id, cx);
            } else {
                self.reset_visible_state();
                self.reset_transcript_rows(self.transcript_row_count());
            }
        }
        cx.notify();
    }
    pub(super) fn selected_host_key(&self) -> HostKey {
        self.state
            .selected_session
            .map(|id| self.hosts.ownership.session(id))
            .or_else(|| {
                self.state
                    .selected_project
                    .map(|id| self.hosts.ownership.project(id))
            })
            .unwrap_or(HostKey::Local)
    }

    pub(super) fn remote_connection_name(&self) -> Option<SharedString> {
        let key = self.selected_host_key();
        (key != HostKey::Local)
            .then(|| self.hosts.get(&key).map(|host| host.name.clone().into()))
            .flatten()
    }

    pub(super) fn selected_host_disconnected(&self) -> bool {
        self.host_disconnected.contains(&self.selected_host_key())
    }

    pub(super) fn selected_daemon(&self) -> &waku_client::DaemonSupervisor {
        &self
            .hosts
            .get(&self.selected_host_key())
            .expect("selected host is registered")
            .daemon
    }

    pub(super) fn daemon_for_session(&self, id: Uuid) -> &waku_client::DaemonSupervisor {
        &self.hosts.for_session(id).daemon
    }

    pub(super) fn daemon_for_project(&self, id: Uuid) -> &waku_client::DaemonSupervisor {
        &self.hosts.for_project(id).daemon
    }

    pub(super) fn selected_store(&self) -> &StateStore {
        &self
            .hosts
            .get(&self.selected_host_key())
            .expect("selected host is registered")
            .store
    }
}
