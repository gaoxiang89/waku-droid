//! Persist terminal identities and split layouts, never terminal processes.
use super::terminal_split::{SplitAxis, SplitMember};
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
enum SavedPane {
    Pane {
        id: Uuid,
        cwd: PathBuf,
    },
    Split {
        horizontal: bool,
        children: Vec<SavedPane>,
        flexes: Vec<f32>,
    },
}
#[derive(Clone, Serialize, Deserialize)]
struct SavedGroup {
    root: SavedPane,
    active: Option<Uuid>,
}
#[derive(Clone, Serialize, Deserialize)]
struct SavedWorkspace {
    scope: PanelScope,
    bottom: Vec<SavedGroup>,
    bottom_active: Option<usize>,
    bottom_visible: bool,
    right: Vec<(Uuid, SavedGroup)>,
    right_active: Option<Uuid>,
    right_visible: bool,
}
#[derive(Default)]
pub(super) struct TerminalLayouts {
    saved: Vec<SavedWorkspace>,
    restored: HashSet<PanelScope>,
    last_bytes: Vec<u8>,
    writer: Option<crossbeam_channel::Sender<(Vec<u8>, Option<crossbeam_channel::Sender<()>>)>>,
    pub(super) loaded: bool,
}

fn layout_path() -> PathBuf {
    StateStore::default_path().with_file_name("terminal-layouts.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_keeps_hosts_ids_and_split_weights() {
        let id = Uuid::new_v4();
        let pane = SavedPane::Pane {
            id,
            cwd: PathBuf::from("/work/project"),
        };
        let split = SavedPane::Split {
            horizontal: true,
            children: vec![pane.clone(), pane],
            flexes: vec![1.0, 2.0],
        };
        assert!(split.valid(0));
        let decoded: SavedPane =
            serde_json::from_slice(&serde_json::to_vec(&split).unwrap()).unwrap();
        let SavedPane::Split {
            children, flexes, ..
        } = decoded
        else {
            panic!("split lost")
        };
        assert_eq!(flexes, vec![1.0, 2.0]);
        assert!(matches!(&children[0], SavedPane::Pane { id: saved, .. } if *saved == id));
        let local = PanelScope {
            host: HostKey::Local,
            workspace: PathBuf::from("/work/project"),
        };
        let remote = PanelScope {
            host: HostKey::Connection("ssh-host".into()),
            workspace: local.workspace.clone(),
        };
        assert_ne!(local, remote);
        assert_eq!(
            serde_json::from_slice::<PanelScope>(&serde_json::to_vec(&remote).unwrap()).unwrap(),
            remote
        );
    }
    #[test]
    fn malformed_split_is_not_restored() {
        assert!(
            !SavedPane::Split {
                horizontal: true,
                children: vec![],
                flexes: vec![]
            }
            .valid(0)
        );
        assert!(
            !SavedPane::Split {
                horizontal: true,
                children: vec![SavedPane::Pane {
                    id: Uuid::nil(),
                    cwd: PathBuf::new()
                }],
                flexes: vec![0.0]
            }
            .valid(0)
        );
    }
}

impl SavedPane {
    fn capture(member: &SplitMember, cx: &App) -> Self {
        match member {
            SplitMember::Pane(pane) => {
                let pane = pane.read(cx);
                Self::Pane {
                    id: pane.persistent_id,
                    cwd: pane.working_directory().to_path_buf(),
                }
            }
            SplitMember::Axis(axis) => Self::Split {
                horizontal: axis.axis == gpui::Axis::Horizontal,
                children: axis.members.iter().map(|m| Self::capture(m, cx)).collect(),
                flexes: axis.flexes.clone(),
            },
        }
    }
    fn restore(&self, app: &Waku, bottom: bool, cx: &mut Context<Waku>) -> SplitMember {
        match self {
            Self::Pane { id, cwd } => SplitMember::Pane(if bottom {
                app.bottom_terminal_with_identity(cwd, Some(*id), cx)
            } else {
                app.right_terminal_with_identity(cwd, Some(*id), cx)
            }),
            Self::Split {
                horizontal,
                children,
                flexes,
            } => SplitMember::Axis(SplitAxis {
                axis: if *horizontal {
                    gpui::Axis::Horizontal
                } else {
                    gpui::Axis::Vertical
                },
                members: children
                    .iter()
                    .map(|p| p.restore(app, bottom, cx))
                    .collect(),
                flexes: flexes.clone(),
            }),
        }
    }
    fn valid(&self, depth: usize) -> bool {
        if depth > 4 {
            return false;
        }
        match self {
            Self::Pane { .. } => true,
            Self::Split {
                children, flexes, ..
            } => {
                !children.is_empty()
                    && children.len() <= 4
                    && children.len() == flexes.len()
                    && flexes.iter().all(|f| f.is_finite() && *f > 0.0)
                    && children.iter().all(|p| p.valid(depth + 1))
            }
        }
    }
}
impl SavedGroup {
    fn capture(group: &SplitGroup, cx: &App) -> Self {
        Self {
            root: SavedPane::capture(&group.root, cx),
            active: group.active_or_first().map(|p| p.read(cx).persistent_id),
        }
    }
    fn restore(&self, app: &Waku, bottom: bool, cx: &mut Context<Waku>) -> SplitGroup {
        let mut group = SplitGroup {
            root: self.root.restore(app, bottom, cx),
            active_pane: None,
        };
        group.active_pane = group
            .panes()
            .iter()
            .find(|p| Some(p.read(cx).persistent_id) == self.active)
            .map(|p| p.entity_id());
        group
    }
}

