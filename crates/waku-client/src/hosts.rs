//! Desktop host ownership. A daemon snapshot is a replacement, so every save
//! must contain exactly one host's catalog, including when that catalog is empty.

use std::collections::{HashMap, HashSet};
use std::io;

use uuid::Uuid;

use crate::DaemonSupervisor;
use crate::model::{AgentSession, Project};
use crate::persistence::{ComposerDraftStore, ComposerDrafts, PersistedState, StateStore};

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum HostKey {
    Local,
    Connection(String),
}

pub struct Host {
    pub generation: u64,
    pub key: HostKey,
    pub name: String,
    pub daemon: DaemonSupervisor,
    pub store: StateStore,
    pub drafts: ComposerDraftStore,
}

impl Host {
    pub fn new(key: HostKey, name: String, daemon: DaemonSupervisor) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            generation: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            key,
            name,
            store: StateStore::remote(daemon.clone()),
            drafts: ComposerDraftStore::remote(daemon.clone()),
            daemon,
        }
    }
}

#[derive(Default)]
pub struct HostOwnership {
    pub projects: HashMap<Uuid, HostKey>,
    pub sessions: HashMap<Uuid, HostKey>,
}

impl HostOwnership {
    pub fn register(&mut self, key: &HostKey, projects: &[Project], sessions: &[AgentSession]) {
        for project in projects {
            self.projects.insert(project.id, key.clone());
        }
        for session in sessions {
            self.sessions.insert(session.id, key.clone());
        }
    }

    pub fn session(&self, id: Uuid) -> HostKey {
        self.sessions.get(&id).cloned().unwrap_or(HostKey::Local)
    }

    pub fn project(&self, id: Uuid) -> HostKey {
        self.projects.get(&id).cloned().unwrap_or(HostKey::Local)
    }

    /// New sessions inherit the owning project; explicit session ownership also
    /// covers projectless sessions and survives removal from the live catalog.
    pub fn register_new_sessions(&mut self, state: &PersistedState) {
        for project in &state.projects {
            self.projects.entry(project.id).or_insert(HostKey::Local);
        }
        for session in &state.sessions {
            let key = self.project(session.project_id);
            self.sessions.entry(session.id).or_insert(key);
        }
    }

    pub fn partition(&self, key: &HostKey) -> (HashSet<Uuid>, HashSet<Uuid>) {
        (
            self.projects
                .iter()
                .filter(|(_, owner)| *owner == key)
                .map(|(id, _)| *id)
                .collect(),
            self.sessions
                .iter()
                .filter(|(_, owner)| *owner == key)
                .map(|(id, _)| *id)
                .collect(),
        )
    }
}

pub struct HostRegistry {
    pub hosts: Vec<Host>,
    pub ownership: HostOwnership,
}

impl HostRegistry {
    pub fn local(
        daemon: DaemonSupervisor,
        store: StateStore,
        drafts: ComposerDraftStore,
        state: &PersistedState,
    ) -> Self {
        let mut ownership = HostOwnership::default();
        ownership.register(&HostKey::Local, &state.projects, &state.sessions);
        Self {
            hosts: vec![Host {
                generation: 0,
                key: HostKey::Local,
                name: "Local".into(),
                daemon,
                store,
                drafts,
            }],
            ownership,
        }
    }

    pub fn get(&self, key: &HostKey) -> Option<&Host> {
        self.hosts.iter().find(|host| &host.key == key)
    }

    /// Find another currently registered host attached to the same live daemon.
    pub fn find_same_daemon(
        &self,
        exclude: &HostKey,
        identity: &crate::DaemonIdentity,
    ) -> Option<HostKey> {
        self.hosts.iter().find_map(|host| {
            (&host.key != exclude
                && host
                    .daemon
                    .daemon_identity()
                    .is_some_and(|existing| existing.matches(identity)))
            .then(|| host.key.clone())
        })
    }

    /// Find another currently registered host that either shares the same live
    /// daemon identity or already owns any project/session UUID from this catalog
    /// (covering daemon restarts after idle-exit or SSH-to-localhost sharing `app.db`).
    pub fn find_duplicate_host(
        &self,
        exclude: &HostKey,
        identity: Option<&crate::DaemonIdentity>,
        projects: &[Project],
        sessions: &[AgentSession],
    ) -> Option<HostKey> {
        if let Some(identity) = identity
            && let Some(existing) = self.find_same_daemon(exclude, identity)
        {
            return Some(existing);
        }
        for project in projects {
            if let Some(owner) = self.ownership.projects.get(&project.id)
                && owner != exclude
                && self.get(owner).is_some()
            {
                return Some(owner.clone());
            }
        }
        for session in sessions {
            if let Some(owner) = self.ownership.sessions.get(&session.id)
                && owner != exclude
                && self.get(owner).is_some()
            {
                return Some(owner.clone());
            }
        }
        None
    }

