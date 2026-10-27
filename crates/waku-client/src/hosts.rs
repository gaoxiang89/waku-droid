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
}
