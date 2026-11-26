//! Provider backend and driver-event wire translation for `waku-daemon`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{
    Backend, Command, EventSink, Request, ResponsePayload, WireDriverEvent, WorkspaceOperation,
    WorkspaceResult,
};
use anyhow::{Context as _, anyhow, bail};
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

use crate::attachments::AttachmentStore;
use crate::driver::{self, DriverHandle, DriverStartOptions, SessionOptions};
use crate::model::{
    ActivityItem, AgentSession, Checkpoint, CheckpointStatus, DriverEvent, Message,
    MessageAttachment, MessageRole, Project, ProviderKind, ProviderProbe, ProviderResumeCursor,
    ReasoningBlock, RuntimeEventCursor, SessionStatus, TranscriptBlock, TurnStatus, unix_time,
    unix_time_millis,
};
use crate::persistence::{ComposerDraftStore, PersistedState, StateStore};
use crate::settings::DaemonSettingsStore;
use waku_protocol::provider_session::{ProviderSessionFork, ProviderSessionForkRequest};
use waku_protocol::{decode_enum, event_to_wire};
pub use waku_protocol::{encode_enum, event_from_wire};

/// How many fully hydrated transcripts the daemon keeps resident.
///
/// Hydration is a cache: consumers reload a released session from the store on
/// demand. Without a cap, a daemon that lives for days adopts the transcript
/// of every session its clients have touched — SaveTaskState pushes, hydrate
/// requests, forks, checkpoints — and resident memory grows without bound.
const RESIDENT_TRANSCRIPT_WINDOW: usize = 24;

/// Releases resident transcripts beyond the recency window after a save.
/// `pinned` names sessions with live runtimes; dirty sessions are skipped
/// inside [`PersistedState::trim_idle_transcripts`] because they hold unsaved
/// work.
fn trim_resident_transcripts(state: &mut PersistedState, pinned: &HashSet<Uuid>) {
    state.trim_idle_transcripts(pinned, RESIDENT_TRANSCRIPT_WINDOW);
}

const MAX_RETIRED_QUEUED_IDS_PER_SESSION: usize = 256;
const OFFLINE_REDUCTION_EVENTS_FLUSH_THRESHOLD: usize = 1500;

#[derive(Clone, Debug)]
struct PendingSteerMeta {
    prompt: String,
    message_id: Option<Uuid>,
    display_content: Option<String>,
    attachments: Vec<MessageAttachment>,
}

struct LiveSessionRuntime {
    runtime_id: Uuid,
    provider: ProviderKind,
    account_id: Option<String>,
    driver: DriverHandle,
    busy: Arc<AtomicBool>,
    last_active_at: Arc<Mutex<Instant>>,
    pending_steers: Arc<Mutex<VecDeque<PendingSteerMeta>>>,
}

impl LiveSessionRuntime {
    fn touch(&self) {
        *self.last_active_at.lock() = Instant::now();
    }
}

struct ValidatedClientConfigDir {
    account_id: String,
    raw: String,
    resolved: PathBuf,
}

fn normalized_account_id(account_id: Option<&str>) -> Option<&str> {
    account_id.map(str::trim).filter(|id| !id.is_empty())
}

fn default_remote_cli_bin_dir() -> Option<PathBuf> {
    let server_subdir = if cfg!(debug_assertions) {
        ".cache/narel-dev/server"
    } else {
        ".cache/narel/server"
    };
    let dir = dirs::home_dir()?.join(server_subdir).join("bin");
    dir.is_dir().then_some(dir)
}

#[derive(Default)]
struct RetiredQueuedMessageIds {
    per_session: HashMap<Uuid, VecDeque<Uuid>>,
}

impl RetiredQueuedMessageIds {
    fn contains(&self, session_id: Uuid, message_id: Uuid) -> bool {
        self.per_session
            .get(&session_id)
            .is_some_and(|ids| ids.contains(&message_id))
    }

    fn insert(&mut self, session_id: Uuid, message_id: Uuid) -> bool {
        let ids = self.per_session.entry(session_id).or_default();
        if ids.contains(&message_id) {
            return false;
        }
        ids.push_back(message_id);
        while ids.len() > MAX_RETIRED_QUEUED_IDS_PER_SESSION {
            ids.pop_front();
        }
        true
    }
}

pub struct WakuBackend {
    sessions: Arc<Mutex<HashMap<Uuid, LiveSessionRuntime>>>,
    terminals: Mutex<HashMap<Uuid, (Uuid, PathBuf, crate::terminal::DaemonTerminal)>>,
    #[cfg(all(test, unix))]
    terminal_shell: Option<alacritty_terminal::tty::Shell>,
    settings: DaemonSettingsStore,
    task_store: Arc<StateStore>,
    task_state: Arc<Mutex<PersistedState>>,
    removed_session_ids: Mutex<HashSet<Uuid>>,
    removed_project_ids: Mutex<HashSet<Uuid>>,
    retired_queued_message_ids: Mutex<RetiredQueuedMessageIds>,
    composer_drafts: ComposerDraftStore,
    attachments: AttachmentStore,
    usage_scan_cache: Mutex<crate::usage_history::ScanCache>,
    checkpoint_capture_locks: Mutex<HashMap<(PathBuf, Uuid, usize), Arc<Mutex<()>>>>,
    usage_rates_dir: std::path::PathBuf,
    default_cwd: std::path::PathBuf,
    cli_bin_dir: Option<PathBuf>,
}

impl WakuBackend {
    fn remove_project(&self, project_id: Uuid) -> anyhow::Result<()> {
        let (session_ids, deleted_project_paths) = {
            let mut state = self.task_state.lock();
            anyhow::ensure!(
                !state
                    .sessions
                    .iter()
                    .any(|session| session.project_id == project_id && session.is_busy()),
                "Stop running tasks before deleting this workspace"
            );
            let session_ids = state
                .sessions
                .iter()
                .filter(|session| session.project_id == project_id)
                .map(|session| session.id)
                .collect::<Vec<_>>();
            let mut projects = self.removed_project_ids.lock();
            let mut sessions = self.removed_session_ids.lock();
            let mut changes = session_ids
                .iter()
                .map(|id| waku_protocol::persistence::ComposerDraftChange {
                    target: waku_protocol::persistence::ComposerDraftTarget::Session {
                        session_id: *id,
                    },
                    draft: None,
                })
                .collect::<Vec<_>>();
            changes.push(waku_protocol::persistence::ComposerDraftChange {
                target: waku_protocol::persistence::ComposerDraftTarget::NewSession { project_id },
                draft: None,
            });
            let previous_drafts = self.composer_drafts.load()?;
            let restore_drafts = changes
                .iter()
                .map(|change| waku_protocol::persistence::ComposerDraftChange {
                    target: change.target,
                    draft: previous_drafts.get(change.target.into()).cloned(),
                })
                .collect();
            self.composer_drafts.apply_changes(changes)?;
            // Move the removed entries out so a failed write can restore memory.
            let (deleted_projects, retained_projects) = std::mem::take(&mut state.projects)
                .into_iter()
                .partition(|project: &Project| project.id == project_id);
            state.projects = retained_projects;
            let (deleted_sessions, retained_sessions) = std::mem::take(&mut state.sessions)
                .into_iter()
                .partition(|session: &AgentSession| session.project_id == project_id);
            state.sessions = retained_sessions;
            if let Err(error) = self
                .task_store
                .save_with_deleted_workspace(&mut state, Some((project_id, &session_ids)))
            {
                state.projects.extend(deleted_projects);
                state.sessions.extend(deleted_sessions);
                self.composer_drafts
                    .apply_changes(restore_drafts)
                    .context("workspace deletion failed and its drafts could not be restored")?;
                return Err(error.into());
            }
            let mut deleted_paths = deleted_projects
                .iter()
                .map(|project| project.path.clone())
                .collect::<Vec<_>>();
            for session in &deleted_sessions {
                if let Some(path) = session.workspace.path() {
                    deleted_paths.push(path.to_owned());
                }
            }
            projects.insert(project_id);
            sessions.extend(session_ids.iter().copied());
            (session_ids, deleted_paths)
        };
        let mut runtimes = self.sessions.lock();
        let removed = session_ids
            .iter()
            .filter_map(|id| runtimes.remove(id))
            .collect::<Vec<_>>();
        drop(runtimes);
        drop(removed);

        if !deleted_project_paths.is_empty() {
            let canonical_roots = deleted_project_paths
                .iter()
                .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
                .collect::<Vec<_>>();
            let mut removed_terminals = Vec::new();
            let mut terminals = self.terminals.lock();
            let matching_ids = terminals
                .iter()
                .filter_map(|(&id, (_, cwd, _))| {
                    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.clone());
                    let matches = deleted_project_paths
                        .iter()
                        .any(|root| cwd == root || cwd.starts_with(root))
                        || canonical_roots
                            .iter()
                            .any(|root| canonical_cwd == *root || canonical_cwd.starts_with(root));
                    matches.then_some(id)
                })
                .collect::<Vec<_>>();
            for id in matching_ids {
                if let Some((_, _, terminal)) = terminals.remove(&id) {
                    removed_terminals.push(terminal);
                }
            }
            drop(terminals);
            drop(removed_terminals);
        }
        Ok(())
    }

    pub fn new(settings: DaemonSettingsStore, task_store: StateStore) -> anyhow::Result<Self> {
        let mut task_state = task_store
            .load()
            .context("could not load Waku task database")?;
        migrate_projectless_state(&task_store, &mut task_state)?;
        let (removed_project_ids, removed_session_ids) = task_store.deleted_workspace_items()?;
        let composer_drafts = ComposerDraftStore::for_state_path(task_store.path());
        let attachments = AttachmentStore::new(
            task_store
                .path()
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("attachments"),
        );
        let usage_rates_dir = task_store
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_owned();
        Ok(Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            terminals: Mutex::new(HashMap::new()),
            #[cfg(all(test, unix))]
            terminal_shell: None,
            settings,
            task_store: Arc::new(task_store),
            task_state: Arc::new(Mutex::new(task_state)),
            removed_session_ids: Mutex::new(removed_session_ids),
            removed_project_ids: Mutex::new(removed_project_ids),
            retired_queued_message_ids: Mutex::new(RetiredQueuedMessageIds::default()),
            composer_drafts,
            attachments,
            usage_scan_cache: Mutex::new(HashMap::new()),
            checkpoint_capture_locks: Mutex::new(HashMap::new()),
            usage_rates_dir,
            default_cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            cli_bin_dir: None,
        })
    }

    /// Optional directory containing the `waku` CLI shim, prepended to `PATH`
    /// in daemon-owned PTYs (for bridge-managed remote daemons).
    pub fn with_cli_bin_dir(mut self, cli_bin_dir: Option<PathBuf>) -> Self {
        self.cli_bin_dir = cli_bin_dir;
        self
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_terminal_shell(mut self, shell: alacritty_terminal::tty::Shell) -> Self {
        self.terminal_shell = Some(shell);
        self
    }

    #[cfg(test)]
    pub(crate) fn insert_test_session(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        provider: ProviderKind,
        account_id: Option<String>,
        driver: DriverHandle,
    ) {
        self.sessions.lock().insert(
            session_id,
            LiveSessionRuntime {
                runtime_id,
                provider,
                account_id,
                driver,
                busy: Arc::new(AtomicBool::new(false)),
                last_active_at: Arc::new(Mutex::new(Instant::now())),
                pending_steers: Arc::new(Mutex::new(VecDeque::new())),
            },
        );
    }

    fn terminal_scope_for_client(&self, client_kind: crate::ClientKind) -> (Option<PathBuf>, bool) {
        match client_kind {
            crate::ClientKind::Desktop => (None, false),
            crate::ClientKind::RemoteDesktop => (
                self.cli_bin_dir
                    .clone()
                    .or_else(default_remote_cli_bin_dir),
                true,
            ),
            _ => (self.cli_bin_dir.clone(), false),
        }
    }

    fn open_terminal(
        &self,
        cwd: &Path,
        cols: u16,
        rows: u16,
        events: EventSink,
    ) -> anyhow::Result<crate::terminal::DaemonTerminal> {
        let (cli_bin_dir, remote_scope) = self.terminal_scope_for_client(events.client_kind());
        #[cfg(all(test, unix))]
        if let Some(shell) = &self.terminal_shell {
            return crate::terminal::DaemonTerminal::open_with_shell_and_scope(
                cwd,
                cols,
                rows,
                events,
                shell.clone(),
                cli_bin_dir.as_deref(),
                remote_scope,
            );
        }
        ensure_shell_environment();
        crate::terminal::DaemonTerminal::open_with_scope(
            cwd,
            cols,
            rows,
            events,
            cli_bin_dir.as_deref(),
            remote_scope,
        )
    }

    /// Capture and persist one ending checkpoint exactly once per daemon.
    /// Desktop and Web may observe the same turn completion concurrently; a
    /// per-turn lock prevents both clients from running the expensive Git
    /// snapshot while leaving unrelated tasks independent.
    fn capture_turn_checkpoint(
        &self,
        cwd: PathBuf,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<Checkpoint> {
        let key = (cwd.clone(), session_id, turn_count);
        let capture_lock = self
            .checkpoint_capture_locks
            .lock()
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _capture = capture_lock.lock();

        {
            let mut state = self.task_state.lock();
            if let Some(index) = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
            {
                self.task_store.hydrate(&mut state.sessions[index])?;
                if let Some(checkpoint) = state.sessions[index]
                    .turns
                    .iter()
                    .find(|turn| turn.turn_count == turn_count)
                    .and_then(|turn| turn.checkpoint.as_ref())
                    .filter(|checkpoint| {
                        matches!(
                            checkpoint.status,
                            CheckpointStatus::Ready | CheckpointStatus::Unavailable
                        )
                    })
                {
                    return Ok(checkpoint.clone());
                }
            }
        }

        let checkpoint = crate::checkpoint::capture_turn(&cwd, session_id, turn_count)?;
        let mut state = self.task_state.lock();
        if let Some(index) = state
            .sessions
            .iter()
            .position(|session| session.id == session_id)
        {
            self.task_store.hydrate(&mut state.sessions[index])?;
            if let Some(turn) = state.sessions[index]
                .turns
                .iter_mut()
                .find(|turn| turn.turn_count == turn_count)
            {
                turn.checkpoint = Some(checkpoint.clone());
                state.mark_session_dirty(session_id);
                self.task_store.save(&mut state)?;
            }
        }
        Ok(checkpoint)
    }
}

/// Storage-layout migrations belong to the daemon because both the database
/// rows and the directories name paths on its host. Persist after each move
/// so a later failure cannot leave an earlier project pointing at its old
/// location in SQLite.
fn migrate_projectless_state(
    task_store: &StateStore,
    task_state: &mut PersistedState,
) -> anyhow::Result<()> {
    let indices = task_state
        .projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| {
            crate::projectless::needs_migration(&project.path).then_some(index)
        })
        .collect::<Vec<_>>();
    for index in indices {
        let old_path = task_state.projects[index].path.clone();
        let workspace = crate::projectless::migrate_workspace(&old_path).with_context(|| {
            format!(
                "could not move projectless workspace {} under ~/.narel/projects",
                old_path.display()
            )
        })?;
        task_state.projects[index].name = crate::model::Project::PROJECTLESS_NAME.to_owned();
        task_state.projects[index].path = workspace.cwd;
        task_store
            .save(task_state)
            .context("could not persist migrated projectless workspace")?;
    }
    Ok(())
}