    pub fn for_session(&self, id: Uuid) -> &Host {
        self.get(&self.ownership.session(id))
            .expect("session host must be registered")
    }

    pub fn for_project(&self, id: Uuid) -> &Host {
        self.get(&self.ownership.project(id))
            .expect("project host must be registered")
    }

    pub fn save(&mut self, state: &mut PersistedState) -> io::Result<()> {
        self.ownership.register_new_sessions(state);
        let mut first_error = None;
        for host in &self.hosts {
            let (projects, sessions) = self.ownership.partition(&host.key);
            if let Err(error) =
                host.store
                    .save_partition(state, &projects, &sessions, host.key == HostKey::Local)
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn session_message_search(
        &self,
        query: String,
        limit: usize,
    ) -> impl FnOnce() -> io::Result<Vec<crate::persistence::SessionMessageMatch>> + Send + 'static
    {
        let searches = self
            .hosts
            .iter()
            .filter(|host| host.store.task_state_loaded())
            .map(|host| host.store.session_message_search(query.clone(), limit))
            .collect::<Vec<_>>();
        move || {
            let mut matches = Vec::new();
            let mut succeeded = false;
            let mut first_error = None;
            std::thread::scope(|scope| {
                let tasks = searches
                    .into_iter()
                    .map(|search| scope.spawn(search))
                    .collect::<Vec<_>>();
                for task in tasks {
                    match task.join() {
                        Ok(Ok(found)) => {
                            succeeded = true;
                            matches.extend(found);
                        }
                        Ok(Err(error)) => {
                            first_error.get_or_insert(error);
                        }
                        Err(_) => {
                            first_error
                                .get_or_insert_with(|| io::Error::other("host search failed"));
                        }
                    }
                }
            });
            if !succeeded && let Some(error) = first_error {
                return Err(error);
            }
            Ok(matches)
        }
    }

    pub fn draft_saves(
        &self,
        drafts: &ComposerDrafts,
    ) -> Vec<(ComposerDraftStore, ComposerDrafts)> {
        self.hosts
            .iter()
            .filter(|host| host.store.task_state_loaded())
            .map(|host| {
                let partition = ComposerDrafts {
                    new_sessions: drafts
                        .new_sessions
                        .iter()
                        .filter(|(id, _)| self.ownership.project(**id) == host.key)
                        .map(|(id, draft)| (*id, draft.clone()))
                        .collect(),
                    sessions: drafts
                        .sessions
                        .iter()
                        .filter(|(id, _)| self.ownership.session(**id) == host.key)
                        .map(|(id, draft)| (*id, draft.clone()))
                        .collect(),
                };
                (host.drafts.clone(), partition)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderKind;

    #[test]
    fn split_snapshot_keeps_each_hosts_catalog_and_projectless_sessions() {
        let local = Project::from_path("/local".into());
        let remote = Project::from_path("/remote".into());
        let local_session = AgentSession::new(local.id, ProviderKind::Codex);
        let remote_session = AgentSession::new(remote.id, ProviderKind::Codex);
        let projectless = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let key = HostKey::Connection("ssh:build".into());
        let mut owners = HostOwnership::default();
        owners.register(&HostKey::Local, &[local.clone()], &[local_session.clone()]);
        owners.register(
            &key,
            &[remote.clone()],
            &[remote_session.clone(), projectless.clone()],
        );
        assert_eq!(
            owners.partition(&HostKey::Local),
            (HashSet::from([local.id]), HashSet::from([local_session.id]))
        );
        assert_eq!(
            owners.partition(&key),
            (
                HashSet::from([remote.id]),
                HashSet::from([remote_session.id, projectless.id])
            )
        );
    }

    #[test]
    fn new_sessions_inherit_host_even_when_hosts_use_the_same_path() {
        let local = Project::from_path("/code".into());
        let remote = Project::from_path("/code".into());
        let key = HostKey::Connection("wsl:Ubuntu".into());
        let mut owners = HostOwnership::default();
        owners.register(&HostKey::Local, &[local.clone()], &[]);
        owners.register(&key, &[remote.clone()], &[]);
        let mut state = PersistedState::empty();
        let session = AgentSession::new(remote.id, ProviderKind::Codex);
        state.sessions.push(session.clone());
        owners.register_new_sessions(&state);
        assert_eq!(owners.session(session.id), key);
        assert_eq!(owners.project(local.id), HostKey::Local);
    }

    fn spawn_mock_daemon_server(
        connections: usize,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            let mut workers = Vec::new();
            for stream in listener.incoming().take(connections) {
                let Ok(stream) = stream else { break };
                workers.push(std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                    let mut ws = tungstenite::accept(stream).unwrap();
                    let _ = ws.read().unwrap();
                    ws.send(tungstenite::Message::Text(
                        serde_json::to_string(&waku_protocol::ServerMessage::Hello {
                            protocol_version: waku_protocol::PROTOCOL_VERSION,
                            daemon_version: "test".into(),
                        })
                        .unwrap()
                        .into(),
                    ))
                    .unwrap();
                    while let Ok(msg) = ws.read() {
                        match msg {
                            tungstenite::Message::Text(text) => {
                                if let Ok(waku_protocol::ClientMessage::Request(request)) =
                                    serde_json::from_str(text.as_ref())
                                {
                                    ws.send(tungstenite::Message::Text(
                                        serde_json::to_string(
                                            &waku_protocol::ServerMessage::Response {
                                                request_id: request.request_id,
                                                outcome: waku_protocol::ResponseOutcome::Ok {
                                                    payload:
                                                        waku_protocol::ResponsePayload::Settings {
                                                            settings: Default::default(),
                                                        },
                                                },
                                            },
                                        )
                                        .unwrap()
                                        .into(),
                                    ))
                                    .unwrap();
                                }
                            }
                            tungstenite::Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        (addr, handle)
    }

    #[test]
    fn host_registry_detects_same_daemon_and_catalog_uuid_duplicates() {
        let (addr, server) = spawn_mock_daemon_server(4);
        let sup_local = DaemonSupervisor::connect(&addr, "local-token".into()).unwrap();
        let sup_a = DaemonSupervisor::connect(&addr, "shared-token".into()).unwrap();
        let sup_b = DaemonSupervisor::connect(&addr, "shared-token".into()).unwrap();
        let sup_c = DaemonSupervisor::connect(&addr, "other-token".into()).unwrap();

        let key_a = HostKey::Connection("ssh:lan".into());
        let key_b = HostKey::Connection("ssh:tailscale".into());
        let key_c = HostKey::Connection("ssh:other".into());

        let mut registry = HostRegistry::local(
            sup_local.clone(),
            StateStore::remote(sup_local.clone()),
            ComposerDraftStore::remote(sup_local.clone()),
            &PersistedState::empty(),
        );
        registry.hosts.push(Host::new(key_a.clone(), "LAN".into(), sup_a.clone()));
        registry.hosts.push(Host::new(key_c.clone(), "Other".into(), sup_c.clone()));

        // Same daemon identity on empty host matches existing host and excludes self.
        let id_b = sup_b.daemon_identity().unwrap();
        assert_eq!(registry.find_same_daemon(&key_b, &id_b), Some(key_a.clone()));
        assert_eq!(registry.find_same_daemon(&key_a, &id_b), None);
        assert_eq!(
            registry.find_duplicate_host(&key_b, Some(&id_b), &[], &[]),
            Some(key_a.clone())
        );

        // Distinct hosts with identical directory paths but fresh UUIDs do not collide.
        let proj_a = Project::from_path("/workspace/repo".into());
        let sess_a = AgentSession::new(proj_a.id, ProviderKind::Codex);
        registry
            .ownership
            .register(&key_a, std::slice::from_ref(&proj_a), std::slice::from_ref(&sess_a));

        let proj_other = Project::from_path("/workspace/repo".into());
        let sess_other = AgentSession::new(proj_other.id, ProviderKind::Codex);
        let id_c = sup_c.daemon_identity().unwrap();
        assert_eq!(
            registry.find_duplicate_host(
                &key_b,
                Some(&id_c),
                std::slice::from_ref(&proj_other),
                std::slice::from_ref(&sess_other),
            ),
            Some(key_c.clone())
        );

        // Catalog UUID overlap catches daemon restart (new token) when project or session matches.
        let fresh_identity = crate::DaemonIdentity {
            token: "restarted-token".into(),
            pid: Some(999),
        };
        assert_eq!(
            registry.find_duplicate_host(
                &key_b,
                Some(&fresh_identity),
                std::slice::from_ref(&proj_a),
                &[],
            ),
            Some(key_a.clone())
        );
        assert_eq!(
            registry.find_duplicate_host(
                &key_b,
                Some(&fresh_identity),
                &[],
                std::slice::from_ref(&sess_a),
            ),
            Some(key_a.clone())
        );
        assert_eq!(
            registry.find_duplicate_host(
                &key_b,
                Some(&fresh_identity),
                std::slice::from_ref(&proj_other),
                std::slice::from_ref(&sess_other),
            ),
            None
        );

        // Ownership entries whose host is no longer registered are ignored.
        let unregistered = HostKey::Connection("ssh:disconnected".into());
        let proj_stale = Project::from_path("/stale".into());
        registry
            .ownership
            .register(&unregistered, std::slice::from_ref(&proj_stale), &[]);
        assert_eq!(
            registry.find_duplicate_host(
                &key_b,
                Some(&fresh_identity),
                std::slice::from_ref(&proj_stale),
                &[],
            ),
            None
        );

        sup_local.client().disconnect();
        sup_a.client().disconnect();
        sup_b.client().disconnect();
        sup_c.client().disconnect();
        drop(registry);
        drop(sup_local);
        drop(sup_a);
        drop(sup_b);
        drop(sup_c);
        server.join().unwrap();
    }
}