impl Waku {
    pub(super) fn detach_host_terminals(&mut self, host: &HostKey, cx: &App) {
        self.save_terminal_layouts(cx, None);
        self.terminal_layouts
            .restored
            .retain(|scope| &scope.host != host);
    }
    pub(super) fn restore_detached_terminals(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.selected_panel_scope() else {
            return;
        };
        let daemon = self.selected_daemon().clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    daemon.client().request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::ListTerminals,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.selected_panel_scope().as_ref() != Some(&scope) {
                    return;
                }
                match result {
                    Ok(waku_client::ResponsePayload::Terminals { terminals }) => {
                        let mut known = this
                            .bottom_panel_terminal_groups
                            .iter()
                            .chain(this.right_panel_terminal_groups.values())
                            .flat_map(SplitGroup::panes)
                            .map(|p| p.read(cx).persistent_id)
                            .collect::<HashSet<_>>();
                        for terminal in terminals {
                            if terminal.cwd != scope.workspace
                                || terminal.exited
                                || !known.insert(terminal.session_id)
                            {
                                continue;
                            }
                            let pane = this.bottom_terminal_with_identity(
                                &terminal.cwd,
                                Some(terminal.session_id),
                                cx,
                            );
                            this.bottom_panel_terminal_groups
                                .push(SplitGroup::single(pane));
                        }
                        if !this.bottom_panel_terminal_groups.is_empty() {
                            this.bottom_panel_active_group.get_or_insert(0);
                            this.bottom_panel_visible = true;
                        }
                    }
                    Err(error) => this.show_toast(error.to_string()),
                    _ => {}
                }
                this.save_terminal_layouts(cx, None);
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn start_terminal_persistence(&mut self, cx: &mut Context<Self>) {
        let (tx, rx) =
            crossbeam_channel::unbounded::<(Vec<u8>, Option<crossbeam_channel::Sender<()>>)>();
        self.terminal_layouts.writer = Some(tx);
        std::thread::spawn(move || {
            let path = layout_path();
            while let Ok((bytes, done)) = rx.recv() {
                let result = (|| -> std::io::Result<()> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let temp = path.with_extension("json.tmp");
                    std::fs::write(&temp, bytes)?;
                    std::fs::rename(temp, &path)
                })();
                if let Err(error) = result {
                    eprintln!("could not save terminal layouts: {error}");
                }
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_executor()
                .spawn(async {
                    let bytes = std::fs::read(layout_path()).unwrap_or_default();
                    if bytes.len() > 2 * 1024 * 1024 {
                        return Vec::new();
                    }
                    serde_json::from_slice::<Vec<SavedWorkspace>>(&bytes)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|s| {
                            s.bottom.iter().all(|g| g.root.valid(0))
                                && s.right.iter().all(|(_, g)| g.root.valid(0))
                        })
                        .collect()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.terminal_layouts.saved = saved;
                this.terminal_layouts.loaded = true;
                this.restore_persistent_terminals(cx);
                cx.notify();
            });
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update(cx, |this, cx| this.save_terminal_layouts(cx, None))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn restore_persistent_terminals(&mut self, cx: &mut Context<Self>) {
        if !self.terminal_layouts.loaded {
            return;
        }
        let Some(scope) = self.selected_panel_scope() else {
            return;
        };
        if !self.terminal_layouts.restored.insert(scope.clone()) {
            return;
        }
        let Some(saved) = self
            .terminal_layouts
            .saved
            .iter()
            .find(|s| s.scope == scope)
            .cloned()
        else {
            return;
        };
        if self.bottom_panel_terminal_groups.is_empty() {
            self.bottom_panel_terminal_groups = saved
                .bottom
                .iter()
                .map(|g| g.restore(self, true, cx))
                .collect();
            self.bottom_panel_active_group = saved
                .bottom_active
                .filter(|i| *i < self.bottom_panel_terminal_groups.len());
            self.bottom_panel_visible = saved.bottom_visible;
        }
        for (id, group) in &saved.right {
            if !self.right_panel_terminal_groups.contains_key(id) {
                let group = group.restore(self, false, cx);
                self.right_panel_terminal_groups.insert(*id, group);
                self.right_panel_surfaces
                    .push(RightPanelSurface::Terminal(*id));
            }
        }
        if let Some(id) = saved.right_active {
            self.right_panel_active_surface = self
                .right_panel_surfaces
                .iter()
                .position(|s| s.terminal_id() == Some(id));
        }
        if !saved.right.is_empty() {
            self.right_panel_visible = saved.right_visible;
        }
    }

    pub(super) fn save_terminal_layouts(
        &mut self,
        cx: &App,
        done: Option<crossbeam_channel::Sender<()>>,
    ) {
        if !self.terminal_layouts.loaded {
            if let Some(done) = done {
                let _ = done.send(());
            }
            return;
        }
        let active = self.selected_panel_scope();
        let scopes: HashSet<_> = self
            .bottom_panel_workspace_states
            .keys()
            .chain(self.right_panel_workspace_states.keys())
            .cloned()
            .chain(active.clone())
            .collect();
        for scope in scopes {
            // A host may still be reconnecting, or navigation may have
            // happened before the asynchronous load completed. Empty live
            // placeholders must not overwrite an unrestored disk layout.
            if !self.terminal_layouts.restored.contains(&scope) {
                continue;
            }
            let is_active = active.as_ref() == Some(&scope);
            let bottom = self.bottom_panel_workspace_states.get(&scope);
            let right = self.right_panel_workspace_states.get(&scope);
            let groups = if is_active {
                &self.bottom_panel_terminal_groups
            } else {
                bottom
                    .map(|s| &s.groups)
                    .unwrap_or(&self.bottom_panel_terminal_groups)
            };
            let surfaces = if is_active {
                self.right_panel_surfaces.as_slice()
            } else {
                right.map(|s| s.surfaces.as_slice()).unwrap_or(&[])
            };
            let right_index = if is_active {
                self.right_panel_active_surface
            } else {
                right.and_then(|s| s.active_surface)
            };
            let state = SavedWorkspace {
                scope: scope.clone(),
                bottom: if is_active || bottom.is_some() {
                    groups.iter().map(|g| SavedGroup::capture(g, cx)).collect()
                } else {
                    Vec::new()
                },
                bottom_active: if is_active {
                    self.bottom_panel_active_group
                } else {
                    bottom.and_then(|s| s.active_group)
                },
                bottom_visible: if is_active {
                    self.bottom_panel_visible
                } else {
                    bottom.is_some_and(|s| s.visible)
                },
                right: surfaces
                    .iter()
                    .filter_map(|s| s.terminal_id())
                    .filter_map(|id| {
                        self.right_panel_terminal_groups
                            .get(&id)
                            .map(|g| (id, SavedGroup::capture(g, cx)))
                    })
                    .collect(),
                right_active: right_index
                    .and_then(|i| surfaces.get(i))
                    .and_then(RightPanelSurface::terminal_id),
                right_visible: if is_active {
                    self.right_panel_visible
                } else {
                    right.is_some_and(|s| s.visible)
                },
            };
            if let Some(old) = self
                .terminal_layouts
                .saved
                .iter_mut()
                .find(|s| s.scope == scope)
            {
                *old = state;
            } else {
                self.terminal_layouts.saved.push(state);
            }
        }
        let bytes = serde_json::to_vec(&self.terminal_layouts.saved).unwrap_or_default();
        if bytes != self.terminal_layouts.last_bytes || done.is_some() {
            self.terminal_layouts.last_bytes = bytes.clone();
            if let Some(writer) = &self.terminal_layouts.writer {
                let _ = writer.send((bytes, done));
            }
        }
    }
}