impl Backend for WakuBackend {
    fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
        let session_id = request.session_id;
        let runtime_id = request.runtime_id;
        match request.command {
            Command::AttachSession => {
                let sessions = self.sessions.lock();
                let Some(live) = sessions.get(&session_id) else {
                    return Ok(ResponsePayload::SessionRuntime {
                        runtime_id: None,
                        supports_steer: false,
                    });
                };
                Ok(ResponsePayload::SessionRuntime {
                    runtime_id: Some(live.runtime_id),
                    supports_steer: live.driver.supports_steer(),
                })
            }
            Command::GetSettings => Ok(ResponsePayload::Settings {
                settings: self.settings.get(),
            }),
            Command::UpdateSettings { settings } => {
                self.settings.replace(settings)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ProbeProvider {
                provider,
                binary_override,
                config_dir,
                discover_models,
                probe_version,
            } => {
                let (probe, version) = self.probe_provider(
                    provider,
                    binary_override.as_deref(),
                    config_dir.as_deref(),
                    discover_models,
                    probe_version,
                )?;
                Ok(ResponsePayload::ProviderProbe { probe, version })
            }
            Command::FetchPlanUsage {
                provider,
                binary_override,
                config_dir,
                cli_version,
            } => {
                let resolved_config_dir = self
                    .validate_client_config_dir(provider, config_dir.as_deref())?
                    .map(|validated| validated.resolved);
                let usage = match provider {
                    crate::model::ProviderKind::Claude => {
                        Some(crate::usage::fetch_claude_plan_usage(
                            cli_version.as_deref(),
                            resolved_config_dir.as_deref(),
                        )?)
                    }
                    crate::model::ProviderKind::Codex => Some(
                        crate::usage::fetch_codex_plan_usage(resolved_config_dir.as_deref())?,
                    ),
                    crate::model::ProviderKind::OpenCode => {
                        ensure_shell_environment();
                        crate::usage::fetch_opencode_go_plan_usage()?
                    }
                    crate::model::ProviderKind::Grok => {
                        ensure_shell_environment();
                        let probe = match binary_override.as_deref() {
                            override_value => {
                                crate::model::provider_probe(provider, override_value)
                            }
                        };
                        let binary = probe.path.ok_or_else(|| anyhow!("grok is not installed"))?;
                        Some(crate::usage::fetch_grok_plan_usage(&binary)?)
                    }
                    crate::model::ProviderKind::Antigravity => {
                        ensure_shell_environment();
                        let probe =
                            crate::model::provider_probe(provider, binary_override.as_deref());
                        let binary = probe
                            .path
                            .ok_or_else(|| anyhow!("antigravity is not installed"))?;
                        Some(crate::usage::fetch_antigravity_plan_usage_in(
                            &binary,
                            resolved_config_dir.as_deref(),
                        )?)
                    }
                    _ => bail!("provider has no plan usage fetcher"),
                };
                Ok(ResponsePayload::PlanUsage { usage })
            }
            Command::ProbeComputerPermissions { prompt } => {
                Ok(ResponsePayload::ComputerPermissions {
                    permissions: crate::computer_use::probe_permissions(prompt)?,
                })
            }
            Command::LoadUsageHistory {
                window,
                project_roots,
            } => {
                ensure_shell_environment();
                let rates = crate::usage_history::load_rate_table(&self.usage_rates_dir);
                let account_dirs = self
                    .resolved_account_dirs()
                    .into_iter()
                    .filter_map(|(provider, dir)| {
                        let provider = match provider {
                            ProviderKind::Antigravity => {
                                crate::usage_history::UsageProvider::Antigravity
                            }
                            ProviderKind::Claude => crate::usage_history::UsageProvider::Claude,
                            ProviderKind::Codex => crate::usage_history::UsageProvider::Codex,
                            _ => return None,
                        };
                        Some((provider, dir))
                    })
                    .collect::<Vec<_>>();
                let history = crate::usage_history::scan_with_account_dirs(
                    &mut self.usage_scan_cache.lock(),
                    &rates,
                    window,
                    &project_roots,
                    &account_dirs,
                );
                Ok(ResponsePayload::UsageHistory { history })
            }
            Command::LoadSkills { projects } => Ok(ResponsePayload::SkillsCatalog {
                catalog: self.load_skills_catalog(&projects),
            }),
            Command::SetSkillsEnabled { dirs, enabled } => {
                for dir in dirs {
                    crate::skills::set_skill_enabled(&dir, enabled)
                        .map_err(|error| anyhow!(error))?;
                }
                Ok(ResponsePayload::Ack)
            }
            Command::TrashSkills { dirs } => {
                crate::skills::trash_skills(&dirs).map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Ack)
            }
            Command::LoadTaskState => {
                let state = self.task_state.lock();
                Ok(ResponsePayload::TaskState {
                    deleted_project_ids: self.removed_project_ids.lock().iter().copied().collect(),
                    projects: state.projects.clone(),
                    sessions: state
                        .sessions
                        .iter()
                        .map(AgentSession::list_projection)
                        .collect(),
                    default_cwd: self.default_cwd.clone(),
                    projectless_root: crate::projectless::workspace_root(),
                })
            }
            Command::SaveTaskState {
                projects,
                live_session_ids: _,
                sessions,
            } => Ok(ResponsePayload::TaskStateSaved {
                sessions: self.save_task_state(projects, sessions)?,
            }),
            Command::RemoveSession => {
                {
                    let mut state = self.task_state.lock();
                    self.removed_session_ids.lock().insert(session_id);
                    let project_id = state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| session.project_id);
                    state.sessions.retain(|session| session.id != session_id);
                    if let Some(project_id) = project_id {
                        let remove_project = state
                            .projects
                            .iter()
                            .find(|project| project.id == project_id)
                            .is_some_and(Project::is_projectless)
                            && !state
                                .sessions
                                .iter()
                                .any(|session| session.project_id == project_id);
                        if remove_project {
                            state.projects.retain(|project| project.id != project_id);
                        }
                    }
                    self.task_store.save(&mut state)?;
                }
                let removed = self.sessions.lock().remove(&session_id);
                drop(removed);
                Ok(ResponsePayload::Ack)
            }
            Command::RemoveProject { project_id } => {
                self.remove_project(project_id)?;
                Ok(ResponsePayload::Ack)
            }
            Command::HydrateSession { session_id } => {
                // Live runtimes stay resident; everything else is trimmed to
                // the recency window once the response is built.
                let pinned = self.sessions.lock().keys().copied().collect();
                let mut state = self.task_state.lock();
                let session = if let Some(session) = state
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    self.task_store.hydrate(session)?;
                    Some(session.clone())
                } else {
                    None
                };
                trim_resident_transcripts(&mut state, &pinned);
                Ok(ResponsePayload::Session { session })
            }
            Command::SearchSessionMessages { query, limit } => {
                let matches = self.task_store.session_message_search(query, limit)()?;
                Ok(ResponsePayload::SessionMessageMatches { matches })
            }
            Command::ListProviderSessions {
                provider,
                account_id,
                limit,
            } => {
                const MAX_PROVIDER_SESSIONS: usize = 500;
                let limit = limit.min(MAX_PROVIDER_SESSIONS);
                if limit == 0 {
                    return Ok(ResponsePayload::ProviderSessions {
                        sessions: Vec::new(),
                    });
                }
                ensure_shell_environment();
                let settings = self.settings.get();
                if settings.disabled_providers.contains(&provider) {
                    return Ok(ResponsePayload::ProviderSessions {
                        sessions: Vec::new(),
                    });
                }
                let binary_override =
                    settings.effective_binary_override(provider, account_id.as_deref());
                let config_dir = settings
                    .effective_config_dir(provider, account_id.as_deref())
                    .and_then(crate::command_env::resolve_config_dir);
                let Some(binary) = crate::model::provider_probe(provider, binary_override).path
                else {
                    return Ok(ResponsePayload::ProviderSessions {
                        sessions: Vec::new(),
                    });
                };
                // Discovery is deliberately provider-scoped. Opening Resume
                // must not start every installed agent CLI, and another
                // provider is queried only after the user explicitly picks it.
                let mut sessions = match provider {
                    ProviderKind::Amp => {
                        crate::amp_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::Claude => {
                        crate::claude_session::list_provider_sessions(limit, config_dir.as_deref())?
                    }
                    ProviderKind::Codex => crate::codex_session::list_provider_sessions(
                        &binary,
                        config_dir.as_deref(),
                        limit,
                    )?,
                    ProviderKind::Antigravity
                    | ProviderKind::Cursor
                    | ProviderKind::Droid
                    | ProviderKind::Fx => crate::acp_session::list_provider_sessions_in(
                        provider,
                        &binary,
                        &[],
                        limit,
                        config_dir.as_deref(),
                    )?,
                    ProviderKind::OpenCode => {
                        crate::opencode_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::DeepSeek => {
                        crate::deepseek_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::Grok => crate::grok_session::list_provider_sessions(limit)?,
                    ProviderKind::Kimi => crate::kimi_session::list_provider_sessions(limit)?,
                    ProviderKind::OhMyPi | ProviderKind::Pi => {
                        crate::pi_session::list_provider_sessions(provider, limit)?
                    }
                };
                sessions.sort_by(|a, b| {
                    b.updated_at
                        .cmp(&a.updated_at)
                        .then_with(|| a.title.cmp(&b.title))
                });
                let imported = {
                    let state = self.task_state.lock();
                    self.task_store
                        .imported_provider_session_keys(&state.sessions)?
                };
                sessions.retain(|session| {
                    !imported.contains(&(session.provider(), session.cursor.native_id().to_owned()))
                });
                sessions.truncate(limit);
                Ok(ResponsePayload::ProviderSessions { sessions })
            }
            Command::LoadProviderSession {
                cursor,
                cwd,
                account_id,
            } => {
                // Preserve every native turn shell for exact provider turn
                // numbering, but bound imported display text to recent turns.
                const VISIBLE_TURN_LIMIT: usize = 100;
                let account_ref = account_id.as_deref();
                let mut history = match &cursor {
                    ProviderResumeCursor::Amp { thread_id, .. } => {
                        let binary = self.provider_binary(ProviderKind::Amp, None)?;
                        crate::amp_session::provider_session_history(
                            &binary,
                            &cwd,
                            thread_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::Claude { session_id, .. } => {
                        self.provider_binary(ProviderKind::Claude, account_ref)?;
                        let config_dir =
                            self.provider_config_dir(ProviderKind::Claude, account_ref);
                        crate::claude_session::provider_session_history(
                            session_id,
                            VISIBLE_TURN_LIMIT,
                            config_dir.as_deref(),
                        )?
                    }
                    ProviderResumeCursor::Codex { thread_id } => {
                        let binary = self.provider_binary(ProviderKind::Codex, account_ref)?;
                        let config_dir = self.provider_config_dir(ProviderKind::Codex, account_ref);
                        crate::codex_session::provider_session_history(
                            &binary,
                            config_dir.as_deref(),
                            thread_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    // OpenCode is not an ACP provider: its history comes from
                    // the adopted background service's own export route.
                    ProviderResumeCursor::OpenCode { session_id, .. } => {
                        let binary = self.provider_binary(ProviderKind::OpenCode, None)?;
                        crate::opencode_session::provider_session_history(
                            &binary,
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::Antigravity { session_id, .. }
                    | ProviderResumeCursor::Cursor { session_id, .. }
                    | ProviderResumeCursor::Droid { session_id }
                    | ProviderResumeCursor::Fx { session_id }
                    | ProviderResumeCursor::Grok { session_id }
                    | ProviderResumeCursor::Kimi { session_id } => {
                        let provider = cursor.provider();
                        let binary = self.provider_binary(provider, account_ref)?;
                        let config_dir = self.provider_config_dir(provider, account_ref);
                        crate::acp_session::provider_session_history_in(
                            provider,
                            &binary,
                            &cwd,
                            session_id,
                            VISIBLE_TURN_LIMIT,
                            config_dir.as_deref(),
                        )?
                    }
                    ProviderResumeCursor::DeepSeek { session_id } => {
                        let binary = self.provider_binary(ProviderKind::DeepSeek, None)?;
                        crate::deepseek_session::provider_session_history(
                            &binary,
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::OhMyPi {
                        session_id,
                        session_file,
                    }
                    | ProviderResumeCursor::Pi {
                        session_id,
                        session_file,
                    } => {
                        self.provider_binary(cursor.provider(), None)?;
                        let session_file = session_file.as_deref().ok_or_else(|| {
                            anyhow!(
                                "{} did not report its native session file",
                                cursor.provider().display_name()
                            )
                        })?;
                        crate::pi_session::provider_session_history(
                            cursor.provider(),
                            session_id,
                            session_file,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                };
                if cursor.provider().supports_multiple_accounts()
                    && let Some(account_id) = account_id.filter(|id| !id.trim().is_empty())
                {
                    for turn in &mut history.turns {
                        turn.account_id = Some(account_id.clone());
                    }
                }
                Ok(ResponsePayload::ProviderSessionHistory { history })
            }
            Command::LoadComposerDrafts => Ok(ResponsePayload::ComposerDrafts {
                drafts: self.composer_drafts.load()?,
            }),
            Command::SaveComposerDrafts {
                mut drafts,
                generation,
            } => {
                let projects = self.removed_project_ids.lock();
                let sessions = self.removed_session_ids.lock();
                drafts.new_sessions.retain(|id, _| !projects.contains(id));
                drafts.sessions.retain(|id, _| !sessions.contains(id));
                self.composer_drafts.save(drafts, generation)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ApplyComposerDraftChanges { mut changes } => {
                let projects = self.removed_project_ids.lock();
                let sessions = self.removed_session_ids.lock();
                changes.retain(|change| match change.target {
                    waku_protocol::persistence::ComposerDraftTarget::NewSession { project_id } => {
                        !projects.contains(&project_id)
                    }
                    waku_protocol::persistence::ComposerDraftTarget::Session { session_id } => {
                        !sessions.contains(&session_id)
                    }
                });
                self.composer_drafts.apply_changes(changes)?;
                Ok(ResponsePayload::Ack)
            }
            Command::StoreBlob { mime_type, bytes } => {
                let reference = self
                    .task_store
                    .blobs()
                    .store_image_bytes(&mime_type, &bytes)?;
                let path = self
                    .task_store
                    .blobs()
                    .path_for(&reference)
                    .ok_or_else(|| anyhow!("stored blob has no daemon path"))?;
                Ok(ResponsePayload::BlobStored { reference, path })
            }
            Command::ImportAttachment { name, upload } => Ok(ResponsePayload::AttachmentStored {
                attachment: self.attachments.import(&name, upload)?,
            }),
            Command::ImportPathAttachment { path } => Ok(ResponsePayload::AttachmentStored {
                attachment: self.attachments.import_path(&path)?,
            }),
            Command::ReadBlob { reference } => {
                let path = self
                    .task_store
                    .blobs()
                    .path_for(&reference)
                    .ok_or_else(|| anyhow!("invalid blob reference"))?;
                Ok(ResponsePayload::BlobData {
                    bytes: std::fs::read(path)?,
                })
            }
            Command::ReadAttachment { reference, path } => Ok(ResponsePayload::BlobData {
                bytes: self.attachments.read_file(&reference, &path)?,
            }),
            Command::SweepBlobs => {
                self.task_store.blob_sweep()();
                Ok(ResponsePayload::Ack)
            }
            Command::ForkSessionFromResponse { turn_count } => {
                let (session, checkpoint_warning) =
                    self.fork_session_from_response(session_id, turn_count)?;
                Ok(ResponsePayload::SessionForked {
                    session,
                    checkpoint_warning,
                })
            }
            Command::RewindSessionToMessage { turn_count } => {
                let (session, cleanup_warning) =
                    self.rewind_session_to_message(session_id, turn_count)?;
                Ok(ResponsePayload::SessionRewound {
                    session,
                    cleanup_warning,
                })
            }
            Command::ForkProviderSession { request } => {
                let request = self.validate_fork_provider_session_request(request)?;
                Ok(ResponsePayload::ProviderSessionForked {
                    result: fork_provider_session(request)?,
                })
            }
            Command::Workspace {
                operation:
                    WorkspaceOperation::CaptureTurn {
                        cwd,
                        session_id,
                        turn_count,
                    },
            } => Ok(ResponsePayload::Workspace {
                result: WorkspaceResult::Checkpoint {
                    checkpoint: self.capture_turn_checkpoint(cwd, session_id, turn_count)?,
                },
            }),
            Command::Workspace { operation } => {
                let operation = self.validate_workspace_operation(operation)?;
                Ok(ResponsePayload::Workspace {
                    result: crate::workspace::execute(operation)?,
                })
            }
            Command::OpenWorkspace { .. } => {
                bail!("openWorkspace must be handled by the daemon server hub")
            }
            Command::OpenTerminal { cwd, cols, rows } => {
                anyhow::ensure!(
                    (2..=4000).contains(&cols) && (1..=2000).contains(&rows),
                    "terminal dimensions out of range"
                );
                // A retried create must never replace a live shell, while an
                // exited terminal record may be replaced even across cwd/runtime changes.
                {
                    let mut terminals = self.terminals.lock();
                    if let Some((owner, directory, existing)) = terminals.get(&session_id) {
                        if !existing.exited() {
                            anyhow::ensure!(
                                *owner == runtime_id && *directory == cwd,
                                "terminal identity already exists"
                            );
                            return Ok(ResponsePayload::Ack);
                        }
                        terminals.remove(&session_id);
                    }
                    if terminals.len() >= 256 {
                        terminals
                            .retain(|id, (_, _, terminal)| *id == session_id || !terminal.exited());
                    }
                    anyhow::ensure!(
                        terminals.len() < 256,
                        "too many terminal sessions; end unused sessions first"
                    );
                }
                let terminal = self.open_terminal(&cwd, cols, rows, events)?;
                self.terminals
                    .lock()
                    .insert(session_id, (runtime_id, cwd, terminal));
                Ok(ResponsePayload::Ack)
            }
            Command::ListTerminals => {
                let terminals = self
                    .terminals
                    .lock()
                    .iter()
                    .map(
                        |(id, (runtime, cwd, terminal))| waku_protocol::TerminalInfo {
                            session_id: *id,
                            runtime_id: *runtime,
                            cwd: cwd.clone(),
                            exited: terminal.exited(),
                        },
                    )
                    .collect();
                Ok(ResponsePayload::Terminals { terminals })
            }
            Command::AttachTerminal { after } => {
                let terminals = self.terminals.lock();
                let (owner, _, terminal) = terminals.get(&session_id).ok_or_else(|| {
                    anyhow!("terminal session is no longer available; start a new terminal")
                })?;
                anyhow::ensure!(*owner == runtime_id, "terminal runtime does not match");
                Ok(ResponsePayload::TerminalOutput {
                    snapshot: terminal.snapshot(after),
                })
            }
            Command::WriteTerminal { data } => {
                let terminals = self.terminals.lock();
                let (active_runtime_id, _, terminal) = terminals
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("daemon terminal {session_id} is not running"))?;
                if *active_runtime_id != runtime_id {
                    bail!(
                        "daemon terminal {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                    );
                }
                terminal.write(data)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ResizeTerminal { cols, rows } => {
                anyhow::ensure!(
                    (2..=4000).contains(&cols) && (1..=2000).contains(&rows),
                    "terminal dimensions out of range"
                );
                let terminals = self.terminals.lock();
                let (active_runtime_id, _, terminal) = terminals
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("daemon terminal {session_id} is not running"))?;
                if *active_runtime_id != runtime_id {
                    bail!(
                        "daemon terminal {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                    );
                }
                terminal.resize(cols, rows);
                Ok(ResponsePayload::Ack)
            }
            Command::CloseTerminal => {
                let removed = {
                    let mut terminals = self.terminals.lock();
                    if let Some((active_runtime_id, _, _)) = terminals.get(&session_id) {
                        if *active_runtime_id != runtime_id {
                            bail!(
                                "daemon terminal {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                            );
                        }
                    }
                    terminals.remove(&session_id)
                };
                drop(removed);
                Ok(ResponsePayload::Ack)
            }
            Command::Start { options } => {
                anyhow::ensure!(
                    !self.removed_session_ids.lock().contains(&session_id),
                    "the task has been deleted"
                );
                let provider = decode_enum(&options.provider)?;
                let mode = decode_enum(&options.mode)?;
                let validated_account =
                    self.validate_client_config_dir(provider, options.config_dir.as_deref())?;
                let account_id = validated_account
                    .as_ref()
                    .map(|validated| validated.account_id.clone());
                let config_dir = validated_account.map(|validated| validated.resolved);
                let provider_cursor = options
                    .provider_cursor
                    .map(serde_json::from_value)
                    .transpose()
                    .context("daemon received an invalid provider cursor")?;
                let previous = self.sessions.lock().remove(&session_id);
                drop(previous);
                let options = DriverStartOptions {
                    binary: options.binary,
                    config_dir,
                    cwd: options.cwd,
                    mode,
                    model: options.model,
                    reasoning_effort: options.reasoning_effort,
                    service_tier: options.service_tier,
                    context_window: options.context_window,
                    agent_preset: options.agent_preset,
                    computer_use_enabled: options.computer_use_enabled,
                    provider_cursor,
                };
                let (wake, _wake_events) = smol::channel::bounded(1);
                let (event_sender, event_receiver) = driver::event_channel(wake);
                let handle = driver::start_local_with_attachment_roots(
                    provider,
                    options,
                    self.attachment_roots(),
                    event_sender,
                )?;
                let supports_steer = handle.supports_steer();
                let busy = Arc::new(AtomicBool::new(false));
                let last_active_at = Arc::new(Mutex::new(Instant::now()));
                let pending_steers = Arc::new(Mutex::new(VecDeque::<PendingSteerMeta>::new()));
                let forward_sessions = Arc::downgrade(&self.sessions);
                let forward_task_state = Arc::downgrade(&self.task_state);
                let forward_task_store = Arc::downgrade(&self.task_store);
                let forward_busy = busy.clone();
                let forward_last_active = last_active_at.clone();
                let forward_pending_steers = pending_steers.clone();
                std::thread::Builder::new()
                    .name(format!("waku-daemon-events-{session_id}"))
                    .spawn(move || {
                        let mut turn_event_count: usize = 0;
                        let mut saw_process_exited = false;
                        while let Ok(mut event) = event_receiver.recv() {
                            *forward_last_active.lock() = Instant::now();
                            match &mut event {
                                DriverEvent::TurnStarted => {
                                    forward_busy.store(true, Ordering::Release);
                                    turn_event_count = 0;
                                }
                                DriverEvent::TurnFinished { .. } => {
                                    forward_busy.store(false, Ordering::Release);
                                }
                                DriverEvent::ProcessExited => {
                                    forward_busy.store(false, Ordering::Release);
                                    saw_process_exited = true;
                                }
                                DriverEvent::SteerAccepted {
                                    message,
                                    message_id,
                                    display_content,
                                    attachments,
                                } => {
                                    let mut steers = forward_pending_steers.lock();
                                    let matched = steers
                                        .iter()
                                        .position(|meta| meta.prompt == *message)
                                        .and_then(|index| steers.remove(index))
                                        .or_else(|| steers.pop_front());
                                    if let Some(meta) = matched {
                                        *message_id = meta.message_id;
                                        *display_content = meta.display_content;
                                        *attachments = meta.attachments;
                                    }
                                }
                                DriverEvent::SteerRejected { message, .. } => {
                                    let mut steers = forward_pending_steers.lock();
                                    let matched = steers
                                        .iter()
                                        .position(|meta| meta.prompt == *message)
                                        .or_else(|| (!steers.is_empty()).then_some(0));
                                    if let Some(index) = matched {
                                        steers.remove(index);
                                    }
                                }
                                _ => {}
                            }
                            turn_event_count = turn_event_count.saturating_add(1);
                            let is_turn_settle = matches!(
                                &event,
                                DriverEvent::TurnFinished { .. }
                                    | DriverEvent::TurnParked
                                    | DriverEvent::ProcessExited
                            );
                            let wire = event_to_wire(event.clone()).unwrap_or_else(|error| {
                                WireDriverEvent::new(
                                    "error",
                                    Value::String(format!(
                                        "could not encode daemon event: {error}"
                                    )),
                                )
                            });
                            let Some(cursor) = events.send_sequenced(wire) else {
                                break;
                            };
                            if let (Some(task_state), Some(task_store), Some(sessions)) = (
                                forward_task_state.upgrade(),
                                forward_task_store.upgrade(),
                                forward_sessions.upgrade(),
                            ) {
                                let should_notify = reduce_and_maybe_persist_daemon_event(
                                    &task_state,
                                    &task_store,
                                    &sessions,
                                    session_id,
                                    &event,
                                    Some(cursor),
                                    is_turn_settle,
                                    events.subscriber_count(),
                                    turn_event_count,
                                );
                                if should_notify {
                                    events.notify_task_state_changed();
                                }
                            }
                            if matches!(event, DriverEvent::TurnFinished { .. }) {
                                turn_event_count = 0;
                            }
                            if saw_process_exited {
                                break;
                            }
                        }
                        let still_owned = forward_sessions.upgrade().is_some_and(|sessions| {
                            sessions
                                .lock()
                                .get(&session_id)
                                .is_some_and(|live| live.runtime_id == runtime_id)
                        });
                        if !saw_process_exited
                            && still_owned
                            && let (Some(task_state), Some(task_store), Some(sessions)) = (
                                forward_task_state.upgrade(),
                                forward_task_store.upgrade(),
                                forward_sessions.upgrade(),
                            )
                        {
                            forward_busy.store(false, Ordering::Release);
                            if reduce_and_maybe_persist_daemon_event(
                                &task_state,
                                &task_store,
                                &sessions,
                                session_id,
                                &DriverEvent::ProcessExited,
                                None,
                                true,
                                events.subscriber_count(),
                                turn_event_count,
                            ) {
                                events.notify_task_state_changed();
                            }
                        }
                        if let Some(sessions) = forward_sessions.upgrade() {
                            let removed = {
                                let mut sessions = sessions.lock();
                                sessions
                                    .get(&session_id)
                                    .is_some_and(|live| live.runtime_id == runtime_id)
                                    .then(|| sessions.remove(&session_id))
                                    .flatten()
                            };
                            drop(removed);
                        }
                        events.end_session_runtime();
                    })
                    .context("could not start daemon event forwarding thread")?;
                let mut sessions = self.sessions.lock();
                anyhow::ensure!(
                    !self.removed_session_ids.lock().contains(&session_id),
                    "the task has been deleted"
                );
                sessions.insert(
                    session_id,
                    LiveSessionRuntime {
                        runtime_id,
                        provider,
                        account_id,
                        driver: handle,
                        busy,
                        last_active_at,
                        pending_steers,
                    },
                );
                Ok(ResponsePayload::Started { supports_steer })
            }
            Command::CloseSession => {
                let removed = {
                    let mut sessions = self.sessions.lock();
                    sessions
                        .get(&session_id)
                        .is_some_and(|live| live.runtime_id == runtime_id)
                        .then(|| sessions.remove(&session_id))
                        .flatten()
                };
                drop(removed);
                Ok(ResponsePayload::Ack)
            }
            Command::ClaimQueuedMessage { message_id } => {
                let mut retired = self.retired_queued_message_ids.lock();
                if retired.contains(session_id, message_id) {
                    return Ok(ResponsePayload::QueuedMessageClaimed { claimed: false });
                }
                retired.insert(session_id, message_id);
                drop(retired);

                let mut state = self.task_state.lock();
                if let Some(index) = state
                    .sessions
                    .iter()
                    .position(|session| session.id == session_id)
                {
                    let _ = self.task_store.hydrate(&mut state.sessions[index]);
                    let before = state.sessions[index].queued_messages.len();
                    state.sessions[index]
                        .queued_messages
                        .retain(|message| message.id != message_id);
                    if state.sessions[index].queued_messages.len() != before {
                        state.sessions[index].updated_at = unix_time();
                        state.mark_session_dirty(session_id);
                        self.task_store.save(&mut state)?;
                    }
                }
                Ok(ResponsePayload::QueuedMessageClaimed { claimed: true })
            }
            command => {
                let (driver, busy, pending_steers) = {
                    let sessions = self.sessions.lock();
                    let live = sessions
                        .get(&session_id)
                        .ok_or_else(|| anyhow!("daemon session {session_id} is not running"))?;
                    if live.runtime_id != runtime_id {
                        bail!(
                            "daemon session {session_id} belongs to runtime {}, not {runtime_id}",
                            live.runtime_id
                        );
                    }
                    live.touch();
                    (
                        live.driver.clone(),
                        live.busy.clone(),
                        live.pending_steers.clone(),
                    )
                };
                if matches!(&command, Command::Prompt { .. }) {
                    busy.store(true, Ordering::Release);
                }
                if let Command::Steer {
                    prompt,
                    message_id,
                    display_content,
                    attachments,
                } = &command
                {
                    pending_steers.lock().push_back(PendingSteerMeta {
                        prompt: prompt.clone(),
                        message_id: Some(message_id.unwrap_or_else(Uuid::new_v4)),
                        display_content: display_content.clone(),
                        attachments: attachments.clone(),
                    });
                }
                if let Some(event) = prompt_submission_wire_event(&command)? {
                    // Publish the submission into the runtime's event stream
                    // before the provider can start the turn. Every attached
                    // client mirrors the user message and its turn from this
                    // event, so the submitting client's own save is no longer
                    // the only record of the prompt — a follower that only
                    // knew the provider's `turnStarted` used to persist a
                    // projection without it, erasing the message for everyone.
                    if let Some(cursor) = events.send_sequenced(event.clone())
                        && let Ok(decoded) = event_from_wire(event)
                    {
                        reduce_and_maybe_persist_daemon_event(
                            &self.task_state,
                            &self.task_store,
                            &self.sessions,
                            session_id,
                            &decoded,
                            Some(cursor),
                            false,
                            events.subscriber_count(),
                            0,
                        );
                    }
                }
                let resolved_event = match &command {
                    Command::Respond { request_id, .. } => Some(DriverEvent::PermissionResolved {
                        request_id: request_id.clone(),
                    }),
                    Command::RespondUserInput { request_id, .. } => {
                        Some(DriverEvent::UserInputResolved {
                            request_id: request_id.clone(),
                        })
                    }
                    _ => None,
                };
                let outcome = handle_driver_command(&driver, command)?;
                if let Some(resolved_event) = resolved_event
                    && let Ok(wire) = event_to_wire(resolved_event.clone())
                    && let Some(cursor) = events.send_sequenced(wire)
                {
                    reduce_and_maybe_persist_daemon_event(
                        &self.task_state,
                        &self.task_store,
                        &self.sessions,
                        session_id,
                        &resolved_event,
                        Some(cursor),
                        false,
                        events.subscriber_count(),
                        0,
                    );
                }
                Ok(outcome)
            }
        }
    }

    fn has_live_work(&self) -> bool {
        self.sessions
            .lock()
            .values()
            .any(|live| live.busy.load(Ordering::Acquire))
            || self
                .task_state
                .lock()
                .sessions
                .iter()
                .any(AgentSession::is_busy)
            || self
                .terminals
                .lock()
                .values()
                .any(|(_, _, terminal)| !terminal.exited())
    }

    fn reap_idle_runtimes(&self, max_idle: Duration) -> Vec<(Uuid, Uuid)> {
        let busy_sessions = self
            .task_state
            .lock()
            .sessions
            .iter()
            .filter(|session| session.is_busy())
            .map(|session| session.id)
            .collect::<HashSet<_>>();
        let mut removed = Vec::new();
        let mut reaped_ids = Vec::new();
        {
            let mut sessions = self.sessions.lock();
            let candidates = sessions
                .iter()
                .filter_map(|(&session_id, live)| {
                    if live.busy.load(Ordering::Acquire)
                        || busy_sessions.contains(&session_id)
                        || live.last_active_at.lock().elapsed() < max_idle
                    {
                        return None;
                    }
                    Some((session_id, live.runtime_id))
                })
                .collect::<Vec<_>>();
            for (session_id, runtime_id) in candidates {
                if let Some(live) = sessions.remove(&session_id) {
                    removed.push(live);
                    reaped_ids.push((session_id, runtime_id));
                }
            }
        }
        drop(removed);
        reaped_ids
    }

    fn is_runtime_live(&self, session_id: Uuid, runtime_id: Uuid) -> bool {
        self.sessions
            .lock()
            .get(&session_id)
            .is_some_and(|live| live.runtime_id == runtime_id)
            || self
                .terminals
                .lock()
                .get(&session_id)
                .is_some_and(|(active_runtime_id, _, _)| *active_runtime_id == runtime_id)
    }

    fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.sessions.lock());
        drop(sessions);
        let terminals = std::mem::take(&mut *self.terminals.lock());
        drop(terminals);

        let mut state = self.task_state.lock();
        let mut settled_any = false;
        let busy_ids = state
            .sessions
            .iter()
            .filter(|session| session.is_busy())
            .map(|session| session.id)
            .collect::<Vec<_>>();
        for session_id in busy_ids {
            if let Some(index) = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
            {
                let _ = self.task_store.hydrate(&mut state.sessions[index]);
                if settle_daemon_session(
                    &mut state.sessions[index],
                    TurnStatus::Failed,
                    Some("The daemon exited before the turn finished."),
                ) {
                    state.sessions[index].state_version =
                        state.sessions[index].state_version.saturating_add(1);
                    state.mark_session_dirty(session_id);
                    settled_any = true;
                }
            }
        }
        if settled_any {
            let _ = self.task_store.save(&mut state);
        }
    }
}

fn prompt_submission_wire_event(command: &Command) -> anyhow::Result<Option<WireDriverEvent>> {
    let Command::Prompt {
        prompt,
        transcript_message,
        turn_id,
        message_id,
        handoff,
    } = command
    else {
        return Ok(None);
    };
    event_to_wire(DriverEvent::PromptSubmitted {
        message: transcript_message.clone().unwrap_or_else(|| prompt.clone()),
        turn_id: turn_id.unwrap_or_else(Uuid::new_v4),
        message_id: message_id.unwrap_or_else(Uuid::new_v4),
        handoff: handoff.clone(),
    })
    .map(Some)
}

fn session_projection_precedes(
    existing: &AgentSession,
    incoming: &AgentSession,
    active_runtime_id: Option<Uuid>,
) -> bool {
    if incoming.state_version != existing.state_version {
        return incoming.state_version < existing.state_version;
    }
    let existing_cursor = existing.runtime_event_cursor;
    let incoming_cursor = incoming.runtime_event_cursor;
    if let Some(active_runtime_id) = active_runtime_id {
        let existing_is_active =
            existing_cursor.is_some_and(|cursor| cursor.runtime_id == active_runtime_id);
        let incoming_is_active =
            incoming_cursor.is_some_and(|cursor| cursor.runtime_id == active_runtime_id);
        if existing_is_active != incoming_is_active {
            return existing_is_active;
        }
    }
    match (existing_cursor, incoming_cursor) {
        (Some(existing), Some(incoming))
            if existing.runtime_id == incoming.runtime_id && existing.epoch == incoming.epoch =>
        {
            incoming.sequence < existing.sequence
        }
        (Some(_), None) if existing.status.is_busy() => true,
        _ => incoming.updated_at < existing.updated_at,
    }
}

/// Merge list-level metadata from an unhydrated client skeleton (`detail_loaded == false`).
///
/// A skeleton's `messages`, `turns`, `transcript_blocks`, `workspace`,
/// `provider_cursor`, `account_id`, and model options are empty placeholders,
/// never authoritative transcript state. Only sidebar-level metadata may be
/// updated so pinning, archiving, or renaming an unopened session cannot erase
/// persisted `session_details` or `messages`.
fn merge_skeleton_session_metadata(existing: &mut AgentSession, incoming: AgentSession) {
    if incoming.updated_at >= existing.updated_at {
        existing.title = incoming.title;
        if incoming.auto_title.is_some() {
            existing.auto_title = incoming.auto_title;
        }
        existing.pinned = incoming.pinned;
        existing.archived = incoming.archived;
        existing.project_id = incoming.project_id;
        existing.updated_at = incoming.updated_at;
        existing.last_reply_at = incoming.last_reply_at.or(existing.last_reply_at);
    }
}

fn merge_stale_session_metadata_with_retired(
    existing: &mut AgentSession,
    incoming: AgentSession,
    retired: Option<&RetiredQueuedMessageIds>,
) {
    if incoming.updated_at >= existing.updated_at {
        existing.title = incoming.title;
        existing.pinned = incoming.pinned;
        existing.archived = incoming.archived;
        existing.project_id = incoming.project_id;
        existing.workspace = incoming.workspace;
        if (existing.provider, existing.account_id.as_deref())
            != (incoming.provider, incoming.account_id.as_deref())
            && !existing.status.is_busy()
        {
            existing.switch_agent(incoming.provider, incoming.account_id.clone());
        }
        // A busy task keeps its agent, so the incoming model and options
        // belong to an agent it is not running and must not be applied to it.
        if (existing.provider, existing.account_id.as_deref())
            == (incoming.provider, incoming.account_id.as_deref())
        {
            existing.model = incoming.model;
            existing.reasoning_effort = incoming.reasoning_effort;
            existing.service_tier = incoming.service_tier;
            existing.context_window = incoming.context_window;
            existing.agent_preset = incoming.agent_preset;
        }
        existing.runtime_mode = incoming.runtime_mode;
        existing.updated_at = incoming.updated_at;
        existing.last_reply_at = incoming.last_reply_at.or(existing.last_reply_at);
    }
    for queued in incoming.queued_messages {
        if retired.is_some_and(|ids| ids.contains(existing.id, queued.id)) {
            continue;
        }
        if !existing
            .queued_messages
            .iter()
            .any(|candidate| candidate.id == queued.id)
        {
            existing.queued_messages.push(queued);
        }
    }
}

#[cfg(test)]
fn merge_stale_session_metadata(existing: &mut AgentSession, incoming: AgentSession) {
    merge_stale_session_metadata_with_retired(existing, incoming, None);
}

/// Ending checkpoints are produced and stored by the daemon. A second client
/// may still save a projection created just before capture completed; never
/// let that stale projection erase the canonical Git snapshot.
fn preserve_daemon_checkpoints(existing: &AgentSession, incoming: &mut AgentSession) {
    for turn in &mut incoming.turns {
        let Some(checkpoint) = existing
            .turns
            .iter()
            .find(|candidate| candidate.turn_count == turn.turn_count)
            .and_then(|candidate| candidate.checkpoint.as_ref())
            .filter(|checkpoint| {
                matches!(
                    checkpoint.status,
                    CheckpointStatus::Ready | CheckpointStatus::Unavailable
                )
            })
        else {
            continue;
        };
        turn.checkpoint = Some(checkpoint.clone());
    }
}

impl WakuBackend {
    /// Fork a response using only daemon-host state.
    ///
    /// A browser must never reconstruct or persist this operation itself:
    /// provider-native sessions, checkpoint refs, and the task database all
    /// belong to the daemon and may be on another machine.
    fn fork_session_from_response(
        &self,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<(AgentSession, Option<String>)> {
        let (source, cwd, fork_title) = {
            let mut state = self.task_state.lock();
            let source_index = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
                .ok_or_else(|| anyhow!("the source task is unavailable"))?;
            self.task_store
                .hydrate(&mut state.sessions[source_index])
                .context("could not load the source task")?;
            let source = state.sessions[source_index].clone();
            let project = state
                .projects
                .iter()
                .find(|project| project.id == source.project_id)
                .ok_or_else(|| anyhow!("the source task project is unavailable"))?;
            let cwd = source.workspace.path().unwrap_or(&project.path).to_owned();
            let fork_title = next_response_fork_title(
                source.display_title(),
                state
                    .sessions
                    .iter()
                    .filter(|session| session.project_id == source.project_id)
                    .map(AgentSession::display_title),
            );
            (source, cwd, fork_title)
        };

        validate_response_fork(&source, turn_count)?;
        let provider_turn_count = source.provider_turns_through(turn_count);
        let turns_to_remove = source.provider_turns_after(turn_count);
        let (provider_cursor, message_ids) = if provider_turn_count == 0 {
            (None, HashMap::new())
        } else {
            let (cursor, message_ids) = self.fork_provider_response(
                &source,
                &cwd,
                &fork_title,
                turn_count,
                provider_turn_count,
                turns_to_remove,
            )?;
            (Some(cursor), message_ids)
        };
        let mut forked = source
            .fork_through_turn(turn_count, provider_cursor, &fork_title)
            .ok_or_else(|| anyhow!("the selected response cannot be copied"))?;
        if !message_ids.is_empty() {
            for turn in &mut forked.turns {
                if let Some(message_id) = turn.provider_resume_at.as_mut()
                    && let Some(remapped) = message_ids.get(message_id)
                {
                    *message_id = remapped.clone();
                }
            }
        }

        let fork_id = forked.id;
        for turn in &mut forked.turns {
            if let Some(checkpoint) = turn.checkpoint.as_mut() {
                checkpoint.git_ref =
                    crate::checkpoint::checkpoint_ref(fork_id, checkpoint.turn_count);
            }
        }
        let checkpoint_warning =
            crate::checkpoint::copy_session_refs(&cwd, source.id, fork_id, turn_count)
                .err()
                .map(|error| error.to_string());

        let pinned = self.sessions.lock().keys().copied().collect();
        let mut state = self.task_state.lock();
        if self.removed_project_ids.lock().contains(&source.project_id)
            || self.removed_session_ids.lock().contains(&source.id)
        {
            let _ = crate::checkpoint::delete_all_session_refs(&cwd, fork_id);
            bail!("the source workspace or task was deleted while copying the response");
        }
        state.push_session(forked.clone());
        if let Err(error) = self.task_store.save(&mut state) {
            state.sessions.retain(|session| session.id != fork_id);
            let _ = crate::checkpoint::delete_all_session_refs(&cwd, fork_id);
            return Err(error).context("could not save the forked task");
        }
        trim_resident_transcripts(&mut state, &pinned);
        Ok((forked, checkpoint_warning))
    }

    /// Restore the daemon-host worktree, provider conversation, and stored
    /// transcript to immediately before one user turn.
    fn rewind_session_to_message(
        &self,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<(AgentSession, Option<String>)> {
        let (source, cwd) = {
            let mut state = self.task_state.lock();
            let source_index = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
                .ok_or_else(|| anyhow!("the task is unavailable"))?;
            self.task_store
                .hydrate(&mut state.sessions[source_index])
                .context("could not load the task")?;
            let source = state.sessions[source_index].clone();
            let project = state
                .projects
                .iter()
                .find(|project| project.id == source.project_id)
                .ok_or_else(|| anyhow!("the task project is unavailable"))?;
            let cwd = source.workspace.path().unwrap_or(&project.path).to_owned();
            (source, cwd)
        };
        validate_message_rewind(&source, turn_count)?;

        let retained_turn_count = turn_count.saturating_sub(1);
        let previous_turn_count = source.turns.len();
        let rollback_turns = source.provider_turns_after(retained_turn_count);
        let provider_turn_count = source.provider_turns_through(retained_turn_count);
        let provider_resume_at = source.provider_resume_at_through(retained_turn_count);
        // Resolve the executable before touching the worktree whenever the
        // native session itself must roll back or fork. Even native transcript
        // operations are immediately followed by a replacement prompt, so
        // accepting a rewind that cannot resume would strand the user at a
        // provider state the UI cannot continue.
        let binary = (rollback_turns > 0 && provider_turn_count > 0)
            .then(|| self.provider_binary(source.provider, source.account_id.as_deref()))
            .transpose()?;

        let turn_start_ref = crate::checkpoint::turn_start_ref(session_id, turn_count);
        let retained_ref = crate::checkpoint::checkpoint_ref(session_id, retained_turn_count);
        let restore_ref = if crate::checkpoint::has_ref(&cwd, &turn_start_ref) {
            turn_start_ref
        } else {
            retained_ref
        };
        if !crate::checkpoint::has_ref(&cwd, &restore_ref) {
            bail!("the checkpoint before this message is unavailable");
        }

        let safety_ref = format!("refs/waku/revert-backup-{session_id}-{}", Uuid::new_v4());
        crate::checkpoint::capture_ref(&cwd, &safety_ref)
            .context("could not create a rewind safety snapshot")?;
        if let Err(error) = crate::checkpoint::restore_ref(&cwd, &restore_ref) {
            return Err(restore_rewind_safety(
                &cwd,
                &safety_ref,
                "could not restore the selected checkpoint",
                error,
            ));
        }

        let provider_rewind = self.rewind_provider_response(
            &source,
            &cwd,
            binary.as_deref(),
            retained_turn_count,
            rollback_turns,
            provider_turn_count,
            provider_resume_at,
        );
        let (provider_cursor, message_ids, reset_native_session) = match provider_rewind {
            Ok(result) => result,
            Err(error) => {
                return Err(restore_rewind_safety(
                    &cwd,
                    &safety_ref,
                    "the provider rejected the rewind",
                    error,
                ));
            }
        };

        let _ = crate::checkpoint::delete_ref(&cwd, &safety_ref);
        let cleanup_warning = crate::checkpoint::delete_turn_refs_after(
            &cwd,
            session_id,
            retained_turn_count,
            previous_turn_count,
        )
        .err()
        .map(|error| error.to_string());

        // Every provider resumes from the newly stored cursor on the next
        // prompt. Dropping a resident source driver also prevents its late
        // events from racing the rewound transcript.
        let removed = self.sessions.lock().remove(&session_id);
        drop(removed);

        let mut rewound = source.clone();
        if !message_ids.is_empty() {
            for turn in rewound.turns.iter_mut().take(retained_turn_count) {
                if let Some(remapped) = turn
                    .provider_resume_at
                    .as_ref()
                    .and_then(|message_id| message_ids.get(message_id))
                    .cloned()
                {
                    turn.provider_resume_at = Some(remapped);
                }
            }
        }
        if reset_native_session {
            rewound.provider_cursor = None;
        } else if let Some(cursor) = provider_cursor {
            rewound.provider_cursor = Some(cursor);
        }
        rewound.truncate_after_turn(retained_turn_count);
        rewound.status = SessionStatus::Idle;
        rewound.runtime_event_cursor = None;
        rewound.updated_at = unix_time().max(source.updated_at);

        let pinned = self.sessions.lock().keys().copied().collect();
        let mut state = self.task_state.lock();
        let existing = state
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .ok_or_else(|| anyhow!("the task was removed while it was being rewound"))?;
        rewound.state_version = existing.state_version.max(source.state_version).saturating_add(1);
        *existing = rewound.clone();
        state.mark_session_dirty(session_id);
        self.task_store
            .save(&mut state)
            .context("could not save the rewound task")?;
        trim_resident_transcripts(&mut state, &pinned);
        Ok((rewound, cleanup_warning))
    }

    fn fork_provider_response(
        &self,
        source: &AgentSession,
        cwd: &Path,
        fork_title: &str,
        turn_count: usize,
        provider_turn_count: usize,
        turns_to_remove: usize,
    ) -> anyhow::Result<(ProviderResumeCursor, HashMap<String, String>)> {
        match source.provider {
            ProviderKind::Claude => {
                let Some(ProviderResumeCursor::Claude { session_id, .. }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Claude's native session is unavailable");
                };
                let resume_at = source.provider_resume_at_through(turn_count);
                let fork = fork_provider_session(ProviderSessionForkRequest::Claude {
                    session_id: session_id.clone(),
                    resume_at,
                    turn_count: provider_turn_count,
                    title: fork_title.to_owned(),
                    config_dir: self
                        .provider_config_dir(ProviderKind::Claude, source.account_id.as_deref()),
                })?;
                Ok((fork.cursor, fork.message_ids))
            }
            ProviderKind::Codex
            | ProviderKind::DeepSeek
            | ProviderKind::OhMyPi
            | ProviderKind::Pi => Ok((
                self.fork_response_with_driver(source, cwd, turns_to_remove)?,
                HashMap::new(),
            )),
            ProviderKind::Antigravity => {
                let fork = fork_provider_session(ProviderSessionForkRequest::Antigravity {
                    source: source.clone(),
                    turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::Cursor => {
                let fork = fork_provider_session(ProviderSessionForkRequest::Cursor {
                    source: source.clone(),
                    turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::Amp => {
                let Some(ProviderResumeCursor::Amp {
                    thread_id,
                    fork_context,
                }) = source.provider_cursor.as_ref()
                else {
                    bail!("Amp's native thread is unavailable");
                };
                let fork = fork_provider_session(ProviderSessionForkRequest::Amp {
                    binary: self.provider_binary(ProviderKind::Amp, None)?,
                    cwd: cwd.to_owned(),
                    thread_id: thread_id.clone(),
                    fork_context: fork_context.clone(),
                    turn_count: provider_turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::OpenCode => {
                let Some(ProviderResumeCursor::OpenCode { session_id, .. }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("OpenCode's native session is unavailable");
                };
                // No cwd: a session carries its own `location`, so there is no
                // server working directory to fork against.
                let fork = fork_provider_session(ProviderSessionForkRequest::OpenCode {
                    binary: self.provider_binary(ProviderKind::OpenCode, None)?,
                    session_id: session_id.clone(),
                    turn_count: provider_turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::Grok => {
                let Some(ProviderResumeCursor::Grok { session_id }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Grok Build's native session is unavailable");
                };
                let fork = fork_provider_session(ProviderSessionForkRequest::Grok {
                    binary: self.provider_binary(ProviderKind::Grok, None)?,
                    cwd: cwd.to_owned(),
                    session_id: session_id.clone(),
                    turn_count: provider_turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            // Unreachable through the UI, which hides branching for providers
            // that answer `supports_conversation_fork` with false.
            ProviderKind::Droid | ProviderKind::Fx | ProviderKind::Kimi => {
                bail!(
                    "{} cannot branch a conversation at a turn",
                    source.provider.display_name()
                )
            }
        }
    }

    fn matching_live_driver(&self, source: &AgentSession) -> Option<DriverHandle> {
        let sessions = self.sessions.lock();
        let live = sessions.get(&source.id)?;
        let matches = live.provider == source.provider
            && normalized_account_id(live.account_id.as_deref())
                == normalized_account_id(source.account_id.as_deref());
        matches.then(|| live.driver.clone())
    }

    fn fork_response_with_driver(
        &self,
        source: &AgentSession,
        cwd: &Path,
        turns_to_remove: usize,
    ) -> anyhow::Result<ProviderResumeCursor> {
        if let Some(driver) = self.matching_live_driver(source) {
            return driver.fork(turns_to_remove);
        }

        match source.provider {
            ProviderKind::Codex
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::Codex { .. })
                ) =>
            {
                bail!("Codex's native thread is unavailable");
            }
            ProviderKind::DeepSeek
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::DeepSeek { .. })
                ) =>
            {
                bail!("DeepSeek Harness's native session is unavailable");
            }
            ProviderKind::Pi
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::Pi {
                        session_file: Some(_),
                        ..
                    })
                ) =>
            {
                bail!("Pi's native session file is unavailable");
            }
            ProviderKind::OhMyPi
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::OhMyPi {
                        session_file: Some(_),
                        ..
                    })
                ) =>
            {
                bail!("Oh My Pi's native session file is unavailable");
            }
            _ => {}
        }

        let (wake, _wake_events) = smol::channel::bounded(1);
        let (event_sender, _event_receiver) = driver::event_channel(wake);
        let driver = driver::start_local_with_attachment_roots(
            source.provider,
            DriverStartOptions {
                binary: self.provider_binary(source.provider, source.account_id.as_deref())?,
                config_dir: self.provider_config_dir(source.provider, source.account_id.as_deref()),
                cwd: cwd.to_owned(),
                mode: source.runtime_mode,
                model: source.model.clone(),
                reasoning_effort: source.reasoning_effort.clone(),
                service_tier: source.service_tier.clone(),
                context_window: source.context_window.clone(),
                agent_preset: source.agent_preset.clone(),
                computer_use_enabled: false,
                provider_cursor: source.provider_cursor.clone(),
            },
            self.attachment_roots(),
            event_sender,
        )?;
        driver.fork(turns_to_remove)
    }

    #[allow(clippy::too_many_arguments)]
    fn rewind_provider_response(
        &self,
        source: &AgentSession,
        cwd: &Path,
        binary: Option<&Path>,
        retained_turn_count: usize,
        rollback_turns: usize,
        provider_turn_count: usize,
        provider_resume_at: Option<String>,
    ) -> anyhow::Result<(Option<ProviderResumeCursor>, HashMap<String, String>, bool)> {
        if rollback_turns == 0 {
            return Ok((None, HashMap::new(), false));
        }
        let reset_native_session = provider_turn_count == 0;
        if reset_native_session {
            return Ok((None, HashMap::new(), true));
        }
        let binary = binary.ok_or_else(|| {
            anyhow!(
                "{} is not installed on the daemon",
                source.provider.display_name()
            )
        })?;

        match source.provider {
            ProviderKind::Claude => {
                let Some(ProviderResumeCursor::Claude { session_id, .. }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Claude's native session is unavailable");
                };
                let fork = fork_provider_session(ProviderSessionForkRequest::Claude {
                    session_id: session_id.clone(),
                    resume_at: provider_resume_at,
                    turn_count: provider_turn_count,
                    title: format!("{} (rewind)", source.display_title()),
                    config_dir: self
                        .provider_config_dir(ProviderKind::Claude, source.account_id.as_deref()),
                })?;
                Ok((Some(fork.cursor), fork.message_ids, false))
            }
            ProviderKind::OpenCode => {
                let cursor = if let Some(driver) = self.matching_live_driver(source) {
                    driver
                        .rollback(rollback_turns)?
                        .ok_or_else(|| anyhow!("OpenCode returned no rewound-session cursor"))?
                } else {
                    let Some(ProviderResumeCursor::OpenCode { session_id, .. }) =
                        source.provider_cursor.as_ref()
                    else {
                        bail!("OpenCode's native session is unavailable");
                    };
                    fork_provider_session(ProviderSessionForkRequest::OpenCode {
                        binary: binary.to_owned(),
                        session_id: session_id.clone(),
                        turn_count: provider_turn_count,
                    })?
                    .cursor
                };
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Amp => {
                let Some(ProviderResumeCursor::Amp {
                    thread_id,
                    fork_context,
                }) = source.provider_cursor.as_ref()
                else {
                    bail!("Amp's native thread is unavailable");
                };
                let cursor = fork_provider_session(ProviderSessionForkRequest::Amp {
                    binary: binary.to_owned(),
                    cwd: cwd.to_owned(),
                    thread_id: thread_id.clone(),
                    fork_context: fork_context.clone(),
                    turn_count: provider_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Antigravity => {
                let cursor = fork_provider_session(ProviderSessionForkRequest::Antigravity {
                    source: source.clone(),
                    turn_count: retained_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Cursor => {
                let cursor = fork_provider_session(ProviderSessionForkRequest::Cursor {
                    source: source.clone(),
                    turn_count: retained_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Grok => {
                let Some(ProviderResumeCursor::Grok { session_id }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Grok Build's native session is unavailable");
                };
                let cursor = fork_provider_session(ProviderSessionForkRequest::Grok {
                    binary: binary.to_owned(),
                    cwd: cwd.to_owned(),
                    session_id: session_id.clone(),
                    turn_count: provider_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Codex
            | ProviderKind::DeepSeek
            | ProviderKind::OhMyPi
            | ProviderKind::Pi => Ok((
                self.rollback_response_with_driver(source, cwd, binary, rollback_turns)?,
                HashMap::new(),
                false,
            )),
            // Unreachable through the UI, which hides rewinding for providers
            // that answer `supports_conversation_rollback` with false.
            ProviderKind::Droid | ProviderKind::Fx | ProviderKind::Kimi => {
                bail!(
                    "{} cannot rewind a conversation to a turn",
                    source.provider.display_name()
                )
            }
        }
    }

    fn rollback_response_with_driver(
        &self,
        source: &AgentSession,
        cwd: &Path,
        binary: &Path,
        rollback_turns: usize,
    ) -> anyhow::Result<Option<ProviderResumeCursor>> {
        if let Some(driver) = self.matching_live_driver(source) {
            return driver.rollback(rollback_turns);
        }

        let (wake, _wake_events) = smol::channel::bounded(1);
        let (event_sender, _event_receiver) = driver::event_channel(wake);
        let driver = driver::start_local_with_attachment_roots(
            source.provider,
            DriverStartOptions {
                binary: binary.to_owned(),
                config_dir: self.provider_config_dir(source.provider, source.account_id.as_deref()),
                cwd: cwd.to_owned(),
                mode: source.runtime_mode,
                model: source.model.clone(),
                reasoning_effort: source.reasoning_effort.clone(),
                service_tier: source.service_tier.clone(),
                context_window: source.context_window.clone(),
                agent_preset: source.agent_preset.clone(),
                computer_use_enabled: false,
                provider_cursor: source.provider_cursor.clone(),
            },
            self.attachment_roots(),
            event_sender,
        )?;
        driver.rollback(rollback_turns)
    }

    fn provider_binary(
        &self,
        provider: ProviderKind,
        account_id: Option<&str>,
    ) -> anyhow::Result<PathBuf> {
        ensure_shell_environment();
        let settings = self.settings.get();
        let binary_override = settings.effective_binary_override(provider, account_id);
        crate::model::provider_probe(provider, binary_override)
            .path
            .ok_or_else(|| anyhow!("{} is not installed on the daemon", provider.display_name()))
    }

    fn provider_config_dir(
        &self,
        provider: ProviderKind,
        account_id: Option<&str>,
    ) -> Option<PathBuf> {
        let settings = self.settings.get();
        settings
            .effective_config_dir(provider, account_id)
            .and_then(crate::command_env::resolve_config_dir)
    }

    fn save_task_state(
        &self,
        projects: Vec<Project>,
        sessions: Vec<AgentSession>,
    ) -> anyhow::Result<Vec<AgentSession>> {
        let active_runtimes = self
            .sessions
            .lock()
            .iter()
            .map(|(session_id, live)| (*session_id, live.runtime_id))
            .collect::<HashMap<_, _>>();
        let mut state = self.task_state.lock();
        let removed_project_ids = self.removed_project_ids.lock();
        let removed_session_ids = self.removed_session_ids.lock();
        for project in projects
            .into_iter()
            .filter(|project| !removed_project_ids.contains(&project.id))
        {
            if let Some(existing) = state
                .projects
                .iter_mut()
                .find(|existing| existing.id == project.id)
            {
                *existing = project;
            } else {
                state.projects.push(project);
            }
        }
        let sessions = sessions
            .into_iter()
            .filter(|session| {
                !removed_session_ids.contains(&session.id)
                    && !removed_project_ids.contains(&session.project_id)
            })
            .collect::<Vec<_>>();
        drop(removed_session_ids);
        let mut retired = self.retired_queued_message_ids.lock();
        let mut saved_ids = Vec::with_capacity(sessions.len());
        let mut dirty_ids = Vec::with_capacity(sessions.len());
        for mut session in sessions {
            if let Some(existing) = state
                .sessions
                .iter_mut()
                .find(|existing| existing.id == session.id)
            {
                if !session.detail_loaded {
                    if session.state_version >= existing.state_version {
                        merge_skeleton_session_metadata(existing, session);
                        dirty_ids.push(existing.id);
                    }
                } else if session.state_version < existing.state_version {
                    // Reject stale projections built before a server-side rewind
                    // or offline turn reduction; still echo back `existing` so
                    // the client can catch up.
                } else if session_projection_precedes(
                    existing,
                    &session,
                    active_runtimes.get(&session.id).copied(),
                ) {
                    merge_stale_session_metadata_with_retired(existing, session, Some(&retired));
                    dirty_ids.push(existing.id);
                } else {
                    if existing.detail_loaded {
                        for prior in &existing.queued_messages {
                            if !session
                                .queued_messages
                                .iter()
                                .any(|candidate| candidate.id == prior.id)
                            {
                                retired.insert(existing.id, prior.id);
                            }
                        }
                    }
                    session
                        .queued_messages
                        .retain(|queued| !retired.contains(session.id, queued.id));
                    session.state_version = existing.state_version.max(session.state_version);
                    preserve_daemon_checkpoints(existing, &mut session);
                    *existing = session;
                    dirty_ids.push(existing.id);
                }
                saved_ids.push(existing.id);
            } else if session.detail_loaded {
                session
                    .queued_messages
                    .retain(|queued| !retired.contains(session.id, queued.id));
                dirty_ids.push(session.id);
                saved_ids.push(session.id);
                state.sessions.push(session);
            }
        }
        drop(retired);
        let used_project_ids = state
            .sessions
            .iter()
            .map(|session| session.project_id)
            .collect::<std::collections::HashSet<_>>();
        state
            .projects
            .retain(|project| !project.is_projectless() || used_project_ids.contains(&project.id));
        for session_id in &dirty_ids {
            state.mark_session_dirty(*session_id);
        }
        self.task_store.save(&mut state)?;
        let sessions = saved_ids
            .into_iter()
            .filter_map(|session_id| {
                state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .cloned()
            })
            .collect();
        // The save above can adopt full transcripts for every session
        // the client touched. Keep only the recent window resident;
        // the echoed clones above still carry the saved detail.
        trim_resident_transcripts(&mut state, &active_runtimes.keys().copied().collect());
        Ok(sessions)
    }

    fn probe_provider(
        &self,
        provider: ProviderKind,
        binary_override: Option<&str>,
        config_dir: Option<&str>,
        discover_models: bool,
        probe_version: bool,
    ) -> anyhow::Result<(ProviderProbe, Option<String>)> {
        let resolved_config_dir = self
            .validate_client_config_dir(provider, config_dir)?
            .map(|validated| validated.resolved);
        ensure_shell_environment();
        let mut probe = initial_provider_probe(
            provider,
            binary_override,
            resolved_config_dir.is_some(),
            discover_models,
            probe_version,
        );
        let version = probe_version
            .then(|| {
                probe
                    .path
                    .as_deref()
                    .and_then(crate::model::probe_provider_version)
            })
            .flatten();
        if discover_models {
            probe = crate::model::discover_provider_models_in(
                probe,
                resolved_config_dir.as_deref(),
            );
        }
        Ok((probe, version))
    }

    fn load_skills_catalog(&self, projects: &[(String, PathBuf)]) -> crate::skills::SkillsCatalog {
        ensure_shell_environment();
        let account_dirs = self.resolved_account_dirs();
        let locations = crate::skills::skill_locations_for(projects, &account_dirs);
        crate::skills::scan_skills(&locations)
    }

    fn attachment_roots(&self) -> Vec<PathBuf> {
        let Some(data_dir) = self.task_store.path().parent() else {
            return Vec::new();
        };
        vec![data_dir.join("blobs"), data_dir.join("attachments")]
    }

    fn resolved_account_dirs(&self) -> Vec<(ProviderKind, PathBuf)> {
        self.settings
            .get()
            .provider_accounts
            .into_iter()
            .filter_map(|account| {
                let dir = crate::command_env::resolve_config_dir(&account.config_dir)?;
                Some((account.provider, dir))
            })
            .collect()
    }

    /// Validate a client-supplied `config_dir` string against the daemon's
    /// registered `provider_accounts` for `provider`.
    ///
    /// Allows `None` (or empty/whitespace, meaning the provider's default
    /// account) and paths that match a registered account for the same
    /// provider, returning the registered account's own raw spec and resolved
    /// path so raw WS clients cannot steer host reads/processes to arbitrary
    /// directories.
    fn validate_client_config_dir(
        &self,
        provider: ProviderKind,
        config_dir: Option<&str>,
    ) -> anyhow::Result<Option<ValidatedClientConfigDir>> {
        let Some(raw) = config_dir.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        let requested = crate::command_env::resolve_config_dir(raw)
            .ok_or_else(|| anyhow!("invalid config_dir for {}", provider.display_name()))?;
        self.validate_client_config_dir_resolved(provider, &requested)
    }

    fn validate_client_config_dir_path(
        &self,
        provider: ProviderKind,
        config_dir: Option<&Path>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let Some(path) = config_dir.filter(|path| !path.as_os_str().is_empty()) else {
            return Ok(None);
        };
        let resolved = match path.to_str() {
            Some(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                crate::command_env::resolve_config_dir(trimmed)
                    .ok_or_else(|| anyhow!("invalid config_dir for {}", provider.display_name()))?
            }
            None => path.to_path_buf(),
        };
        Ok(self
            .validate_client_config_dir_resolved(provider, &resolved)?
            .map(|validated| validated.resolved))
    }

    fn validate_client_config_dir_resolved(
        &self,
        provider: ProviderKind,
        requested: &Path,
    ) -> anyhow::Result<Option<ValidatedClientConfigDir>> {
        let Some(requested_key) = normalize_config_dir_for_comparison(requested) else {
            bail!(
                "config_dir is not a valid path for {}",
                provider.display_name()
            );
        };
        let settings = self.settings.get();
        for account in settings.accounts_for(provider) {
            let trimmed = account.config_dir.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some(registered_resolved) = crate::command_env::resolve_config_dir(trimmed) else {
                continue;
            };
            let Some(registered_key) = normalize_config_dir_for_comparison(&registered_resolved)
            else {
                continue;
            };
            if requested_key == registered_key {
                return Ok(Some(ValidatedClientConfigDir {
                    account_id: account.id.clone(),
                    raw: trimmed.to_owned(),
                    resolved: registered_resolved,
                }));
            }
        }
        bail!(
            "config_dir is not registered for {} in daemon settings",
            provider.display_name()
        )
    }

    fn validate_fork_provider_session_request(
        &self,
        mut request: ProviderSessionForkRequest,
    ) -> anyhow::Result<ProviderSessionForkRequest> {
        if let ProviderSessionForkRequest::Claude { config_dir, .. } = &mut request {
            *config_dir =
                self.validate_client_config_dir_path(ProviderKind::Claude, config_dir.as_deref())?;
        }
        Ok(request)
    }

    fn validate_workspace_operation(
        &self,
        mut operation: WorkspaceOperation,
    ) -> anyhow::Result<WorkspaceOperation> {
        match &mut operation {
            WorkspaceOperation::DiscoverSlashCommands {
                provider,
                config_dir,
                ..
            } => {
                *config_dir = self
                    .validate_client_config_dir(*provider, config_dir.as_deref())?
                    .map(|validated| validated.raw);
            }
            WorkspaceOperation::GenerateCommitMessage { invocation, .. } => {
                invocation.config_dir = self
                    .validate_client_config_dir(
                        invocation.provider,
                        invocation.config_dir.as_deref(),
                    )?
                    .map(|validated| validated.raw);
            }
            WorkspaceOperation::ReadAntigravitySwarm {
                session_id,
                config_dir,
            } => {
                crate::antigravity_swarm::validate_session_id(session_id)?;
                *config_dir = self
                    .validate_client_config_dir(ProviderKind::Antigravity, config_dir.as_deref())?
                    .map(|validated| validated.raw);
            }
            _ => {}
        }
        Ok(operation)
    }
}

/// Normalize an absolute host path for security comparison without requiring
/// the leaf directory to exist yet.
///
/// Existing paths use `fs::canonicalize` directly so symlinks and `..`
/// components resolve against the real filesystem. Non-existent paths reject
/// `..` components and canonicalize the deepest existing ancestor before
/// appending the remaining normal segments.
fn initial_provider_probe(
    provider: ProviderKind,
    binary_override: Option<&str>,
    has_config_dir: bool,
    discover_models: bool,
    probe_version: bool,
) -> ProviderProbe {
    initial_provider_probe_with(
        provider,
        binary_override,
        has_config_dir,
        discover_models,
        probe_version,
        crate::model::cached_provider_probe,
        crate::model::provider_probe,
    )
}

fn initial_provider_probe_with(
    provider: ProviderKind,
    binary_override: Option<&str>,
    has_config_dir: bool,
    discover_models: bool,
    probe_version: bool,
    cached_probe: fn(ProviderKind, Option<&str>) -> ProviderProbe,
    fresh_probe: fn(ProviderKind, Option<&str>) -> ProviderProbe,
) -> ProviderProbe {
    // Never hydrate default-account disk-cached models when probing an isolated
    // account (`config_dir.is_some()`).
    if discover_models || probe_version || has_config_dir {
        fresh_probe(provider, binary_override)
    } else {
        cached_probe(provider, binary_override)
    }
}

fn normalize_config_dir_for_comparison(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty()
        || !path.is_absolute()
        || path.as_os_str().to_string_lossy().contains('\0')
    {
        return None;
    }
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(comparable_path(canonical));
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    let mut tail = Vec::new();
    let mut cursor = path;
    let base = loop {
        match std::fs::canonicalize(cursor) {
            Ok(canonical) => break canonical,
            Err(_) => {
                let name = cursor.file_name()?;
                tail.push(name.to_os_string());
                let parent = cursor.parent()?;
                if parent == cursor {
                    return None;
                }
                cursor = parent;
            }
        }
    };
    let mut normalized = base;
    for part in tail.into_iter().rev() {
        normalized.push(part);
    }
    Some(comparable_path(normalized))
}

fn comparable_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        let stripped = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            rest.to_owned()
        } else {
            text.into_owned()
        };
        PathBuf::from(stripped.to_lowercase())
    }
    #[cfg(not(windows))]
    {
        path
    }
}

fn validate_message_rewind(source: &AgentSession, turn_count: usize) -> anyhow::Result<()> {
    if !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed) {
        bail!("stop the task before editing a prior message");
    }
    let Some(turn) = source
        .turns
        .iter()
        .find(|turn| turn.turn_count == turn_count)
    else {
        bail!("the selected message is unavailable");
    };
    if !source.messages.iter().any(|message| {
        message.turn_id == Some(turn.id) && message.role == crate::model::MessageRole::User
    }) {
        bail!("the selected user message is unavailable");
    }
    let rollback_turns = source.provider_turns_after(turn_count.saturating_sub(1));
    if rollback_turns > 0 && source.provider_cursor.is_none() {
        bail!("the provider conversation is unavailable");
    }
    Ok(())
}

fn restore_rewind_safety(
    cwd: &Path,
    safety_ref: &str,
    context: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    match crate::checkpoint::restore_ref(cwd, safety_ref) {
        Ok(()) => {
            let _ = crate::checkpoint::delete_ref(cwd, safety_ref);
            anyhow!("{context}: {error}; the original worktree was restored")
        }
        Err(restore_error) => anyhow!(
            "{context}: {error}; restoring the safety snapshot also failed: {restore_error}; snapshot: {safety_ref}"
        ),
    }
}

fn validate_response_fork(source: &AgentSession, turn_count: usize) -> anyhow::Result<()> {
    if !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed) {
        bail!("stop the task before forking a response");
    }
    if !source.provider.supports_conversation_fork() {
        bail!(
            "{} cannot branch a conversation at a turn",
            source.provider.display_name()
        );
    }
    if source
        .turns
        .get(turn_count.saturating_sub(1))
        .is_none_or(|turn| turn.turn_count != turn_count || !turn.provider_turn_started)
    {
        bail!("the selected response cannot be forked");
    }
    if source.provider_turns_through(turn_count) > 0 {
        let cursor = source
            .provider_cursor
            .as_ref()
            .ok_or_else(|| anyhow!("the provider conversation is unavailable"))?;
        if cursor.provider() != source.provider {
            bail!("the provider conversation does not match this task");
        }
    }
    Ok(())
}

fn numbered_title_suffix(title: &str) -> Option<(&str, usize)> {
    let (base, suffix) = title.rsplit_once(" (")?;
    let number = suffix.strip_suffix(')')?.parse().ok()?;
    (!base.is_empty() && number >= 2).then_some((base, number))
}

fn next_response_fork_title<'a>(
    source_title: &str,
    existing_titles: impl IntoIterator<Item = &'a str>,
) -> String {
    let existing_titles = existing_titles.into_iter().collect::<Vec<_>>();
    let base = numbered_title_suffix(source_title)
        .filter(|(base, _)| existing_titles.iter().any(|title| title == base))
        .map_or(source_title, |(base, _)| base);
    let highest_number = existing_titles
        .iter()
        .filter_map(|title| {
            if *title == base {
                Some(1)
            } else {
                numbered_title_suffix(title)
                    .filter(|(candidate_base, _)| *candidate_base == base)
                    .map(|(_, number)| number)
            }
        })
        .max()
        .unwrap_or(1);
    format!("{base} ({})", highest_number.saturating_add(1).max(2))
}

fn fork_provider_session(
    request: ProviderSessionForkRequest,
) -> anyhow::Result<ProviderSessionFork> {
    use crate::model::ProviderResumeCursor;

    let (cursor, message_ids, source_resume_at) = match request {
        ProviderSessionForkRequest::Claude {
            session_id,
            resume_at,
            turn_count,
            title,
            config_dir,
        } => {
            let source_resume_at = resume_at.map(Ok).unwrap_or_else(|| {
                crate::claude_session::message_id_for_turn(
                    &session_id,
                    turn_count,
                    config_dir.as_deref(),
                )
            })?;
            let fork = crate::claude_session::fork_session_at(
                &session_id,
                &source_resume_at,
                &title,
                config_dir.as_deref(),
            )?;
            let fork_resume_at = fork
                .message_ids
                .get(&source_resume_at)
                .cloned()
                .ok_or_else(|| anyhow!("Claude fork did not include its target message"))?;
            (
                ProviderResumeCursor::Claude {
                    session_id: fork.session_id,
                    resume_at: Some(fork_resume_at),
                },
                fork.message_ids,
                Some(source_resume_at),
            )
        }
        ProviderSessionForkRequest::Amp {
            binary,
            cwd,
            thread_id,
            fork_context,
            turn_count,
        } => (
            crate::amp_session::fork_session_at_turn(
                &binary,
                &cwd,
                &thread_id,
                fork_context.as_deref(),
                turn_count,
            )?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::Antigravity { source, turn_count } => (
            crate::antigravity_session::fork_session_at_turn(&source, turn_count)?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::Cursor { source, turn_count } => (
            crate::cursor_session::fork_session_at_turn(&source, turn_count)?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::OpenCode {
            binary,
            session_id,
            turn_count,
        } => (
            crate::opencode_session::fork_session_at_turn(&binary, &session_id, turn_count)?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::Grok {
            binary,
            cwd,
            session_id,
            turn_count,
        } => (
            crate::grok_session::fork_session_at_turn(&binary, &cwd, &session_id, turn_count)?,
            HashMap::new(),
            None,
        ),
    };
    Ok(ProviderSessionFork {
        cursor,
        message_ids,
        source_resume_at,
    })
}

fn handle_driver_command(
    driver: &DriverHandle,
    command: Command,
) -> anyhow::Result<ResponsePayload> {
    match command {
        Command::Prompt { prompt, .. } => driver.prompt(prompt),
        Command::Steer { prompt, .. } => driver.steer(prompt),
        Command::Cancel => driver.cancel(),
        Command::CancelComputerUse => driver.cancel_computer_use(),
        Command::RefreshBackgroundWork => driver.refresh_background_work(),
        Command::StopBackgroundWork { key, control_id } => {
            driver.stop_background_work(
                serde_json::from_value(key).context("invalid background-work key")?,
                control_id,
            );
        }
        Command::Respond {
            request_id,
            option_id,
        } => driver.respond(request_id, option_id),
        Command::RespondUserInput {
            request_id,
            answers,
        } => driver.respond_user_input(request_id, answers),
        Command::Goal { operation } => driver.goal(operation),
        Command::RunComputerTool { request } => {
            driver.run_computer_tool(crate::computer_use::ComputerToolRequest {
                call_id: request.call_id,
                tool: request.tool,
                arguments: request.arguments,
            });
        }
        Command::RejectComputerTool { request, reason } => {
            driver.reject_computer_tool(
                crate::computer_use::ComputerToolRequest {
                    call_id: request.call_id,
                    tool: request.tool,
                    arguments: request.arguments,
                },
                reason,
            );
        }
        Command::ApplyOptions { options } => {
            return Ok(ResponsePayload::OptionsApplied {
                applied: driver.apply_options(SessionOptions {
                    mode: decode_enum(&options.mode)?,
                    model: options.model,
                    reasoning_effort: options.reasoning_effort,
                    service_tier: options.service_tier,
                    context_window: options.context_window,
                }),
            });
        }
        Command::Rollback { turns } => {
            let cursor = driver
                .rollback(turns)?
                .map(serde_json::to_value)
                .transpose()?;
            return Ok(ResponsePayload::Cursor { cursor });
        }
        Command::Fork { turns_to_remove } => {
            let cursor = Some(serde_json::to_value(driver.fork(turns_to_remove)?)?);
            return Ok(ResponsePayload::Cursor { cursor });
        }
        Command::AttachSession
        | Command::Start { .. }
        | Command::ClaimQueuedMessage { .. }
        | Command::GetSettings
        | Command::UpdateSettings { .. }
        | Command::ProbeProvider { .. }
        | Command::FetchPlanUsage { .. }
        | Command::ProbeComputerPermissions { .. }
        | Command::LoadUsageHistory { .. }
        | Command::LoadSkills { .. }
        | Command::SetSkillsEnabled { .. }
        | Command::TrashSkills { .. }
        | Command::LoadTaskState
        | Command::SaveTaskState { .. }
        | Command::RemoveSession
        | Command::RemoveProject { .. }
        | Command::HydrateSession { .. }
        | Command::SearchSessionMessages { .. }
        | Command::ListProviderSessions { .. }
        | Command::LoadProviderSession { .. }
        | Command::LoadComposerDrafts
        | Command::SaveComposerDrafts { .. }
        | Command::ApplyComposerDraftChanges { .. }
        | Command::StoreBlob { .. }
        | Command::ImportAttachment { .. }
        | Command::ImportPathAttachment { .. }
        | Command::ReadBlob { .. }
        | Command::ReadAttachment { .. }
        | Command::SweepBlobs
        | Command::ForkSessionFromResponse { .. }
        | Command::RewindSessionToMessage { .. }
        | Command::ForkProviderSession { .. }
        | Command::Workspace { .. }
        | Command::OpenWorkspace { .. }
        | Command::OpenTerminal { .. }
        | Command::WriteTerminal { .. }
        | Command::ResizeTerminal { .. }
        | Command::CloseTerminal
        | Command::AttachTerminal { .. }
        | Command::ListTerminals
        | Command::CloseSession => {
            bail!("daemon received a command in the wrong dispatch path")
        }
    }
    Ok(ResponsePayload::Ack)
}

#[allow(clippy::too_many_arguments)]
fn reduce_and_maybe_persist_daemon_event(
    task_state: &Mutex<PersistedState>,
    task_store: &StateStore,
    live_sessions: &Mutex<HashMap<Uuid, LiveSessionRuntime>>,
    session_id: Uuid,
    event: &DriverEvent,
    cursor: Option<RuntimeEventCursor>,
    flush_to_store: bool,
    subscriber_count: usize,
    turn_event_count: usize,
) -> bool {
    let pinned: HashSet<Uuid> = live_sessions.lock().keys().copied().collect();
    let mut state = task_state.lock();
    let Some(index) = state
        .sessions
        .iter()
        .position(|session| session.id == session_id)
    else {
        return false;
    };
    if !state.sessions[index].detail_loaded && task_store.hydrate(&mut state.sessions[index]).is_err()
    {
        return false;
    }
    let (changed, settled) = reduce_daemon_driver_event(&mut state.sessions[index], event, cursor);
    let bump_version = settled
        && (matches!(event, DriverEvent::ProcessExited)
            || subscriber_count == 0
            || turn_event_count >= OFFLINE_REDUCTION_EVENTS_FLUSH_THRESHOLD);
    if bump_version {
        state.sessions[index].state_version = state.sessions[index].state_version.saturating_add(1);
    }
    if changed {
        state.mark_session_dirty(session_id);
    }
    if flush_to_store && (changed || bump_version) {
        if let Err(error) = task_store.save(&mut state) {
            eprintln!("waku-daemon could not flush session {session_id} to store: {error}");
        } else {
            trim_resident_transcripts(&mut state, &pinned);
        }
    }
    bump_version
}

fn reduce_daemon_driver_event(
    session: &mut AgentSession,
    event: &DriverEvent,
    cursor: Option<RuntimeEventCursor>,
) -> (bool, bool) {
    if let Some(cursor) = cursor {
        session.runtime_event_cursor = Some(cursor);
    }
    let mut changed = cursor.is_some();
    let mut settled = false;
    match event {
        DriverEvent::Connected { provider_cursor } => {
            session.provider_cursor = provider_cursor.clone();
            if let Some(crate::model::ProviderResumeCursor::Claude {
                resume_at: Some(resume_at),
                ..
            }) = session.provider_cursor.as_ref()
            {
                session.mark_active_turn_provider_resume_at(resume_at.clone());
            }
            if session.status == SessionStatus::Connecting {
                session.status = SessionStatus::Working;
            }
            changed = true;
        }
        DriverEvent::AgentPresetSelected(preset) => {
            session.agent_preset = preset.clone();
            changed = true;
        }
        DriverEvent::AutoTitleUpdated(title) => {
            session.auto_title = title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
            changed = true;
        }
        DriverEvent::AvailableCommands(commands) => {
            session.available_commands = commands.clone();
            changed = true;
        }
        DriverEvent::PromptSubmitted {
            message,
            turn_id,
            message_id,
            handoff,
        } => {
            changed |=
                session.adopt_submitted_prompt(message, *turn_id, *message_id, handoff.as_ref());
        }
        DriverEvent::SteerAccepted {
            message,
            message_id,
            display_content,
            attachments,
        } => {
            let duplicate = message_id.is_some_and(|id| {
                session.messages.iter().any(|candidate| candidate.id == id)
            });
            if !duplicate {
                let mut pushed = match session.active_turn_id() {
                    Some(turn_id) => Message::new_for_turn(MessageRole::User, message, turn_id),
                    None => Message::new(MessageRole::User, message),
                }
                .with_presentation(display_content.clone(), attachments.clone());
                if let Some(id) = message_id {
                    pushed.id = *id;
                }
                session.messages.push(pushed);
                changed = true;
            }
        }
        DriverEvent::TurnStarted => {
            if session.active_turn_id().is_some() {
                session.mark_active_turn_provider_started();
                session.status = SessionStatus::Working;
                changed = true;
            } else if matches!(
                session.provider,
                ProviderKind::Codex
                    | ProviderKind::Claude
                    | ProviderKind::OpenCode
                    | ProviderKind::Antigravity
            ) && !session.is_busy()
            {
                session.begin_provider_turn();
                session.mark_active_turn_provider_started();
                session.status = SessionStatus::Working;
                changed = true;
            }
        }
        DriverEvent::TurnParked => {
            if session.active_turn_id().is_some() {
                finish_daemon_streaming_messages(session);
                complete_daemon_activities(session);
                session.status = SessionStatus::Background;
                changed = true;
            }
        }
        DriverEvent::TextDelta(delta) => {
            if session.is_busy() && !delta.is_empty() {
                complete_daemon_reasoning(session);
                if let Some(last) = session
                    .messages
                    .last_mut()
                    .filter(|message| message.role == MessageRole::Assistant && message.streaming)
                {
                    last.content.push_str(delta);
                } else {
                    let mut message = match session.active_turn_id() {
                        Some(turn_id) => Message::new_for_turn(MessageRole::Assistant, delta, turn_id),
                        None => Message::new(MessageRole::Assistant, delta),
                    };
                    message.streaming = true;
                    session.messages.push(message);
                }
                changed = true;
            }
        }
        DriverEvent::ReasoningDelta(delta) => {
            if session.is_busy() && (!delta.trim().is_empty() || has_open_daemon_reasoning(session)) {
                finish_daemon_streaming_messages(session);
                append_daemon_reasoning(session, delta);
                changed = true;
            }
        }
        DriverEvent::Activity {
            id,
            kind,
            title,
            detail,
            complete,
        } => {
            if session.is_busy() {
                finish_daemon_streaming_messages(session);
                complete_daemon_reasoning(session);
                let item =
                    ActivityItem::new(id.clone(), *kind, title.clone(), detail.clone(), *complete);
                upsert_daemon_activity(session, item);
                changed = true;
            }
        }
        DriverEvent::RichActivity(item) => {
            if session.is_busy() {
                finish_daemon_streaming_messages(session);
                complete_daemon_reasoning(session);
                upsert_daemon_activity(session, item.clone());
                changed = true;
            }
        }
        DriverEvent::Permission { .. } | DriverEvent::UserInputRequested { .. } => {
            if session.is_busy() {
                session.status = SessionStatus::Waiting;
                changed = true;
            }
        }
        DriverEvent::PermissionResolved { .. } | DriverEvent::UserInputResolved { .. } => {
            if session.status == SessionStatus::Waiting {
                session.status = SessionStatus::Working;
                changed = true;
            }
        }
        DriverEvent::UsageUpdated {
            context_tokens,
            context_window,
        } => {
            let usage = session
                .context_usage
                .get_or_insert_with(crate::model::ContextUsage::default);
            if let Some(tokens) = context_tokens {
                usage.tokens = *tokens;
            }
            if let Some(window) = context_window {
                usage.window = Some(*window);
            }
            changed = true;
        }
        DriverEvent::GoalUpdated(goal) => {
            if let Some(goal) = goal
                && session.messages.is_empty()
            {
                session.set_title_from_prompt(&goal.objective);
            }
            session.thread_goal = goal.clone();
            changed = true;
        }
        DriverEvent::TurnFinished { success, summary } => {
            let turn_status = if *success {
                TurnStatus::Completed
            } else {
                TurnStatus::Failed
            };
            if settle_daemon_session(session, turn_status, summary.as_deref()) {
                changed = true;
                settled = true;
            }
        }
        DriverEvent::Error(error) => {
            if session.active_turn_is_unconfirmed_pursuit() {
                if let Some(turn_id) = session.active_turn_id() {
                    session.unwind_unstarted_turn(turn_id);
                    if session.is_busy() {
                        session.status = SessionStatus::Idle;
                    }
                    changed = true;
                }
            } else if let Some(turn_id) = session.active_turn_id()
                && session.status != SessionStatus::Working
            {
                let has_assistant = session.messages.iter().any(|message| {
                    message.turn_id == Some(turn_id) && message.role == MessageRole::Assistant
                });
                session.status = SessionStatus::Failed;
                if !has_assistant {
                    session
                        .messages
                        .push(Message::new_for_turn(MessageRole::Assistant, error, turn_id));
                }
                changed = true;
            }
        }
        DriverEvent::ProcessExited => {
            if settle_daemon_session(
                session,
                TurnStatus::Failed,
                Some("The agent exited before responding."),
            ) {
                changed = true;
                settled = true;
            } else if session.is_busy() {
                session.status = SessionStatus::Failed;
                changed = true;
                settled = true;
            }
        }
        DriverEvent::RuntimeEventCursorAdvanced(_)
        | DriverEvent::BackgroundWork(_)
        | DriverEvent::ComputerUseUpdated(_)
        | DriverEvent::SteerRejected { .. }
        | DriverEvent::PlanUsageUpdated(_) => {}
    }
    if changed {
        session.updated_at = unix_time();
    }
    (changed, settled)
}

fn settle_daemon_session(
    session: &mut AgentSession,
    status: TurnStatus,
    fallback: Option<&str>,
) -> bool {
    finish_daemon_streaming_messages(session);
    complete_daemon_activities(session);
    let Some(turn_id) = session.active_turn_id() else {
        return false;
    };
    let has_assistant = session
        .messages
        .iter()
        .any(|message| message.turn_id == Some(turn_id) && message.role == MessageRole::Assistant);
    if !has_assistant {
        let content = fallback.unwrap_or(if status == TurnStatus::Completed {
            "The turn completed without a text response."
        } else {
            "The turn stopped before a response."
        });
        session
            .messages
            .push(Message::new_for_turn(MessageRole::Assistant, content, turn_id));
    }
    session.finish_active_turn(status);
    session.status = if status == TurnStatus::Completed {
        SessionStatus::Idle
    } else {
        SessionStatus::Failed
    };
    true
}

fn finish_daemon_streaming_messages(session: &mut AgentSession) {
    for message in &mut session.messages {
        if message.role == MessageRole::Assistant {
            message.streaming = false;
        }
    }
}

fn has_open_daemon_reasoning(session: &AgentSession) -> bool {
    session
        .transcript_blocks
        .iter()
        .rev()
        .flat_map(|block| block.activities.iter().rev())
        .find(|activity| activity.kind == crate::model::ActivityKind::Reasoning)
        .is_some_and(|activity| !activity.complete)
}

fn complete_daemon_reasoning(session: &mut AgentSession) {
    for block in session.transcript_blocks.iter_mut().rev() {
        if let Some(activity) = block
            .activities
            .iter_mut()
            .rev()
            .find(|activity| activity.kind == crate::model::ActivityKind::Reasoning)
        {
            activity.complete = true;
            return;
        }
    }
}

fn complete_daemon_activities(session: &mut AgentSession) {
    for block in &mut session.transcript_blocks {
        for activity in &mut block.activities {
            activity.complete = true;
        }
    }
}

fn append_daemon_reasoning(session: &mut AgentSession, delta: &str) {
    let now_ms = unix_time_millis();
    for block in session.transcript_blocks.iter_mut().rev() {
        if let Some(activity) = block
            .activities
            .iter_mut()
            .rev()
            .find(|activity| activity.kind == crate::model::ActivityKind::Reasoning)
        {
            if !activity.complete {
                let reasoning = activity.reasoning.get_or_insert_with(|| ReasoningBlock {
                    content: String::new(),
                    started_at_ms: now_ms,
                    finished_at_ms: now_ms,
                });
                reasoning.content.push_str(delta);
                reasoning.finished_at_ms = now_ms;
                return;
            }
            break;
        }
    }
    let mut activity = ActivityItem::new(
        None,
        crate::model::ActivityKind::Reasoning,
        "Reasoning",
        None,
        false,
    );
    activity.reasoning = Some(ReasoningBlock {
        content: delta.to_owned(),
        started_at_ms: now_ms,
        finished_at_ms: now_ms,
    });
    push_daemon_activity(session, activity);
}

fn upsert_daemon_activity(session: &mut AgentSession, incoming: ActivityItem) {
    for block in session.transcript_blocks.iter_mut().rev() {
        let matching = block.activities.iter_mut().rev().find(|activity| {
            if let Some(source_id) = incoming.source_id.as_deref() {
                activity.source_id.as_deref() == Some(source_id)
            } else {
                activity.title == incoming.title && !activity.complete
            }
        });
        if let Some(existing) = matching {
            let preserved_id = existing.id;
            let tool_name = incoming.tool_name.or_else(|| existing.tool_name.clone());
            let mcp_server = incoming.mcp_server.or_else(|| existing.mcp_server.clone());
            let detail = incoming.detail.or_else(|| existing.detail.clone());
            let arguments = incoming.arguments.or_else(|| existing.arguments.clone());
            let output = incoming.output.or_else(|| existing.output.clone());
            let image_urls = if incoming.image_urls.is_empty() {
                std::mem::take(&mut existing.image_urls)
            } else {
                incoming.image_urls
            };
            let file_changes = if incoming.file_changes.is_empty() {
                std::mem::take(&mut existing.file_changes)
            } else {
                incoming.file_changes
            };
            let display_target = incoming
                .display_target
                .or_else(|| existing.display_target.clone());
            let display_description = incoming
                .display_description
                .or_else(|| existing.display_description.clone());
            let reasoning = incoming.reasoning.or_else(|| existing.reasoning.clone());
            *existing = ActivityItem {
                id: preserved_id,
                tool_name,
                mcp_server,
                detail,
                arguments,
                output,
                image_urls,
                file_changes,
                display_target,
                display_description,
                reasoning,
                ..incoming
            };
            return;
        }
    }
    push_daemon_activity(session, incoming);
}

fn push_daemon_activity(session: &mut AgentSession, activity: ActivityItem) {
    let after_message = session.messages.len();
    let turn_id = session.active_turn_id();
    if let Some(last) = session
        .transcript_blocks
        .last_mut()
        .filter(|block| block.after_message == after_message && block.turn_id == turn_id)
    {
        last.activities.push(activity);
        return;
    }
    session.transcript_blocks.push(TranscriptBlock {
        after_message,
        turn_id,
        activities: vec![activity],
    });
}

fn ensure_shell_environment() {
    static REFRESHED: AtomicBool = AtomicBool::new(false);
    static REFRESH_LOCK: Mutex<()> = Mutex::new(());
    if REFRESHED.load(Ordering::Acquire) {
        return;
    }
    let _guard = REFRESH_LOCK.lock();
    if !REFRESHED.load(Ordering::Acquire) && crate::command_env::refresh_from_default_shell() {
        REFRESHED.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SubmittedHandoff;

    #[test]
    fn stale_runtime_projection_keeps_newer_transcript_cursor() {
        let runtime_id = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        existing.status = SessionStatus::Working;
        existing.runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 10,
        });
        existing.push_message(crate::model::MessageRole::Assistant, "complete so far");

        let mut stale = existing.clone();
        stale.title = "Renamed elsewhere".into();
        assert!(stale.set_pinned(true));
        assert!(stale.set_archived(true));
        stale.messages.clear();
        stale.runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 7,
        });

        assert!(session_projection_precedes(
            &existing,
            &stale,
            Some(runtime_id)
        ));
        merge_stale_session_metadata(&mut existing, stale);
        assert_eq!(existing.title, "Renamed elsewhere");
        assert!(existing.pinned);
        assert!(existing.archived);
        assert_eq!(existing.messages.len(), 1);
        assert_eq!(existing.runtime_event_cursor.unwrap().sequence, 10);
    }

    #[test]
    fn older_projection_cannot_overwrite_newer_organization_flags() {
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        assert!(existing.set_pinned(true));
        assert!(existing.set_archived(true));
        existing.updated_at = 100;
        let mut older = existing.clone();
        older.pinned = false;
        older.archived = false;
        older.updated_at = 99;

        merge_stale_session_metadata(&mut existing, older);

        assert_eq!(existing.pinned, true);
        assert_eq!(existing.archived, true);
        assert_eq!(existing.updated_at, 100);
    }

    #[test]
    fn stale_metadata_applies_same_provider_account_switch_only_when_idle() {
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        existing.set_live_provider_cursor(Some(ProviderResumeCursor::Claude {
            session_id: "default-session".into(),
            resume_at: None,
        }));

        let mut switched = existing.clone();
        switched.switch_agent(ProviderKind::Claude, Some("claude-work".into()));
        switched.set_live_provider_cursor(Some(ProviderResumeCursor::Claude {
            session_id: "work-session".into(),
            resume_at: None,
        }));

        let mut busy = existing.clone();
        busy.status = SessionStatus::Working;
        merge_stale_session_metadata(&mut busy, switched.clone());
        assert_eq!(busy.account_id, None);
        assert_eq!(
            busy.provider_cursor.as_ref().map(|c| c.native_id()),
            Some("default-session")
        );

        merge_stale_session_metadata(&mut existing, switched);
        assert_eq!(existing.account_id.as_deref(), Some("claude-work"));
        assert_eq!(existing.provider_cursor, None);
        existing.switch_agent(ProviderKind::Claude, None);
        assert_eq!(
            existing.provider_cursor.as_ref().map(|c| c.native_id()),
            Some("default-session")
        );
    }

    #[test]
    fn client_projection_cannot_replace_a_daemon_checkpoint() {
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        existing.begin_turn("change it");
        existing.finish_active_turn(crate::model::TurnStatus::Completed);
        let checkpoint = Checkpoint {
            turn_count: 1,
            git_ref: "refs/waku/canonical".into(),
            status: CheckpointStatus::Ready,
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            created_at: 1,
        };
        existing.turns[0].checkpoint = Some(checkpoint.clone());

        let mut incoming = existing.clone();
        incoming.turns[0].checkpoint = Some(Checkpoint {
            git_ref: "refs/waku/stale-client".into(),
            ..checkpoint.clone()
        });
        preserve_daemon_checkpoints(&existing, &mut incoming);

        assert_eq!(incoming.turns[0].checkpoint.as_ref(), Some(&checkpoint));
    }

    #[test]
    fn response_fork_titles_follow_one_numbered_sequence() {
        assert_eq!(
            next_response_fork_title("Fix the bug", ["Fix the bug"]),
            "Fix the bug (2)"
        );
        assert_eq!(
            next_response_fork_title(
                "Fix the bug (2)",
                ["Fix the bug", "Fix the bug (2)", "Fix the bug (4)"]
            ),
            "Fix the bug (5)"
        );
        assert_eq!(
            next_response_fork_title("Plan (2026)", ["Plan (2026)"]),
            "Plan (2026) (2)"
        );
    }

    #[test]
    fn message_rewind_requires_a_settled_user_turn_and_provider_cursor() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.begin_turn("change it");
        session.mark_active_turn_provider_started();
        session.provider_cursor = Some(ProviderResumeCursor::Codex {
            thread_id: "thread".into(),
        });
        session.finish_active_turn(crate::model::TurnStatus::Completed);

        assert!(validate_message_rewind(&session, 1).is_ok());

        let mut busy = session.clone();
        busy.status = SessionStatus::Working;
        assert!(validate_message_rewind(&busy, 1).is_err());

        let mut missing_cursor = session.clone();
        missing_cursor.provider_cursor = None;
        assert!(validate_message_rewind(&missing_cursor, 1).is_err());

        let mut missing_message = session;
        missing_message.messages.clear();
        assert!(validate_message_rewind(&missing_message, 1).is_err());
    }

    #[test]
    fn wire_event_round_trip_preserves_ordered_delta_payload() {
        let wire = event_to_wire(DriverEvent::TextDelta("hello".into())).unwrap();
        assert_eq!(wire.kind, "textDelta");
        assert!(matches!(
            event_from_wire(wire).unwrap(),
            DriverEvent::TextDelta(text) if text == "hello"
        ));
    }

    #[test]
    fn wire_event_round_trip_preserves_prompt_submission_identity() {
        let turn_id = Uuid::new_v4();
        let message_id = Uuid::new_v4();
        let wire = event_to_wire(DriverEvent::PromptSubmitted {
            message: "ship it".into(),
            turn_id,
            message_id,
            handoff: None,
        })
        .unwrap();
        assert_eq!(wire.kind, "promptSubmitted");
        assert_eq!(wire.payload["message"], "ship it");
        assert_eq!(wire.payload["turnId"], turn_id.to_string());
        assert_eq!(wire.payload["messageId"], message_id.to_string());
        assert!(wire.payload.get("handoff").is_none());
        assert!(matches!(
            event_from_wire(wire).unwrap(),
            DriverEvent::PromptSubmitted { message, turn_id: decoded_turn, message_id: decoded_message, handoff: None }
                if message == "ship it" && decoded_turn == turn_id && decoded_message == message_id
        ));
    }

    #[test]
    fn wire_event_round_trip_preserves_prompt_submission_handoff() {
        let handoff = SubmittedHandoff {
            notice_id: Uuid::new_v4(),
            notice: "Continuing with Claude".into(),
            record: crate::model::TurnHandoff {
                from_providers: vec![ProviderKind::Codex],
                covered_turns: 2,
                omitted_turns: 1,
                resumed: false,
            },
        };
        let wire = event_to_wire(DriverEvent::PromptSubmitted {
            message: "ship it".into(),
            turn_id: Uuid::new_v4(),
            message_id: Uuid::new_v4(),
            handoff: Some(handoff.clone()),
        })
        .unwrap();
        assert_eq!(wire.payload["handoff"]["notice"], "Continuing with Claude");
        let DriverEvent::PromptSubmitted {
            handoff: decoded, ..
        } = event_from_wire(wire).unwrap()
        else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(decoded, Some(handoff));
    }

    #[test]
    fn prompt_submission_broadcasts_clean_transcript_message_instead_of_handoff_prompt() {
        let turn_id = Uuid::new_v4();
        let message_id = Uuid::new_v4();
        let command = Command::Prompt {
            prompt: "<handoff_context>\nprior codex turn\n</handoff_context>\n\n---\n\nship it"
                .into(),
            transcript_message: Some("ship it".into()),
            turn_id: Some(turn_id),
            message_id: Some(message_id),
            handoff: None,
        };
        let wire = prompt_submission_wire_event(&command)
            .unwrap()
            .expect("prompt command emits submission event");
        assert_eq!(wire.kind, "promptSubmitted");
        assert_eq!(wire.payload["message"], "ship it");
        assert!(matches!(
            event_from_wire(wire).unwrap(),
            DriverEvent::PromptSubmitted {
                message,
                turn_id: decoded_turn,
                message_id: decoded_message,
                ..
            } if message == "ship it" && decoded_turn == turn_id && decoded_message == message_id
        ));

        let fallback = Command::Prompt {
            prompt: "legacy client prompt".into(),
            transcript_message: None,
            turn_id: Some(turn_id),
            message_id: Some(message_id),
            handoff: None,
        };
        let fallback_wire = prompt_submission_wire_event(&fallback)
            .unwrap()
            .expect("prompt command emits submission event");
        assert_eq!(fallback_wire.payload["message"], "legacy client prompt");
    }

    #[test]
    fn stale_metadata_merge_switches_provider_cleanly_when_idle_and_preserves_busy_provider() {
        let mut idle = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        idle.updated_at = 10;
        idle.provider_cursor = Some(ProviderResumeCursor::Codex {
            thread_id: "codex-1".into(),
        });

        let mut incoming = idle.clone();
        incoming.switch_provider(ProviderKind::Claude);
        incoming.model = Some("claude-sonnet-4-6".into());
        incoming.updated_at = 20;

        merge_stale_session_metadata(&mut idle, incoming.clone());
        assert_eq!(idle.provider, ProviderKind::Claude);
        assert!(idle.provider_cursor.is_none());
        assert_eq!(
            idle.provider_cursors,
            vec![ProviderResumeCursor::Codex {
                thread_id: "codex-1".into(),
            }]
        );
        assert_eq!(idle.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(idle.updated_at, 20);

        let mut busy = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        busy.status = SessionStatus::Working;
        busy.updated_at = 10;
        busy.model = Some("gpt-5.5".into());
        busy.provider_cursor = Some(ProviderResumeCursor::Codex {
            thread_id: "codex-live".into(),
        });
        merge_stale_session_metadata(&mut busy, incoming);
        assert_eq!(busy.provider, ProviderKind::Codex);
        // Claude's model must not ride onto the Codex task that kept running.
        assert_eq!(busy.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            busy.provider_cursor,
            Some(ProviderResumeCursor::Codex {
                thread_id: "codex-live".into(),
            })
        );
    }

    #[test]
    fn mixed_agent_fork_and_rewind_with_zero_current_agent_turns_reset_cursor_cleanly() {
        let dir = std::env::temp_dir().join(format!("waku-daemon-test-{}", Uuid::new_v4()));
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let store = StateStore::daemon(dir.join("state.db"));
        let mut state = PersistedState::fresh(workspace.clone());
        let session_id = state.sessions[0].id;
        {
            let session = state.session_mut(session_id).unwrap();
            session.provider = ProviderKind::Codex;
            session.begin_turn("codex turn 1");
            session.mark_active_turn_provider_started();
            session.push_message(crate::model::MessageRole::Assistant, "codex answer 1");
            session.finish_active_turn(crate::model::TurnStatus::Completed);
            session.provider_cursor = Some(ProviderResumeCursor::Codex {
                thread_id: "codex-thread-1".into(),
            });

            // Switch to OpenCode: 0 OpenCode turns exist at turn 1, and cursor is None.
            session.switch_provider(ProviderKind::OpenCode);
        }
        store.save(&mut state).unwrap();

        let settings = DaemonSettingsStore::open(dir.join("daemon-settings.json")).unwrap();
        let backend = WakuBackend::new(settings, store).unwrap();

        // Forking at turn 1 while OpenCode has 0 turns must validate and succeed
        // without requiring an OpenCode cursor or binary.
        let (forked, _) = backend.fork_session_from_response(session_id, 1).unwrap();
        assert_eq!(forked.provider, ProviderKind::OpenCode);
        assert!(forked.provider_cursor.is_none());
        assert!(forked.provider_cursors.is_empty());
        assert_eq!(forked.turns.len(), 1);
        assert_eq!(forked.turn_provider(&forked.turns[0]), ProviderKind::Codex);

        // Now simulate OpenCode having run turn 2, then rewinding turn 2 back to
        // retained_turn_count = 1 where OpenCode has provider_turn_count = 0.
        let mut with_second_turn = forked.clone();
        with_second_turn.provider_cursor = Some(ProviderResumeCursor::OpenCode {
            session_id: "ses_2".into(),
            directory: None,
        });
        with_second_turn.begin_turn("opencode turn 2");
        with_second_turn.mark_active_turn_provider_started();
        with_second_turn.finish_active_turn(crate::model::TurnStatus::Completed);

        assert!(validate_message_rewind(&with_second_turn, 2).is_ok());
        let rollback_turns = with_second_turn.provider_turns_after(1);
        let provider_turn_count = with_second_turn.provider_turns_through(1);
        assert_eq!(rollback_turns, 1);
        assert_eq!(provider_turn_count, 0);

        let (cursor, message_ids, reset_native_session) = backend
            .rewind_provider_response(
                &with_second_turn,
                &workspace,
                None,
                1,
                rollback_turns,
                provider_turn_count,
                with_second_turn.provider_resume_at_through(1),
            )
            .unwrap();
        assert!(cursor.is_none());
        assert!(message_ids.is_empty());
        assert!(reset_native_session);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_task_state_skeleton_pin_archive_rename_preserves_messages_and_details() {
        let dir = std::env::temp_dir().join(format!("waku-daemon-skeleton-{}", Uuid::new_v4()));
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let db_path = dir.join("state.db");
        let settings_path = dir.join("daemon-settings.json");
        let store = StateStore::daemon(db_path.clone());
        let mut state = PersistedState::fresh(workspace.clone());
        let project = state.projects[0].clone();
        let session_id = state.sessions[0].id;
        {
            let session = state.session_mut(session_id).unwrap();
            session.provider = ProviderKind::Claude;
            session.account_id = Some("claude-work".into());
            session.reasoning_effort = Some("high".into());
            session.workspace = crate::model::SessionWorkspace::Worktree {
                path: dir.join("worktree"),
                branch: "feat/keep".into(),
            };
            session.begin_turn("Important user prompt");
            session.mark_active_turn_provider_started();
            session.push_message(crate::model::MessageRole::Assistant, "Important answer");
            session.finish_active_turn(crate::model::TurnStatus::Completed);
            session.provider_cursor = Some(ProviderResumeCursor::Claude {
                session_id: "claude-native-1".into(),
                resume_at: Some("msg-1".into()),
            });
        }
        store.save(&mut state).unwrap();

        // Case 1: Daemon restarted (holds skeleton in memory) and receives a
        // wire-serialized mutated skeleton (pin + archive + rename).
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(settings_path.clone()).unwrap(),
            StateStore::daemon(db_path.clone()),
        )
        .unwrap();
        let mut skeleton = {
            let loaded = backend.task_state.lock();
            assert!(!loaded.sessions[0].detail_loaded);
            loaded.sessions[0].list_projection()
        };
        assert!(skeleton.set_pinned(true));
        assert!(skeleton.set_archived(true));
        assert!(skeleton.set_title("Renamed via skeleton"));

        // Round-trip through JSON to verify the wire preserves `detail_loaded = false`.
        let wire = serde_json::to_vec(&Command::SaveTaskState {
            projects: vec![project.clone()],
            live_session_ids: vec![session_id],
            sessions: vec![skeleton],
        })
        .unwrap();
        let Command::SaveTaskState {
            projects, sessions, ..
        } = serde_json::from_slice(&wire).unwrap()
        else {
            panic!("expected SaveTaskState");
        };
        assert!(!sessions[0].detail_loaded);

        let saved = backend.save_task_state(projects, sessions).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].title, "Renamed via skeleton");
        assert!(saved[0].pinned);
        assert!(saved[0].archived);

        // Hydrate from SQLite and verify every detail and message survived.
        {
            let mut task_state = backend.task_state.lock();
            backend
                .task_store
                .hydrate(&mut task_state.sessions[0])
                .unwrap();
            let hydrated = &task_state.sessions[0];
            assert!(hydrated.detail_loaded);
            assert_eq!(hydrated.title, "Renamed via skeleton");
            assert!(hydrated.pinned);
            assert!(hydrated.archived);
            assert_eq!(hydrated.messages.len(), 2);
            assert_eq!(hydrated.messages[0].content, "Important user prompt");
            assert_eq!(hydrated.messages[1].content, "Important answer");
            assert_eq!(hydrated.turns.len(), 1);
            assert_eq!(hydrated.account_id.as_deref(), Some("claude-work"));
            assert_eq!(hydrated.reasoning_effort.as_deref(), Some("high"));
            assert_eq!(
                hydrated.workspace,
                crate::model::SessionWorkspace::Worktree {
                    path: dir.join("worktree"),
                    branch: "feat/keep".into(),
                }
            );
            assert_eq!(
                hydrated.provider_cursor,
                Some(ProviderResumeCursor::Claude {
                    session_id: "claude-native-1".into(),
                    resume_at: Some("msg-1".into()),
                })
            );
        }

        // Case 2: Daemon holds the hydrated session in memory while another
        // client sends an unhydrated skeleton mutation (unpin + rename).
        let mut second_skeleton = {
            let loaded = backend.task_state.lock();
            assert!(loaded.sessions[0].detail_loaded);
            loaded.sessions[0].list_projection()
        };
        assert!(second_skeleton.set_pinned(false));
        assert!(second_skeleton.set_title("Renamed while daemon hydrated"));
        let wire = serde_json::to_vec(&second_skeleton).unwrap();
        let decoded_skeleton: AgentSession = serde_json::from_slice(&wire).unwrap();
        assert!(!decoded_skeleton.detail_loaded);

        backend
            .save_task_state(vec![project], vec![decoded_skeleton])
            .unwrap();

        // Reload fresh from SQLite and verify nothing was lost.
        let fresh_store = StateStore::daemon(db_path);
        let mut fresh_state = fresh_store.load().unwrap();
        assert_eq!(fresh_state.sessions.len(), 1);
        assert_eq!(
            fresh_state.sessions[0].title,
            "Renamed while daemon hydrated"
        );
        assert!(!fresh_state.sessions[0].pinned);
        assert!(fresh_state.sessions[0].archived);
        fresh_store.hydrate(&mut fresh_state.sessions[0]).unwrap();
        let reloaded = &fresh_state.sessions[0];
        assert_eq!(reloaded.messages.len(), 2);
        assert_eq!(reloaded.messages[0].content, "Important user prompt");
        assert_eq!(reloaded.messages[1].content, "Important answer");
        assert_eq!(reloaded.turns.len(), 1);
        assert_eq!(reloaded.account_id.as_deref(), Some("claude-work"));
        assert_eq!(reloaded.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            reloaded.workspace,
            crate::model::SessionWorkspace::Worktree {
                path: dir.join("worktree"),
                branch: "feat/keep".into(),
            }
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_config_dir_validation_allows_none_and_registered_accounts_only() {
        let dir = std::env::temp_dir().join(format!("waku-daemon-cfgdir-{}", Uuid::new_v4()));
        let existing_account = dir.join("claude-work");
        let pending_account = dir.join("codex-pending");
        let outside_dir = dir.join("outside-secret");
        std::fs::create_dir_all(&existing_account).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        let settings_store = DaemonSettingsStore::open(dir.join("settings.json")).unwrap();
        let mut settings = settings_store.get();
        settings.provider_accounts = vec![
            waku_protocol::ProviderAccount {
                id: "claude-1".into(),
                provider: ProviderKind::Claude,
                label: "Work".into(),
                config_dir: existing_account.to_string_lossy().into_owned(),
                binary_override: None,
            },
            waku_protocol::ProviderAccount {
                id: "codex-1".into(),
                provider: ProviderKind::Codex,
                label: "Pending".into(),
                config_dir: pending_account.to_string_lossy().into_owned(),
                binary_override: None,
            },
        ];
        settings_store.replace(settings).unwrap();

        let backend =
            WakuBackend::new(settings_store, StateStore::daemon(dir.join("state.db"))).unwrap();

        // None and whitespace-only map to default account (Ok(None)).
        assert!(
            backend
                .validate_client_config_dir(ProviderKind::Claude, None)
                .unwrap()
                .is_none()
        );
        assert!(
            backend
                .validate_client_config_dir(ProviderKind::Claude, Some("   "))
                .unwrap()
                .is_none()
        );

        // Registered existing path (including trailing slash) succeeds.
        let with_slash = format!("{}/", existing_account.to_string_lossy());
        let validated_existing = backend
            .validate_client_config_dir(ProviderKind::Claude, Some(&with_slash))
            .unwrap()
            .expect("registered existing account accepted");
        assert_eq!(validated_existing.account_id, "claude-1");
        assert_eq!(validated_existing.resolved, existing_account);

        // Registered not-yet-created path succeeds.
        let validated_pending = backend
            .validate_client_config_dir(
                ProviderKind::Codex,
                Some(&pending_account.to_string_lossy()),
            )
            .unwrap()
            .expect("registered pending account accepted");
        assert_eq!(validated_pending.account_id, "codex-1");
        assert_eq!(validated_pending.resolved, pending_account);

        // Unregistered host paths are rejected.
        assert!(
            backend
                .validate_client_config_dir(
                    ProviderKind::Claude,
                    Some(&outside_dir.to_string_lossy()),
                )
                .is_err()
        );

        // Cross-provider account paths are rejected.
        assert!(
            backend
                .validate_client_config_dir(
                    ProviderKind::Codex,
                    Some(&existing_account.to_string_lossy()),
                )
                .is_err()
        );

        // Relative and `..` traversal paths are rejected.
        assert!(
            backend
                .validate_client_config_dir(ProviderKind::Claude, Some("../outside-secret"))
                .is_err()
        );
        let traversal = format!("{}/../outside-secret", pending_account.to_string_lossy());
        assert!(
            backend
                .validate_client_config_dir(ProviderKind::Codex, Some(&traversal))
                .is_err()
        );

        // Workspace and fork request validators enforce the same policy.
        assert!(
            backend
                .validate_fork_provider_session_request(ProviderSessionForkRequest::Claude {
                    session_id: "s1".into(),
                    resume_at: None,
                    turn_count: 1,
                    title: "fork".into(),
                    config_dir: Some(outside_dir.clone()),
                })
                .is_err()
        );
        assert!(
            backend
                .validate_fork_provider_session_request(ProviderSessionForkRequest::Claude {
                    session_id: "s1".into(),
                    resume_at: None,
                    turn_count: 1,
                    title: "fork".into(),
                    config_dir: Some(existing_account.clone()),
                })
                .is_ok()
        );
        assert!(
            backend
                .validate_workspace_operation(WorkspaceOperation::DiscoverSlashCommands {
                    provider: ProviderKind::Claude,
                    project_root: dir.clone(),
                    binary_override: None,
                    config_dir: Some(outside_dir.to_string_lossy().into_owned()),
                })
                .is_err()
        );
        assert!(
            backend
                .validate_workspace_operation(WorkspaceOperation::GenerateCommitMessage {
                    cwd: dir.clone(),
                    include_unstaged: false,
                    invocation: waku_protocol::git::AgentInvocation {
                        provider: ProviderKind::Claude,
                        binary: PathBuf::from("claude"),
                        model: None,
                        reasoning_effort: None,
                        config_dir: Some(outside_dir.to_string_lossy().into_owned()),
                    },
                })
                .is_err()
        );
        assert!(
            backend
                .validate_workspace_operation(WorkspaceOperation::DiscoverSlashCommands {
                    provider: ProviderKind::Claude,
                    project_root: dir.clone(),
                    binary_override: None,
                    config_dir: Some(existing_account.to_string_lossy().into_owned()),
                })
                .is_ok()
        );
        assert!(
            backend
                .validate_workspace_operation(WorkspaceOperation::ReadAntigravitySwarm {
                    session_id: "b0f6aa6a-fe94-4446-91d5-814e6342a5de".into(),
                    config_dir: None,
                })
                .is_ok()
        );
        for invalid in ["", "../escape", "a/b", "a\\b", "has space"] {
            assert!(
                backend
                    .validate_workspace_operation(WorkspaceOperation::ReadAntigravitySwarm {
                        session_id: invalid.into(),
                        config_dir: None,
                    })
                    .is_err(),
                "session_id {invalid:?} should be rejected"
            );
        }
        assert!(
            backend
                .validate_workspace_operation(WorkspaceOperation::ReadAntigravitySwarm {
                    session_id: "b0f6aa6a-fe94-4446-91d5-814e6342a5de".into(),
                    config_dir: Some(outside_dir.to_string_lossy().into_owned()),
                })
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    struct RecordingDriverControl {
        fork_calls: Arc<std::sync::atomic::AtomicUsize>,
        rollback_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::driver::DriverControl for RecordingDriverControl {
        fn prompt(&self, _prompt: String) {}
        fn cancel(&self) {}
        fn respond(&self, _request_id: String, _option_id: String) {}
        fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
            self.rollback_calls.fetch_add(1, Ordering::AcqRel);
            Ok(Some(ProviderResumeCursor::Codex {
                thread_id: "rolled-back".into(),
            }))
        }
        fn fork(&self, _turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
            self.fork_calls.fetch_add(1, Ordering::AcqRel);
            Ok(ProviderResumeCursor::Codex {
                thread_id: "forked".into(),
            })
        }
    }

    #[test]
    fn live_driver_is_reused_for_fork_and_rollback_only_when_provider_and_account_match() {
        let dir =
            std::env::temp_dir().join(format!("waku-daemon-live-driver-match-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let backend = WakuBackend::new(
            DaemonSettingsStore::open(dir.join("settings.json")).unwrap(),
            StateStore::daemon(dir.join("state.db")),
        )
        .unwrap();

        let fork_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rollback_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let driver = DriverHandle::from_control(Arc::new(RecordingDriverControl {
            fork_calls: fork_calls.clone(),
            rollback_calls: rollback_calls.clone(),
        }));

        let mut source = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        source.account_id = Some("codex-work".into());
        backend.insert_test_session(
            source.id,
            Uuid::new_v4(),
            ProviderKind::Codex,
            Some("codex-work".into()),
            driver,
        );

        // Matching (provider, account_id) reuses the live driver.
        assert!(backend.matching_live_driver(&source).is_some());
        let forked = backend
            .fork_response_with_driver(&source, &dir, 1)
            .expect("matching live driver handles fork");
        assert_eq!(forked.native_id(), "forked");
        assert_eq!(fork_calls.load(Ordering::Acquire), 1);

        let rewound = backend
            .rollback_response_with_driver(&source, &dir, Path::new("missing-binary"), 1)
            .expect("matching live driver handles rollback");
        assert_eq!(rewound.as_ref().map(|c| c.native_id()), Some("rolled-back"));
        assert_eq!(rollback_calls.load(Ordering::Acquire), 1);

        // Different account_id must NOT reuse the live driver.
        let mut other_account = source.clone();
        other_account.account_id = None;
        assert!(backend.matching_live_driver(&other_account).is_none());
        assert!(
            backend
                .fork_response_with_driver(&other_account, &dir, 1)
                .is_err()
        );
        assert!(
            backend
                .rollback_response_with_driver(&other_account, &dir, &dir.join("missing-codex"), 1)
                .is_err()
        );
        assert_eq!(fork_calls.load(Ordering::Acquire), 1);
        assert_eq!(rollback_calls.load(Ordering::Acquire), 1);

        // Different provider must NOT reuse the live driver.
        let mut other_provider = source.clone();
        other_provider.provider = ProviderKind::DeepSeek;
        other_provider.account_id = Some("codex-work".into());
        assert!(backend.matching_live_driver(&other_provider).is_none());
        assert!(
            backend
                .fork_response_with_driver(&other_provider, &dir, 1)
                .is_err()
        );
        assert!(
            backend
                .rollback_response_with_driver(&other_provider, &dir, &dir.join("missing-dsh"), 1)
                .is_err()
        );
        assert_eq!(fork_calls.load(Ordering::Acquire), 1);
        assert_eq!(rollback_calls.load(Ordering::Acquire), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn antigravity_fork_provider_session_seeds_retained_turns() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Antigravity);
        session.begin_turn("first question");
        session.mark_active_turn_provider_started();
        session.push_message(crate::model::MessageRole::Assistant, "first answer");
        session.finish_active_turn(crate::model::TurnStatus::Completed);
        session.begin_turn("second question");
        session.mark_active_turn_provider_started();
        session.push_message(crate::model::MessageRole::Assistant, "second answer");
        session.finish_active_turn(crate::model::TurnStatus::Completed);

        let forked = fork_provider_session(ProviderSessionForkRequest::Antigravity {
            source: session,
            turn_count: 1,
        })
        .expect("antigravity fork should succeed");

        match forked.cursor {
            ProviderResumeCursor::Antigravity {
                session_id,
                fork_context: Some(context),
            } => {
                assert!(session_id.is_empty());
                assert!(context.contains("first question"));
                assert!(context.contains("first answer"));
                assert!(!context.contains("second question"));
                assert!(!context.contains("second answer"));
            }
            other => panic!("unexpected cursor: {other:?}"),
        }
    }

    #[test]
    fn probe_provider_bypasses_default_cache_when_config_dir_is_set() {
        fn fake_cached(provider: ProviderKind, _override: Option<&str>) -> ProviderProbe {
            ProviderProbe {
                provider,
                installed: true,
                path: Some(PathBuf::from("agy")),
                models: vec![waku_protocol::model::ProviderModel::new(
                    "default-cached-model",
                    "Default Cached",
                )],
                agent_presets: Vec::new(),
                discovery_error: None,
            }
        }
        fn fake_fresh(provider: ProviderKind, _override: Option<&str>) -> ProviderProbe {
            ProviderProbe {
                provider,
                installed: true,
                path: Some(PathBuf::from("agy")),
                models: Vec::new(),
                agent_presets: Vec::new(),
                discovery_error: None,
            }
        }

        let default_probe = initial_provider_probe_with(
            ProviderKind::Antigravity,
            None,
            false,
            false,
            false,
            fake_cached,
            fake_fresh,
        );
        assert_eq!(default_probe.models.len(), 1);
        assert_eq!(default_probe.models[0].id, "default-cached-model");

        let isolated_probe = initial_provider_probe_with(
            ProviderKind::Antigravity,
            None,
            true,
            false,
            false,
            fake_cached,
            fake_fresh,
        );
        assert!(isolated_probe.models.is_empty());

        let dir = std::env::temp_dir().join(format!("waku-daemon-probe-{}", Uuid::new_v4()));
        let agy_work = dir.join("agy-work");
        std::fs::create_dir_all(&agy_work).unwrap();
        let settings_store = DaemonSettingsStore::open(dir.join("settings.json")).unwrap();
        let mut settings = settings_store.get();
        settings.provider_accounts = vec![waku_protocol::ProviderAccount {
            id: "agy-1".into(),
            provider: ProviderKind::Antigravity,
            label: "Work".into(),
            config_dir: agy_work.to_string_lossy().into_owned(),
            binary_override: None,
        }];
        settings_store.replace(settings).unwrap();
        let backend =
            WakuBackend::new(settings_store, StateStore::daemon(dir.join("state.db"))).unwrap();
        let (probe, version) = backend
            .probe_provider(
                ProviderKind::Antigravity,
                None,
                Some(&agy_work.to_string_lossy()),
                false,
                false,
            )
            .expect("probe provider with registered config_dir succeeds");
        assert!(version.is_none());
        assert_eq!(
            probe.models,
            crate::model_catalog::fallback_models(ProviderKind::Antigravity)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_skills_discovers_antigravity_account_config_dir_skills() {
        let dir = std::env::temp_dir().join(format!("waku-daemon-skills-{}", Uuid::new_v4()));
        let agy_work = dir.join("agy-work");
        let skill_name = format!("isolated-agy-skill-{}", Uuid::new_v4());
        let skill_dir = agy_work.join("skills").join(&skill_name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {skill_name}\ndescription: Isolated account skill\n---\nSteps"),
        )
        .unwrap();

        let settings_store = DaemonSettingsStore::open(dir.join("settings.json")).unwrap();
        let mut settings = settings_store.get();
        settings.provider_accounts = vec![waku_protocol::ProviderAccount {
            id: "agy-1".into(),
            provider: ProviderKind::Antigravity,
            label: "Work".into(),
            config_dir: agy_work.to_string_lossy().into_owned(),
            binary_override: None,
        }];
        settings_store.replace(settings).unwrap();

        let backend =
            WakuBackend::new(settings_store, StateStore::daemon(dir.join("state.db"))).unwrap();
        let catalog = backend.load_skills_catalog(&[]);
        assert!(
            catalog.skills.iter().any(|skill| skill.name == skill_name),
            "isolated Antigravity skill {skill_name} missing from catalog"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claim_queued_message_allows_single_winner_and_blocks_stale_resurrection() {
        let dir = std::env::temp_dir().join(format!("waku-daemon-claim-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(dir.join("settings.json")).unwrap(),
            StateStore::daemon(dir.join("state.db")),
        )
        .unwrap();
        let state = PersistedState::fresh(dir.clone());
        let project = state.projects[0].clone();
        let mut session = state.sessions[0].clone();
        let queued = crate::model::QueuedMessage::new("Run follow-up");
        let queued_id = queued.id;
        session.queued_messages.push(queued.clone());
        let saved_with_queue = backend
            .save_task_state(vec![project.clone()], vec![session.clone()])
            .unwrap();
        assert_eq!(saved_with_queue[0].queued_messages.len(), 1);

        let first = backend
            .handle(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: session.id,
                    runtime_id: Uuid::nil(),
                    command: Command::ClaimQueuedMessage {
                        message_id: queued_id,
                    },
                },
                EventSink::test_sink(session.id, Uuid::nil()),
            )
            .unwrap();
        assert!(matches!(
            first,
            ResponsePayload::QueuedMessageClaimed { claimed: true }
        ));

        let second = backend
            .handle(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: session.id,
                    runtime_id: Uuid::nil(),
                    command: Command::ClaimQueuedMessage {
                        message_id: queued_id,
                    },
                },
                EventSink::test_sink(session.id, Uuid::nil()),
            )
            .unwrap();
        assert!(matches!(
            second,
            ResponsePayload::QueuedMessageClaimed { claimed: false }
        ));

        // A stale client with an older snapshot re-saving the retired queue item cannot resurrect it.
        let resurrected = backend
            .save_task_state(vec![project], vec![session])
            .unwrap();
        assert!(
            resurrected[0].queued_messages.is_empty(),
            "retired queued message was resurrected by stale SaveTaskState"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_task_state_rejects_stale_lower_state_version_overwrite() {
        let dir = std::env::temp_dir().join(format!("waku-daemon-ver-{}", Uuid::new_v4()));
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let git = |args: &[&str]| {
            let status = crate::command_env::plain_command("git")
                .args(args)
                .current_dir(&workspace)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&status.stderr)
            );
        };
        git(&["init"]);
        git(&["config", "user.name", "Waku Test"]);
        git(&["config", "user.email", "test@waku.example"]);
        std::fs::write(workspace.join("note.txt"), "initial\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);

        let backend = WakuBackend::new(
            DaemonSettingsStore::open(dir.join("settings.json")).unwrap(),
            StateStore::daemon(dir.join("state.db")),
        )
        .unwrap();
        let state = PersistedState::fresh(workspace.clone());
        let project = state.projects[0].clone();
        let mut session = state.sessions[0].clone();
        session.begin_turn("Turn 1");
        session.finish_active_turn(crate::model::TurnStatus::Completed);
        session.begin_turn("Turn 2");
        session.finish_active_turn(crate::model::TurnStatus::Completed);
        let stale_snapshot = session.clone();

        crate::checkpoint::capture_ref(
            &workspace,
            &crate::checkpoint::turn_start_ref(session.id, 2),
        )
        .unwrap();
        backend
            .save_task_state(vec![project.clone()], vec![session])
            .unwrap();
        // Rewind to before turn 2, truncating turn 2 and bumping server state_version.
        let rewound = backend
            .rewind_session_to_message(stale_snapshot.id, 2)
            .unwrap()
            .0;
        assert!(rewound.state_version > stale_snapshot.state_version);
        assert_eq!(rewound.turns.len(), 1);

        // Stale save carrying state_version 0 and 2 turns must NOT overwrite turn truncation.
        let merged = backend
            .save_task_state(vec![project], vec![stale_snapshot])
            .unwrap();
        assert_eq!(merged[0].state_version, rewound.state_version);
        assert_eq!(merged[0].turns.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn has_live_work_ignores_idle_resident_driver_and_reap_evicts_it() {
        let dir = std::env::temp_dir().join(format!("waku-daemon-reap-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(dir.join("settings.json")).unwrap(),
            StateStore::daemon(dir.join("state.db")),
        )
        .unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        backend.insert_test_session(
            session_id,
            runtime_id,
            ProviderKind::Codex,
            None,
            DriverHandle::from_control(Arc::new(RecordingDriverControl {
                fork_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                rollback_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })),
        );
        assert!(!backend.has_live_work());
        // With max_idle = 0, an unbusy idle resident driver is reaped.
        let reaped = backend.reap_idle_runtimes(Duration::ZERO);
        assert_eq!(reaped, vec![(session_id, runtime_id)]);
        assert!(!backend.is_runtime_live(session_id, runtime_id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_terminal_respawns_exited_terminal_with_changed_cwd_or_runtime() {
        let dir = std::env::temp_dir().join(format!("waku-daemon-term-t2-{}", Uuid::new_v4()));
        let cwd1 = dir.join("one");
        let cwd2 = dir.join("two");
        std::fs::create_dir_all(&cwd1).unwrap();
        std::fs::create_dir_all(&cwd2).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(dir.join("settings.json")).unwrap(),
            StateStore::daemon(dir.join("state.db")),
        )
        .unwrap();
        let term_id = Uuid::new_v4();
        let rt1 = Uuid::new_v4();
        let rt2 = Uuid::new_v4();

        backend
            .handle(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: term_id,
                    runtime_id: rt1,
                    command: Command::OpenTerminal {
                        cwd: cwd1.clone(),
                        cols: 80,
                        rows: 24,
                    },
                },
                EventSink::test_sink(term_id, rt1),
            )
            .unwrap();
        // Live terminal with different cwd is rejected.
        assert!(
            backend
                .handle(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id: term_id,
                        runtime_id: rt2,
                        command: Command::OpenTerminal {
                            cwd: cwd2.clone(),
                            cols: 80,
                            rows: 24,
                        },
                    },
                    EventSink::test_sink(term_id, rt2),
                )
                .is_err()
        );
        // Exit the shell and verify respawn succeeds with changed cwd and runtime_id.
        let exit_cmd = if cfg!(windows) {
            b"exit 0\r\n".as_slice()
        } else {
            b"exit 0\r".as_slice()
        };
        backend
            .handle(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: term_id,
                    runtime_id: rt1,
                    command: Command::WriteTerminal {
                        data: exit_cmd.to_vec(),
                    },
                },
                EventSink::test_sink(term_id, rt1),
            )
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let ResponsePayload::Terminals { terminals } = backend
                .handle(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id: Uuid::nil(),
                        runtime_id: Uuid::nil(),
                        command: Command::ListTerminals,
                    },
                    EventSink::test_sink(Uuid::nil(), Uuid::nil()),
                )
                .unwrap()
            else {
                unreachable!();
            };
            if terminals
                .iter()
                .find(|t| t.session_id == term_id)
                .is_some_and(|t| t.exited)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        backend
            .handle(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: term_id,
                    runtime_id: rt2,
                    command: Command::OpenTerminal {
                        cwd: cwd2,
                        cols: 80,
                        rows: 24,
                    },
                },
                EventSink::test_sink(term_id, rt2),
            )
            .expect("exited terminal should allow respawn with changed cwd and runtime_id");
        backend
            .handle(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: term_id,
                    runtime_id: rt2,
                    command: Command::CloseTerminal,
                },
                EventSink::test_sink(term_id, rt2),
            )
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
