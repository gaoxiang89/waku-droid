//! Agent Client Protocol transport backed by the official Rust SDK.
//!
//! The SDK owns JSON-RPC framing, request IDs, response routing, cancellation,
//! unknown-method errors, stdio lifetime, and protocol type validation. Waku
//! only adapts typed ACP messages to its provider-neutral [`DriverEvent`]s.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, CreateElicitationRequest,
    CreateElicitationResponse, CreateTerminalRequest, CreateTerminalResponse,
    ElicitationAcceptAction, ElicitationAction, ElicitationCapabilities, ElicitationContentValue,
    ElicitationFormCapabilities, ElicitationMode, ElicitationPropertySchema, EmbeddedResource,
    EmbeddedResourceResource, EnumOption, FileSystemCapabilities, ForkSessionRequest, ImageContent,
    Implementation, InitializeRequest, InitializeResponse, KillTerminalRequest,
    KillTerminalResponse, LoadSessionRequest, McpServer, MultiSelectItems, NewSessionRequest,
    PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse, RequestId,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ResourceLink,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOptions, SessionId,
    SessionModeId, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason, TerminalExitStatus,
    TerminalOutputRequest, TerminalOutputResponse, TextContent, TextResourceContents,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo, ConnectionTo, Handled, LineDirection,
    Responder, UntypedMessage,
};
use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use super::activity;
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityItem, ActivityKind, BackgroundWorkEvent, BackgroundWorkItem, BackgroundWorkKey,
    BackgroundWorkKind, BackgroundWorkStatus, DriverEvent, PermissionOption, ProviderKind,
    ProviderResumeCursor, RuntimeMode, UserInputAnswer, UserInputOption, UserInputQuestion,
    unix_time_millis,
};
use waku_protocol::model_catalog::{
    CursorModelSelection, cursor_suffix_has, normalize_cursor_reasoning_effort,
    resolve_cursor_model,
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum PromptKind {
    Turn,
    Steer,
}

enum CommandMessage {
    Prompt(String),
    Steer(String),
    Cancel,
    RefreshBackgroundWork,
    StopBackgroundWork {
        key: BackgroundWorkKey,
        control_id: String,
    },
    Respond {
        request_id: String,
        option_id: String,
    },
    RespondUserInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    /// `mode_applied` is present only for a live access-mode switch, and
    /// answers whether the agent accepted it.
    Options {
        options: SessionOptions,
        mode_applied: Option<smol::channel::Sender<bool>>,
    },
    Fork {
        turns_to_remove: usize,
        reply: smol::channel::Sender<anyhow::Result<ProviderResumeCursor>>,
    },
    /// The provider no longer has the native session a prompt was sent to;
    /// start a fresh one and resend the prompt. Raised internally by the
    /// prompt callback, never by the handle.
    RecoverLostSession {
        failed_session_id: String,
        text: String,
    },
    Shutdown,
}

/// How long `apply_options` waits for the agent to accept a live access-mode
/// switch before asking the app to restart the session instead.
const MODE_SWITCH_TIMEOUT: Duration = Duration::from_secs(10);
const ACP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const ACP_CANCEL_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
struct AcpTimeouts {
    request: Duration,
    cancel: Duration,
}

impl Default for AcpTimeouts {
    fn default() -> Self {
        Self {
            request: ACP_REQUEST_TIMEOUT,
            cancel: ACP_CANCEL_TIMEOUT,
        }
    }
}

async fn await_acp_request<T>(
    future: impl std::future::Future<Output = agent_client_protocol::Result<T>>,
    timeout: Duration,
    label: &str,
) -> agent_client_protocol::Result<T> {
    smol::future::race(future, async {
        smol::Timer::after(timeout).await;
        Err(agent_client_protocol::util::internal_error(format!(
            "{label} timed out after {}s",
            timeout.as_secs()
        )))
    })
    .await
}

pub struct AcpDriver {
    commands: smol::channel::Sender<CommandMessage>,
    supports_steer: bool,
    mode: Mutex<RuntimeMode>,
    /// Factory Droid (config option) and Antigravity (`session/set_mode`)
    /// switch access mode in place; every other ACP agent restarts to change it.
    switches_mode_live: bool,
    computer_use: Option<super::support::HeadlessComputerUseRuntime>,
}

/// Per-provider launch details. Everything after process launch is ACP.
struct AcpLaunch {
    args: Vec<String>,
    env: Vec<(String, String)>,
}

fn launch_for(
    provider: ProviderKind,
    reasoning_effort: Option<&str>,
    config_dir: Option<&Path>,
) -> anyhow::Result<AcpLaunch> {
    match provider {
        // Antigravity's CLI already carries the user's Google sign-in and its
        // account catalog. `agy acp` is the CLI's own long-lived ACP server;
        // Waku takes the installed route instead of T3 Code's downloaded
        // `antigravity-acp` bridge.
        ProviderKind::Antigravity => {
            let mut args = vec!["acp".into()];
            let env = if let Some(dir) = config_dir {
                args.push("--gemini-dir".into());
                args.push(dir.to_string_lossy().into_owned());
                args.push("--app-data-dir".into());
                args.push(".".into());
                crate::command_env::antigravity_account_env(dir)
            } else {
                Vec::new()
            };
            Ok(AcpLaunch { args, env })
        }
        ProviderKind::Cursor => Ok(AcpLaunch {
            args: vec!["acp".into()],
            env: Vec::new(),
        }),
        ProviderKind::Grok => {
            let mut args = vec!["agent".into()];
            if let Some(effort) = reasoning_effort.filter(|effort| !effort.is_empty()) {
                args.push("--reasoning-effort".into());
                args.push(effort.to_owned());
            }
            args.push("stdio".into());
            Ok(AcpLaunch {
                args,
                env: vec![("GROK_OAUTH2_REFERRER".into(), "waku".into())],
            })
        }
        ProviderKind::Fx => Ok(AcpLaunch {
            args: vec!["acp".into()],
            env: Vec::new(),
        }),
        ProviderKind::Kimi => Ok(AcpLaunch {
            args: vec!["acp".into()],
            env: Vec::new(),
        }),
        // Registry manifest `factory-droid`: native ACP, with auto-update held
        // off so a session is not restarted out from under the client.
        ProviderKind::Droid => Ok(AcpLaunch {
            args: vec!["exec".into(), "--output-format".into(), "acp-daemon".into()],
            env: vec![
                ("DROID_DISABLE_AUTO_UPDATE".into(), "true".into()),
                ("FACTORY_DROID_AUTO_UPDATE_ENABLED".into(), "false".into()),
            ],
        }),
        _ => Err(anyhow!(
            "{} does not speak the Agent Client Protocol",
            provider.display_name()
        )),
    }
}

impl AcpDriver {
    pub fn start(
        provider: ProviderKind,
        options: DriverStartOptions,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        Self::start_with_attachment_roots(provider, options, daemon_attachment_roots(), events)
    }

    pub fn start_with_attachment_roots(
        provider: ProviderKind,
        options: DriverStartOptions,
        attachment_roots: Vec<PathBuf>,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            model,
            reasoning_effort,
            service_tier,
            context_window,
            agent_preset: _,
            computer_use_enabled,
            provider_cursor,
            config_dir,
        } = options;
        let fork_context = match &provider_cursor {
            Some(
                ProviderResumeCursor::Cursor { fork_context, .. }
                | ProviderResumeCursor::Antigravity { fork_context, .. },
            ) => fork_context.clone(),
            _ => None,
        };
        let resume_session_id = match provider_cursor {
            Some(cursor) if cursor.provider() == provider => {
                let id = cursor.native_id();
                (!id.is_empty()).then(|| id.to_owned())
            }
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume {} from a {} cursor",
                    provider.display_name(),
                    cursor.provider().display_name()
                ));
            }
            None => None,
        };

        let launch = launch_for(provider, reasoning_effort.as_deref(), config_dir.as_deref())?;
        let computer_use = computer_use_enabled
            .then(|| super::support::HeadlessComputerUseRuntime::start(provider, events.clone()))
            .transpose()?;
        let acp_mcp_servers = computer_use
            .as_ref()
            .and_then(super::support::HeadlessComputerUseRuntime::acp_mcp_server)
            .into_iter()
            .collect::<Vec<_>>();
        let computer_use_instructions = computer_use
            .as_ref()
            .and_then(super::support::HeadlessComputerUseRuntime::acp_prompt_instructions)
            .map(ToOwned::to_owned);
        let grok_title_home = computer_use
            .as_ref()
            .and_then(super::support::HeadlessComputerUseRuntime::grok_home)
            .map(ToOwned::to_owned);
        let stderr_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let agent = sdk_agent(
            &binary,
            &cwd,
            launch,
            computer_use.as_ref().map(|runtime| &runtime.config),
            stderr_lines.clone(),
        )?;
        let (commands, command_rx) = smol::channel::unbounded();
        let provider_name = provider.display_name();
        let thread_events = events.clone();

        thread::Builder::new()
            .name(format!("waku-{}-acp", provider.id()))
            .spawn(move || {
                if let Err(error) = crate::command_env::unblock_sigchld_for_current_thread() {
                    let _ = thread_events.send(DriverEvent::Error(format!(
                        "{provider_name}: failed to normalize the provider signal mask: {error}"
                    )));
                    let _ = thread_events.send(DriverEvent::ProcessExited);
                    return;
                }
                let result = smol::block_on(run_sdk_connection(
                    agent,
                    provider,
                    cwd,
                    attachment_roots,
                    mode,
                    model,
                    reasoning_effort,
                    service_tier,
                    context_window,
                    resume_session_id,
                    fork_context,
                    config_dir,
                    acp_mcp_servers,
                    computer_use_instructions,
                    grok_title_home,
                    command_rx,
                    thread_events.clone(),
                ));
                if let Err(error) = result {
                    let stderr = super::support::provider_stderr_error(stderr_lines.lock().clone());
                    let detail = stderr.unwrap_or_else(|| error.to_string());
                    let _ = thread_events
                        .send(DriverEvent::Error(format!("{provider_name}: {detail}")));
                }
                let _ = thread_events.send(DriverEvent::ProcessExited);
            })
            .with_context(|| format!("failed to start {provider_name} ACP runtime"))?;

        Ok(Self {
            commands,
            // These reject a second `session/prompt` while one is running, so
            // Waku keeps follow-ups queued until the current turn settles.
            // Antigravity (`agy acp`) accepts mid-turn `session/prompt` and
            // settles the earlier request cleanly before streaming the steer.
            supports_steer: !matches!(provider, ProviderKind::Fx | ProviderKind::Droid),
            mode: Mutex::new(mode),
            switches_mode_live: matches!(provider, ProviderKind::Droid | ProviderKind::Antigravity),
            computer_use,
        })
    }
}

/// An ACP agent process. On Windows the SDK can only kill the direct child,
/// so the whole process tree is tied to a kill-on-close job instead — a
/// wrapper such as a `.cmd` key-injection shim would otherwise leave the real
/// agent running after the session ends.
pub(crate) struct ProviderAgent {
    agent: AcpAgent,
    #[cfg_attr(not(windows), allow(dead_code))]
    stderr_lines: Arc<Mutex<Vec<String>>>,
}

fn record_stderr_line(lines: &Mutex<Vec<String>>, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    let mut lines = lines.lock();
    if lines.len() == 128 {
        lines.remove(0);
    }
    lines.push(line.to_owned());
}

impl agent_client_protocol::ConnectTo<Client> for ProviderAgent {
    #[cfg(not(windows))]
    async fn connect_to(
        self,
        client: impl agent_client_protocol::ConnectTo<Agent>,
    ) -> agent_client_protocol::Result<()> {
        agent_client_protocol::ConnectTo::<Client>::connect_to(self.agent, client).await
    }

    #[cfg(windows)]
    async fn connect_to(
        self,
        client: impl agent_client_protocol::ConnectTo<Agent>,
    ) -> agent_client_protocol::Result<()> {
        use agent_client_protocol::util::internal_error;
        use smol::io::AsyncBufReadExt as _;
        use smol::stream::StreamExt as _;

        const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(1);

        enum First {
            Protocol(agent_client_protocol::Result<()>),
            Exited(std::io::Result<std::process::ExitStatus>),
        }

        let (stdin, stdout, stderr, mut child) = self.agent.spawn_process()?;
        let job = match crate::process_tree::KillOnCloseJob::adopt(child.id()) {
            Ok(job) => Some(job),
            Err(error) => {
                record_stderr_line(
                    &self.stderr_lines,
                    &format!("waku: could not contain the agent process tree: {error}"),
                );
                None
            }
        };

        let stderr_lines = self.stderr_lines.clone();
        let drain_stderr = async move {
            let mut lines = smol::io::BufReader::new(stderr).lines();
            while let Some(Ok(line)) = lines.next().await {
                record_stderr_line(&stderr_lines, &line);
            }
            std::future::pending::<First>().await
        };
        let mut protocol = Box::pin(agent_client_protocol::ConnectTo::<Client>::connect_to(
            agent_client_protocol::ByteStreams::new(stdin, stdout),
            client,
        ));

        let first = smol::future::or(
            smol::future::or(async { First::Protocol((&mut protocol).await) }, async {
                First::Exited(child.status().await)
            }),
            drain_stderr,
        )
        .await;
        let result = match first {
            First::Protocol(result) => result,
            // Let the protocol finish routing what the agent wrote before it
            // exited, as the SDK's own launcher does.
            First::Exited(Ok(status)) if status.success() => {
                smol::future::or(&mut protocol, async {
                    smol::Timer::after(SHUTDOWN_GRACE_PERIOD).await;
                    Ok(())
                })
                .await
            }
            First::Exited(Ok(status)) => {
                Err(internal_error(format!("Process exited with {status}")))
            }
            First::Exited(Err(error)) => Err(internal_error(format!(
                "Failed to wait for process: {error}"
            ))),
        };
        drop(protocol);
        let _ = child.kill();
        drop(job);
        result
    }
}

fn child_search_path_for(program: &Path) -> Option<OsString> {
    let base = crate::command_env::executable_search_path();
    child_search_path_with_base(base.as_deref(), program)
}

fn child_search_path_with_base(base_path: Option<&OsStr>, program: &Path) -> Option<OsString> {
    let mut directories: Vec<PathBuf> = base_path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .collect();
    if let Some(parent) = program
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        directories.push(parent.to_path_buf());
    }
    if directories.is_empty() {
        return None;
    }
    let mut seen = HashSet::new();
    #[cfg(windows)]
    directories.retain(|dir| seen.insert(dir.as_os_str().to_string_lossy().to_lowercase()));
    #[cfg(not(windows))]
    directories.retain(|dir| seen.insert(dir.clone()));
    std::env::join_paths(directories).ok()
}

fn sdk_agent_environment(
    binary: &Path,
    shell_env: Vec<(OsString, OsString)>,
    base_search_path: Option<&OsStr>,
    mut launch_env: Vec<(String, String)>,
    computer_env: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut environment = shell_env
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect::<Vec<_>>();
    if let Some(search_path) = child_search_path_with_base(base_search_path, binary) {
        // Remove any prior PATH entry (case-insensitively on Windows) so
        // `AcpAgentConfig`'s BTreeMap cannot keep a stale `Path` entry that
        // sorts after and overrides our merged `PATH`.
        environment.retain(|(name, _)| {
            #[cfg(windows)]
            {
                !name.eq_ignore_ascii_case("PATH")
            }
            #[cfg(not(windows))]
            {
                name != "PATH"
            }
        });
        environment.push((
            "PATH".to_owned(),
            search_path.to_string_lossy().into_owned(),
        ));
    }
    environment.append(&mut launch_env);
    environment.extend(computer_env);
    environment
}

fn sdk_agent(
    binary: &Path,
    cwd: &Path,
    mut launch: AcpLaunch,
    computer_use: Option<&super::support::HeadlessComputerUseConfig>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
) -> anyhow::Result<ProviderAgent> {
    let binary_str = binary
        .to_str()
        .ok_or_else(|| anyhow!("the ACP executable path is not valid UTF-8"))?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow!("the ACP working directory is not valid UTF-8"))?;
    let (computer_args, computer_env) =
        super::support::grok_computer_use_launch_configuration(computer_use);
    launch.args.extend(computer_args);
    let base_search_path = crate::command_env::executable_search_path();
    let environment = sdk_agent_environment(
        binary,
        crate::command_env::shell_environment(),
        base_search_path.as_deref(),
        launch.env,
        computer_env,
    );

    // `AcpAgentConfig` deliberately contains only argv and environment. macOS
    // `env -C` supplies the session cwd without a shell, preserving exact
    // argument boundaries and the SDK's process-group lifecycle management.
    // Windows has no `/usr/bin/env`: the resolved agent is spawned directly —
    // `Command` runs `.cmd`/`.bat` shims through `cmd.exe` itself — and the
    // session cwd reaches the agent through the ACP `session/new`, `resume`,
    // and `load` requests instead.
    let (program, args) = if cfg!(windows) {
        (binary_str, launch.args)
    } else {
        let mut args = vec!["-C".to_owned(), cwd.to_owned(), binary_str.to_owned()];
        args.extend(launch.args);
        ("/usr/bin/env", args)
    };
    let config = AcpAgentConfig::new(program).args(args).envs(environment);
    let debug_lines = stderr_lines.clone();
    let agent = AcpAgent::new(config).with_debug(move |line, direction| {
        if direction == LineDirection::Stderr {
            record_stderr_line(&debug_lines, line);
        }
    });
    Ok(ProviderAgent {
        agent,
        stderr_lines,
    })
}

/// Builds a short-lived ACP process for session discovery or history replay.
///
/// Catalog work runs on the daemon request thread, never a render path. It
/// intentionally shares the production launch contract so provider argv and
/// environment quirks cannot drift between a resumed task and the picker that
/// discovered it.
pub(crate) fn catalog_agent(
    provider: ProviderKind,
    binary: &Path,
    cwd: &Path,
) -> anyhow::Result<ProviderAgent> {
    catalog_agent_in(provider, binary, cwd, None)
}

/// `config_dir`-scoped variant of [`catalog_agent`] for isolated provider
/// account profiles (such as Antigravity `--gemini-dir` roots).
pub(crate) fn catalog_agent_in(
    provider: ProviderKind,
    binary: &Path,
    cwd: &Path,
    config_dir: Option<&Path>,
) -> anyhow::Result<ProviderAgent> {
    let launch = launch_for(provider, None, config_dir)?;
    sdk_agent(binary, cwd, launch, None, Arc::new(Mutex::new(Vec::new())))
}

type PermissionResponder = Responder<RequestPermissionResponse>;

const CLIENT_PERMISSION_ALLOW_ONCE: &str = "allow_once";
const CLIENT_PERMISSION_DENY: &str = "deny";

enum PendingPermission {
    Protocol(PermissionResponder),
    WriteFile {
        request: WriteTextFileRequest,
        cwd: PathBuf,
        responder: Responder<WriteTextFileResponse>,
    },
    CreateTerminal {
        request: CreateTerminalRequest,
        cwd: PathBuf,
        registry: Arc<AcpTerminalRegistry>,
        events: DriverEventSender,
        responder: Responder<CreateTerminalResponse>,
    },
}

type PendingPermissions = Arc<Mutex<HashMap<String, PendingPermission>>>;

fn client_permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            id: CLIENT_PERMISSION_ALLOW_ONCE.to_owned(),
            label: tr!("permission.allow_once"),
            allow: true,
        },
        PermissionOption {
            id: CLIENT_PERMISSION_DENY.to_owned(),
            label: tr!("common.deny"),
            allow: false,
        },
    ]
}

#[derive(Clone, Copy)]
enum AcpUserInputKind {
    Cursor,
    Xai,
}

enum PendingAcpUserInput {
    Json {
        kind: AcpUserInputKind,
        params: Value,
        responder: Responder<Value>,
    },
    /// Factory Droid, and any other ACP agent, asks through `elicitation/create`.
    /// Zed answers that request from a form; this holds the same request until
    /// the question UI submits.
    Elicitation {
        request: CreateElicitationRequest,
        responder: Responder<CreateElicitationResponse>,
    },
    /// Antigravity asks its multiple-choice questions through
    /// `session/request_permission` with an `interaction_` tool-call id. The
    /// options are the choices, and the answer selects one by label.
    Permission {
        request: RequestPermissionRequest,
        responder: PermissionResponder,
    },
}

type PendingAcpUserInputs = Arc<Mutex<HashMap<String, PendingAcpUserInput>>>;

#[derive(Default)]
struct PendingPrompts(Vec<PendingPrompt>);

struct PendingPrompt {
    request_id: RequestId,
    extension_id: Option<String>,
    session_id: String,
}

impl PendingPrompts {
    fn insert(&mut self, request_id: RequestId, extension_id: Option<String>, session_id: String) {
        self.0.push(PendingPrompt {
            request_id,
            extension_id,
            session_id,
        });
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn settle_request(&mut self, request_id: &RequestId) -> bool {
        let Some(index) = self
            .0
            .iter()
            .position(|prompt| &prompt.request_id == request_id)
        else {
            return false;
        };
        self.0.remove(index);
        self.0.is_empty()
    }

    fn settle_extension(&mut self, session_id: &str, extension_id: Option<&str>) -> bool {
        let Some(index) = self.0.iter().position(|prompt| {
            prompt.session_id == session_id
                && extension_id
                    .is_none_or(|extension_id| prompt.extension_id.as_deref() == Some(extension_id))
        }) else {
            return false;
        };
        self.0.remove(index);
        self.0.is_empty()
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

type PendingPromptRequests = Arc<Mutex<PendingPrompts>>;
type SharedSessionModes = Arc<Mutex<Option<SessionModeState>>>;

/// `agy acp` can return `session/load` mid-replay and continue streaming
/// historical transcript chunks afterward. Draining a short quiet window before
/// reporting `Connected` and holding a brief post-load guard keeps replay
/// chunks from duplicating history or opening a spurious reactive turn.
const ANTIGRAVITY_LOAD_REPLAY_QUIET_WINDOW: Duration = Duration::from_millis(150);
const ANTIGRAVITY_LOAD_REPLAY_MAX_DRAIN: Duration = Duration::from_secs(2);
const ANTIGRAVITY_POST_LOAD_GUARD_WINDOW: Duration = Duration::from_millis(500);

/// Out-of-turn reactive wakeups (e.g. `/swarm` board posts, `/schedule`
/// timers, subagents) have no `session/prompt` response to mark turn end.
/// Settle once no tools/prompts are active and updates stay quiet.
const ANTIGRAVITY_UNSOLICITED_USAGE_SETTLE_DELAY: Duration = Duration::from_secs(2);
const ANTIGRAVITY_UNSOLICITED_IDLE_SETTLE_DELAY: Duration = Duration::from_secs(6);
const ANTIGRAVITY_UNSOLICITED_THINKING_SETTLE_DELAY: Duration = Duration::from_secs(25);

struct ReplaySuppressionInner {
    in_flight: bool,
    draining_until: Option<Instant>,
    last_replay_at: Option<Instant>,
}

struct ReplaySuppression {
    inner: Mutex<ReplaySuppressionInner>,
    replay_seen_tx: smol::channel::Sender<()>,
    replay_seen_rx: smol::channel::Receiver<()>,
}

impl Default for ReplaySuppression {
    fn default() -> Self {
        let (replay_seen_tx, replay_seen_rx) = smol::channel::bounded(1);
        Self {
            inner: Mutex::new(ReplaySuppressionInner {
                in_flight: false,
                draining_until: None,
                last_replay_at: None,
            }),
            replay_seen_tx,
            replay_seen_rx,
        }
    }
}

impl ReplaySuppression {
    fn begin_load(&self) {
        let mut inner = self.inner.lock();
        inner.in_flight = true;
        inner.draining_until = None;
        inner.last_replay_at = None;
        while self.replay_seen_rx.try_recv().is_ok() {}
    }

    fn end_load(&self, post_load_guard: Option<Duration>) {
        let mut inner = self.inner.lock();
        inner.in_flight = false;
        inner.draining_until = post_load_guard.map(|window| Instant::now() + window);
    }

    fn clear_guard(&self) {
        self.inner.lock().draining_until = None;
    }

    fn is_in_flight(&self) -> bool {
        self.inner.lock().in_flight
    }

    fn should_suppress_replay(&self, now: Instant, extend_window: Duration) -> bool {
        let mut inner = self.inner.lock();
        if inner.in_flight {
            inner.last_replay_at = Some(now);
            let _ = self.replay_seen_tx.try_send(());
            return true;
        }
        if let Some(until) = inner.draining_until {
            if now < until {
                inner.last_replay_at = Some(now);
                inner.draining_until = Some(now + extend_window);
                let _ = self.replay_seen_tx.try_send(());
                return true;
            }
            inner.draining_until = None;
        }
        false
    }

    async fn drain_trailing_replay(
        &self,
        quiet_window: Duration,
        max_drain: Duration,
        post_load_guard: Duration,
    ) {
        while self.replay_seen_rx.try_recv().is_ok() {}
        let drain_start = Instant::now();
        let hard_deadline = drain_start + max_drain;
        loop {
            let now = Instant::now();
            if now >= hard_deadline {
                break;
            }
            let reference = self
                .inner
                .lock()
                .last_replay_at
                .map_or(drain_start, |last| last.max(drain_start));
            let quiet_deadline = (reference + quiet_window).min(hard_deadline);
            if now >= quiet_deadline {
                break;
            }
            let saw_more = smol::future::or(
                async {
                    let _ = self.replay_seen_rx.recv().await;
                    true
                },
                async {
                    smol::Timer::at(quiet_deadline).await;
                    false
                },
            )
            .await;
            if !saw_more {
                break;
            }
        }
        self.end_load(Some(post_load_guard));
    }
}

#[derive(Default)]
struct UnsolicitedTurnState {
    active: bool,
    generation: u64,
    settle_at: Option<(Instant, u64)>,
    saw_message_since_tool: bool,
    parked_on_swarm_wait: bool,
}

impl UnsolicitedTurnState {
    fn on_content_update(&mut self) -> bool {
        self.generation = self.generation.wrapping_add(1);
        self.settle_at = None;
        if !self.active {
            self.active = true;
            self.saw_message_since_tool = false;
            self.parked_on_swarm_wait = false;
            true
        } else {
            false
        }
    }

    fn on_stream_update(&mut self, update: &SessionUpdate, completed_tool_name: Option<&str>) {
        match update {
            SessionUpdate::AgentMessageChunk(_) => {
                self.saw_message_since_tool = true;
                self.parked_on_swarm_wait = false;
            }
            SessionUpdate::ToolCall(_) => {
                self.saw_message_since_tool = false;
                self.parked_on_swarm_wait = false;
            }
            SessionUpdate::ToolCallUpdate(_) => {
                if completed_tool_name == Some("swarm_wait") {
                    self.parked_on_swarm_wait = true;
                } else {
                    self.saw_message_since_tool = false;
                    self.parked_on_swarm_wait = false;
                }
            }
            _ => {}
        }
    }

    fn settle_delay(&self, update: &SessionUpdate) -> Duration {
        if self.parked_on_swarm_wait {
            ANTIGRAVITY_UNSOLICITED_USAGE_SETTLE_DELAY
        } else if matches!(update, SessionUpdate::UsageUpdate(_)) && self.saw_message_since_tool {
            ANTIGRAVITY_UNSOLICITED_USAGE_SETTLE_DELAY
        } else if matches!(update, SessionUpdate::AgentMessageChunk(_)) {
            ANTIGRAVITY_UNSOLICITED_IDLE_SETTLE_DELAY
        } else {
            ANTIGRAVITY_UNSOLICITED_THINKING_SETTLE_DELAY
        }
    }

    fn schedule_settle(
        &mut self,
        now: Instant,
        delay: Duration,
        has_in_flight_work: bool,
    ) -> bool {
        if !self.active {
            return false;
        }
        if has_in_flight_work {
            let changed = self.settle_at.is_some();
            self.settle_at = None;
            return changed;
        }
        self.generation = self.generation.wrapping_add(1);
        self.settle_at = Some((now + delay, self.generation));
        true
    }

    fn pause_settle(&mut self) -> bool {
        if !self.active {
            return false;
        }
        let changed = self.settle_at.is_some();
        self.settle_at = None;
        changed
    }

    fn take_active(&mut self) -> bool {
        let was_active = self.active;
        self.active = false;
        self.generation = self.generation.wrapping_add(1);
        self.settle_at = None;
        self.saw_message_since_tool = false;
        self.parked_on_swarm_wait = false;
        was_active
    }

    fn prepare_settle(&mut self, expected_generation: u64, has_in_flight_work: bool) -> bool {
        if !self.active || self.generation != expected_generation {
            return false;
        }
        if has_in_flight_work {
            self.settle_at = None;
            return false;
        }
        self.settle_at = None;
        true
    }

    fn try_settle(&mut self, expected_generation: u64, has_in_flight_work: bool) -> bool {
        if !self.active || self.generation != expected_generation {
            return false;
        }
        if has_in_flight_work {
            self.settle_at = None;
            return false;
        }
        self.active = false;
        self.generation = self.generation.wrapping_add(1);
        self.settle_at = None;
        self.saw_message_since_tool = false;
        self.parked_on_swarm_wait = false;
        true
    }
}

type SharedUnsolicitedTurn = Arc<Mutex<UnsolicitedTurnState>>;

#[allow(clippy::too_many_arguments)]
async fn run_sdk_connection(
    agent: ProviderAgent,
    provider: ProviderKind,
    cwd: std::path::PathBuf,
    attachment_roots: Vec<PathBuf>,
    mode: RuntimeMode,
    model: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    context_window: Option<String>,
    resume_session_id: Option<String>,
    fork_context: Option<String>,
    config_dir: Option<std::path::PathBuf>,
    acp_mcp_servers: Vec<McpServer>,
    computer_use_instructions: Option<String>,
    grok_title_home: Option<std::path::PathBuf>,
    commands: smol::channel::Receiver<CommandMessage>,
    events: DriverEventSender,
) -> agent_client_protocol::Result<()> {
    run_sdk_connection_with(
        agent,
        provider,
        cwd,
        attachment_roots,
        mode,
        model,
        reasoning_effort,
        service_tier,
        context_window,
        resume_session_id,
        fork_context,
        config_dir,
        acp_mcp_servers,
        computer_use_instructions,
        grok_title_home,
        commands,
        events,
        AcpTimeouts::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_sdk_connection_with<A>(
    agent: A,
    provider: ProviderKind,
    cwd: std::path::PathBuf,
    attachment_roots: Vec<PathBuf>,
    mode: RuntimeMode,
    model: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    context_window: Option<String>,
    resume_session_id: Option<String>,
    fork_context: Option<String>,
    config_dir: Option<std::path::PathBuf>,
    acp_mcp_servers: Vec<McpServer>,
    computer_use_instructions: Option<String>,
    grok_title_home: Option<std::path::PathBuf>,
    commands: smol::channel::Receiver<CommandMessage>,
    events: DriverEventSender,
    timeouts: AcpTimeouts,
) -> agent_client_protocol::Result<()>
where
    A: ConnectTo<Client> + 'static,
{
    let suppress_session_updates = Arc::new(ReplaySuppression::default());
    let terminals = Arc::new(AcpTerminalRegistry::default());
    let stream_state = Arc::new(Mutex::new(AcpStreamState {
        terminals: terminals.clone(),
        ..AcpStreamState::default()
    }));
    let pending_permissions: PendingPermissions = Arc::new(Mutex::new(HashMap::new()));
    let pending_user_inputs: PendingAcpUserInputs = Arc::new(Mutex::new(HashMap::new()));
    let prompt_requests = Arc::new(Mutex::new(PendingPrompts::default()));
    let unsolicited_turn: SharedUnsolicitedTurn =
        Arc::new(Mutex::new(UnsolicitedTurnState::default()));
    let (unsolicited_wake_tx, unsolicited_wake_rx) = smol::channel::bounded::<()>(1);
    let title_refresh = super::title_refresh::NativeTitleRefresh::default();
    // The access mode can change in place (Droid, Antigravity), so every reader
    // goes through this cell rather than the mode the session started with.
    let access_mode = Arc::new(Mutex::new(mode));
    let session_modes: SharedSessionModes = Arc::new(Mutex::new(None));
    let config_options: SharedConfigOptions = Arc::new(Mutex::new(None));
    // Set once the session's own autonomy has been written, so drift
    // correction never races the initial write.
    let droid_autonomy_ready = Arc::new(AtomicBool::new(false));
    let fs_read_cwd = cwd.clone();
    let fs_write_cwd = cwd.clone();
    let terminal_cwd = cwd.clone();
    let terminal_create_registry = terminals.clone();
    let terminal_create_events = events.clone();
    let terminal_output_registry = terminals.clone();
    let terminal_wait_registry = terminals.clone();
    let terminal_kill_registry = terminals.clone();
    let terminal_kill_events = events.clone();
    let terminal_release_registry = terminals.clone();
    let terminal_release_events = events.clone();
    let cleanup_terminals = terminals.clone();

    let result = Client
        .builder()
        .name("waku")
        .on_receive_notification(
            {
                let events = events.clone();
                let suppress_session_updates = suppress_session_updates.clone();
                let stream_state = stream_state.clone();
                let prompt_requests = prompt_requests.clone();
                let pending_permissions = pending_permissions.clone();
                let pending_user_inputs = pending_user_inputs.clone();
                let unsolicited_turn = unsolicited_turn.clone();
                let unsolicited_wake_tx = unsolicited_wake_tx.clone();
                let access_mode = access_mode.clone();
                let session_modes = session_modes.clone();
                let config_options = config_options.clone();
                let droid_autonomy_ready = droid_autonomy_ready.clone();
                async move |notification: SessionNotification, connection: ConnectionTo<Agent>| {
                    let is_replay_chunk = is_replay_transcript_update(&notification.update);
                    if is_replay_chunk
                        && suppress_session_updates.should_suppress_replay(
                            Instant::now(),
                            ANTIGRAVITY_POST_LOAD_GUARD_WINDOW,
                        )
                    {
                        return Ok(());
                    }
                    let in_load = suppress_session_updates.is_in_flight();
                    if !in_load {
                        if let SessionUpdate::CurrentModeUpdate(update) = &notification.update
                            && let Some(modes) = session_modes.lock().as_mut()
                        {
                            modes.current_mode_id = update.current_mode_id.clone();
                        }
                        let drifted = track_session_state(
                            provider,
                            &notification.update,
                            *access_mode.lock(),
                            &config_options,
                        );
                        if drifted && droid_autonomy_ready.load(Ordering::Acquire) {
                            restore_droid_autonomy(
                                &connection,
                                notification.session_id.clone(),
                                droid_autonomy_level(*access_mode.lock()),
                                &config_options,
                                &events,
                            )?;
                        }
                    }

                    let is_antigravity_out_of_turn = provider == ProviderKind::Antigravity
                        && !in_load
                        && prompt_requests.lock().is_empty();
                    let is_content = is_unsolicited_turn_content(&notification.update);
                    let is_usage = matches!(notification.update, SessionUpdate::UsageUpdate(_));
                    if is_antigravity_out_of_turn && is_content {
                        let started = unsolicited_turn.lock().on_content_update();
                        if started {
                            {
                                let mut state = stream_state.lock();
                                state.tools.clear();
                                state.produced_content = false;
                            }
                            let _ = events.send(DriverEvent::TurnStarted);
                        }
                    }

                    let update_snapshot = is_antigravity_out_of_turn.then(|| notification.update.clone());
                    let (has_in_flight_tools, completed_tool_name) = {
                        let mut state = stream_state.lock();
                        handle_session_update(provider, notification, &events, &mut state)?;
                        (
                            state.has_in_flight_tools(),
                            state.last_completed_tool_name.take(),
                        )
                    };

                    if let Some(update_snapshot) = update_snapshot.as_ref()
                        && (is_content || is_usage)
                    {
                        let has_in_flight_work = has_in_flight_tools
                            || !pending_permissions.lock().is_empty()
                            || !pending_user_inputs.lock().is_empty();
                        let mut unsolicited = unsolicited_turn.lock();
                        unsolicited
                            .on_stream_update(update_snapshot, completed_tool_name.as_deref());
                        let delay = unsolicited.settle_delay(update_snapshot);
                        if unsolicited.schedule_settle(Instant::now(), delay, has_in_flight_work) {
                            let _ = unsolicited_wake_tx.try_send(());
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let events = events.clone();
                let prompt_requests = prompt_requests.clone();
                let stream_state = stream_state.clone();
                let grok_title_home = grok_title_home.clone();
                let title_refresh = title_refresh.clone();
                async move |notification: UntypedMessage, _connection| {
                    if notification.method() == "_x.ai/session/prompt_complete" {
                        if let Some(session_id) = finish_xai_prompt_complete(
                            notification.params(),
                            &prompt_requests,
                            &stream_state,
                            &events,
                        ) {
                            start_grok_title_refresh(
                                grok_title_home.as_deref(),
                                &session_id,
                                &title_refresh,
                                events.clone(),
                            );
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let events = events.clone();
                let prompt_requests = prompt_requests.clone();
                let pending_permissions = pending_permissions.clone();
                let pending_user_inputs = pending_user_inputs.clone();
                let unsolicited_turn = unsolicited_turn.clone();
                let unsolicited_wake_tx = unsolicited_wake_tx.clone();
                let access_mode = access_mode.clone();
                async move |request: RequestPermissionRequest, responder, _connection| {
                    if provider == ProviderKind::Antigravity && prompt_requests.lock().is_empty() {
                        let started = unsolicited_turn.lock().on_content_update();
                        if started {
                            let _ = events.send(DriverEvent::TurnStarted);
                        }
                        if unsolicited_turn.lock().pause_settle() {
                            let _ = unsolicited_wake_tx.try_send(());
                        }
                    }
                    handle_permission_request(
                        provider,
                        request,
                        responder,
                        auto_approves_permissions(provider, *access_mode.lock()),
                        provider == ProviderKind::Droid,
                        &pending_permissions,
                        &pending_user_inputs,
                        &events,
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                async move |request: ReadTextFileRequest, responder, _connection| {
                    match handle_read_text_file(request, &fs_read_cwd) {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let access_mode = access_mode.clone();
                let pending_permissions = pending_permissions.clone();
                let events = events.clone();
                async move |request: WriteTextFileRequest, responder, _connection| {
                    handle_write_text_file_request(
                        request,
                        &fs_write_cwd,
                        *access_mode.lock(),
                        &pending_permissions,
                        &events,
                        responder,
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let access_mode = access_mode.clone();
                let pending_permissions = pending_permissions.clone();
                async move |request: CreateTerminalRequest, responder, _connection| {
                    handle_create_terminal_request(
                        request,
                        &terminal_cwd,
                        *access_mode.lock(),
                        &terminal_create_registry,
                        &pending_permissions,
                        &terminal_create_events,
                        responder,
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                async move |request: TerminalOutputRequest, responder, _connection| {
                    match terminal_output_registry.terminal_output(request) {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                async move |request: WaitForTerminalExitRequest, responder, _connection| {
                    terminal_wait_registry.wait_for_exit(request, responder)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                async move |request: KillTerminalRequest, responder, _connection| {
                    match terminal_kill_registry.kill_terminal(request, &terminal_kill_events) {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                async move |request: ReleaseTerminalRequest, responder, _connection| {
                    match terminal_release_registry
                        .release_terminal(request, &terminal_release_events)
                    {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let events = events.clone();
                let pending = pending_user_inputs.clone();
                async move |request: UntypedMessage, responder, _connection| {
                    let kind = match request.method() {
                        "cursor/ask_question" => AcpUserInputKind::Cursor,
                        "_x.ai/ask_user_question" | "x.ai/ask_user_question" => {
                            AcpUserInputKind::Xai
                        }
                        _ => {
                            return Ok(Handled::No {
                                message: (request, responder),
                                retry: false,
                            });
                        }
                    };
                    let request_id = responder.id().to_string();
                    let params = match kind {
                        AcpUserInputKind::Cursor => request.params().clone(),
                        AcpUserInputKind::Xai => {
                            unwrap_xai_question_params(request.params()).clone()
                        }
                    };
                    let questions = match kind {
                        AcpUserInputKind::Cursor => cursor_user_input_questions(&params),
                        AcpUserInputKind::Xai => xai_user_input_questions(&params),
                    };
                    if questions.is_empty() {
                        responder.respond(cancelled_user_input_response(kind))?;
                        return Ok(Handled::Yes);
                    }
                    pending.lock().insert(
                        request_id.clone(),
                        PendingAcpUserInput::Json {
                            kind,
                            params,
                            responder,
                        },
                    );
                    if events
                        .send(DriverEvent::UserInputRequested {
                            request_id: request_id.clone(),
                            questions,
                        })
                        .is_err()
                        && let Some(PendingAcpUserInput::Json {
                            kind, responder, ..
                        }) = pending.lock().remove(&request_id)
                    {
                        let _ = responder.respond(cancelled_user_input_response(kind));
                    }
                    Ok(Handled::Yes)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let events = events.clone();
                let pending = pending_user_inputs.clone();
                async move |request: CreateElicitationRequest, responder, _connection| {
                    let ElicitationMode::Form(form) = &request.mode else {
                        responder.respond(cancelled_elicitation_response())?;
                        return Ok(Handled::Yes);
                    };
                    let questions =
                        elicitation_user_input_questions(&request.message, &form.requested_schema);
                    if questions.is_empty() {
                        responder.respond(cancelled_elicitation_response())?;
                        return Ok(Handled::Yes);
                    }
                    let request_id = responder.id().to_string();
                    pending.lock().insert(
                        request_id.clone(),
                        PendingAcpUserInput::Elicitation { request, responder },
                    );
                    if events
                        .send(DriverEvent::UserInputRequested {
                            request_id: request_id.clone(),
                            questions,
                        })
                        .is_err()
                        && let Some(PendingAcpUserInput::Elicitation { responder, .. }) =
                            pending.lock().remove(&request_id)
                    {
                        let _ = responder.respond(cancelled_elicitation_response());
                    }
                    Ok(Handled::Yes)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |connection: ConnectionTo<Agent>| {
            // Antigravity swaps its native diff edits for whole-file
            // `client_edit_file` calls whenever a client advertises writes, and
            // runs its own commands, so it gets reads only. Every other ACP
            // agent uses Waku's write and terminal handlers.
            let native_tools = provider != ProviderKind::Antigravity;
            let mut client_capabilities = ClientCapabilities::new()
                .fs(FileSystemCapabilities::new()
                    .read_text_file(true)
                    .write_text_file(native_tools))
                .terminal(native_tools)
                .elicitation(
                    ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
                );
            if provider == ProviderKind::Cursor {
                // Cursor only exposes its parameterized model controls to
                // clients that opt in. Waku applies the returned config option
                // ids rather than assuming Cursor's private ids stay stable.
                let mut meta = Map::new();
                meta.insert("parameterizedModelPicker".to_owned(), Value::Bool(true));
                client_capabilities = client_capabilities.meta(meta);
            }
            let initialize = await_acp_request(
                connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(client_capabilities)
                            .client_info(Implementation::new("waku", env!("CARGO_PKG_VERSION"))),
                    )
                    .block_task(),
                timeouts.request,
                "ACP initialize",
            )
            .await?;
            let prompt_capabilities = initialize.agent_capabilities.prompt_capabilities.clone();
            let supports_fork = initialize
                .agent_capabilities
                .session_capabilities
                .fork
                .is_some();
            let (mut session_id, modes, initial_config_options) = establish_session(
                &connection,
                provider,
                &initialize,
                resume_session_id.as_deref(),
                &cwd,
                &acp_mcp_servers,
                &suppress_session_updates,
                timeouts.request,
            )
            .await?;
            *session_modes.lock() = modes.clone();
            *config_options.lock() = initial_config_options;

            if let Some(mode_id) = desired_access_mode(provider, modes.as_ref(), mode) {
                // Mode selection is opportunistic: an agent can advertise a
                // mode but reject a later transition without invalidating the
                // session itself.
                if await_acp_request(
                    connection
                        .send_request(SetSessionModeRequest::new(
                            session_id.clone(),
                            mode_id.clone(),
                        ))
                        .block_task(),
                    timeouts.request,
                    "ACP session/set_mode",
                )
                .await
                .is_ok()
                    && let Some(state) = session_modes.lock().as_mut()
                {
                    state.current_mode_id = mode_id;
                }
            }
            if provider == ProviderKind::Droid {
                let current = config_options.lock().clone();
                match apply_droid_autonomy(
                    &connection,
                    &session_id,
                    current.as_deref(),
                    mode,
                    timeouts.request,
                )
                .await
                {
                    Ok(updated) => store_config_options(&config_options, updated),
                    Err(error) => {
                        let _ = events.send(DriverEvent::Error(error));
                        // Fail closed: an unwritten level is the user's CLI
                        // default, which can pre-approve far more than the
                        // selected mode. Only Full access has nothing to lose.
                        if mode != RuntimeMode::FullAccess {
                            return Ok(());
                        }
                    }
                }
                droid_autonomy_ready.store(true, Ordering::Release);
            }
            let mut native_session_id = session_id.to_string();
            let antigravity_step_offset = Arc::new(AtomicI64::new(
                if provider == ProviderKind::Antigravity {
                    let sid = native_session_id.clone();
                    let cfg = config_dir.clone();
                    smol::unblock(move || {
                        crate::antigravity_session::step_offset_in(&sid, cfg.as_deref())
                    })
                    .await
                } else {
                    -1
                },
            ));
            let _ = events.send(DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::from_session_id(
                    provider,
                    native_session_id.clone(),
                )),
            });

            let mut current_model = model;
            let mut current_effort = reasoning_effort;
            let mut current_tier = service_tier;
            let mut current_window = context_window;
            sync_cached_session_model(
                &connection,
                provider,
                &session_id,
                &config_options,
                current_model.as_deref(),
                current_effort.as_deref(),
                current_tier.as_deref(),
                current_window.as_deref(),
                &events,
                timeouts.request,
            )
            .await;
            let mut fork_context = fork_context;
            let mut computer_use_instructions = computer_use_instructions;
            // Prompt callbacks raise lost-session recovery here rather than on
            // `commands`: a clone of the handle's sender would keep this loop
            // alive after the handle is dropped.
            let (recovery_tx, recovery_rx) = smol::channel::unbounded::<CommandMessage>();
            let mut cancel_deadline: Option<Instant> = None;

            enum LoopEvent {
                Command(Option<CommandMessage>),
                Wake,
                SettleUnsolicited(u64),
                CancelTimeout,
            }

            loop {
                if cancel_deadline.is_some() && prompt_requests.lock().is_empty() {
                    cancel_deadline = None;
                }
                let settle_target = unsolicited_turn.lock().settle_at;
                let event = smol::future::or(
                    smol::future::or(
                        async { LoopEvent::Command(recovery_rx.recv().await.ok()) },
                        async { LoopEvent::Command(commands.recv().await.ok()) },
                    ),
                    smol::future::or(
                        async {
                            if unsolicited_wake_rx.recv().await.is_ok() {
                                LoopEvent::Wake
                            } else {
                                std::future::pending().await
                            }
                        },
                        smol::future::or(
                            async {
                                match settle_target {
                                    Some((deadline, generation)) => {
                                        smol::Timer::at(deadline).await;
                                        LoopEvent::SettleUnsolicited(generation)
                                    }
                                    None => std::future::pending().await,
                                }
                            },
                            async {
                                match cancel_deadline {
                                    Some(deadline) => {
                                        smol::Timer::at(deadline).await;
                                        LoopEvent::CancelTimeout
                                    }
                                    None => std::future::pending().await,
                                }
                            },
                        ),
                    ),
                )
                .await;
                let command = match event {
                    LoopEvent::Wake => continue,
                    LoopEvent::CancelTimeout => {
                        cancel_deadline = None;
                        if !prompt_requests.lock().is_empty() {
                            prompt_requests.lock().clear();
                            stream_state.lock().tools.clear();
                            if provider == ProviderKind::Antigravity {
                                let sid = native_session_id.clone();
                                let cfg = config_dir.clone();
                                let next_offset = smol::unblock(move || {
                                    crate::antigravity_session::step_offset_in(&sid, cfg.as_deref())
                                })
                                .await;
                                antigravity_step_offset.store(next_offset, Ordering::Release);
                            }
                            let _ = events.send(DriverEvent::Error(format!(
                                "{} ACP session/cancel timed out",
                                provider.display_name()
                            )));
                            let _ = events.send(DriverEvent::TurnFinished {
                                success: false,
                                summary: None,
                            });
                            // Tear down the wedged ACP connection so lingering server output
                            // from the cancelled turn cannot desync future prompts or open
                            // spurious unsolicited turns.
                            break;
                        }
                        continue;
                    }
                    LoopEvent::SettleUnsolicited(generation) => {
                        let has_in_flight_work = !prompt_requests.lock().is_empty()
                            || stream_state.lock().has_in_flight_tools()
                            || !pending_permissions.lock().is_empty()
                            || !pending_user_inputs.lock().is_empty();
                        if !unsolicited_turn
                            .lock()
                            .prepare_settle(generation, has_in_flight_work)
                        {
                            continue;
                        }
                        let produced_content = stream_state.lock().produced_content;
                        let native_outcome = if provider == ProviderKind::Antigravity {
                            let offset = antigravity_step_offset.load(Ordering::Acquire);
                            let stop_reason = produced_content.then_some(StopReason::EndTurn);
                            let failure = resolve_native_turn_failure(
                                None,
                                Some(offset),
                                native_session_id.clone(),
                                config_dir.clone(),
                                produced_content,
                                stop_reason,
                            )
                            .await;
                            let sid = native_session_id.clone();
                            let cfg = config_dir.clone();
                            let next_offset = smol::unblock(move || {
                                crate::antigravity_session::step_offset_in(&sid, cfg.as_deref())
                            })
                            .await;
                            Some((failure, next_offset))
                        } else {
                            None
                        };
                        let has_in_flight_work = !prompt_requests.lock().is_empty()
                            || stream_state.lock().has_in_flight_tools()
                            || !pending_permissions.lock().is_empty()
                            || !pending_user_inputs.lock().is_empty();
                        if unsolicited_turn
                            .lock()
                            .try_settle(generation, has_in_flight_work)
                        {
                            stream_state.lock().tools.clear();
                            if let Some((Some(failure), next_offset)) = native_outcome {
                                antigravity_step_offset.store(next_offset, Ordering::Release);
                                let _ = events.send(DriverEvent::Error(failure));
                                let _ = events.send(DriverEvent::TurnFinished {
                                    success: false,
                                    summary: None,
                                });
                            } else {
                                if let Some((None, next_offset)) = native_outcome {
                                    antigravity_step_offset.store(next_offset, Ordering::Release);
                                }
                                let _ = events.send(DriverEvent::TurnFinished {
                                    success: true,
                                    summary: None,
                                });
                            }
                        }
                        continue;
                    }
                    LoopEvent::Command(Some(command)) => command,
                    LoopEvent::Command(None) => break,
                };
                match command {
                    CommandMessage::RecoverLostSession {
                        failed_session_id,
                        text,
                    } => {
                        cancel_deadline = None;
                        if failed_session_id != native_session_id {
                            // Another in-flight request (e.g. the primary Turn
                            // before a Steer) already established a fresh
                            // session; resend on that session without creating
                            // a duplicate or wiping active stream state.
                            let resend_kind = if !prompt_requests.lock().is_empty() {
                                PromptKind::Steer
                            } else {
                                PromptKind::Turn
                            };
                            if let Err(error) = send_prompt(
                                &connection,
                                &session_id,
                                text,
                                &cwd,
                                &attachment_roots,
                                &prompt_capabilities,
                                &prompt_requests,
                                &events,
                                provider,
                                &native_session_id,
                                config_dir.clone(),
                                grok_title_home.clone(),
                                title_refresh.clone(),
                                stream_state.clone(),
                                None,
                                resend_kind,
                                Some(antigravity_step_offset.clone()),
                            ) && prompt_requests.lock().is_empty()
                            {
                                let _ = events.send(DriverEvent::Error(error.to_string()));
                                let _ = events.send(DriverEvent::TurnFinished {
                                    success: false,
                                    summary: None,
                                });
                            }
                            continue;
                        }
                        match establish_session(
                            &connection,
                            provider,
                            &initialize,
                            None,
                            &cwd,
                            &acp_mcp_servers,
                            &suppress_session_updates,
                            timeouts.request,
                        )
                        .await
                        {
                            Ok((fresh_session_id, modes, fresh_config_options)) => {
                                session_id = fresh_session_id;
                                native_session_id = session_id.to_string();
                                *session_modes.lock() = modes.clone();
                                *config_options.lock() = fresh_config_options;
                                if provider == ProviderKind::Antigravity {
                                    let sid = native_session_id.clone();
                                    let cfg = config_dir.clone();
                                    let next_offset = smol::unblock(move || {
                                        crate::antigravity_session::step_offset_in(
                                            &sid,
                                            cfg.as_deref(),
                                        )
                                    })
                                    .await;
                                    antigravity_step_offset.store(next_offset, Ordering::Release);
                                }
                                let live_mode = *access_mode.lock();
                                if let Some(mode_id) =
                                    desired_access_mode(provider, modes.as_ref(), live_mode)
                                {
                                    if await_acp_request(
                                        connection
                                            .send_request(SetSessionModeRequest::new(
                                                session_id.clone(),
                                                mode_id.clone(),
                                            ))
                                            .block_task(),
                                        timeouts.request,
                                        "ACP session/set_mode",
                                    )
                                    .await
                                    .is_ok()
                                        && let Some(state) = session_modes.lock().as_mut()
                                    {
                                        state.current_mode_id = mode_id;
                                    }
                                }
                                sync_cached_session_model(
                                    &connection,
                                    provider,
                                    &session_id,
                                    &config_options,
                                    current_model.as_deref(),
                                    current_effort.as_deref(),
                                    current_tier.as_deref(),
                                    current_window.as_deref(),
                                    &events,
                                    timeouts.request,
                                )
                                .await;
                                let _ = events.send(DriverEvent::Connected {
                                    provider_cursor: Some(ProviderResumeCursor::from_session_id(
                                        provider,
                                        native_session_id.clone(),
                                    )),
                                });
                                // The turn already started; only resend it,
                                // and never recover the same prompt twice.
                                if let Err(error) = send_prompt(
                                    &connection,
                                    &session_id,
                                    text,
                                    &cwd,
                                    &attachment_roots,
                                    &prompt_capabilities,
                                    &prompt_requests,
                                    &events,
                                    provider,
                                    &native_session_id,
                                    config_dir.clone(),
                                    grok_title_home.clone(),
                                    title_refresh.clone(),
                                    stream_state.clone(),
                                    None,
                                    PromptKind::Turn,
                                    Some(antigravity_step_offset.clone()),
                                ) && prompt_requests.lock().is_empty()
                                {
                                    let _ = events.send(DriverEvent::Error(error.to_string()));
                                    let _ = events.send(DriverEvent::TurnFinished {
                                        success: false,
                                        summary: None,
                                    });
                                }
                            }
                            Err(error) => {
                                if prompt_requests.lock().is_empty() {
                                    let _ = events.send(DriverEvent::Error(error.to_string()));
                                    let _ = events.send(DriverEvent::TurnFinished {
                                        success: false,
                                        summary: None,
                                    });
                                }
                            }
                        }
                    }
                    CommandMessage::Prompt(text) => {
                        cancel_deadline = None;
                        suppress_session_updates.clear_guard();
                        unsolicited_turn.lock().take_active();
                        let prompt_text = format_prompt_with_seed(
                            provider,
                            fork_context.as_deref(),
                            computer_use_instructions.as_deref(),
                            text,
                        );
                        let _ = events.send(DriverEvent::TurnStarted);
                        match send_prompt(
                            &connection,
                            &session_id,
                            prompt_text,
                            &cwd,
                            &attachment_roots,
                            &prompt_capabilities,
                            &prompt_requests,
                            &events,
                            provider,
                            &native_session_id,
                            config_dir.clone(),
                            grok_title_home.clone(),
                            title_refresh.clone(),
                            stream_state.clone(),
                            (provider == ProviderKind::Antigravity).then(|| recovery_tx.clone()),
                            PromptKind::Turn,
                            Some(antigravity_step_offset.clone()),
                        ) {
                            Ok(()) => {
                                fork_context = None;
                                computer_use_instructions = None;
                            }
                            Err(error) => {
                                let _ = events.send(DriverEvent::Error(error.to_string()));
                                let _ = events.send(DriverEvent::TurnFinished {
                                    success: false,
                                    summary: None,
                                });
                            }
                        }
                    }
                    CommandMessage::Steer(text) => {
                        let had_unsolicited =
                            provider == ProviderKind::Antigravity && unsolicited_turn.lock().active;
                        if prompt_requests.lock().is_empty() && !had_unsolicited {
                            let _ = events.send(DriverEvent::SteerRejected {
                                message: text,
                                reason: format!(
                                    "{} has no active turn to steer.",
                                    provider.display_name()
                                ),
                            });
                            continue;
                        }
                        suppress_session_updates.clear_guard();
                        match send_prompt(
                            &connection,
                            &session_id,
                            text.clone(),
                            &cwd,
                            &attachment_roots,
                            &prompt_capabilities,
                            &prompt_requests,
                            &events,
                            provider,
                            &native_session_id,
                            config_dir.clone(),
                            grok_title_home.clone(),
                            title_refresh.clone(),
                            stream_state.clone(),
                            (provider == ProviderKind::Antigravity).then(|| recovery_tx.clone()),
                            PromptKind::Steer,
                            Some(antigravity_step_offset.clone()),
                        ) {
                            Ok(()) => {
                                cancel_deadline = None;
                                if had_unsolicited {
                                    unsolicited_turn.lock().take_active();
                                }
                                let _ = events.send(DriverEvent::SteerAccepted { message: text });
                            }
                            Err(error) => {
                                let _ = events.send(DriverEvent::SteerRejected {
                                    message: text,
                                    reason: error.to_string(),
                                });
                            }
                        }
                    }
                    CommandMessage::Cancel => {
                        let _ = connection
                            .send_notification(CancelNotification::new(session_id.clone()));
                        cancel_pending_permissions(&pending_permissions);
                        cancel_pending_user_inputs(&pending_user_inputs);
                        let unsolicited_active =
                            unsolicited_turn.lock().active && prompt_requests.lock().is_empty();
                        if unsolicited_active {
                            let next_offset = if provider == ProviderKind::Antigravity {
                                let sid = native_session_id.clone();
                                let cfg = config_dir.clone();
                                Some(
                                    smol::unblock(move || {
                                        crate::antigravity_session::step_offset_in(
                                            &sid,
                                            cfg.as_deref(),
                                        )
                                    })
                                    .await,
                                )
                            } else {
                                None
                            };
                            if unsolicited_turn.lock().take_active()
                                && prompt_requests.lock().is_empty()
                            {
                                stream_state.lock().tools.clear();
                                if let Some(next_offset) = next_offset {
                                    antigravity_step_offset.store(next_offset, Ordering::Release);
                                }
                                let _ = events.send(DriverEvent::TurnFinished {
                                    success: true,
                                    summary: None,
                                });
                                cancel_deadline = None;
                            }
                        } else if !prompt_requests.lock().is_empty() {
                            cancel_deadline
                                .get_or_insert_with(|| Instant::now() + timeouts.cancel);
                        }
                    }
                    CommandMessage::RefreshBackgroundWork => {
                        terminals.emit_reconcile(&events);
                    }
                    CommandMessage::StopBackgroundWork { key, control_id } => {
                        let id = if control_id.is_empty() {
                            &key.provider_id
                        } else {
                            &control_id
                        };
                        terminals.stop_by_id(id, &key, &events);
                    }
                    CommandMessage::Respond {
                        request_id,
                        option_id,
                    } => {
                        if let Some(pending) = pending_permissions.lock().remove(&request_id) {
                            match pending {
                                PendingPermission::Protocol(responder) => {
                                    let _ = responder.respond(RequestPermissionResponse::new(
                                        RequestPermissionOutcome::Selected(
                                            SelectedPermissionOutcome::new(option_id),
                                        ),
                                    ));
                                }
                                PendingPermission::WriteFile {
                                    request,
                                    cwd,
                                    responder,
                                } => {
                                    if option_id == CLIENT_PERMISSION_ALLOW_ONCE {
                                        let _ = match handle_write_text_file(request, &cwd) {
                                            Ok(response) => responder.respond(response),
                                            Err(error) => responder.respond_with_error(error),
                                        };
                                    } else {
                                        let _ = responder.respond_with_error(
                                            agent_client_protocol::util::internal_error(
                                                "File write denied by user",
                                            ),
                                        );
                                    }
                                }
                                PendingPermission::CreateTerminal {
                                    request,
                                    cwd,
                                    registry,
                                    events,
                                    responder,
                                } => {
                                    if option_id == CLIENT_PERMISSION_ALLOW_ONCE {
                                        let _ = match registry
                                            .create_terminal_authorized(request, &cwd, &events)
                                        {
                                            Ok(response) => responder.respond(response),
                                            Err(error) => responder.respond_with_error(error),
                                        };
                                    } else {
                                        let _ = responder.respond_with_error(
                                            agent_client_protocol::util::internal_error(
                                                "Terminal execution denied by user",
                                            ),
                                        );
                                    }
                                }
                            }
                            if provider == ProviderKind::Antigravity
                                && prompt_requests.lock().is_empty()
                            {
                                let has_in_flight_work = stream_state.lock().has_in_flight_tools()
                                    || !pending_permissions.lock().is_empty()
                                    || !pending_user_inputs.lock().is_empty();
                                if unsolicited_turn.lock().schedule_settle(
                                    Instant::now(),
                                    ANTIGRAVITY_UNSOLICITED_IDLE_SETTLE_DELAY,
                                    has_in_flight_work,
                                ) {
                                    let _ = unsolicited_wake_tx.try_send(());
                                }
                            }
                        }
                    }
                    CommandMessage::RespondUserInput {
                        request_id,
                        answers,
                    } => {
                        if let Some(pending) = pending_user_inputs.lock().remove(&request_id) {
                            match pending {
                                PendingAcpUserInput::Json {
                                    kind,
                                    params,
                                    responder,
                                } => {
                                    let response = match kind {
                                        AcpUserInputKind::Cursor => {
                                            cursor_user_input_response(&params, &answers)
                                        }
                                        AcpUserInputKind::Xai => {
                                            xai_user_input_response(&params, &answers)
                                        }
                                    };
                                    let _ = responder.respond(response);
                                }
                                PendingAcpUserInput::Elicitation { request, responder } => {
                                    let _ = responder
                                        .respond(elicitation_accept_response(&request, &answers));
                                }
                                PendingAcpUserInput::Permission { request, responder } => {
                                    let _ = responder
                                        .respond(antigravity_question_response(&request, &answers));
                                }
                            }
                            if provider == ProviderKind::Antigravity
                                && prompt_requests.lock().is_empty()
                            {
                                let has_in_flight_work = stream_state.lock().has_in_flight_tools()
                                    || !pending_permissions.lock().is_empty()
                                    || !pending_user_inputs.lock().is_empty();
                                if unsolicited_turn.lock().schedule_settle(
                                    Instant::now(),
                                    ANTIGRAVITY_UNSOLICITED_IDLE_SETTLE_DELAY,
                                    has_in_flight_work,
                                ) {
                                    let _ = unsolicited_wake_tx.try_send(());
                                }
                            }
                        }
                    }
                    CommandMessage::Options {
                        options,
                        mode_applied,
                    } => {
                        if let Some(mode_applied) = mode_applied {
                            // Stop answering permissions under the old mode
                            // before the agent hears about the new one.
                            let mode_timeout = timeouts.request.min(MODE_SWITCH_TIMEOUT);
                            *access_mode.lock() = options.mode;
                            let applied = if provider == ProviderKind::Droid {
                                let current = config_options.lock().clone();
                                match apply_droid_autonomy(
                                    &connection,
                                    &session_id,
                                    current.as_deref(),
                                    options.mode,
                                    mode_timeout,
                                )
                                .await
                                {
                                    Ok(updated) => {
                                        store_config_options(&config_options, updated);
                                        true
                                    }
                                    Err(_) => {
                                        // The app restarts the session on a
                                        // refusal; until then nothing is approved
                                        // on the user's behalf.
                                        *access_mode.lock() = RuntimeMode::Ask;
                                        false
                                    }
                                }
                            } else if provider == ProviderKind::Antigravity {
                                let current_modes = session_modes.lock().clone();
                                match apply_antigravity_mode(
                                    &connection,
                                    &session_id,
                                    current_modes.as_ref(),
                                    options.mode,
                                    mode_timeout,
                                )
                                .await
                                {
                                    Ok(updated_mode_id) => {
                                        if let (Some(mode_id), Some(state)) =
                                            (updated_mode_id, session_modes.lock().as_mut())
                                        {
                                            state.current_mode_id = mode_id;
                                        }
                                        true
                                    }
                                    Err(_) => {
                                        *access_mode.lock() = RuntimeMode::Ask;
                                        false
                                    }
                                }
                            } else {
                                false
                            };
                            let _ = mode_applied.try_send(applied);
                            if !applied {
                                continue;
                            }
                        }
                        if options.model != current_model
                            || options.reasoning_effort != current_effort
                            || options.service_tier != current_tier
                            || options.context_window != current_window
                        {
                            current_model = options.model;
                            current_effort = options.reasoning_effort;
                            current_tier = options.service_tier;
                            current_window = options.context_window;
                            sync_cached_session_model(
                                &connection,
                                provider,
                                &session_id,
                                &config_options,
                                current_model.as_deref(),
                                current_effort.as_deref(),
                                current_tier.as_deref(),
                                current_window.as_deref(),
                                &events,
                                timeouts.request,
                            )
                            .await;
                        }
                    }
                    CommandMessage::Fork {
                        turns_to_remove,
                        reply,
                    } => {
                        let outcome = if turns_to_remove > 0 {
                            Err(anyhow!(
                                "{} ACP session/fork only supports branching the full conversation",
                                provider.display_name()
                            ))
                        } else if !supports_fork {
                            Err(anyhow!(
                                "{} did not advertise ACP session/fork support",
                                provider.display_name()
                            ))
                        } else {
                            let mut request = ForkSessionRequest::new(session_id.clone(), &cwd);
                            if !acp_mcp_servers.is_empty() {
                                request = request.mcp_servers(acp_mcp_servers.clone());
                            }
                            await_acp_request(
                                connection.send_request(request).block_task(),
                                timeouts.request.min(MODE_SWITCH_TIMEOUT),
                                "ACP session/fork",
                            )
                            .await
                            .map(|response| {
                                ProviderResumeCursor::from_session_id(
                                    provider,
                                    response.session_id.to_string(),
                                )
                            })
                            .map_err(|error| anyhow!(error.to_string()))
                        };
                        let _ = reply.try_send(outcome);
                    }
                    CommandMessage::Shutdown => break,
                }
            }
            cancel_pending_permissions(&pending_permissions);
            cancel_pending_user_inputs(&pending_user_inputs);
            Ok(())
        })
        .await;
    cleanup_terminals.shutdown_all();
    result
}

/// Whether a failed `session/resume` or `session/load` error indicates that the
/// saved session genuinely does not exist (or the method is unsupported) rather
/// than an authentication, transport, or transient server failure. Only in the
/// former case is falling back to `session/new` safe; surfacing other errors
/// preserves the task's `provider_cursor` so the user can retry without losing
/// conversation history.
fn can_fallback_from_resume_error(error: &agent_client_protocol::Error) -> bool {
    if matches!(
        error.code,
        agent_client_protocol::ErrorCode::ResourceNotFound
            | agent_client_protocol::ErrorCode::MethodNotFound
    ) {
        return true;
    }
    if matches!(
        error.code,
        agent_client_protocol::ErrorCode::AuthRequired
            | agent_client_protocol::ErrorCode::RequestCancelled
            | agent_client_protocol::ErrorCode::ParseError
    ) {
        return false;
    }
    let detail = error.to_string().to_ascii_lowercase();
    is_session_not_found_message(&detail)
}

fn is_session_not_found_message(detail: &str) -> bool {
    if detail.contains("auth")
        || detail.contains("unauthorized")
        || detail.contains("forbidden")
        || detail.contains("credential")
        || detail.contains("token")
        || detail.contains("login")
        || detail.contains("api key")
        || detail.contains("api_key")
        || detail.contains("permission denied")
        || detail.contains("connection closed")
        || detail.contains("broken pipe")
        || detail.contains("eof")
        || detail.contains("process exited")
    {
        return false;
    }
    if detail.contains("unknown session")
        || detail.contains("no such session")
        || detail.contains("session expired")
    {
        return true;
    }
    let names_missing = detail.contains("not found")
        || detail.contains("not_found")
        || detail.contains("does not exist");
    let names_session = detail.contains("session")
        || detail.contains("conversation")
        || detail.contains("trajectory")
        || detail.contains("thread")
        || detail.contains("chat");
    names_missing && names_session
}

#[allow(clippy::too_many_arguments)]
async fn establish_session(
    connection: &ConnectionTo<Agent>,
    provider: ProviderKind,
    initialize: &InitializeResponse,
    resume_session_id: Option<&str>,
    cwd: &Path,
    mcp_servers: &[McpServer],
    suppress_session_updates: &ReplaySuppression,
    request_timeout: Duration,
) -> agent_client_protocol::Result<(
    SessionId,
    Option<SessionModeState>,
    Option<Vec<SessionConfigOption>>,
)> {
    if let Some(existing) = resume_session_id {
        let mut fatal_resume_error = None;
        // `agy acp` answers `session/resume` with the session's settings and no
        // conversation history; its next turn starts with an empty model
        // context. `session/load` replays the history and does restore it, so
        // Antigravity takes that rung even though resume is advertised.
        let resume_restores_context = provider != ProviderKind::Antigravity;
        if resume_restores_context
            && initialize
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some()
        {
            let mut request = ResumeSessionRequest::new(existing.to_owned(), cwd);
            if !mcp_servers.is_empty() {
                request = request.mcp_servers(mcp_servers.to_vec());
            }
            match await_acp_request(
                connection.send_request(request).block_task(),
                request_timeout,
                "ACP session/resume",
            )
            .await
            {
                Ok(response) => {
                    return Ok((
                        SessionId::new(existing.to_owned()),
                        response.modes,
                        response.config_options,
                    ));
                }
                Err(error) => {
                    if error.code == agent_client_protocol::ErrorCode::AuthRequired
                        || error.message.contains("timed out")
                    {
                        return Err(error);
                    }
                    if !can_fallback_from_resume_error(&error) {
                        fatal_resume_error = Some(error);
                    }
                }
            }
        }

        if initialize.agent_capabilities.load_session {
            suppress_session_updates.begin_load();
            let mut request = LoadSessionRequest::new(existing.to_owned(), cwd);
            if !mcp_servers.is_empty() {
                request = request.mcp_servers(mcp_servers.to_vec());
            }
            let response = await_acp_request(
                connection.send_request(request).block_task(),
                request_timeout,
                "ACP session/load",
            )
            .await;
            if response.is_ok() && provider == ProviderKind::Antigravity {
                suppress_session_updates
                    .drain_trailing_replay(
                        ANTIGRAVITY_LOAD_REPLAY_QUIET_WINDOW,
                        ANTIGRAVITY_LOAD_REPLAY_MAX_DRAIN,
                        ANTIGRAVITY_POST_LOAD_GUARD_WINDOW,
                    )
                    .await;
            } else {
                suppress_session_updates.end_load(None);
            }
            match response {
                Ok(response) => {
                    return Ok((
                        SessionId::new(existing.to_owned()),
                        response.modes,
                        response.config_options,
                    ));
                }
                Err(error) => {
                    if !can_fallback_from_resume_error(&error) {
                        return Err(error);
                    }
                }
            }
        }

        if let Some(error) = fatal_resume_error {
            return Err(error);
        }
    }

    let mut request = NewSessionRequest::new(cwd);
    if !mcp_servers.is_empty() {
        request = request.mcp_servers(mcp_servers.to_vec());
    }
    let response = await_acp_request(
        connection.send_request(request).block_task(),
        request_timeout,
        "ACP session/new",
    )
    .await?;
    Ok((response.session_id, response.modes, response.config_options))
}

fn desired_access_mode(
    provider: ProviderKind,
    modes: Option<&SessionModeState>,
    mode: RuntimeMode,
) -> Option<SessionModeId> {
    let modes = modes?;
    let desired = if provider == ProviderKind::Antigravity {
        // `agy acp` advertises `default`, `accept-edits`, `auto`, and `plan`.
        // `RuntimeMode::Auto` selects native `auto` when advertised (falling
        // back to `default` on older builds); `Ask` and `FullAccess` stay on
        // `default` while Waku's own permission policy handles prompts.
        let find_mode = |target: &str| {
            modes
                .available_modes
                .iter()
                .find(|mode| mode.id.to_string().eq_ignore_ascii_case(target))
                .map(|mode| mode.id.clone())
        };
        match mode {
            RuntimeMode::AutoAcceptEdits => find_mode("accept-edits")?,
            RuntimeMode::Auto => find_mode("auto").or_else(|| find_mode("default"))?,
            RuntimeMode::Ask | RuntimeMode::FullAccess => find_mode("default")?,
        }
    } else if provider == ProviderKind::Fx {
        let desired = if mode == RuntimeMode::Ask {
            "ask"
        } else {
            "code"
        };
        modes
            .available_modes
            .iter()
            .find(|mode| mode.id.to_string().eq_ignore_ascii_case(desired))?
            .id
            .clone()
    } else {
        // Sessions created before the interaction toggle was removed may
        // retain the provider's read-only mode. Return only those sessions to
        // the provider's ordinary execution mode; otherwise leave externally
        // selected native modes untouched.
        if !modes
            .current_mode_id
            .to_string()
            .eq_ignore_ascii_case("plan")
        {
            return None;
        }
        modes
            .available_modes
            .iter()
            .find(|mode| {
                let id = mode.id.to_string();
                id.eq_ignore_ascii_case("agent") || id.eq_ignore_ascii_case("default")
            })?
            .id
            .clone()
    };
    (modes.current_mode_id != desired).then_some(desired)
}

/// Which session config option carries reasoning effort. ACP leaves the id to
/// the agent: Kimi Code exposes it as its `thinking` level, while the other
/// agents Waku drives keep it on `mode`. Grok does not use this path: its
/// effort rides on `session/set_model` as `_meta.reasoningEffort`.
fn reasoning_effort_config_id(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Antigravity => "thought_level",
        ProviderKind::Kimi => "thinking",
        _ => "mode",
    }
}

fn session_config_select_values(option: &SessionConfigOption) -> Vec<&str> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return Vec::new();
    };
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|option| option.value.0.as_ref())
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .map(|option| option.value.0.as_ref())
            .collect(),
        _ => Vec::new(),
    }
}

fn cursor_model_selection(
    option: &SessionConfigOption,
    requested: &str,
) -> Option<CursorModelSelection> {
    resolve_cursor_model(session_config_select_values(option), requested)
}

fn cursor_option_id(option: &SessionConfigOption) -> String {
    option.id.to_string().to_ascii_lowercase()
}

fn cursor_option_name(option: &SessionConfigOption) -> String {
    option.name.to_ascii_lowercase()
}

fn is_cursor_thinking_option(option: &SessionConfigOption) -> bool {
    option.category == Some(SessionConfigOptionCategory::ModelConfig) && {
        let id = cursor_option_id(option);
        let name = cursor_option_name(option);
        id == "thinking" || name.contains("thinking")
    }
}

fn is_cursor_fast_option(option: &SessionConfigOption) -> bool {
    option.category == Some(SessionConfigOptionCategory::ModelConfig) && {
        let id = cursor_option_id(option);
        let name = cursor_option_name(option);
        id == "fast" || name == "fast" || name.contains("fast mode")
    }
}

fn is_cursor_context_option(option: &SessionConfigOption) -> bool {
    option.category == Some(SessionConfigOptionCategory::ModelConfig) && {
        let id = cursor_option_id(option);
        let name = cursor_option_name(option);
        id == "context" || id == "context_size" || name.contains("context")
    }
}

fn is_cursor_effort_option(option: &SessionConfigOption) -> bool {
    if !matches!(option.kind, SessionConfigKind::Select(_)) {
        return false;
    }
    let id = cursor_option_id(option);
    let name = cursor_option_name(option);
    id == "effort"
        || id == "reasoning"
        || name == "effort"
        || name == "reasoning"
        || name.contains("effort")
        || name.contains("reasoning")
}

fn find_cursor_effort_option(options: &[SessionConfigOption]) -> Option<&SessionConfigOption> {
    let candidates: Vec<&SessionConfigOption> = options
        .iter()
        .filter(|option| is_cursor_effort_option(option))
        .collect();
    candidates
        .iter()
        .copied()
        .find(|option| {
            matches!(
                option.category.as_ref(),
                Some(SessionConfigOptionCategory::Other(value))
                    if value.eq_ignore_ascii_case("model_option")
            )
        })
        .or_else(|| {
            candidates
                .iter()
                .copied()
                .find(|option| option.id.to_string().eq_ignore_ascii_case("effort"))
        })
        .or_else(|| {
            candidates
                .iter()
                .copied()
                .find(|option| option.category == Some(SessionConfigOptionCategory::ThoughtLevel))
        })
        .or_else(|| candidates.first().copied())
}

fn cursor_matching_select_value<'a>(values: &[&'a str], requested: &str) -> Option<&'a str> {
    let normalized = normalize_cursor_reasoning_effort(requested);
    values
        .iter()
        .find(|value| normalize_cursor_reasoning_effort(value) == normalized)
        .copied()
}

fn cursor_desired_effort_value(
    option: &SessionConfigOption,
    selection: &CursorModelSelection,
    reasoning_effort: Option<&str>,
) -> Option<String> {
    let values = session_config_select_values(option);
    if let Some(effort) = reasoning_effort.filter(|effort| !effort.is_empty())
        && let Some(value) = cursor_matching_select_value(&values, effort)
    {
        return Some(value.to_owned());
    }
    if selection.suffix.contains("extra-high")
        && let Some(value) = values
            .iter()
            .find(|value| normalize_cursor_reasoning_effort(value) == "xhigh")
    {
        return Some((*value).to_owned());
    }
    values
        .iter()
        .find(|value| cursor_suffix_has(&selection.suffix, value))
        .map(|value| (*value).to_owned())
}

fn cursor_desired_thinking(
    selection: &CursorModelSelection,
    reasoning_effort: Option<&str>,
) -> Option<bool> {
    if let Some(effort) = reasoning_effort {
        return Some(normalize_cursor_reasoning_effort(effort) != "none");
    }
    cursor_suffix_has(&selection.suffix, "thinking").then_some(true)
}

fn cursor_desired_fast(
    selection: &CursorModelSelection,
    service_tier: Option<&str>,
) -> Option<bool> {
    match service_tier {
        Some("fast") => Some(true),
        Some(_) => Some(false),
        None if cursor_suffix_has(&selection.suffix, "fast") => Some(true),
        None => None,
    }
}

fn cursor_desired_context_value(
    option: &SessionConfigOption,
    context_window: Option<&str>,
) -> Option<String> {
    let requested = context_window.filter(|value| !value.is_empty())?;
    let values = session_config_select_values(option);
    let normalized = requested.replace(['_', ' '], "-");
    values
        .iter()
        .find(|value| {
            value.eq_ignore_ascii_case(requested)
                || value
                    .replace(['_', ' '], "-")
                    .eq_ignore_ascii_case(&normalized)
        })
        .map(|value| (*value).to_owned())
}

fn cursor_flag_request_value(
    option: &SessionConfigOption,
    enabled: bool,
) -> Option<SessionConfigOptionValue> {
    match &option.kind {
        SessionConfigKind::Boolean(_) => Some(SessionConfigOptionValue::boolean(enabled)),
        SessionConfigKind::Select(_) => {
            let value = if enabled { "true" } else { "false" };
            session_config_select_values(option)
                .contains(&value)
                .then(|| SessionConfigOptionValue::value_id(value))
        }
        _ => None,
    }
}

fn cursor_flag_matches(option: &SessionConfigOption, enabled: bool) -> bool {
    match &option.kind {
        SessionConfigKind::Boolean(boolean) => boolean.current_value == enabled,
        SessionConfigKind::Select(_) => {
            let value = if enabled { "true" } else { "false" };
            session_config_current_value(option) == Some(value)
        }
        _ => false,
    }
}

fn session_config_current_value(option: &SessionConfigOption) -> Option<&str> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    Some(select.current_value.0.as_ref())
}

#[allow(clippy::too_many_arguments)]
async fn apply_cursor_variant_configs(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    options: &mut Vec<SessionConfigOption>,
    selection: &CursorModelSelection,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
    context_window: Option<&str>,
    request_timeout: Duration,
) -> agent_client_protocol::Result<()> {
    // Thinking can reveal a thought-level option, so apply it first and use
    // each response's refreshed option set for the next selection.
    for target in ["thinking", "effort", "context", "fast"] {
        let Some(option) = (match target {
            "thinking" => options
                .iter()
                .find(|option| is_cursor_thinking_option(option)),
            "effort" => find_cursor_effort_option(options),
            "context" => options
                .iter()
                .find(|option| is_cursor_context_option(option)),
            "fast" => options.iter().find(|option| is_cursor_fast_option(option)),
            _ => None,
        })
        .cloned() else {
            continue;
        };
        let value = match target {
            "thinking" => {
                let Some(enabled) = cursor_desired_thinking(selection, reasoning_effort) else {
                    continue;
                };
                if cursor_flag_matches(&option, enabled) {
                    continue;
                }
                let Some(value) = cursor_flag_request_value(&option, enabled) else {
                    continue;
                };
                value
            }
            "effort" => {
                let Some(value) = cursor_desired_effort_value(&option, selection, reasoning_effort)
                else {
                    continue;
                };
                if session_config_current_value(&option) == Some(value.as_str()) {
                    continue;
                }
                SessionConfigOptionValue::value_id(value)
            }
            "context" => {
                let Some(value) = cursor_desired_context_value(&option, context_window) else {
                    continue;
                };
                if session_config_current_value(&option) == Some(value.as_str()) {
                    continue;
                }
                SessionConfigOptionValue::value_id(value)
            }
            "fast" => {
                let Some(enabled) = cursor_desired_fast(selection, service_tier) else {
                    continue;
                };
                if cursor_flag_matches(&option, enabled) {
                    continue;
                }
                let Some(value) = cursor_flag_request_value(&option, enabled) else {
                    continue;
                };
                value
            }
            _ => continue,
        };
        *options = await_acp_request(
            connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    option.id,
                    value,
                ))
                .block_task(),
            request_timeout,
            "ACP session/set_config_option",
        )
        .await?
        .config_options;
    }
    Ok(())
}

fn find_config_option(
    config_options: &[SessionConfigOption],
    category: SessionConfigOptionCategory,
) -> Option<&SessionConfigOption> {
    config_options
        .iter()
        .find(|option| option.category.as_ref() == Some(&category))
}

fn fx_model_option(config_options: &[SessionConfigOption]) -> Option<&SessionConfigOption> {
    config_options.iter().find(|option| {
        option.category == Some(SessionConfigOptionCategory::Model)
            && option.id.to_string().eq_ignore_ascii_case("model")
    })
}

fn fx_model_provider_switch<'a>(
    config_options: &'a [SessionConfigOption],
    model: &str,
) -> Option<(&'a SessionConfigOption, &'static str)> {
    if fx_model_option(config_options)
        .is_some_and(|option| session_config_select_values(option).contains(&model))
    {
        return None;
    }
    // Fx scopes model options to the selected account route. AI Gateway IDs
    // are provider/model pairs, while subscription IDs are flat. Selecting the
    // Gateway route returns a refreshed model option that contains these IDs.
    if !model.contains('/') {
        return None;
    }
    let provider = config_options.iter().find(|option| {
        option.category == Some(SessionConfigOptionCategory::Model)
            && option.id.to_string().eq_ignore_ascii_case("provider")
    })?;
    (session_config_current_value(provider) != Some("gateway")
        && session_config_select_values(provider).contains(&"gateway"))
    .then_some((provider, "gateway"))
}

/// Factory Droid's `autonomy_level` selector, whose values match its ACP mode
/// ids. Supervised is `normal`, which auto-approves only reads, so every other
/// permission request reaches the user. The other modes pre-authorize work at
/// the matching risk tier; `auto-high` keeps Droid's own command blocks.
/// The selector must always be written: an unset session inherits the user's
/// CLI default, which may already be `auto-high`.
fn droid_autonomy_level(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::Ask => "normal",
        RuntimeMode::AutoAcceptEdits => "auto-low",
        RuntimeMode::Auto => "auto-medium",
        RuntimeMode::FullAccess => "auto-high",
    }
}

fn config_option_by_id<'a>(
    options: &'a [SessionConfigOption],
    id: &str,
) -> Option<&'a SessionConfigOption> {
    options
        .iter()
        .find(|option| option.id.to_string().eq_ignore_ascii_case(id))
}

/// The values of Droid's `autonomy_level`, which are also its ACP mode ids.
const DROID_AUTONOMY_LEVELS: [&str; 4] = ["normal", "auto-low", "auto-medium", "auto-high"];

/// Whether Waku answers the agent's permission requests for the user.
///
/// Droid's autonomy level already pre-approves everything the selected mode
/// allows, so a request that still arrives is exactly what that mode keeps
/// for the user; answering it would turn every mode into Full access. Other
/// ACP agents stay in their ordinary mode and rely on Waku to answer.
fn auto_approves_permissions(provider: ProviderKind, mode: RuntimeMode) -> bool {
    match provider {
        ProviderKind::Droid => mode == RuntimeMode::FullAccess,
        _ => mode != RuntimeMode::Ask,
    }
}

/// Applies Antigravity's live ACP mode switch via `session/set_mode`.
/// `Ok(None)` means the native mode already matches `mode` (e.g. `Ask` and
/// `FullAccess` both use `default` while Waku's permission handler governs
/// approvals).
async fn apply_antigravity_mode(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    modes: Option<&SessionModeState>,
    mode: RuntimeMode,
    request_timeout: Duration,
) -> Result<Option<SessionModeId>, String> {
    let Some(modes) = modes else {
        return Err(tr!(
            "errors.set_session_option",
            error = "Antigravity did not advertise session modes"
        ));
    };
    let Some(desired) = desired_access_mode(ProviderKind::Antigravity, Some(modes), mode) else {
        return Ok(None);
    };
    await_acp_request(
        connection
            .send_request(SetSessionModeRequest::new(
                session_id.clone(),
                desired.clone(),
            ))
            .block_task(),
        request_timeout,
        "ACP session/set_mode",
    )
    .await
    .map(|_| Some(desired))
    .map_err(|error| tr!("errors.set_session_option", error = error))
}

/// Writes Droid's autonomy level for `mode`. `Ok(None)` means the session
/// already had it; `Err` carries a user-facing reason.
async fn apply_droid_autonomy(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    config_options: Option<&[SessionConfigOption]>,
    mode: RuntimeMode,
    request_timeout: Duration,
) -> Result<Option<Vec<SessionConfigOption>>, String> {
    let desired = droid_autonomy_level(mode);
    let Some(option) =
        config_options.and_then(|options| config_option_by_id(options, "autonomy_level"))
    else {
        return Err(tr!(
            "errors.set_session_option",
            error = "Factory Droid did not advertise its autonomy setting"
        ));
    };
    if !session_config_select_values(option).contains(&desired) {
        return Err(tr!(
            "errors.set_session_option",
            error = format!("Factory Droid did not advertise autonomy level {desired}")
        ));
    }
    if session_config_current_value(option) == Some(desired) {
        return Ok(None);
    }
    await_acp_request(
        connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id.clone(),
                desired,
            ))
            .block_task(),
        request_timeout,
        "ACP session/set_config_option",
    )
    .await
    .map(|response| Some(response.config_options))
    .map_err(|error| tr!("errors.set_session_option", error = error))
}

/// The session's config options as the agent last reported them. Responses
/// and `config_option_update` notifications both write here, so a later model
/// change compares against what the agent holds, not what Waku last sent.
type SharedConfigOptions = Arc<Mutex<Option<Vec<SessionConfigOption>>>>;

fn store_config_options(cache: &SharedConfigOptions, updated: Option<Vec<SessionConfigOption>>) {
    if let Some(updated) = updated {
        *cache.lock() = Some(updated);
    }
}

/// Records what a session update says about the session's own settings and
/// reports whether Droid's autonomy has left the level the access mode chose.
fn track_session_state(
    provider: ProviderKind,
    update: &SessionUpdate,
    mode: RuntimeMode,
    config_options: &SharedConfigOptions,
) -> bool {
    let desired = droid_autonomy_level(mode);
    match update {
        // Droid also reports its non-autonomy modes (such as `spec`) through
        // `autonomy_level` config updates; leave those user choices alone.
        SessionUpdate::ConfigOptionUpdate(update) => {
            *config_options.lock() = Some(update.config_options.clone());
            provider == ProviderKind::Droid
                && config_option_by_id(&update.config_options, "autonomy_level")
                    .and_then(session_config_current_value)
                    .is_some_and(|current| {
                        DROID_AUTONOMY_LEVELS.contains(&current) && current != desired
                    })
        }
        // Droid's mode ids are its autonomy levels. Other mode ids (its spec
        // mode) are the user's own choice and are left alone.
        SessionUpdate::CurrentModeUpdate(update) => {
            let current = update.current_mode_id.to_string();
            provider == ProviderKind::Droid
                && DROID_AUTONOMY_LEVELS.contains(&current.as_str())
                && current != desired
        }
        _ => false,
    }
}

/// Puts Droid's autonomy back at the access mode's level after the agent
/// moved it. Runs from the notification handler, so the reply is handled in
/// a callback rather than awaited.
fn restore_droid_autonomy(
    connection: &ConnectionTo<Agent>,
    session_id: SessionId,
    desired: &'static str,
    config_options: &SharedConfigOptions,
    events: &DriverEventSender,
) -> agent_client_protocol::Result<()> {
    let config_id = config_options
        .lock()
        .as_deref()
        .and_then(|options| config_option_by_id(options, "autonomy_level"))
        .map_or_else(|| "autonomy_level".into(), |option| option.id.clone());
    let cache = config_options.clone();
    let events = events.clone();
    let cancel = connection.clone();
    connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id.clone(),
            config_id,
            desired,
        ))
        .on_receiving_result(async move |result| {
            match result {
                Ok(response) => *cache.lock() = Some(response.config_options),
                Err(error) => {
                    // The session now runs at a level nobody chose; stop the
                    // turn rather than let it continue there.
                    let _ = events.send(DriverEvent::Error(tr!(
                        "errors.set_session_option",
                        error = error
                    )));
                    let _ = cancel.send_notification(CancelNotification::new(session_id));
                }
            }
            Ok(())
        })
}

async fn apply_droid_model(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    config_options: Option<&[SessionConfigOption]>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    events: &DriverEventSender,
    request_timeout: Duration,
) -> Option<Vec<SessionConfigOption>> {
    let mut options = config_options.unwrap_or_default().to_vec();
    let mut updated = false;
    if let Some(model) = model {
        let Some(option) = config_option_by_id(&options, "model").cloned() else {
            let _ = events.send(DriverEvent::Error(tr!(
                "errors.select_model",
                error = "Factory Droid did not advertise its model configuration"
            )));
            return None;
        };
        if !session_config_select_values(&option).contains(&model) {
            let _ = events.send(DriverEvent::Error(tr!(
                "errors.select_model",
                error = format!("Factory Droid did not advertise model {model}")
            )));
            return None;
        }
        if session_config_current_value(&option) != Some(model) {
            match await_acp_request(
                connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        option.id.clone(),
                        model,
                    ))
                    .block_task(),
                request_timeout,
                "ACP session/set_config_option",
            )
            .await
            {
                Ok(response) => {
                    options = response.config_options;
                    updated = true;
                }
                Err(error) => {
                    let _ = events.send(DriverEvent::Error(tr!(
                        "errors.select_model",
                        error = error
                    )));
                    return None;
                }
            }
        }
    }
    if let Some(effort) = reasoning_effort.filter(|effort| !effort.is_empty())
        && let Some(option) = config_option_by_id(&options, "reasoning_effort")
        && session_config_select_values(option).contains(&effort)
        && session_config_current_value(option) != Some(effort)
    {
        let config_id = option.id.clone();
        // Effort is optional. A rejected value must not discard the model the
        // session already accepted.
        if let Ok(response) = await_acp_request(
            connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    config_id,
                    effort,
                ))
                .block_task(),
            request_timeout,
            "ACP session/set_config_option",
        )
        .await
        {
            options = response.config_options;
            updated = true;
        }
    }
    updated.then_some(options)
}

/// Resolves a stored Antigravity model against the advertised families. A
/// family value is used as-is; a variant such as `gemini-4-carbon-medium`
/// reduces to its longest advertised prefix and carries the suffix's thinking
/// level as a fallback when the session has no explicit effort.
fn antigravity_model_selection(
    advertised: &[&str],
    model: &str,
) -> Option<(String, Option<String>)> {
    if advertised.contains(&model) {
        return Some((model.to_owned(), None));
    }
    let family = advertised
        .iter()
        .filter(|family| {
            model
                .strip_prefix(**family)
                .and_then(|suffix| suffix.strip_prefix('-'))
                .is_some_and(|suffix| !suffix.is_empty())
        })
        .max_by_key(|family| family.len())?;
    let suffix = model
        .strip_prefix(*family)
        .and_then(|suffix| suffix.strip_prefix('-'))
        .unwrap_or_default();
    let effort = suffix.rsplit('-').next().and_then(|level| {
        ["none", "minimal", "low", "medium", "high", "xhigh"]
            .contains(&level)
            .then(|| level.to_owned())
    });
    Some(((*family).to_owned(), effort))
}

/// Antigravity advertises model families as its `model` config option and the
/// selected family's thinking levels as `thought_level`; the full variant ids
/// from `models.availableModels` are accepted but not listed there. A stored
/// variant id is therefore reduced to its advertised family, with any effort
/// suffix used when the session has no explicit effort of its own.
async fn apply_antigravity_model(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    config_options: Option<&[SessionConfigOption]>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    events: &DriverEventSender,
    request_timeout: Duration,
) -> Option<Vec<SessionConfigOption>> {
    let mut options = config_options.unwrap_or_default().to_vec();
    let mut updated = false;
    let mut inferred_effort = None;
    if let Some(model) = model {
        let Some(model_option) = config_option_by_id(&options, "model").cloned() else {
            let _ = events.send(DriverEvent::Error(tr!(
                "errors.select_model",
                error = "Antigravity did not advertise its model configuration"
            )));
            return None;
        };
        let advertised = session_config_select_values(&model_option);
        let Some((selected, inferred)) = antigravity_model_selection(&advertised, model) else {
            let _ = events.send(DriverEvent::Error(tr!(
                "errors.select_model",
                error = format!("Antigravity did not advertise model {model}")
            )));
            return None;
        };
        inferred_effort = inferred;
        if session_config_current_value(&model_option) != Some(selected.as_str()) {
            match await_acp_request(
                connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        model_option.id.clone(),
                        selected.as_str(),
                    ))
                    .block_task(),
                request_timeout,
                "ACP session/set_config_option",
            )
            .await
            {
                Ok(response) => {
                    options = response.config_options;
                    updated = true;
                }
                Err(error) => {
                    let _ = events.send(DriverEvent::Error(tr!(
                        "errors.select_model",
                        error = error
                    )));
                    return None;
                }
            }
        }
    }
    let effort = reasoning_effort
        .filter(|effort| !effort.is_empty())
        .map(str::to_owned)
        .or(inferred_effort);
    if let Some(effort) = effort
        && let Some(option) = config_option_by_id(&options, "thought_level")
        && session_config_select_values(option).contains(&effort.as_str())
        && session_config_current_value(option) != Some(effort.as_str())
    {
        // Thinking is optional and per model; a rejected level must not drop
        // the model the session already accepted.
        if let Ok(response) = await_acp_request(
            connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    option.id.clone(),
                    effort.as_str(),
                ))
                .block_task(),
            request_timeout,
            "ACP session/set_config_option",
        )
        .await
        {
            options = response.config_options;
            updated = true;
        }
    }
    updated.then_some(options)
}

/// [`sync_session_model`] against the shared cache. Only an answer from the
/// agent is written back, so a notification that lands while the requests are
/// in flight is not overwritten by the snapshot they started from.
#[allow(clippy::too_many_arguments)]
async fn sync_cached_session_model(
    connection: &ConnectionTo<Agent>,
    provider: ProviderKind,
    session_id: &SessionId,
    cache: &SharedConfigOptions,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
    context_window: Option<&str>,
    events: &DriverEventSender,
    request_timeout: Duration,
) {
    let before = cache.lock().clone();
    let mut config_options = before.clone();
    sync_session_model(
        connection,
        provider,
        session_id,
        &mut config_options,
        model,
        reasoning_effort,
        service_tier,
        context_window,
        events,
        request_timeout,
    )
    .await;
    if config_options != before {
        store_config_options(cache, config_options);
    }
}

#[allow(clippy::too_many_arguments)]
async fn sync_session_model(
    connection: &ConnectionTo<Agent>,
    provider: ProviderKind,
    session_id: &SessionId,
    config_options: &mut Option<Vec<SessionConfigOption>>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
    context_window: Option<&str>,
    events: &DriverEventSender,
    request_timeout: Duration,
) {
    if provider == ProviderKind::Antigravity {
        if let Some(updated) = apply_antigravity_model(
            connection,
            session_id,
            config_options.as_deref(),
            model,
            reasoning_effort,
            events,
            request_timeout,
        )
        .await
        {
            *config_options = Some(updated);
        }
        return;
    }
    if provider == ProviderKind::Droid {
        if let Some(updated) = apply_droid_model(
            connection,
            session_id,
            config_options.as_deref(),
            model,
            reasoning_effort,
            events,
            request_timeout,
        )
        .await
        {
            *config_options = Some(updated);
        }
        return;
    }
    apply_model(
        connection,
        provider,
        session_id,
        config_options,
        model,
        reasoning_effort,
        service_tier,
        context_window,
        events,
        request_timeout,
    )
    .await;
}

fn set_model_params(
    session_id: &SessionId,
    model: &str,
    reasoning_effort: Option<&str>,
    provider: ProviderKind,
) -> serde_json::Value {
    let mut params = json!({"sessionId": session_id, "modelId": model});
    if provider == ProviderKind::Grok
        && let Some(effort) = reasoning_effort.filter(|effort| !effort.is_empty())
    {
        params["_meta"] = json!({"reasoningEffort": effort});
    }
    params
}

fn has_cursor_variant_options(options: &[SessionConfigOption]) -> bool {
    find_config_option(options, SessionConfigOptionCategory::Model).is_some()
        || options.iter().any(|option| {
            is_cursor_thinking_option(option)
                || is_cursor_effort_option(option)
                || is_cursor_context_option(option)
                || is_cursor_fast_option(option)
        })
}

#[allow(clippy::too_many_arguments)]
async fn apply_model(
    connection: &ConnectionTo<Agent>,
    provider: ProviderKind,
    session_id: &SessionId,
    config_options: &mut Option<Vec<SessionConfigOption>>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
    context_window: Option<&str>,
    events: &DriverEventSender,
    request_timeout: Duration,
) {
    if provider == ProviderKind::Cursor
        && let Some(options) = config_options.as_mut()
        && has_cursor_variant_options(options)
    {
        if let Some(model) = model {
            if let Some(option) =
                find_config_option(options, SessionConfigOptionCategory::Model).cloned()
                && let Some(selection) = cursor_model_selection(&option, model)
            {
                match await_acp_request(
                    connection
                        .send_request(SetSessionConfigOptionRequest::new(
                            session_id.clone(),
                            option.id.clone(),
                            selection.value.as_str(),
                        ))
                        .block_task(),
                    request_timeout,
                    "ACP session/set_config_option",
                )
                .await
                {
                    Ok(response) => {
                        *options = response.config_options;
                        if let Err(error) = apply_cursor_variant_configs(
                            connection,
                            session_id,
                            options,
                            &selection,
                            reasoning_effort,
                            service_tier,
                            context_window,
                            request_timeout,
                        )
                        .await
                        {
                            let _ = events.send(DriverEvent::Error(tr!(
                                "errors.select_model",
                                error = error
                            )));
                        }
                    }
                    Err(error) => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.select_model",
                            error = error
                        )));
                    }
                }
                return;
            }
        } else {
            let empty_selection = CursorModelSelection {
                value: String::new(),
                suffix: String::new(),
            };
            if let Err(error) = apply_cursor_variant_configs(
                connection,
                session_id,
                options,
                &empty_selection,
                reasoning_effort,
                service_tier,
                context_window,
                request_timeout,
            )
            .await
            {
                let _ = events.send(DriverEvent::Error(tr!(
                    "errors.select_model",
                    error = error
                )));
            }
            return;
        }
    }

    if provider == ProviderKind::Fx {
        let Some(model) = model else {
            return;
        };
        let mut options = config_options.as_deref().unwrap_or_default().to_vec();
        if let Some((provider_option, value)) = fx_model_provider_switch(&options, model) {
            let config_id = provider_option.id.clone();
            match await_acp_request(
                connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        config_id,
                        value,
                    ))
                    .block_task(),
                request_timeout,
                "ACP session/set_config_option",
            )
            .await
            {
                Ok(response) => {
                    options = response.config_options;
                    *config_options = Some(options.clone());
                }
                Err(error) => {
                    let _ = events.send(DriverEvent::Error(tr!(
                        "errors.select_model",
                        error = error
                    )));
                    return;
                }
            }
        }
        let Some(option) = fx_model_option(&options) else {
            let _ = events.send(DriverEvent::Error(tr!(
                "errors.select_model",
                error = "Fx did not advertise its model configuration"
            )));
            return;
        };
        if !session_config_select_values(option).contains(&model) {
            let _ = events.send(DriverEvent::Error(tr!(
                "errors.select_model",
                error = format!("Fx did not advertise model {model}")
            )));
            return;
        }
        match await_acp_request(
            connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    option.id.clone(),
                    model,
                ))
                .block_task(),
            request_timeout,
            "ACP session/set_config_option",
        )
        .await
        {
            Ok(response) => {
                *config_options = Some(response.config_options);
            }
            Err(error) => {
                let _ = events.send(DriverEvent::Error(tr!(
                    "errors.select_model",
                    error = error
                )));
            }
        }
        return;
    }

    // Grok, Kimi, and Cursor agents that do not advertise a model config
    // option retain the legacy request unchanged. Fx intentionally
    // stays on session/set_config_option, its documented model API.
    if let Some(model) = model {
        let request = match UntypedMessage::new(
            "session/set_model",
            set_model_params(session_id, model, reasoning_effort, provider),
        ) {
            Ok(request) => request,
            Err(error) => {
                let _ = events.send(DriverEvent::Error(tr!(
                    "errors.select_model",
                    error = error
                )));
                return;
            }
        };
        if let Err(error) = await_acp_request(
            connection.send_request(request).block_task(),
            request_timeout,
            "ACP session/set_model",
        )
        .await
        {
            let _ = events.send(DriverEvent::Error(tr!(
                "errors.select_model",
                error = error
            )));
            return;
        }
    }
    if provider != ProviderKind::Grok
        && let Some(effort) = reasoning_effort.filter(|effort| !effort.is_empty())
    {
        // Reasoning effort is an optional config extension and is deliberately
        // non-fatal when an agent does not expose it.
        if let Ok(response) = await_acp_request(
            connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    reasoning_effort_config_id(provider),
                    effort,
                ))
                .block_task(),
            request_timeout,
            "ACP session/set_config_option",
        )
        .await
        {
            *config_options = Some(response.config_options);
        }
    }
}

fn format_prompt_with_seed(
    provider: ProviderKind,
    fork_context: Option<&str>,
    computer_use_instructions: Option<&str>,
    text: String,
) -> String {
    let text = fork_context
        .map(|context| match provider {
            ProviderKind::Antigravity => {
                crate::antigravity_session::prompt_with_fork_context(context, &text)
            }
            _ => crate::cursor_session::prompt_with_fork_context(context, &text),
        })
        .unwrap_or(text);
    computer_use_instructions
        .map(|instructions| format!("{instructions}\n\n{text}"))
        .unwrap_or(text)
}

async fn resolve_native_turn_failure(
    kimi_offset: Option<u64>,
    antigravity_offset: Option<i64>,
    native_session_id: String,
    config_dir: Option<std::path::PathBuf>,
    produced_content: bool,
    stop_reason: Option<StopReason>,
) -> Option<String> {
    if let Some(offset) = kimi_offset {
        if produced_content {
            return None;
        }
        smol::unblock(move || crate::kimi_session::turn_failure(&native_session_id, offset)).await
    } else if let Some(offset) = antigravity_offset {
        let (refusal, empty_end_turn) = match stop_reason {
            Some(StopReason::Refusal) => (true, false),
            Some(StopReason::EndTurn) => (false, !produced_content),
            _ => (false, false),
        };
        smol::unblock(move || {
            crate::antigravity_session::turn_failure_in(
                &native_session_id,
                offset,
                refusal,
                empty_end_turn,
                config_dir.as_deref(),
            )
        })
        .await
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn send_prompt(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    text: String,
    cwd: &Path,
    attachment_roots: &[PathBuf],
    prompt_capabilities: &PromptCapabilities,
    prompt_requests: &PendingPromptRequests,
    events: &DriverEventSender,
    provider: ProviderKind,
    native_session_id: &str,
    config_dir: Option<std::path::PathBuf>,
    grok_title_home: Option<std::path::PathBuf>,
    title_refresh: super::title_refresh::NativeTitleRefresh,
    stream_state: Arc<Mutex<AcpStreamState>>,
    lost_session_recovery: Option<smol::channel::Sender<CommandMessage>>,
    prompt_kind: PromptKind,
    antigravity_step_offset: Option<Arc<AtomicI64>>,
) -> agent_client_protocol::Result<()> {
    if prompt_kind == PromptKind::Turn {
        let mut state = stream_state.lock();
        state.produced_content = false;
        state.tools.clear();
    }
    let retry_text = lost_session_recovery.as_ref().map(|_| text.clone());
    // Read before the turn runs, so the failure lookup cannot mistake an
    // earlier turn's record for this one's.
    let kimi_offset = (provider == ProviderKind::Kimi)
        .then(|| crate::kimi_session::wire_offset(native_session_id));
    let antigravity_offset = (provider == ProviderKind::Antigravity)
        .then(|| crate::antigravity_session::step_offset_in(native_session_id, config_dir.as_deref()));
    let extension_id =
        (provider == ProviderKind::Grok).then(|| format!("waku-{}", uuid::Uuid::new_v4()));
    let content_blocks =
        build_prompt_content_blocks_with_roots(&text, cwd, attachment_roots, prompt_capabilities);
    let mut request = PromptRequest::new(session_id.clone(), content_blocks);
    if let Some(extension_id) = extension_id.as_ref() {
        let mut meta = serde_json::Map::new();
        meta.insert("promptId".into(), Value::String(extension_id.clone()));
        meta.insert("requestId".into(), Value::String(extension_id.clone()));
        request = request.meta(meta);
    }
    let sent = connection.send_request(request);
    let request_id = sent.id().clone();
    prompt_requests.lock().insert(
        request_id.clone(),
        extension_id,
        native_session_id.to_owned(),
    );
    let callback_request_id = request_id.clone();
    let callback_requests = prompt_requests.clone();
    let callback_events = events.clone();
    let native_session_id = native_session_id.to_owned();
    let registered = sent.on_receiving_result(async move |result| {
        let is_lost_session = result
            .as_ref()
            .err()
            .is_some_and(|error| is_lost_native_session(&error.to_string()));
        let settled_last = settle_prompt_request(&callback_requests, &callback_request_id);
        // The resumed session no longer exists on the provider's side, so every
        // prompt to it fails the same way. Recover even when a concurrent Steer
        // is still in flight so the primary Turn is never dropped.
        if is_lost_session
            && let (Some(recovery), Some(text)) = (lost_session_recovery, retry_text)
            && recovery
                .try_send(CommandMessage::RecoverLostSession {
                    failed_session_id: native_session_id.clone(),
                    text,
                })
                .is_ok()
        {
            if settled_last {
                stream_state.lock().tools.clear();
            }
            return Ok(());
        }
        if settled_last {
            let produced_content = {
                let mut state = stream_state.lock();
                state.tools.clear();
                state.produced_content
            };
            // Only an empty turn pays for Kimi's lookup. Antigravity always
            // checks its own step log once, because it reports upstream API
            // failures as a chat message and a clean `end_turn`. Both lookups
            // poll with blocking sleeps/I/O, so offload them via `smol::unblock`
            // off the single-threaded `smol::block_on` executor.
            let stop_reason = result.as_ref().ok().map(|response| response.stop_reason);
            let native_failure = resolve_native_turn_failure(
                kimi_offset,
                antigravity_offset,
                native_session_id.clone(),
                config_dir.clone(),
                produced_content,
                stop_reason,
            )
            .await;
            if provider == ProviderKind::Antigravity
                && let Some(step_offset) = antigravity_step_offset
            {
                let sid = native_session_id.clone();
                let next_offset = smol::unblock(move || {
                    crate::antigravity_session::step_offset_in(&sid, config_dir.as_deref())
                })
                .await;
                step_offset.store(next_offset, Ordering::Release);
            }
            let success = finish_prompt(result, native_failure, &callback_events);
            if provider == ProviderKind::Grok && success {
                start_grok_title_refresh(
                    grok_title_home.as_deref(),
                    &native_session_id,
                    &title_refresh,
                    callback_events,
                );
            }
        }
        Ok(())
    });
    if registered.is_err() {
        prompt_requests.lock().settle_request(&request_id);
    }
    registered
}

/// Antigravity answers `session/load` for a conversation it never saved, then
/// rejects the first prompt with `not_found: trajectory not found: <id>` —
/// observed when its conversation store was unwritable, so the original turn
/// never persisted.
fn is_lost_native_session(error: &str) -> bool {
    error.contains("trajectory not found")
}

const MAX_PROMPT_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_PROMPT_TEXT_RESOURCE_BYTES: u64 = 256 * 1024;

fn strip_windows_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

fn validate_requested_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("Path cannot be empty".to_owned());
    }
    if path.to_string_lossy().contains('\0') {
        return Err("Path contains an invalid NUL character".to_owned());
    }
    let has_prefix_or_root = path
        .components()
        .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir));
    if has_prefix_or_root && !path.is_absolute() {
        return Err(format!(
            "Path `{}` uses an unsupported drive-relative or root-relative form",
            path.display()
        ));
    }
    #[cfg(windows)]
    {
        for component in path.components() {
            if let Component::Normal(part) = component
                && part.to_string_lossy().contains(':')
            {
                return Err(format!(
                    "Path `{}` contains an invalid colon in a filename component",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Resolves `path` component-by-component, canonicalizing every existing
/// segment (including symlinks and Windows junctions) and allowing a missing
/// tail of pure `Normal` segments. Fails closed on dangling symlinks and on
/// `..` segments following a non-existent segment.
fn canonicalize_allowing_missing(path: &Path) -> Result<PathBuf, String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("Failed to resolve current directory: {error}"))?
            .join(path)
    };
    let mut current = PathBuf::new();
    let mut missing = false;
    for component in abs.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => {
                current.push(Component::RootDir.as_os_str());
                if let Ok(canonical) = std::fs::canonicalize(&current) {
                    current = canonical;
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if missing || !current.pop() {
                    return Err(format!(
                        "Path `{}` traverses above a non-existent or root directory",
                        path.display()
                    ));
                }
            }
            Component::Normal(part) => {
                current.push(part);
                if !missing {
                    match std::fs::symlink_metadata(&current) {
                        Ok(_) => {
                            current = std::fs::canonicalize(&current).map_err(|error| {
                                format!("Failed to resolve `{}`: {error}", current.display())
                            })?;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            missing = true;
                        }
                        Err(error) => {
                            return Err(format!(
                                "Failed to inspect `{}`: {error}",
                                current.display()
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(current)
}

fn resolve_confined_path_with_canonical_root(
    workspace_root: &Path,
    workspace_exists: bool,
    requested: &Path,
    allow_workspace_root: bool,
) -> agent_client_protocol::Result<PathBuf> {
    validate_requested_path(requested).map_err(agent_client_protocol::util::internal_error)?;

    let resolved = if requested.is_absolute() {
        canonicalize_allowing_missing(requested)
            .map_err(agent_client_protocol::util::internal_error)?
    } else {
        let mut current = workspace_root.to_path_buf();
        let mut missing = !workspace_exists;
        for component in requested.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(agent_client_protocol::util::internal_error(format!(
                        "Path `{}` is outside the session workspace",
                        requested.display()
                    )));
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    if missing || !current.pop() {
                        return Err(agent_client_protocol::util::internal_error(format!(
                            "Path `{}` escapes the session workspace",
                            requested.display()
                        )));
                    }
                }
                Component::Normal(part) => {
                    current.push(part);
                    if !missing {
                        match std::fs::symlink_metadata(&current) {
                            Ok(_) => {
                                current = std::fs::canonicalize(&current).map_err(|error| {
                                    agent_client_protocol::util::internal_error(format!(
                                        "Failed to resolve `{}`: {error}",
                                        requested.display()
                                    ))
                                })?;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                missing = true;
                            }
                            Err(error) => {
                                return Err(agent_client_protocol::util::internal_error(format!(
                                    "Failed to inspect `{}`: {error}",
                                    requested.display()
                                )));
                            }
                        }
                    }
                }
            }
        }
        current
    };

    if !resolved.starts_with(workspace_root)
        || (!allow_workspace_root && resolved == workspace_root)
    {
        return Err(agent_client_protocol::util::internal_error(format!(
            "Path `{}` is outside the session workspace",
            requested.display()
        )));
    }

    Ok(strip_windows_verbatim(resolved))
}

fn resolve_confined_path(
    workspace_cwd: &Path,
    requested: &Path,
    allow_missing_workspace: bool,
    allow_workspace_root: bool,
) -> agent_client_protocol::Result<PathBuf> {
    let (workspace_root, workspace_exists) = if allow_missing_workspace {
        let root = canonicalize_allowing_missing(workspace_cwd)
            .map_err(agent_client_protocol::util::internal_error)?;
        let exists = match std::fs::symlink_metadata(&root) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(agent_client_protocol::util::internal_error(format!(
                    "Failed to inspect workspace `{}`: {error}",
                    workspace_cwd.display()
                )));
            }
        };
        (root, exists)
    } else {
        let root = std::fs::canonicalize(workspace_cwd).map_err(|error| {
            agent_client_protocol::util::internal_error(format!(
                "Failed to resolve workspace `{}`: {error}",
                workspace_cwd.display()
            ))
        })?;
        (root, true)
    };
    resolve_confined_path_with_canonical_root(
        &workspace_root,
        workspace_exists,
        requested,
        allow_workspace_root,
    )
}

fn resolve_workspace_read_path(
    workspace_cwd: &Path,
    requested: &Path,
) -> agent_client_protocol::Result<PathBuf> {
    resolve_confined_path(workspace_cwd, requested, false, false)
}

fn resolve_workspace_write_path(
    workspace_cwd: &Path,
    requested: &Path,
) -> agent_client_protocol::Result<PathBuf> {
    resolve_confined_path(workspace_cwd, requested, true, false)
}

fn resolve_workspace_dir(
    workspace_cwd: &Path,
    requested: Option<&Path>,
) -> agent_client_protocol::Result<PathBuf> {
    let resolved = match requested {
        Some(path) => resolve_confined_path(workspace_cwd, path, false, true)?,
        None => strip_windows_verbatim(std::fs::canonicalize(workspace_cwd).map_err(|error| {
            agent_client_protocol::util::internal_error(format!(
                "Failed to resolve workspace `{}`: {error}",
                workspace_cwd.display()
            ))
        })?),
    };
    let metadata = std::fs::metadata(&resolved).map_err(|error| {
        agent_client_protocol::util::internal_error(format!(
            "Terminal working directory `{}` is unavailable: {error}",
            resolved.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(agent_client_protocol::util::internal_error(format!(
            "Terminal working directory `{}` is not a directory",
            resolved.display()
        )));
    }
    Ok(resolved)
}

fn auto_allows_client_write(mode: RuntimeMode) -> bool {
    !matches!(mode, RuntimeMode::Ask)
}

fn auto_allows_client_terminal(mode: RuntimeMode) -> bool {
    matches!(mode, RuntimeMode::Auto | RuntimeMode::FullAccess)
}

fn is_blocked_terminal_env_var(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.contains(['=', '\0']) {
        return true;
    }
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("LD_") || upper.starts_with("DYLD_") || upper.starts_with("BASH_FUNC_") {
        return true;
    }
    matches!(
        upper.as_str(),
        "PATH"
            | "PATHEXT"
            | "COMSPEC"
            | "SYSTEMROOT"
            | "WINDIR"
            | "APPINIT_DLLS"
            | "BASH_ENV"
            | "ENV"
            | "CDPATH"
            | "GLOBIGNORE"
            | "PROMPT_COMMAND"
            | "IFS"
            | "ZDOTDIR"
            | "SHELLOPTS"
            | "BASHOPTS"
            | "GCONV_PATH"
            | "LOCPATH"
            | "HOSTALIASES"
            | "RESOLV_HOST_CONF"
            | "RES_OPTIONS"
            | "NODE_OPTIONS"
            | "NODE_PATH"
            | "PYTHONSTARTUP"
            | "PYTHONPATH"
            | "PYTHONHOME"
            | "RUBYOPT"
            | "RUBYLIB"
            | "PERL5OPT"
            | "PERL5LIB"
            | "PERLLIB"
            | "JAVA_TOOL_OPTIONS"
            | "_JAVA_OPTIONS"
            | "JDK_JAVA_OPTIONS"
            | "DOTNET_STARTUP_HOOKS"
            | "CORECLR_PROFILER"
            | "CORECLR_PROFILER_PATH"
            | "COR_PROFILER"
            | "COR_PROFILER_PATH"
            | "GIT_SSH_COMMAND"
            | "GIT_SSH"
            | "GIT_EXEC_PATH"
            | "GIT_TEMPLATE_DIR"
            | "GIT_EXTERNAL_DIFF"
            | "GIT_ASKPASS"
            | "SSH_ASKPASS"
            | "EDITOR"
            | "VISUAL"
    )
}

fn sanitized_terminal_env(
    env: &[agent_client_protocol::schema::v1::EnvVariable],
) -> Vec<(&str, &str)> {
    env.iter()
        .filter(|var| !is_blocked_terminal_env_var(&var.name))
        .map(|var| (var.name.as_str(), var.value.as_str()))
        .collect()
}

const MAX_MENTION_SPACE_SEGMENTS: usize = 32;

fn daemon_attachment_roots() -> Vec<PathBuf> {
    let Some(data_dir) = crate::persistence::StateStore::default_path()
        .parent()
        .map(Path::to_path_buf)
    else {
        return Vec::new();
    };
    vec![data_dir.join("blobs"), data_dir.join("attachments")]
}

fn resolve_prompt_attachment_path(
    canonical_cwd: Option<&Path>,
    canonical_attachment_roots: &[PathBuf],
    mention: &str,
) -> Option<(PathBuf, std::fs::Metadata)> {
    if mention.is_empty() {
        return None;
    }
    let candidate = Path::new(mention);
    let resolved = canonical_cwd
        .and_then(|root| resolve_confined_path_with_canonical_root(root, true, candidate, false).ok())
        .or_else(|| {
            if !candidate.is_absolute() {
                return None;
            }
            canonical_attachment_roots.iter().find_map(|root| {
                resolve_confined_path_with_canonical_root(root, true, candidate, false).ok()
            })
        })?;
    let metadata = std::fs::metadata(&resolved).ok()?;
    metadata.is_file().then_some((resolved, metadata))
}

fn is_mention_prefix_boundary(text: &str, at_idx: usize) -> bool {
    let Some(prev) = text[..at_idx].chars().next_back() else {
        return true;
    };
    prev.is_whitespace() || matches!(prev, '(' | '[' | '"' | '\'')
}

fn find_next_mention_boundary(text: &str) -> usize {
    let line_end = text.find(['\n', '\r']).unwrap_or(text.len());
    if let Some(quote) = text.chars().next().filter(|ch| matches!(ch, '"' | '\'')) {
        let qlen = quote.len_utf8();
        if let Some(rel_close) = text[qlen..line_end].find(quote) {
            return qlen + rel_close + qlen;
        }
        return line_end;
    }
    let bytes = text.as_bytes();
    for (idx, ch) in text[..line_end].char_indices() {
        if ch.is_whitespace() {
            let next_start = idx + ch.len_utf8();
            if bytes.get(next_start) == Some(&b'@') {
                return idx;
            }
        }
    }
    line_end
}

fn mention_end_offsets(slice: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut in_word = false;
    for (idx, ch) in slice.char_indices() {
        if ch.is_whitespace() {
            if in_word {
                offsets.push(idx);
                if offsets.len() >= MAX_MENTION_SPACE_SEGMENTS {
                    return offsets;
                }
                in_word = false;
            }
        } else {
            in_word = true;
        }
    }
    if in_word && offsets.len() < MAX_MENTION_SPACE_SEGMENTS {
        offsets.push(slice.len());
    }
    offsets
}

fn resolve_mention_candidate<'a>(
    slice: &'a str,
    canonical_cwd: Option<&Path>,
    canonical_attachment_roots: &[PathBuf],
) -> Option<(&'a str, PathBuf, std::fs::Metadata, usize)> {
    const TRAILING_PUNCTUATION: [char; 8] = [',', ';', ':', '.', ')', ']', '"', '\''];

    if let Some(quote) = slice.chars().next().filter(|ch| matches!(ch, '"' | '\'')) {
        let line = slice.split(['\n', '\r']).next().unwrap_or("");
        if let Some(rel_close) = line[quote.len_utf8()..].find(quote) {
            let inner = &line[quote.len_utf8()..quote.len_utf8() + rel_close];
            if let Some((resolved, metadata)) =
                resolve_prompt_attachment_path(canonical_cwd, canonical_attachment_roots, inner)
            {
                return Some((
                    inner,
                    resolved,
                    metadata,
                    quote.len_utf8() + rel_close + quote.len_utf8(),
                ));
            }
        }
    }

    for end in mention_end_offsets(slice).into_iter().rev() {
        let raw = &slice[..end];
        if let Some((resolved, metadata)) =
            resolve_prompt_attachment_path(canonical_cwd, canonical_attachment_roots, raw)
        {
            return Some((raw, resolved, metadata, end));
        }
        let trimmed = raw.trim_end_matches(TRAILING_PUNCTUATION);
        if trimmed != raw
            && let Some((resolved, metadata)) =
                resolve_prompt_attachment_path(canonical_cwd, canonical_attachment_roots, trimmed)
        {
            return Some((trimmed, resolved, metadata, trimmed.len()));
        }
    }
    None
}

#[cfg(test)]
pub(crate) fn build_prompt_content_blocks(
    text: &str,
    cwd: &Path,
    capabilities: &PromptCapabilities,
) -> Vec<ContentBlock> {
    build_prompt_content_blocks_with_roots(text, cwd, &daemon_attachment_roots(), capabilities)
}

pub(crate) fn build_prompt_content_blocks_with_roots(
    text: &str,
    cwd: &Path,
    allowed_attachment_roots: &[PathBuf],
    capabilities: &PromptCapabilities,
) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::Text(TextContent::new(text.to_owned()))];
    if !capabilities.image && !capabilities.embedded_context {
        return blocks;
    }

    let canonical_cwd = std::fs::canonicalize(cwd).ok();
    let canonical_attachment_roots: Vec<PathBuf> = allowed_attachment_roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    if canonical_cwd.is_none() && canonical_attachment_roots.is_empty() {
        return blocks;
    }

    let mut seen_paths = HashSet::new();
    let mut cursor = 0;
    while let Some(rel_at) = text[cursor..].find('@') {
        let at_idx = cursor + rel_at;
        if !is_mention_prefix_boundary(text, at_idx) {
            cursor = at_idx + 1;
            continue;
        }
        let after_at = &text[at_idx + 1..];
        let slice_end = find_next_mention_boundary(after_at);
        let slice = &after_at[..slice_end];
        let Some((mention, resolved, metadata, consumed)) = resolve_mention_candidate(
            slice,
            canonical_cwd.as_deref(),
            &canonical_attachment_roots,
        ) else {
            cursor = at_idx + 1;
            continue;
        };
        cursor = at_idx + 1 + consumed;
        if !seen_paths.insert(resolved.clone()) {
            continue;
        }

        if let Some(mime_type) = image_mime_type(&resolved) {
            if capabilities.image
                && metadata.len() <= MAX_PROMPT_IMAGE_BYTES
                && let Ok(bytes) = std::fs::read(&resolved)
            {
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                let mut image = ImageContent::new(encoded, mime_type);
                if let Ok(uri) = url::Url::from_file_path(&resolved) {
                    image = image.uri(uri.to_string());
                }
                blocks.push(ContentBlock::Image(image));
            }
            continue;
        }

        if capabilities.embedded_context
            && let Ok(uri) = url::Url::from_file_path(&resolved)
        {
            if metadata.len() <= MAX_PROMPT_TEXT_RESOURCE_BYTES
                && let Ok(contents) = std::fs::read_to_string(&resolved)
            {
                let resource = TextResourceContents::new(contents, uri.to_string());
                blocks.push(ContentBlock::Resource(EmbeddedResource::new(
                    EmbeddedResourceResource::TextResourceContents(resource),
                )));
            } else {
                let name = resolved
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(mention)
                    .to_owned();
                blocks.push(ContentBlock::ResourceLink(
                    ResourceLink::new(name, uri.to_string()).size(metadata.len() as i64),
                ));
            }
        }
    }
    blocks
}

fn image_mime_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        "ico" => Some("image/x-icon"),
        "tif" | "tiff" => Some("image/tiff"),
        _ => None,
    }
}

pub(crate) fn handle_read_text_file(
    request: ReadTextFileRequest,
    default_cwd: &Path,
) -> agent_client_protocol::Result<ReadTextFileResponse> {
    let path = resolve_workspace_read_path(default_cwd, &request.path)?;
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        agent_client_protocol::util::internal_error(format!(
            "Failed to read {}: {error}",
            path.display()
        ))
    })?;
    let content = if request.line.is_some() || request.limit.is_some() {
        let start_index = request.line.unwrap_or(1).saturating_sub(1) as usize;
        let limit = request.limit.map_or(usize::MAX, |limit| limit as usize);
        raw.lines()
            .skip(start_index)
            .take(limit)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        raw
    };
    Ok(ReadTextFileResponse::new(content))
}

fn write_text_file_at(
    path: &Path,
    content: &str,
) -> agent_client_protocol::Result<WriteTextFileResponse> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            agent_client_protocol::util::internal_error(format!(
                "Failed to create parent directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    std::fs::write(path, content).map_err(|error| {
        agent_client_protocol::util::internal_error(format!(
            "Failed to write {}: {error}",
            path.display()
        ))
    })?;
    Ok(WriteTextFileResponse::new())
}

pub(crate) fn handle_write_text_file(
    request: WriteTextFileRequest,
    default_cwd: &Path,
) -> agent_client_protocol::Result<WriteTextFileResponse> {
    let path = resolve_workspace_write_path(default_cwd, &request.path)?;
    write_text_file_at(&path, &request.content)
}

fn handle_write_text_file_request(
    request: WriteTextFileRequest,
    default_cwd: &Path,
    access_mode: RuntimeMode,
    pending: &PendingPermissions,
    events: &impl DriverEventSink,
    responder: Responder<WriteTextFileResponse>,
) -> agent_client_protocol::Result<()> {
    let resolved = match resolve_workspace_write_path(default_cwd, &request.path) {
        Ok(path) => path,
        Err(error) => return responder.respond_with_error(error),
    };
    if auto_allows_client_write(access_mode) {
        return match write_text_file_at(&resolved, &request.content) {
            Ok(response) => responder.respond(response),
            Err(error) => responder.respond_with_error(error),
        };
    }

    let request_id = responder.id().to_string();
    let display_path = resolved
        .strip_prefix(default_cwd)
        .unwrap_or(&resolved)
        .display()
        .to_string();
    pending.lock().insert(
        request_id.clone(),
        PendingPermission::WriteFile {
            request,
            cwd: default_cwd.to_path_buf(),
            responder,
        },
    );
    if events
        .send(DriverEvent::Permission {
            request_id: request_id.clone(),
            title: tr!("permission.run_tool", tool = "fs/write_text_file"),
            detail: tr!(
                "permission.agent_wants_to",
                action = format!("write {display_path}")
            ),
            options: client_permission_options(),
        })
        .is_err()
        && let Some(PendingPermission::WriteFile { responder, .. }) =
            pending.lock().remove(&request_id)
    {
        let _ = responder.respond_with_error(agent_client_protocol::util::internal_error(
            "File write cancelled",
        ));
    }
    Ok(())
}

fn handle_create_terminal_request(
    request: CreateTerminalRequest,
    default_cwd: &Path,
    access_mode: RuntimeMode,
    registry: &Arc<AcpTerminalRegistry>,
    pending: &PendingPermissions,
    events: &DriverEventSender,
    responder: Responder<CreateTerminalResponse>,
) -> agent_client_protocol::Result<()> {
    if let Err(error) = resolve_workspace_dir(default_cwd, request.cwd.as_deref()) {
        return responder.respond_with_error(error);
    }
    if auto_allows_client_terminal(access_mode) {
        return match registry.create_terminal(request, default_cwd, access_mode, events) {
            Ok(response) => responder.respond(response),
            Err(error) => responder.respond_with_error(error),
        };
    }

    let request_id = responder.id().to_string();
    let command_display = if request.args.is_empty() {
        request.command.clone()
    } else {
        format!("{} {}", request.command, request.args.join(" "))
    };
    pending.lock().insert(
        request_id.clone(),
        PendingPermission::CreateTerminal {
            request,
            cwd: default_cwd.to_path_buf(),
            registry: registry.clone(),
            events: events.clone(),
            responder,
        },
    );
    if events
        .send(DriverEvent::Permission {
            request_id: request_id.clone(),
            title: tr!("permission.run_tool", tool = truncate(&command_display, 80)),
            detail: tr!(
                "permission.agent_wants_to_run",
                tool = truncate(&command_display, 200)
            ),
            options: client_permission_options(),
        })
        .is_err()
        && let Some(PendingPermission::CreateTerminal { responder, .. }) =
            pending.lock().remove(&request_id)
    {
        let _ = responder.respond_with_error(agent_client_protocol::util::internal_error(
            "Terminal execution cancelled",
        ));
    }
    Ok(())
}

const DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT: usize = 1_048_576;
const MAX_COMPLETED_TERMINALS: usize = 64;

struct AcpTerminalProcess {
    id: String,
    title: String,
    command_display: String,
    cwd_display: String,
    started_at_ms: u64,
    updated_at_ms: u64,
    completed_seq: Option<u64>,
    output: String,
    output_truncated: bool,
    output_byte_limit: usize,
    exit_status: Option<TerminalExitStatus>,
    killed: bool,
    child_pid: u32,
    #[cfg(windows)]
    job: Option<crate::process_tree::KillOnCloseJob>,
    waiters: Vec<Responder<WaitForTerminalExitResponse>>,
}

impl AcpTerminalProcess {
    fn append_output(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.updated_at_ms = unix_time_millis();
        if self.output_byte_limit == 0 {
            self.output.clear();
            self.output_truncated = true;
            return;
        }
        if chunk.len() >= self.output_byte_limit {
            let start = chunk.floor_char_boundary(chunk.len() - self.output_byte_limit);
            self.output.clear();
            self.output.push_str(&chunk[start..]);
            self.output_truncated = true;
            return;
        }
        self.output.push_str(chunk);
        if self.output.len() > self.output_byte_limit {
            self.output_truncated = true;
            let slack = (self.output_byte_limit / 4).max(65_536);
            if self.output.len() > self.output_byte_limit.saturating_add(slack) {
                let excess = self.output.len() - self.output_byte_limit;
                let boundary = self.output.ceil_char_boundary(excess);
                self.output.drain(..boundary);
            }
        }
    }

    fn visible_output(&self) -> &str {
        if self.output.len() <= self.output_byte_limit {
            &self.output
        } else {
            let start = self
                .output
                .ceil_char_boundary(self.output.len() - self.output_byte_limit);
            &self.output[start..]
        }
    }

    fn background_work_status(&self) -> BackgroundWorkStatus {
        match &self.exit_status {
            None => BackgroundWorkStatus::Running,
            Some(_) if self.killed => BackgroundWorkStatus::Stopped,
            Some(status) if status.exit_code == Some(0) && status.signal.is_none() => {
                BackgroundWorkStatus::Completed
            }
            Some(_) => BackgroundWorkStatus::Failed,
        }
    }

    fn to_background_work_item(&self) -> BackgroundWorkItem {
        let status = self.background_work_status();
        let duration_ms = self
            .exit_status
            .as_ref()
            .map(|_| self.updated_at_ms.saturating_sub(self.started_at_ms));
        let exit_code = self
            .exit_status
            .as_ref()
            .and_then(|status| status.exit_code)
            .and_then(|code| i32::try_from(code).ok());
        BackgroundWorkItem {
            key: BackgroundWorkKey::new(BackgroundWorkKind::Process, self.id.clone()),
            title: self.title.clone(),
            detail: None,
            command: Some(self.command_display.clone()),
            cwd: Some(self.cwd_display.clone()),
            output: {
                let visible = self.visible_output();
                (!visible.is_empty()).then(|| visible.to_owned())
            },
            output_truncated: self.output_truncated,
            started_at_ms: self.started_at_ms,
            updated_at_ms: self.updated_at_ms,
            duration_ms,
            exit_code,
            background: true,
            can_stop: status.is_stoppable(),
            control_id: Some(self.id.clone()),
            origin_activity_id: None,
            role: None,
            model: None,
            parent_id: None,
            status,
        }
    }

    fn kill_process_tree(&mut self) {
        self.killed = true;
        let pid = self.child_pid;
        #[cfg(windows)]
        {
            drop(self.job.take());
            if pid != 0 {
                unsafe {
                    use windows_sys::Win32::Foundation::CloseHandle;
                    use windows_sys::Win32::System::Threading::{
                        OpenProcess, PROCESS_TERMINATE, TerminateProcess,
                    };
                    let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
                    if !process.is_null() {
                        let _ = TerminateProcess(process, 1);
                        CloseHandle(process);
                    }
                }
            }
        }
        #[cfg(unix)]
        {
            if pid != 0 {
                unsafe {
                    let _ = libc::kill(-(pid as i32), libc::SIGKILL);
                    let _ = libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct AcpTerminalRegistry {
    terminals: Arc<Mutex<HashMap<String, Arc<Mutex<AcpTerminalProcess>>>>>,
    next_completed_seq: Arc<AtomicU64>,
}

fn evict_completed_terminals_in(
    terminals: &Mutex<HashMap<String, Arc<Mutex<AcpTerminalProcess>>>>,
    max_completed: usize,
) {
    let mut map = terminals.lock();
    let mut completed = Vec::new();
    for (id, entry) in map.iter() {
        let proc = entry.lock();
        if proc.exit_status.is_some() && proc.waiters.is_empty() {
            completed.push((
                proc.completed_seq.unwrap_or(0),
                proc.updated_at_ms,
                proc.started_at_ms,
                id.clone(),
            ));
        }
    }
    if completed.len() <= max_completed {
        return;
    }
    completed.sort();
    let remove_count = completed.len() - max_completed;
    for (_, _, _, id) in completed.into_iter().take(remove_count) {
        map.remove(&id);
    }
}

fn terminal_command_search_path(command: &str, work_dir: &Path) -> Option<OsString> {
    let raw = Path::new(command);
    let resolved = if raw.is_relative() && raw.components().count() > 1 {
        work_dir.join(raw)
    } else {
        raw.to_path_buf()
    };
    child_search_path_for(&resolved)
}

impl AcpTerminalRegistry {
    fn spawn_command(
        request: &CreateTerminalRequest,
        work_dir: &Path,
    ) -> std::io::Result<std::process::Child> {
        let env_vars = sanitized_terminal_env(&request.env);
        let search_path = terminal_command_search_path(&request.command, work_dir);
        let configure = |cmd: &mut std::process::Command| {
            cmd.current_dir(work_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .envs(crate::command_env::shell_environment());
            if let Some(search_path) = search_path.as_deref() {
                cmd.env("PATH", search_path);
            }
            for (name, value) in &env_vars {
                cmd.env(name, value);
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt as _;
                cmd.process_group(0);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt as _;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
        };

        let mut direct = std::process::Command::new(&request.command);
        direct.args(&request.args);
        configure(&mut direct);
        match direct.spawn() {
            Ok(child) => Ok(child),
            Err(error)
                if request.args.is_empty() && error.kind() == std::io::ErrorKind::NotFound =>
            {
                #[cfg(windows)]
                let mut fallback = {
                    let mut cmd = std::process::Command::new("cmd.exe");
                    cmd.args(["/C", &request.command]);
                    cmd
                };
                #[cfg(not(windows))]
                let mut fallback = {
                    let mut cmd = std::process::Command::new("/bin/sh");
                    cmd.args(["-c", &request.command]);
                    cmd
                };
                configure(&mut fallback);
                fallback.spawn()
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn create_terminal(
        &self,
        request: CreateTerminalRequest,
        default_cwd: &Path,
        access_mode: RuntimeMode,
        events: &DriverEventSender,
    ) -> agent_client_protocol::Result<CreateTerminalResponse> {
        let work_dir = resolve_workspace_dir(default_cwd, request.cwd.as_deref())?;
        if !auto_allows_client_terminal(access_mode) {
            return Err(agent_client_protocol::util::internal_error(
                "Terminal execution requires user approval in the current access mode",
            ));
        }
        self.create_terminal_in_dir(request, work_dir, events)
    }

    fn create_terminal_authorized(
        &self,
        request: CreateTerminalRequest,
        default_cwd: &Path,
        events: &DriverEventSender,
    ) -> agent_client_protocol::Result<CreateTerminalResponse> {
        let work_dir = resolve_workspace_dir(default_cwd, request.cwd.as_deref())?;
        self.create_terminal_in_dir(request, work_dir, events)
    }

    fn create_terminal_in_dir(
        &self,
        request: CreateTerminalRequest,
        work_dir: PathBuf,
        events: &DriverEventSender,
    ) -> agent_client_protocol::Result<CreateTerminalResponse> {
        let mut child = Self::spawn_command(&request, &work_dir).map_err(|error| {
            agent_client_protocol::util::internal_error(format!(
                "Failed to spawn terminal command `{}`: {error}",
                request.command
            ))
        })?;
        let child_pid = child.id();
        #[cfg(windows)]
        let job = crate::process_tree::KillOnCloseJob::adopt(child_pid).ok();

        let id = format!("term-{}", uuid::Uuid::new_v4());
        let command_display = if request.args.is_empty() {
            request.command.clone()
        } else {
            format!("{} {}", request.command, request.args.join(" "))
        };
        let title = truncate(&command_display, 80);
        let output_byte_limit = request
            .output_byte_limit
            .and_then(|limit| usize::try_from(limit).ok())
            .unwrap_or(DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT);
        let now = unix_time_millis();
        let entry = Arc::new(Mutex::new(AcpTerminalProcess {
            id: id.clone(),
            title,
            command_display,
            cwd_display: work_dir.display().to_string(),
            started_at_ms: now,
            updated_at_ms: now,
            completed_seq: None,
            output: String::new(),
            output_truncated: false,
            output_byte_limit,
            exit_status: None,
            killed: false,
            child_pid,
            #[cfg(windows)]
            job,
            waiters: Vec::new(),
        }));
        self.terminals.lock().insert(id.clone(), entry.clone());
        evict_completed_terminals_in(&self.terminals, MAX_COMPLETED_TERMINALS);
        let _ = events.send(DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(
            entry.lock().to_background_work_item(),
        )));

        let stdout_handle = child.stdout.take().and_then(|stdout| {
            Self::spawn_output_reader(id.clone(), stdout, entry.clone(), events.clone())
        });
        let stderr_handle = child.stderr.take().and_then(|stderr| {
            Self::spawn_output_reader(id.clone(), stderr, entry.clone(), events.clone())
        });
        let wait_entry = entry;
        let wait_events = events.clone();
        let wait_terminals = Arc::clone(&self.terminals);
        let wait_completed_seq = Arc::clone(&self.next_completed_seq);
        let _ = thread::Builder::new()
            .name("waku-acp-term-wait".into())
            .spawn(move || {
                let wait_result = child.wait();
                {
                    let mut proc = wait_entry.lock();
                    proc.child_pid = 0;
                    #[cfg(windows)]
                    drop(proc.job.take());
                }
                drop(child);
                if let Some(handle) = stdout_handle {
                    let _ = handle.join();
                }
                if let Some(handle) = stderr_handle {
                    let _ = handle.join();
                }
                let status = match wait_result {
                    Ok(exit) => {
                        let code = exit.code().and_then(|code| u32::try_from(code).ok());
                        #[cfg(unix)]
                        let signal = {
                            use std::os::unix::process::ExitStatusExt as _;
                            exit.signal().map(|sig| sig.to_string())
                        };
                        #[cfg(not(unix))]
                        let signal: Option<String> = None;
                        TerminalExitStatus::new().exit_code(code).signal(signal)
                    }
                    Err(_) => TerminalExitStatus::new(),
                };
                let (item, waiters) = {
                    let mut proc = wait_entry.lock();
                    proc.updated_at_ms = unix_time_millis();
                    proc.exit_status = Some(status.clone());
                    if proc.completed_seq.is_none() {
                        proc.completed_seq =
                            Some(wait_completed_seq.fetch_add(1, Ordering::Relaxed) + 1);
                    }
                    let waiters = std::mem::take(&mut proc.waiters);
                    (proc.to_background_work_item(), waiters)
                };
                for waiter in waiters {
                    let _ = waiter.respond(WaitForTerminalExitResponse::new(status.clone()));
                }
                evict_completed_terminals_in(&wait_terminals, MAX_COMPLETED_TERMINALS);
                let _ = wait_events.send(DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(
                    item,
                )));
            });

        Ok(CreateTerminalResponse::new(id))
    }

    fn spawn_output_reader<R: std::io::Read + Send + 'static>(
        terminal_id: String,
        mut reader: R,
        entry: Arc<Mutex<AcpTerminalProcess>>,
        events: DriverEventSender,
    ) -> Option<thread::JoinHandle<()>> {
        thread::Builder::new()
            .name("waku-acp-term-out".into())
            .spawn(move || {
                let mut buffer = [0u8; 4096];
                let mut pending_len = 0usize;
                loop {
                    match reader.read(&mut buffer[pending_len..]) {
                        Ok(0) | Err(_) => {
                            if pending_len > 0 {
                                let delta =
                                    String::from_utf8_lossy(&buffer[..pending_len]).into_owned();
                                entry.lock().append_output(&delta);
                                let _ = events.send(DriverEvent::BackgroundWork(
                                    BackgroundWorkEvent::OutputDelta {
                                        key: BackgroundWorkKey::new(
                                            BackgroundWorkKind::Process,
                                            terminal_id.clone(),
                                        ),
                                        delta,
                                    },
                                ));
                            }
                            break;
                        }
                        Ok(read) => {
                            let total = pending_len + read;
                            let valid_len = match std::str::from_utf8(&buffer[..total]) {
                                Ok(_) => total,
                                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                                Err(_) => total,
                            };
                            if valid_len > 0 {
                                let delta =
                                    String::from_utf8_lossy(&buffer[..valid_len]).into_owned();
                                entry.lock().append_output(&delta);
                                let _ = events.send(DriverEvent::BackgroundWork(
                                    BackgroundWorkEvent::OutputDelta {
                                        key: BackgroundWorkKey::new(
                                            BackgroundWorkKind::Process,
                                            terminal_id.clone(),
                                        ),
                                        delta,
                                    },
                                ));
                            }
                            let leftover = total - valid_len;
                            if leftover > 0 {
                                buffer.copy_within(valid_len..total, 0);
                            }
                            pending_len = leftover;
                        }
                    }
                }
            })
            .ok()
    }

    pub(crate) fn terminal_output(
        &self,
        request: TerminalOutputRequest,
    ) -> agent_client_protocol::Result<TerminalOutputResponse> {
        let terminal_id = request.terminal_id.to_string();
        let entry = self
            .terminals
            .lock()
            .get(&terminal_id)
            .cloned()
            .ok_or_else(|| {
                agent_client_protocol::util::internal_error(format!(
                    "Unknown terminal ID `{terminal_id}`"
                ))
            })?;
        let proc = entry.lock();
        Ok(
            TerminalOutputResponse::new(proc.visible_output().to_owned(), proc.output_truncated)
                .exit_status(proc.exit_status.clone()),
        )
    }

    pub(crate) fn output_text(&self, terminal_id: &str) -> Option<String> {
        let entry = self.terminals.lock().get(terminal_id).cloned()?;
        let proc = entry.lock();
        let trimmed = proc.visible_output().trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    pub(crate) fn wait_for_exit(
        &self,
        request: WaitForTerminalExitRequest,
        responder: Responder<WaitForTerminalExitResponse>,
    ) -> agent_client_protocol::Result<()> {
        let terminal_id = request.terminal_id.to_string();
        let Some(entry) = self.terminals.lock().get(&terminal_id).cloned() else {
            return responder.respond_with_error(agent_client_protocol::util::internal_error(
                format!("Unknown terminal ID `{terminal_id}`"),
            ));
        };
        let mut proc = entry.lock();
        if let Some(status) = proc.exit_status.clone() {
            drop(proc);
            return responder.respond(WaitForTerminalExitResponse::new(status));
        }
        proc.waiters.push(responder);
        Ok(())
    }

    pub(crate) fn kill_terminal(
        &self,
        request: KillTerminalRequest,
        events: &DriverEventSender,
    ) -> agent_client_protocol::Result<KillTerminalResponse> {
        let terminal_id = request.terminal_id.to_string();
        let entry = self
            .terminals
            .lock()
            .get(&terminal_id)
            .cloned()
            .ok_or_else(|| {
                agent_client_protocol::util::internal_error(format!(
                    "Unknown terminal ID `{terminal_id}`"
                ))
            })?;
        let item = {
            let mut proc = entry.lock();
            if proc.exit_status.is_none() {
                proc.kill_process_tree();
                proc.updated_at_ms = unix_time_millis();
            }
            proc.to_background_work_item()
        };
        let _ = events.send(DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(
            item,
        )));
        Ok(KillTerminalResponse::new())
    }

    pub(crate) fn release_terminal(
        &self,
        request: ReleaseTerminalRequest,
        events: &DriverEventSender,
    ) -> agent_client_protocol::Result<ReleaseTerminalResponse> {
        let terminal_id = request.terminal_id.to_string();
        if let Some(entry) = self.terminals.lock().remove(&terminal_id) {
            let (item, waiters) = {
                let mut proc = entry.lock();
                if proc.exit_status.is_none() {
                    proc.kill_process_tree();
                    proc.exit_status = Some(TerminalExitStatus::new());
                }
                proc.updated_at_ms = unix_time_millis();
                let waiters = std::mem::take(&mut proc.waiters);
                (proc.to_background_work_item(), waiters)
            };
            for waiter in waiters {
                let _ = waiter.respond(WaitForTerminalExitResponse::new(TerminalExitStatus::new()));
            }
            let _ = events.send(DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(
                item,
            )));
        }
        Ok(ReleaseTerminalResponse::new())
    }

    pub(crate) fn emit_reconcile(&self, events: &DriverEventSender) {
        let items = self
            .terminals
            .lock()
            .values()
            .map(|entry| entry.lock().to_background_work_item())
            .collect::<Vec<_>>();
        let _ = events.send(DriverEvent::BackgroundWork(
            BackgroundWorkEvent::ReconcileProcesses { items },
        ));
    }

    pub(crate) fn stop_by_id(
        &self,
        terminal_id: &str,
        key: &BackgroundWorkKey,
        events: &DriverEventSender,
    ) {
        let _ = events.send(DriverEvent::BackgroundWork(
            BackgroundWorkEvent::StopRequested(key.clone()),
        ));
        let Some(entry) = self.terminals.lock().get(terminal_id).cloned() else {
            let _ = events.send(DriverEvent::BackgroundWork(
                BackgroundWorkEvent::StopFailed {
                    key: key.clone(),
                    message: format!("Terminal `{terminal_id}` is no longer active"),
                },
            ));
            return;
        };
        let item = {
            let mut proc = entry.lock();
            if proc.exit_status.is_none() {
                proc.kill_process_tree();
                proc.exit_status = Some(TerminalExitStatus::new());
                proc.updated_at_ms = unix_time_millis();
                if proc.completed_seq.is_none() {
                    proc.completed_seq =
                        Some(self.next_completed_seq.fetch_add(1, Ordering::Relaxed) + 1);
                }
            }
            proc.to_background_work_item()
        };
        evict_completed_terminals_in(&self.terminals, MAX_COMPLETED_TERMINALS);
        let _ = events.send(DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(
            item,
        )));
    }

    pub(crate) fn shutdown_all(&self) {
        let entries = self
            .terminals
            .lock()
            .drain()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        for entry in entries {
            let waiters = {
                let mut proc = entry.lock();
                if proc.exit_status.is_none() {
                    proc.kill_process_tree();
                    proc.exit_status = Some(TerminalExitStatus::new());
                }
                std::mem::take(&mut proc.waiters)
            };
            for waiter in waiters {
                let _ = waiter.respond(WaitForTerminalExitResponse::new(TerminalExitStatus::new()));
            }
        }
    }
}

fn settle_prompt_request(prompt_requests: &Mutex<PendingPrompts>, request_id: &RequestId) -> bool {
    prompt_requests.lock().settle_request(request_id)
}

fn finish_xai_prompt_complete(
    params: &Value,
    prompt_requests: &Mutex<PendingPrompts>,
    stream_state: &Mutex<AcpStreamState>,
    events: &DriverEventSender,
) -> Option<String> {
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        return None;
    };
    let prompt_id = params.get("promptId").and_then(Value::as_str);
    if !prompt_requests
        .lock()
        .settle_extension(session_id, prompt_id)
    {
        return None;
    }
    stream_state.lock().tools.clear();

    let stop_reason = match params.get("stopReason").and_then(Value::as_str) {
        Some("cancelled") => StopReason::Cancelled,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("max_turn_requests") => StopReason::MaxTurnRequests,
        Some("refusal") => StopReason::Refusal,
        _ => StopReason::EndTurn,
    };
    finish_prompt(Ok(PromptResponse::new(stop_reason)), None, events).then(|| session_id.to_owned())
}

fn start_grok_title_refresh(
    grok_title_home: Option<&Path>,
    native_session_id: &str,
    title_refresh: &super::title_refresh::NativeTitleRefresh,
    events: DriverEventSender,
) {
    let grok_title_home = grok_title_home.map(ToOwned::to_owned);
    let native_session_id = native_session_id.to_owned();
    title_refresh.start(
        "waku-grok-title",
        vec![
            Duration::ZERO,
            Duration::from_millis(250),
            Duration::from_millis(750),
            Duration::from_millis(1_500),
            Duration::from_secs(3),
            Duration::from_secs(5),
            Duration::from_millis(7_500),
            Duration::from_secs(10),
        ],
        events,
        move || match grok_title_home.as_deref() {
            Some(home) => crate::grok_session::generated_title_in(home, &native_session_id),
            None => crate::grok_session::generated_title(&native_session_id),
        },
    );
}

fn finish_prompt(
    result: agent_client_protocol::Result<PromptResponse>,
    native_failure: Option<String>,
    events: &impl DriverEventSink,
) -> bool {
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            let _ = events.send(DriverEvent::Error(error.to_string()));
            let _ = events.send(DriverEvent::TurnFinished {
                success: false,
                summary: None,
            });
            return false;
        }
    };
    // An agent can end a turn cleanly and still have failed upstream. Where
    // that failure is recoverable from the provider's own records, it outranks
    // the protocol's verdict: reporting success here would show the user an
    // empty answer and no reason for it.
    if let Some(failure) = native_failure {
        let _ = events.send(DriverEvent::Error(failure));
        let _ = events.send(DriverEvent::TurnFinished {
            success: false,
            summary: None,
        });
        return false;
    }
    let (success, summary) = match response.stop_reason {
        StopReason::EndTurn | StopReason::Cancelled => (true, None),
        StopReason::MaxTokens => (false, Some(tr!("session.agent_ran_out_of_context"))),
        StopReason::Refusal => (false, Some(tr!("session.agent_declined_turn"))),
        StopReason::MaxTurnRequests => (
            false,
            Some(tr!(
                "session.agent_stopped_reason",
                reason = "max_turn_requests"
            )),
        ),
        _ => (
            false,
            Some(tr!("session.agent_stopped_reason", reason = "unknown")),
        ),
    };
    let _ = events.send(DriverEvent::TurnFinished { success, summary });
    success
}

fn cancel_pending_permissions(pending: &PendingPermissions) {
    for (_, entry) in pending.lock().drain() {
        match entry {
            PendingPermission::Protocol(responder) => {
                let _ = responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ));
            }
            PendingPermission::WriteFile { responder, .. } => {
                let _ = responder.respond_with_error(agent_client_protocol::util::internal_error(
                    "File write cancelled by user",
                ));
            }
            PendingPermission::CreateTerminal { responder, .. } => {
                let _ = responder.respond_with_error(agent_client_protocol::util::internal_error(
                    "Terminal execution cancelled by user",
                ));
            }
        }
    }
}

fn cancel_pending_user_inputs(pending: &PendingAcpUserInputs) {
    for (_, pending) in pending.lock().drain() {
        match pending {
            PendingAcpUserInput::Json {
                kind, responder, ..
            } => {
                let _ = responder.respond(cancelled_user_input_response(kind));
            }
            PendingAcpUserInput::Elicitation { responder, .. } => {
                let _ = responder.respond(cancelled_elicitation_response());
            }
            PendingAcpUserInput::Permission { responder, .. } => {
                let _ = responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ));
            }
        }
    }
}

fn cancelled_elicitation_response() -> CreateElicitationResponse {
    CreateElicitationResponse::new(ElicitationAction::Cancel)
}

fn cancelled_user_input_response(kind: AcpUserInputKind) -> Value {
    match kind {
        AcpUserInputKind::Cursor => json!({"answers": {}}),
        AcpUserInputKind::Xai => json!({"outcome": "cancelled"}),
    }
}

fn unwrap_xai_question_params(params: &Value) -> &Value {
    if matches!(
        params.get("method").and_then(Value::as_str),
        Some("x.ai/ask_user_question" | "_x.ai/ask_user_question")
    ) {
        params.get("params").unwrap_or(params)
    } else {
        params
    }
}

fn cursor_user_input_questions(params: &Value) -> Vec<UserInputQuestion> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|question| {
            let text = question.get("prompt").and_then(Value::as_str)?.trim();
            if text.is_empty() {
                return None;
            }
            let mut options = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    let label = option.get("label").and_then(Value::as_str)?.trim();
                    (!label.is_empty()).then(|| UserInputOption {
                        label: label.to_owned(),
                        description: Some(label.to_owned()),
                    })
                })
                .collect::<Vec<_>>();
            if options.is_empty() {
                options.push(UserInputOption {
                    label: "OK".into(),
                    description: Some("Continue".into()),
                });
            }
            Some(UserInputQuestion {
                id: question
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .unwrap_or(text)
                    .to_owned(),
                header: "Question".into(),
                question: text.to_owned(),
                options,
                multi_select: question
                    .get("allowMultiple")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                required: true,
                accepts_custom: true,
            })
        })
        .collect()
}

fn cursor_user_input_response(params: &Value, submitted: &[UserInputAnswer]) -> Value {
    let mut answers = serde_json::Map::new();
    for question in params
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = question.get("id").and_then(Value::as_str) else {
            continue;
        };
        let values = submitted
            .iter()
            .find(|answer| answer.question_id == id)
            .map(|answer| answer.answers.as_slice())
            .unwrap_or_default();
        let value = if question
            .get("allowMultiple")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            json!(values)
        } else {
            values
                .first()
                .map_or(Value::String(String::new()), |value| json!(value))
        };
        answers.insert(id.to_owned(), value);
    }
    json!({"answers": answers})
}

fn xai_user_input_questions(params: &Value) -> Vec<UserInputQuestion> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, question)| {
            let text = question.get("question").and_then(Value::as_str)?.trim();
            if text.is_empty() {
                return None;
            }
            let mut options = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    let label = option.get("label").and_then(Value::as_str)?.trim();
                    (!label.is_empty()).then(|| UserInputOption {
                        label: label.to_owned(),
                        description: option
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|description| !description.is_empty())
                            .map(str::to_owned),
                    })
                })
                .collect::<Vec<_>>();
            if options.is_empty() {
                options.push(UserInputOption {
                    label: "OK".into(),
                    description: Some("Continue".into()),
                });
            }
            Some(UserInputQuestion {
                id: question
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .unwrap_or(text)
                    .to_owned(),
                header: format!("Question {}", index + 1),
                question: text.to_owned(),
                options,
                multi_select: question
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                required: true,
                accepts_custom: true,
            })
        })
        .collect()
}

fn xai_user_input_response(params: &Value, submitted: &[UserInputAnswer]) -> Value {
    let mut answers = serde_json::Map::new();
    let mut annotations = serde_json::Map::new();
    for question in params
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(question_text) = question.get("question").and_then(Value::as_str) else {
            continue;
        };
        let id = question
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(question_text);
        let values = submitted
            .iter()
            .find(|answer| answer.question_id == id || answer.question_id == question_text)
            .map(|answer| answer.answers.as_slice())
            .unwrap_or_default();
        let options = question
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let option_labels = options
            .iter()
            .filter_map(|option| option.get("label").and_then(Value::as_str))
            .collect::<Vec<_>>();
        let selected = values
            .iter()
            .filter(|value| option_labels.contains(&value.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let notes = values
            .iter()
            .filter(|value| !option_labels.contains(&value.as_str()))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let preview = if question
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            None
        } else {
            selected.iter().find_map(|selected| {
                options.iter().find_map(|option| {
                    (option.get("label").and_then(Value::as_str) == Some(selected.as_str()))
                        .then(|| {
                            option
                                .get("preview")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|preview| !preview.is_empty())
                                .map(str::to_owned)
                        })
                        .flatten()
                })
            })
        };
        answers.insert(
            question_text.to_owned(),
            json!(if selected.is_empty() && !notes.is_empty() {
                vec!["Other".to_owned()]
            } else {
                selected
            }),
        );
        let mut annotation = serde_json::Map::new();
        if let Some(preview) = preview {
            annotation.insert("preview".into(), Value::String(preview));
        }
        if !notes.is_empty() {
            annotation.insert("notes".into(), Value::String(notes));
        }
        if !annotation.is_empty() {
            annotations.insert(question_text.to_owned(), Value::Object(annotation));
        }
    }
    let mut response = json!({"outcome": "accepted", "answers": answers});
    if !annotations.is_empty() {
        response["annotations"] = Value::Object(annotations);
    }
    response
}

struct ElicitationChoice {
    label: String,
    value: String,
    description: Option<String>,
}

fn elicitation_user_input_questions(
    message: &str,
    schema: &agent_client_protocol::schema::v1::ElicitationSchema,
) -> Vec<UserInputQuestion> {
    let mut questions = Vec::new();
    for (name, property) in ordered_elicitation_properties(schema) {
        let required = schema
            .required
            .as_ref()
            .is_some_and(|names| names.iter().any(|item| item == name));
        let Some(question) =
            elicitation_property_question(message, name, property, questions.is_empty(), required)
        else {
            continue;
        };
        questions.push(question);
    }
    questions
}

fn ordered_elicitation_properties(
    schema: &agent_client_protocol::schema::v1::ElicitationSchema,
) -> Vec<(&str, &ElicitationPropertySchema)> {
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    if let Some(required) = &schema.required {
        for name in required {
            if let Some(property) = schema.properties.get(name)
                && seen.insert(name.as_str())
            {
                ordered.push((name.as_str(), property));
            }
        }
    }
    for (name, property) in &schema.properties {
        if seen.insert(name.as_str()) {
            ordered.push((name.as_str(), property));
        }
    }
    ordered
}

fn elicitation_property_question(
    message: &str,
    name: &str,
    property: &ElicitationPropertySchema,
    first: bool,
    required: bool,
) -> Option<UserInputQuestion> {
    let (header, description, choices, multi_select) = match property {
        ElicitationPropertySchema::String(schema) => (
            schema.title.as_deref(),
            schema.description.as_deref(),
            string_elicitation_choices(schema),
            false,
        ),
        ElicitationPropertySchema::Array(schema) => (
            schema.title.as_deref(),
            schema.description.as_deref(),
            array_elicitation_choices(schema),
            true,
        ),
        ElicitationPropertySchema::Boolean(schema) => (
            schema.title.as_deref(),
            schema.description.as_deref(),
            boolean_elicitation_choices(),
            false,
        ),
        ElicitationPropertySchema::Integer(schema) => (
            schema.title.as_deref(),
            schema.description.as_deref(),
            Vec::new(),
            false,
        ),
        ElicitationPropertySchema::Number(schema) => (
            schema.title.as_deref(),
            schema.description.as_deref(),
            Vec::new(),
            false,
        ),
        ElicitationPropertySchema::Other(_) | &_ => return None,
    };
    let header = header
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(name);
    Some(UserInputQuestion {
        id: name.to_owned(),
        header: header.to_owned(),
        question: elicitation_question_text(message, header, description, first),
        options: choices
            .iter()
            .map(|choice| UserInputOption {
                label: choice.label.clone(),
                description: choice.description.clone(),
            })
            .collect(),
        multi_select,
        required,
        accepts_custom: choices.is_empty(),
    })
}

fn boolean_elicitation_choices() -> Vec<ElicitationChoice> {
    vec![
        ElicitationChoice {
            label: "Yes".into(),
            value: "true".into(),
            description: None,
        },
        ElicitationChoice {
            label: "No".into(),
            value: "false".into(),
            description: None,
        },
    ]
}

fn elicitation_question_text(
    message: &str,
    header: &str,
    description: Option<&str>,
    first: bool,
) -> String {
    let message = message.trim();
    let detail = description
        .map(str::trim)
        .filter(|detail| !detail.is_empty());
    match (first, message.is_empty(), detail) {
        (true, false, Some(detail)) if message != detail => format!("{message}\n\n{detail}"),
        (true, false, None) if message != header => message.to_owned(),
        (_, _, Some(detail)) => detail.to_owned(),
        _ => header.to_owned(),
    }
}

fn string_elicitation_choices(
    schema: &agent_client_protocol::schema::v1::StringPropertySchema,
) -> Vec<ElicitationChoice> {
    if let Some(options) = schema.one_of.as_ref().filter(|options| !options.is_empty()) {
        return titled_elicitation_choices(options);
    }
    schema
        .enum_values
        .as_ref()
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .map(|value| ElicitationChoice {
            label: value.clone(),
            value: value.clone(),
            description: None,
        })
        .collect()
}

fn array_elicitation_choices(
    schema: &agent_client_protocol::schema::v1::MultiSelectPropertySchema,
) -> Vec<ElicitationChoice> {
    match &schema.items {
        MultiSelectItems::Titled(items) => titled_elicitation_choices(&items.options),
        MultiSelectItems::String(items) => items
            .values
            .iter()
            .filter(|value| !value.trim().is_empty())
            .map(|value| ElicitationChoice {
                label: value.clone(),
                value: value.clone(),
                description: None,
            })
            .collect(),
        MultiSelectItems::Other(_) | &_ => Vec::new(),
    }
}

fn titled_elicitation_choices(options: &[EnumOption]) -> Vec<ElicitationChoice> {
    let mut used = HashSet::new();
    options
        .iter()
        .filter(|option| !option.value.trim().is_empty())
        .map(|option| {
            let title = option.title.trim();
            let mut label = if title.is_empty() {
                option.value.clone()
            } else {
                title.to_owned()
            };
            if !used.insert(label.clone()) {
                label = format!("{label} ({})", option.value);
                used.insert(label.clone());
            }
            ElicitationChoice {
                label,
                value: option.value.clone(),
                description: option
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(str::to_owned),
            }
        })
        .collect()
}

fn elicitation_accept_response(
    request: &CreateElicitationRequest,
    submitted: &[UserInputAnswer],
) -> CreateElicitationResponse {
    let ElicitationMode::Form(form) = &request.mode else {
        return cancelled_elicitation_response();
    };
    let mut content = BTreeMap::new();
    for (name, property) in ordered_elicitation_properties(&form.requested_schema) {
        let Some(answer) = submitted.iter().find(|answer| answer.question_id == name) else {
            continue;
        };
        if let Some(value) = elicitation_content_value(property, &answer.answers) {
            content.insert(name.to_owned(), value);
        }
    }
    CreateElicitationResponse::new(ElicitationAcceptAction::new().content(content))
}

fn elicitation_content_value(
    property: &ElicitationPropertySchema,
    answers: &[String],
) -> Option<ElicitationContentValue> {
    let answers = answers
        .iter()
        .map(|answer| answer.trim())
        .filter(|answer| !answer.is_empty())
        .collect::<Vec<_>>();
    let first = answers.first().copied()?;
    Some(match property {
        ElicitationPropertySchema::String(schema) => {
            let choices = string_elicitation_choices(schema);
            ElicitationContentValue::String(if choices.is_empty() {
                first.to_owned()
            } else {
                resolve_elicitation_choice(&choices, first)
            })
        }
        ElicitationPropertySchema::Array(schema) => {
            let choices = array_elicitation_choices(schema);
            ElicitationContentValue::StringArray(if choices.is_empty() {
                answers.into_iter().map(str::to_owned).collect()
            } else {
                answers
                    .into_iter()
                    .map(|answer| resolve_elicitation_choice(&choices, answer))
                    .collect()
            })
        }
        ElicitationPropertySchema::Boolean(_) => {
            let resolved = resolve_elicitation_choice(&boolean_elicitation_choices(), first);
            ElicitationContentValue::Boolean(matches!(
                resolved.to_ascii_lowercase().as_str(),
                "true" | "yes" | "1"
            ))
        }
        ElicitationPropertySchema::Integer(_) => match first.parse::<i64>() {
            Ok(value) => ElicitationContentValue::Integer(value),
            Err(_) => ElicitationContentValue::String(first.to_owned()),
        },
        ElicitationPropertySchema::Number(_) => match first.parse::<f64>() {
            Ok(value) => ElicitationContentValue::Number(value),
            Err(_) => ElicitationContentValue::String(first.to_owned()),
        },
        ElicitationPropertySchema::Other(_) | &_ => return None,
    })
}

fn resolve_elicitation_choice(choices: &[ElicitationChoice], answer: &str) -> String {
    choices
        .iter()
        .find(|choice| choice.label == answer || choice.value == answer)
        .map(|choice| choice.value.clone())
        .unwrap_or_else(|| answer.to_owned())
}

/// Droid's "always" answer also raises the session's autonomy level, which
/// would drift the session away from the selected access mode.
fn auto_permission_choice(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
    prefer_allow_once: bool,
) -> Option<&agent_client_protocol::schema::v1::PermissionOption> {
    let (first, second) = if prefer_allow_once {
        (
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        )
    } else {
        (
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::AllowOnce,
        )
    };
    options
        .iter()
        .find(|option| option.kind == first)
        .or_else(|| options.iter().find(|option| option.kind == second))
}

fn offered_permission_options(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
    allow_once_only: bool,
) -> Vec<PermissionOption> {
    // Without a one-time allow, dropping "always" would leave only refusals.
    let hide_allow_always = allow_once_only
        && options
            .iter()
            .any(|option| option.kind == PermissionOptionKind::AllowOnce);
    options
        .iter()
        .filter(|option| !(hide_allow_always && option.kind == PermissionOptionKind::AllowAlways))
        .map(|option| PermissionOption {
            id: option.option_id.to_string(),
            label: option.name.clone(),
            allow: matches!(
                option.kind,
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
            ),
        })
        .collect()
}

fn antigravity_question_label(
    option: &agent_client_protocol::schema::v1::PermissionOption,
) -> String {
    let label = option.name.trim();
    let label = if label.is_empty() {
        option.option_id.to_string()
    } else {
        label.to_owned()
    };
    truncate(&label, 512)
}

/// `agy` puts the Allow Always warning under the option's
/// `_meta["agy.security.warning"]` rather than in the visible label.
fn antigravity_security_warning(request: &RequestPermissionRequest) -> Option<String> {
    let option = request
        .options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowAlways)?;
    let warning = option.meta.as_ref()?.get("agy.security.warning")?;
    warning
        .get("message")
        .or_else(|| warning.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|warning| !warning.is_empty())
        .map(|warning| warning.chars().take(512).collect())
}

/// Antigravity's `ask_question` arrives as a permission request whose tool-call
/// id starts with `interaction_`. Its options are the choices, not allow/deny
/// decisions, so the permission prompt must not auto-approve it or show a tool
/// approval.
fn antigravity_user_input_questions(
    request: &RequestPermissionRequest,
) -> Option<Vec<UserInputQuestion>> {
    if !request
        .tool_call
        .tool_call_id
        .to_string()
        .starts_with("interaction_")
        || request.options.is_empty()
    {
        return None;
    }
    let mut ids = std::collections::HashSet::new();
    for option in &request.options {
        let id = option.option_id.to_string();
        if id.trim().is_empty() || !ids.insert(id) {
            return None;
        }
    }
    let question = request
        .tool_call
        .fields
        .title
        .as_deref()
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .unwrap_or("Choose an option.");
    Some(vec![UserInputQuestion {
        id: request.tool_call.tool_call_id.to_string(),
        header: "Question".into(),
        question: truncate(question, 8_000),
        options: request
            .options
            .iter()
            .map(|option| {
                let label = antigravity_question_label(option);
                UserInputOption {
                    description: Some(label.clone()),
                    label,
                }
            })
            .collect(),
        multi_select: false,
        required: true,
        accepts_custom: false,
    }])
}

fn antigravity_question_response(
    request: &RequestPermissionRequest,
    answers: &[UserInputAnswer],
) -> RequestPermissionResponse {
    let question_id = request.tool_call.tool_call_id.to_string();
    let value = answers
        .iter()
        .find(|answer| answer.question_id == question_id)
        .and_then(|answer| answer.answers.first());
    let option = value.and_then(|value| {
        request
            .options
            .iter()
            .find(|option| option.option_id.to_string() == *value)
            .or_else(|| {
                let matching = request
                    .options
                    .iter()
                    .filter(|option| antigravity_question_label(option) == *value)
                    .collect::<Vec<_>>();
                (matching.len() == 1).then(|| matching[0])
            })
    });
    match option {
        Some(option) => RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(option.option_id.clone()),
        )),
        None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
    }
}

/// `allow_once_only` is Droid's: its "always" answer raises the session's
/// autonomy level, so it is neither chosen automatically nor offered, and the
/// access mode stays the one control over what runs unasked.
#[allow(clippy::too_many_arguments)]
fn handle_permission_request(
    provider: ProviderKind,
    request: RequestPermissionRequest,
    responder: PermissionResponder,
    auto_approve: bool,
    allow_once_only: bool,
    pending: &PendingPermissions,
    pending_user_inputs: &PendingAcpUserInputs,
    events: &impl DriverEventSink,
) -> agent_client_protocol::Result<()> {
    let request_id = responder.id().to_string();
    let params = serde_json::to_value(&request)?;

    // Questions are not approvals: answer them even in Full access, and route
    // them to the question UI before the permission path can take the options
    // for allow/deny choices.
    if provider == ProviderKind::Antigravity
        && let Some(questions) = antigravity_user_input_questions(&request)
    {
        pending_user_inputs.lock().insert(
            request_id.clone(),
            PendingAcpUserInput::Permission { request, responder },
        );
        if events
            .send(DriverEvent::UserInputRequested {
                request_id: request_id.clone(),
                questions,
            })
            .is_err()
            && let Some(PendingAcpUserInput::Permission { responder, .. }) =
                pending_user_inputs.lock().remove(&request_id)
        {
            let _ = responder.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
        }
        return Ok(());
    }

    let options = offered_permission_options(&request.options, allow_once_only);

    if auto_approve {
        return match auto_permission_choice(&request.options, allow_once_only) {
            Some(choice) => responder.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    choice.option_id.clone(),
                )),
            )),
            None => responder.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            )),
        };
    }

    let title = params
        .pointer("/toolCall/title")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| tr!("permission.run_a_tool"));
    let detail = permission_reason(&params).unwrap_or_else(|| {
        params
            .pointer("/toolCall/kind")
            .and_then(Value::as_str)
            .map(|kind| tr!("permission.agent_wants_to", action = kind))
            .unwrap_or_else(|| tr!("permission.agent_asks_for_permission"))
    });
    let detail = if provider == ProviderKind::Antigravity {
        antigravity_security_warning(&request)
            .map(|warning| format!("{detail}\n\n{warning}"))
            .unwrap_or(detail)
    } else {
        detail
    };
    pending
        .lock()
        .insert(request_id.clone(), PendingPermission::Protocol(responder));
    if events
        .send(DriverEvent::Permission {
            request_id: request_id.clone(),
            title,
            detail,
            options,
        })
        .is_err()
        && let Some(PendingPermission::Protocol(responder)) = pending.lock().remove(&request_id)
    {
        let _ = responder.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
    }
    Ok(())
}

/// Replay notifications emitted during `session/load` that duplicate the
/// already-stored transcript. Metadata updates (`available_commands_update`,
/// `usage_update`, `session_info_update`) are intentionally NOT suppressed so
/// resumed sessions receive their live slash commands, context meter, and title.
fn is_replay_transcript_update(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::ToolCallUpdate(_)
            | SessionUpdate::Plan(_)
    )
}

/// Agent-generated content that opens an Antigravity out-of-turn reactive
/// wakeup (e.g. `/swarm` board mention or `/schedule` timer fire). Deliberately
/// excludes `UserMessageChunk` and session metadata so replay noise never
/// spawns a reactive turn.
fn is_unsolicited_turn_content(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::ToolCallUpdate(_)
            | SessionUpdate::Plan(_)
    )
}

fn handle_session_update(
    provider: ProviderKind,
    notification: SessionNotification,
    events: &impl DriverEventSink,
    state: &mut AcpStreamState,
) -> agent_client_protocol::Result<()> {
    state.last_completed_tool_name = None;
    let update = serde_json::to_value(notification.update)?;
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    if provider == ProviderKind::Fx
        && !state.produced_content
        && kind == Some("agent_message_chunk")
        && update
            .pointer("/content/text")
            .and_then(Value::as_str)
            .is_some_and(fx_context_notice)
    {
        return Ok(());
    }
    if matches!(
        kind,
        Some(
            "agent_message_chunk"
                | "agent_thought_chunk"
                | "tool_call"
                | "tool_call_update"
                | "plan"
        )
    ) {
        state.produced_content = true;
    }
    match kind {
        Some("agent_message_chunk") => {
            if let Some(text) = update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
            }
        }
        Some("agent_thought_chunk") => {
            if let Some(text) = update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let _ = events.send(DriverEvent::ReasoningDelta(text.to_owned()));
            }
        }
        Some("tool_call" | "tool_call_update") => tool_activity(&update, events, state),
        // Each update carries the whole plan, so it replaces the previous one
        // in place and is already settled when it arrives.
        Some("plan") => {
            let item = plan_activity(&update);
            let _ = events.send(DriverEvent::RichActivity(item));
        }
        Some("available_commands_update") => {
            let commands = update
                .get("availableCommands")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|command| {
                            let name = command
                                .get("name")
                                .and_then(Value::as_str)?
                                .trim()
                                .trim_start_matches('/');
                            if name.is_empty() || name.chars().any(char::is_whitespace) {
                                return None;
                            }
                            let argument_hint = command
                                .pointer("/input/hint")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|hint| !hint.is_empty())
                                .map(str::to_owned);
                            Some(crate::model::ReportedCommand {
                                name: name.to_owned(),
                                description: command
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .trim()
                                    .to_owned(),
                                argument_hint,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let _ = events.send(DriverEvent::AvailableCommands(commands));
        }
        Some("session_info_update") => {
            if update.get("title").is_some() {
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let _ = events.send(DriverEvent::AutoTitleUpdated(title));
            }
        }
        Some("usage_update") => {
            let used = update
                .get("used")
                .and_then(Value::as_u64)
                .filter(|used| *used > 0);
            let window = ["max", "limit", "size", "contextWindow", "context_window"]
                .into_iter()
                .find_map(|key| update.get(key).and_then(Value::as_u64))
                .filter(|window| *window > 0);
            if used.is_some() || window.is_some() {
                let _ = events.send(DriverEvent::UsageUpdated {
                    context_tokens: used,
                    context_window: window,
                });
            }
        }
        // `user_message_chunk` is Waku's own prompt echoed back. Other typed
        // updates currently have no transcript representation.
        _ => {}
    }
    Ok(())
}

pub(crate) fn plan_activity(update: &Value) -> ActivityItem {
    let steps = plan_steps_text(update);
    activity::tool_activity(
        Some("acp-plan".into()),
        ActivityKind::Plan,
        tr!("activity.plan_updated"),
        None,
        steps.as_ref(),
        None,
        false,
        true,
    )
}

/// An ACP plan's entries as one line per step, marked by status.
fn plan_steps_text(update: &Value) -> Option<Value> {
    let steps = update
        .get("entries")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|entry| {
            let content = entry.get("content").and_then(Value::as_str)?.trim();
            (!content.is_empty()).then(|| {
                let marker = match entry.get("status").and_then(Value::as_str) {
                    Some("completed") => "[x]",
                    Some("in_progress") => "[~]",
                    _ => "[ ]",
                };
                format!("{marker} {content}")
            })
        })
        .collect::<Vec<_>>();
    (!steps.is_empty()).then(|| Value::String(steps.join("\n")))
}

fn fx_context_notice(text: &str) -> bool {
    text.starts_with("[context] ") || text.starts_with("skill discovery warning: ")
}

#[derive(Clone)]
pub(crate) struct StoredToolCall {
    kind: ActivityKind,
    title: String,
    raw_input: Option<Value>,
    locations: Option<Value>,
    content: Option<Value>,
    tool_name: Option<String>,
    mcp_server: Option<String>,
    complete: bool,
    failed: bool,
}

#[derive(Default)]
struct AcpStreamState {
    tools: HashMap<String, StoredToolCall>,
    terminals: Arc<AcpTerminalRegistry>,
    /// Whether the running turn has produced anything visible. A turn that
    /// ends having produced nothing is the shape a swallowed provider error
    /// takes, which is what makes a native failure worth looking up.
    produced_content: bool,
    last_completed_tool_name: Option<String>,
}

impl AcpStreamState {
    fn has_in_flight_tools(&self) -> bool {
        self.tools.values().any(|tool| !tool.complete)
    }
}

/// Pull the agent's explanation out of a permission request's tool call.
fn permission_reason(params: &Value) -> Option<String> {
    let content = params
        .pointer("/toolCall/content")
        .and_then(Value::as_array)?;
    let reason = content
        .iter()
        .filter_map(|entry| {
            entry
                .pointer("/content/text")
                .or_else(|| entry.get("text"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!reason.is_empty()).then(|| truncate(&reason, 400))
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    text.chars()
        .take(max_chars)
        .chain(std::iter::once('…'))
        .collect()
}

fn content_has_diff(content: Option<&Value>) -> bool {
    let Some(items) = content.and_then(Value::as_array) else {
        return false;
    };
    items
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("diff"))
}

fn resolve_terminal_content_output(
    content: Option<&Value>,
    terminals: &AcpTerminalRegistry,
) -> Option<String> {
    let items = content?.as_array()?;
    let outputs = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("terminal"))
        .filter_map(|item| {
            item.get("terminalId")
                .or_else(|| item.get("terminal_id"))
                .and_then(Value::as_str)
        })
        .filter_map(|terminal_id| terminals.output_text(terminal_id))
        .collect::<Vec<_>>();
    (!outputs.is_empty()).then(|| outputs.join("\n\n"))
}

fn first_non_empty_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| text.trim())
}

fn arg_find<'a, T>(
    arguments: Option<&'a Value>,
    keys: &[&str],
    extract: impl Fn(&'a Value) -> Option<T> + Copy,
) -> Option<T> {
    let object = arguments?.as_object()?;
    if let Some(value) = lookup_in_map(object, keys, extract) {
        return Some(value);
    }
    for nested_key in ["arguments", "input", "tool_input", "params"] {
        if let Some(nested) = object.get(nested_key).and_then(Value::as_object)
            && let Some(value) = lookup_in_map(nested, keys, extract)
        {
            return Some(value);
        }
    }
    None
}

fn lookup_in_map<'a, T>(
    object: &'a Map<String, Value>,
    keys: &[&str],
    extract: impl Fn(&'a Value) -> Option<T>,
) -> Option<T> {
    for key in keys {
        if let Some(value) = object.get(*key).and_then(&extract) {
            return Some(value);
        }
    }
    for key in keys {
        if let Some(value) = object
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .and_then(|(_, value)| extract(value))
        {
            return Some(value);
        }
    }
    None
}

fn arg_str<'a>(arguments: Option<&'a Value>, keys: &[&str]) -> Option<&'a str> {
    arg_find(arguments, keys, |value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
    })
}

fn arg_u64(arguments: Option<&Value>, keys: &[&str]) -> Option<u64> {
    arg_find(arguments, keys, Value::as_u64)
}

fn arg_bool(arguments: Option<&Value>, keys: &[&str]) -> Option<bool> {
    arg_find(arguments, keys, Value::as_bool)
}

fn arg_array<'a>(arguments: Option<&'a Value>, keys: &[&str]) -> Option<&'a [Value]> {
    arg_find(arguments, keys, |value| value.as_array().map(Vec::as_slice))
}

fn arg_object<'a>(arguments: Option<&'a Value>, keys: &[&str]) -> Option<&'a Value> {
    arg_find(arguments, keys, |value| value.is_object().then_some(value))
}

fn normalize_acp_windows_path(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 4
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && (bytes[3] == b'/' || bytes[3] == b'\\')
    {
        return path[1..].to_owned();
    }
    path.to_owned()
}

fn path_basename(raw_path: &str) -> Option<&str> {
    let normalized = raw_path.trim();
    let trimmed = if normalized.as_bytes().len() >= 4
        && normalized.as_bytes()[0] == b'/'
        && normalized.as_bytes()[1].is_ascii_alphabetic()
        && normalized.as_bytes()[2] == b':'
        && (normalized.as_bytes()[3] == b'/' || normalized.as_bytes()[3] == b'\\')
    {
        &normalized[1..]
    } else {
        normalized
    };
    trimmed
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
}

fn normalize_acp_locations(locations: &Value) -> Value {
    let Some(items) = locations.as_array() else {
        return locations.clone();
    };
    let normalized = items
        .iter()
        .map(|entry| {
            let Some(obj) = entry.as_object() else {
                return entry.clone();
            };
            let Some(path) = obj.get("path").and_then(Value::as_str) else {
                return entry.clone();
            };
            let clean = normalize_acp_windows_path(path);
            if clean == path {
                return entry.clone();
            }
            let mut updated = obj.clone();
            updated.insert("path".to_owned(), Value::String(clean));
            Value::Object(updated)
        })
        .collect();
    Value::Array(normalized)
}

/// Maps Antigravity's coordination/agent tool names (with or without ACP's
/// `"Run "` display prefix or namespace prefixes such as `default_api:` or
/// `mcp__server__`) to their canonical name.
fn antigravity_canonical_tool_name(raw_name: &str) -> Option<&'static str> {
    let trimmed = raw_name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_run = trimmed
        .strip_prefix("Run ")
        .or_else(|| trimmed.strip_prefix("run "))
        .map(str::trim)
        .unwrap_or(trimmed);
    let normalized = without_run.to_ascii_lowercase().replace(['-', ' '], "_");
    let leaf = normalized
        .rsplit("__")
        .next()
        .unwrap_or(&normalized)
        .rsplit([':', '.', '/'])
        .next()
        .unwrap_or(&normalized);
    match leaf {
        "swarm" => Some("swarm"),
        "post_message" => Some("post_message"),
        "read_board" => Some("read_board"),
        "swarm_wait" => Some("swarm_wait"),
        "invoke_subagent" => Some("invoke_subagent"),
        "define_subagent" => Some("define_subagent"),
        "send_message" => Some("send_message"),
        "manage_subagents" => Some("manage_subagents"),
        "schedule" => Some("schedule"),
        "manage_task" => Some("manage_task"),
        "run_workflow" => Some("run_workflow"),
        "exit_plan_mode" => Some("exit_plan_mode"),
        "boost" => Some("boost"),
        _ => None,
    }
}

/// Produces a clean `(title, detail)` pair for Antigravity's native swarm,
/// subagent, plan, schedule, workflow, and boost tools so transcript cards stay
/// legible instead of falling back to raw tool ids or colliding on generic
/// input keys (`action`, `title`).
fn antigravity_tool_presentation(
    canonical_name: &str,
    arguments: Option<&Value>,
) -> (String, Option<String>) {
    match canonical_name {
        "swarm" => {
            let action = arg_str(arguments, &["action"]);
            let name = arg_str(arguments, &["name"]);
            let title = arg_str(arguments, &["title"]);
            let model = arg_str(arguments, &["model"]);
            let brief = arg_str(arguments, &["brief"]);
            let count = arg_u64(arguments, &["n"]);
            let tasks_len = arg_array(arguments, &["tasks"]).map(<[Value]>::len);
            match action {
                Some("add") => {
                    let heading = match name {
                        Some(name) if tasks_len.is_some_and(|len| len > 0) => {
                            format!("Swarm: add {name} ({} tasks)", tasks_len.unwrap_or(0))
                        }
                        Some(name) if count.is_some_and(|n| n > 1) => {
                            format!("Swarm: add {name} (×{})", count.unwrap_or(1))
                        }
                        Some(name) => format!("Swarm: add {name}"),
                        None => "Swarm: add worker".to_owned(),
                    };
                    let detail = brief
                        .map(|brief| truncate(first_non_empty_line(brief), 120))
                        .or_else(|| model.map(|model| format!("model: {model}")));
                    (heading, detail)
                }
                Some("title") => (
                    title
                        .map(|title| format!("Swarm: title \"{}\"", truncate(title, 60)))
                        .unwrap_or_else(|| "Swarm: set title".to_owned()),
                    None,
                ),
                Some("kill") => (
                    name.map(|name| format!("Swarm: kill {name}"))
                        .unwrap_or_else(|| "Swarm: kill worker".to_owned()),
                    None,
                ),
                Some("model") => {
                    let heading = match (name, model) {
                        (Some(name), Some(model)) => format!("Swarm: switch {name} to {model}"),
                        (None, Some(model)) => format!("Swarm: switch model to {model}"),
                        (Some(name), None) => format!("Swarm: switch model for {name}"),
                        (None, None) => "Swarm: switch model".to_owned(),
                    };
                    (heading, None)
                }
                Some("channel") => {
                    let op = arg_str(arguments, &["op"]);
                    let channel = arg_str(arguments, &["channel"]);
                    let topic = arg_str(arguments, &["topic"]);
                    let heading = match (op, channel) {
                        (Some(op), Some(channel)) => format!(
                            "Swarm: channel {} #{}",
                            truncate(op, 20),
                            truncate(channel.trim_start_matches('#'), 40)
                        ),
                        (Some(op), None) => format!("Swarm: channel {}", truncate(op, 20)),
                        (None, Some(channel)) => {
                            format!("Swarm: #{}", truncate(channel.trim_start_matches('#'), 40))
                        }
                        (None, None) => "Swarm: channel".to_owned(),
                    };
                    let detail = topic
                        .map(|topic| truncate(first_non_empty_line(topic), 120))
                        .or_else(|| name.map(|target| format!("@{target}")));
                    (heading, detail)
                }
                Some("status") => ("Swarm: status".to_owned(), None),
                Some("models") => ("Swarm: list models".to_owned(), None),
                Some(other) => (
                    format!("Swarm: {}", truncate(other, 40)),
                    name.map(str::to_owned),
                ),
                None => ("Swarm".to_owned(), None),
            }
        }
        "post_message" => {
            let text = arg_str(arguments, &["text", "message", "content"]);
            let mention = text
                .and_then(|text| text.split_whitespace().next())
                .filter(|token| token.starts_with('@') && token.len() > 1);
            let heading = match mention {
                Some(mention) => format!("Post to swarm board ({})", truncate(mention, 32)),
                None => "Post to swarm board".to_owned(),
            };
            let detail = text.map(|text| truncate(first_non_empty_line(text), 120));
            (heading, detail)
        }
        "read_board" => {
            if arg_bool(arguments, &["status"]) == Some(true) {
                return ("Read swarm roster".to_owned(), None);
            }
            if let Some(seqs) = arg_array(arguments, &["seqs"]) {
                let ids = seqs
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|seq| format!("#{seq}"))
                    .collect::<Vec<_>>();
                if !ids.is_empty() {
                    let preview = if ids.len() <= 4 {
                        ids.join(", ")
                    } else {
                        format!("{}, +{}", ids[..4].join(", "), ids.len() - 4)
                    };
                    return (format!("Read swarm board ({preview})"), None);
                }
            }
            let heading = match arg_u64(arguments, &["since"]) {
                Some(since) => format!("Read swarm board (since #{since})"),
                None => "Read swarm board".to_owned(),
            };
            let detail = arg_u64(arguments, &["limit"]).map(|limit| format!("limit {limit}"));
            (heading, detail)
        }
        "swarm_wait" => {
            let heading = match arg_u64(arguments, &["since"]) {
                Some(since) => format!("Wait on swarm board (since #{since})"),
                None => "Wait on swarm board".to_owned(),
            };
            let detail = match arg_bool(arguments, &["mentions_only"]) {
                Some(false) => Some("Waiting for any board message".to_owned()),
                Some(true) => Some("Waiting for @mention".to_owned()),
                None => None,
            };
            (heading, detail)
        }
        "invoke_subagent" => {
            let agent = arg_str(
                arguments,
                &[
                    "Role",
                    "TypeName",
                    "subagent_name",
                    "subagent_type",
                    "name",
                    "agent",
                    "agent_name",
                ],
            );
            let task = arg_str(
                arguments,
                &[
                    "Prompt",
                    "description",
                    "title",
                    "task",
                    "instruction",
                    "message",
                ],
            );
            let model = arg_str(arguments, &["Model", "model"]);
            let (heading, detail) = match (agent, task) {
                (Some(agent), Some(task)) => (
                    format!("Subagent: {}", truncate(agent, 60)),
                    Some(truncate(first_non_empty_line(task), 120)),
                ),
                (Some(agent), None) => (
                    format!("Subagent: {}", truncate(agent, 60)),
                    model.map(|model| format!("model: {model}")),
                ),
                (None, Some(task)) => (
                    format!("Subagent: {}", truncate(first_non_empty_line(task), 60)),
                    None,
                ),
                (None, None) => ("Subagent".to_owned(), None),
            };
            (heading, detail)
        }
        "define_subagent" => {
            let name = arg_str(arguments, &["Name", "name"]);
            let description = arg_str(arguments, &["Description", "description"]);
            let heading = match name {
                Some(name) => format!("Define subagent: {}", truncate(name, 60)),
                None => "Define subagent".to_owned(),
            };
            let detail = description.map(|desc| truncate(first_non_empty_line(desc), 120));
            (heading, detail)
        }
        "send_message" => {
            let recipient = arg_str(arguments, &["Recipient", "recipient"]);
            let message = arg_str(arguments, &["Message", "message", "text", "content"]);
            let heading = match recipient {
                Some(recipient) if recipient.chars().count() <= 24 => {
                    format!("Message subagent ({recipient})")
                }
                _ => "Message subagent".to_owned(),
            };
            let detail = message.map(|msg| truncate(first_non_empty_line(msg), 120));
            (heading, detail)
        }
        "manage_subagents" => {
            let action = arg_str(arguments, &["Action", "action"]);
            let heading = match action {
                Some(action) => format!("Subagents: {}", truncate(action, 40)),
                None => "Manage subagents".to_owned(),
            };
            (heading, None)
        }
        "schedule" => {
            let cron = arg_str(arguments, &["CronExpression", "cron_expression", "cron"]);
            let duration = arg_u64(arguments, &["DurationSeconds", "duration_seconds"]);
            let prompt = arg_str(arguments, &["Prompt", "prompt", "task"]);
            let heading = match (cron, duration) {
                (Some(cron), _) => format!("Schedule: {}", truncate(cron, 40)),
                (None, Some(seconds)) => format!("Schedule: {seconds}s timer"),
                (None, None) => "Schedule".to_owned(),
            };
            let detail = prompt.map(|prompt| truncate(first_non_empty_line(prompt), 120));
            (heading, detail)
        }
        "manage_task" => {
            let action = arg_str(arguments, &["Action", "action"]);
            let task_id = arg_str(arguments, &["TaskId", "task_id"]);
            let heading = match action {
                Some(action) => format!("Task: {}", truncate(action, 40)),
                None => "Manage task".to_owned(),
            };
            let detail = task_id.map(|id| truncate(id, 60));
            (heading, detail)
        }
        "run_workflow" => {
            let description = arg_str(arguments, &["Description", "description"]);
            let script_path = arg_str(arguments, &["ScriptPath", "script_path"]);
            let heading = description
                .map(|desc| format!("Workflow: {}", truncate(first_non_empty_line(desc), 60)))
                .or_else(|| {
                    script_path
                        .and_then(path_basename)
                        .map(|name| format!("Workflow: {}", truncate(name, 60)))
                })
                .unwrap_or_else(|| "Run workflow".to_owned());
            (heading, None)
        }
        "exit_plan_mode" => {
            let plan = arg_str(arguments, &["plan", "content", "summary"]);
            (
                "Plan ready".to_owned(),
                plan.map(|plan| truncate(first_non_empty_line(plan), 120)),
            )
        }
        "boost" => {
            let mode = arg_str(arguments, &["mode", "action", "level", "model"]);
            let prompt = arg_str(
                arguments,
                &["prompt", "task", "query", "description", "title"],
            );
            let (heading, detail) = match (mode, prompt) {
                (Some(mode), Some(prompt)) => (
                    format!("Boost: {}", truncate(mode, 60)),
                    Some(truncate(first_non_empty_line(prompt), 120)),
                ),
                (Some(mode), None) => (format!("Boost: {}", truncate(mode, 60)), None),
                (None, Some(prompt)) => (
                    format!("Boost: {}", truncate(first_non_empty_line(prompt), 60)),
                    None,
                ),
                (None, None) => ("Boost".to_owned(), None),
            };
            (heading, detail)
        }
        _ => (canonical_name.to_owned(), None),
    }
}

fn antigravity_artifact_presentation(
    arguments: Option<&Value>,
) -> Option<(ActivityKind, String, Option<String>, Option<String>)> {
    let meta = arg_object(arguments, &["ArtifactMetadata"])?;
    let request_feedback = arg_bool(Some(meta), &["RequestFeedback"]).unwrap_or(false);
    let user_facing = arg_bool(Some(meta), &["UserFacing"]).unwrap_or(false);
    if !request_feedback && !user_facing {
        return None;
    }
    let summary = arg_str(Some(meta), &["Summary"]);
    let basename = arg_str(arguments, &["Path", "TargetFile", "path"]).and_then(path_basename);
    let detail = summary.map(|text| truncate(first_non_empty_line(text), 140));
    if request_feedback {
        let title = basename
            .map(|name| format!("Plan: {}", truncate(name, 60)))
            .unwrap_or_else(|| "Implementation plan".to_owned());
        let inline_content = arg_str(arguments, &["Content", "content"]).map(str::to_owned);
        Some((ActivityKind::Plan, title, detail, inline_content))
    } else {
        let title = basename
            .map(|name| format!("Artifact: {}", truncate(name, 60)))
            .unwrap_or_else(|| "Artifact".to_owned());
        Some((ActivityKind::FileChange, title, detail, None))
    }
}

fn extract_factory_mcp_identity(
    update: &Value,
    wire_title: Option<&str>,
) -> (Option<String>, Option<String>) {
    let factory = update
        .pointer("/_meta/factory")
        .or_else(|| update.pointer("/rawInput/_meta/factory"))
        .and_then(Value::as_object);
    let Some(factory) = factory else {
        return (None, None);
    };
    let is_mcp = factory
        .get("source")
        .and_then(Value::as_str)
        .is_some_and(|source| source.eq_ignore_ascii_case("mcp"));
    let meta_server = factory
        .get("server")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|server| !server.is_empty())
        .map(str::to_owned);
    let meta_tool = factory
        .get("tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|tool| !tool.is_empty())
        .map(str::to_owned);
    if !is_mcp && (meta_server.is_none() || meta_tool.is_none()) {
        return (None, None);
    }
    let title_pair = wire_title
        .map(str::trim)
        .and_then(|title| title.split_once(": "))
        .map(|(server, tool)| (server.trim(), tool.trim()))
        .filter(|(server, tool)| !server.is_empty() && !tool.is_empty());
    let server = meta_server.or_else(|| title_pair.map(|(server, _)| server.to_owned()));
    let tool = meta_tool.or_else(|| title_pair.map(|(_, tool)| tool.to_owned()));
    (server, tool)
}

fn is_file_search_tool(wire_title: Option<&str>, arguments: Option<&Value>) -> bool {
    if arg_str(arguments, &["url"]).is_some() {
        return false;
    }
    if let Some(title) = wire_title.map(str::trim)
        && (title.starts_with("Search web")
            || title.starts_with("search web")
            || title.starts_with("Fetch "))
    {
        return false;
    }
    if arg_str(arguments, &["pattern", "glob", "regex", "folder", "file_pattern", "include"])
        .is_some()
        || arg_array(arguments, &["patterns"]).is_some_and(|items| !items.is_empty())
    {
        return true;
    }
    wire_title.map(str::trim).is_some_and(|title| {
        ActivityKind::from_tool_name(title) == ActivityKind::FileSearch
            || title.starts_with("Search \"")
            || title.starts_with("Find ")
            || title.starts_with("Grep ")
            || title.starts_with("Glob ")
    })
}

fn is_file_list_tool(wire_title: Option<&str>, arguments: Option<&Value>) -> bool {
    if let Some(title) = wire_title.map(str::trim)
        && (ActivityKind::from_tool_name(title) == ActivityKind::FileList
            || title.starts_with("List ")
            || title.starts_with("list "))
    {
        return true;
    }
    arg_str(arguments, &["file_path", "filePath"]).is_none()
        && arg_str(arguments, &["directory_path", "directoryPath"]).is_some()
}

fn is_spec_approval_title(wire_title: Option<&str>) -> bool {
    wire_title.map(str::trim).is_some_and(|title| {
        title.eq_ignore_ascii_case("Approve Spec")
            || title.eq_ignore_ascii_case("Propose Specification")
    })
}

pub(crate) fn build_tool_activity_item(
    update: &Value,
    tools: &mut HashMap<String, StoredToolCall>,
    terminals: Option<&AcpTerminalRegistry>,
) -> ActivityItem {
    let id = update
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let stored = id.as_ref().and_then(|id| tools.get(id).cloned());
    let status = update.get("status").and_then(Value::as_str);
    let complete = match status {
        Some("completed" | "failed") => true,
        Some(_) => false,
        None => stored.as_ref().is_some_and(|tool| tool.complete),
    };
    let failed = match status {
        Some("failed") => true,
        Some(_) => false,
        None => stored.as_ref().is_some_and(|tool| tool.failed),
    };

    let wire_kind = update.get("kind").and_then(Value::as_str);
    let wire_title = update.get("title").and_then(Value::as_str);
    let arguments = update
        .get("rawInput")
        .filter(|value| !value.is_null())
        .or_else(|| stored.as_ref().and_then(|tool| tool.raw_input.as_ref()));
    let locations = update
        .get("locations")
        .filter(|value| !value.is_null())
        .or_else(|| stored.as_ref().and_then(|tool| tool.locations.as_ref()));
    let content = update
        .get("content")
        .filter(|value| !value.is_null())
        .or_else(|| stored.as_ref().and_then(|tool| tool.content.as_ref()));
    let (factory_mcp_server, factory_mcp_tool) = extract_factory_mcp_identity(update, wire_title);

    let canonical_antigravity_tool = arguments
        .and_then(|input| input.get("tool_name"))
        .and_then(Value::as_str)
        .and_then(antigravity_canonical_tool_name)
        .or_else(|| wire_title.and_then(antigravity_canonical_tool_name))
        .or_else(|| {
            stored
                .as_ref()
                .and_then(|tool| tool.tool_name.as_deref())
                .and_then(antigravity_canonical_tool_name)
        });

    let artifact_presentation = canonical_antigravity_tool
        .is_none()
        .then(|| antigravity_artifact_presentation(arguments))
        .flatten();

    let mut kind = if canonical_antigravity_tool == Some("exit_plan_mode") {
        ActivityKind::Plan
    } else if canonical_antigravity_tool.is_some() {
        ActivityKind::Tool
    } else if let Some((artifact_kind, _, _, _)) = &artifact_presentation {
        *artifact_kind
    } else {
        let mut kind = wire_kind
            .map(classify)
            .filter(|classified| *classified != ActivityKind::Tool || stored.is_none())
            .or_else(|| stored.as_ref().map(|tool| tool.kind))
            .unwrap_or(ActivityKind::Tool);
        if matches!(kind, ActivityKind::Search | ActivityKind::Tool)
            && let Some(wire_title) = wire_title
        {
            let named_kind = ActivityKind::from_tool_name(wire_title);
            if named_kind != ActivityKind::Tool {
                kind = named_kind;
            }
        }
        if kind == ActivityKind::Search && is_file_search_tool(wire_title, arguments) {
            kind = ActivityKind::FileSearch;
        } else if kind == ActivityKind::FileRead && is_file_list_tool(wire_title, arguments) {
            kind = ActivityKind::FileList;
        } else if kind == ActivityKind::Tool && is_spec_approval_title(wire_title) {
            kind = ActivityKind::Plan;
        }
        kind
    };
    if kind == ActivityKind::Tool && content_has_diff(content) {
        kind = ActivityKind::FileChange;
    }

    let antigravity_presentation = canonical_antigravity_tool
        .map(|canonical| antigravity_tool_presentation(canonical, arguments))
        .or_else(|| {
            artifact_presentation
                .as_ref()
                .map(|(_, title, detail, _)| (title.clone(), detail.clone()))
        });
    let title = antigravity_presentation
        .as_ref()
        .map(|(title, _)| title.clone())
        .or_else(|| activity::input_title(arguments))
        .or_else(|| {
            wire_title
                .map(str::trim)
                .filter(|title| {
                    !title.is_empty() && (!title.eq_ignore_ascii_case("tool") || stored.is_none())
                })
                .map(str::to_owned)
        })
        .or_else(|| stored.as_ref().map(|tool| tool.title.clone()))
        .unwrap_or_else(|| {
            if kind == ActivityKind::FileChange {
                tr!("activity.edit_file")
            } else {
                "Tool".to_owned()
            }
        });
    let mcp_server = factory_mcp_server
        .or_else(|| stored.as_ref().and_then(|tool| tool.mcp_server.clone()));
    let tool_name = canonical_antigravity_tool
        .map(str::to_owned)
        .or(factory_mcp_tool)
        .or_else(|| {
            arguments
                .and_then(|input| input.get("tool_name"))
                .and_then(Value::as_str)
                .or_else(|| wire_title.filter(|title| title.starts_with("mcp__")))
                .or_else(|| stored.as_ref().and_then(|tool| tool.tool_name.as_deref()))
                .map(str::to_owned)
        });
    let normalized_locations = locations.map(normalize_acp_locations);
    if let Some(id) = id.as_ref() {
        tools.insert(
            id.clone(),
            StoredToolCall {
                kind,
                title: title.clone(),
                raw_input: arguments.cloned(),
                locations: normalized_locations.clone(),
                content: (!complete).then(|| content.cloned()).flatten(),
                tool_name: tool_name.clone(),
                mcp_server: mcp_server.clone(),
                complete,
                failed,
            },
        );
    }

    let fallback_plan_output = if canonical_antigravity_tool == Some("exit_plan_mode")
        || kind == ActivityKind::Plan
    {
        arg_str(arguments, &["plan", "spec", "content", "summary"])
            .map(|plan| Value::String(plan.to_owned()))
    } else {
        artifact_presentation
            .as_ref()
            .and_then(|(_, _, _, content)| content.as_ref())
            .map(|text| Value::String(text.clone()))
    };
    let raw_output = content.or_else(|| update.get("rawOutput").filter(|value| !value.is_null()));
    let output = raw_output.or(fallback_plan_output.as_ref());
    let mut item =
        activity::tool_activity(id, kind, title, arguments, output, raw_output, failed, complete)
            .with_tool_name(tool_name.as_deref())
            .with_mcp_server(mcp_server.as_deref());
    if normalized_locations.is_some() || content.is_some() {
        let mut source = Map::new();
        if let Some(arguments) = arguments {
            source.insert("rawInput".to_owned(), arguments.clone());
        }
        if let Some(locations) = normalized_locations.as_ref() {
            source.insert("locations".to_owned(), locations.clone());
        }
        if let Some(content) = content {
            source.insert("content".to_owned(), content.clone());
        }
        item = item.with_activity_source(Some(&Value::Object(source)));
    }
    if let Some((presentation_title, presentation_detail)) = antigravity_presentation {
        item.display_target = Some(presentation_title);
        if item.detail.is_none() {
            item.detail = presentation_detail;
        }
    }
    if item.output.is_none()
        && let Some(terminals) = terminals
        && let Some(terminal_output) = resolve_terminal_content_output(content, terminals)
    {
        item.output = Some(terminal_output);
    }
    item
}

fn tool_activity(update: &Value, events: &impl DriverEventSink, state: &mut AcpStreamState) {
    let item = build_tool_activity_item(update, &mut state.tools, Some(&state.terminals));
    if item.complete {
        state.last_completed_tool_name = item.tool_name.clone();
    }
    let _ = events.send(DriverEvent::RichActivity(item));
}

fn classify(kind: &str) -> ActivityKind {
    match kind {
        "execute" => ActivityKind::Command,
        "edit" | "delete" | "move" => ActivityKind::FileChange,
        "read" => ActivityKind::FileRead,
        "search" | "fetch" => ActivityKind::Search,
        "switch_mode" => ActivityKind::Plan,
        "think" => ActivityKind::Reasoning,
        _ => ActivityKind::Tool,
    }
}

impl DriverControl for AcpDriver {
    fn prompt(&self, prompt: String) {
        let _ = self.commands.try_send(CommandMessage::Prompt(prompt));
    }

    fn supports_steer(&self) -> bool {
        self.supports_steer
    }

    fn steer(&self, prompt: String) {
        let _ = self.commands.try_send(CommandMessage::Steer(prompt));
    }

    fn cancel(&self) {
        let _ = self.commands.try_send(CommandMessage::Cancel);
    }

    fn cancel_computer_use(&self) {
        if let Some(computer_use) = self.computer_use.as_ref() {
            computer_use.stop();
        }
    }

    fn refresh_background_work(&self) {
        let _ = self
            .commands
            .try_send(CommandMessage::RefreshBackgroundWork);
    }

    fn stop_background_work(&self, key: BackgroundWorkKey, control_id: String) {
        let _ = self
            .commands
            .try_send(CommandMessage::StopBackgroundWork { key, control_id });
    }

    fn respond(&self, request_id: String, option_id: String) {
        let _ = self.commands.try_send(CommandMessage::Respond {
            request_id,
            option_id,
        });
    }

    fn respond_user_input(&self, request_id: String, answers: Vec<UserInputAnswer>) {
        let _ = self.commands.try_send(CommandMessage::RespondUserInput {
            request_id,
            answers,
        });
    }

    fn apply_options(&self, options: SessionOptions) -> bool {
        let mode = options.mode;
        if mode == *self.mode.lock() {
            return self
                .commands
                .try_send(CommandMessage::Options {
                    options,
                    mode_applied: None,
                })
                .is_ok();
        }
        if !self.switches_mode_live {
            return false;
        }
        // Callers run this off the UI thread, and a refusal must be known
        // before answering: `false` makes the app restart under the new mode.
        let (mode_applied, reply) = smol::channel::bounded(1);
        if self
            .commands
            .try_send(CommandMessage::Options {
                options,
                mode_applied: Some(mode_applied),
            })
            .is_err()
        {
            return false;
        }
        let applied = smol::block_on(smol::future::or(
            async { reply.recv().await.unwrap_or(false) },
            async {
                smol::Timer::after(MODE_SWITCH_TIMEOUT).await;
                false
            },
        ));
        if applied {
            *self.mode.lock() = mode;
        }
        applied
    }

    fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        Err(anyhow!(
            "conversation rollback is not supported by this provider transport"
        ))
    }

    fn fork(&self, turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
        let (tx, rx) = smol::channel::bounded(1);
        self.commands
            .try_send(CommandMessage::Fork {
                turns_to_remove,
                reply: tx,
            })
            .map_err(|_| anyhow!("ACP runtime is no longer running"))?;
        smol::block_on(smol::future::or(
            async {
                rx.recv()
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("ACP runtime closed before fork completed")))
            },
            async {
                smol::Timer::after(MODE_SWITCH_TIMEOUT).await;
                Err(anyhow!("timed out waiting for ACP session/fork"))
            },
        ))
    }
}

impl Drop for AcpDriver {
    fn drop(&mut self) {
        self.cancel_computer_use();
        let _ = self.commands.try_send(CommandMessage::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        SessionConfigSelectOption, SessionMode, SessionModeState, ToolCallUpdate,
        ToolCallUpdateFields,
    };

    fn select_config_option(
        id: &str,
        category: SessionConfigOptionCategory,
        current: &str,
        values: &[&str],
    ) -> SessionConfigOption {
        SessionConfigOption::select(
            id.to_owned(),
            id.to_owned(),
            current.to_owned(),
            values
                .iter()
                .map(|value| SessionConfigSelectOption::new((*value).to_owned(), *value))
                .collect::<Vec<_>>(),
        )
        .category(category)
    }

    #[test]
    fn cursor_question_response_uses_native_scalar_and_array_answers() {
        let params = json!({
            "toolCallId": "ask-1",
            "questions": [
                {
                    "id": "scope",
                    "prompt": "Which scope?",
                    "options": [{"id": "workspace", "label": "Workspace"}]
                },
                {
                    "id": "checks",
                    "prompt": "Which checks?",
                    "options": [
                        {"id": "tests", "label": "Tests"},
                        {"id": "lint", "label": "Lint"}
                    ],
                    "allowMultiple": true
                }
            ]
        });

        let questions = cursor_user_input_questions(&params);
        assert_eq!(questions.len(), 2);
        assert!(!questions[0].multi_select);
        assert!(questions[1].multi_select);

        let response = cursor_user_input_response(
            &params,
            &[
                UserInputAnswer {
                    question_id: "scope".into(),
                    answers: vec!["Workspace".into()],
                },
                UserInputAnswer {
                    question_id: "checks".into(),
                    answers: vec!["Tests".into(), "Lint".into()],
                },
            ],
        );
        assert_eq!(
            response.pointer("/answers/scope"),
            Some(&json!("Workspace"))
        );
        assert_eq!(
            response.pointer("/answers/checks"),
            Some(&json!(["Tests", "Lint"]))
        );
    }

    #[test]
    fn droid_elicitation_form_keeps_titles_on_screen_and_const_values_in_the_reply() {
        use agent_client_protocol::schema::v1::{
            BooleanPropertySchema, ElicitationFormMode, ElicitationSchema, ElicitationSessionScope,
            IntegerPropertySchema, MultiSelectPropertySchema, SessionId, StringPropertySchema,
        };

        let message = "The review leaves a few decisions to you. Before I change any code, I need your answers on these:";
        let schema = ElicitationSchema::new()
            .property(
                "scope",
                StringPropertySchema::new()
                    .title("Scope")
                    .description("Which part should change?")
                    .one_of(vec![
                        EnumOption::new("keep", "Keep the review")
                            .description("Leave the findings in place"),
                        EnumOption::new("rewrite", "Rewrite it"),
                    ]),
                true,
            )
            .property(
                "checks",
                MultiSelectPropertySchema::titled(vec![
                    EnumOption::new("tests", "Tests"),
                    EnumOption::new("lint", "Lint"),
                ])
                .title("Checks"),
                true,
            )
            .property("ship", BooleanPropertySchema::new().title("Ship"), true)
            .property("limit", IntegerPropertySchema::new().title("Limit"), false);
        let request = CreateElicitationRequest::new(
            ElicitationFormMode::new(
                ElicitationSessionScope::new(SessionId::new("session-1")),
                schema,
            ),
            message,
        );
        let ElicitationMode::Form(form) = &request.mode else {
            panic!("expected a form elicitation");
        };
        let questions = elicitation_user_input_questions(message, &form.requested_schema);

        assert_eq!(
            questions
                .iter()
                .map(|question| question.id.as_str())
                .collect::<Vec<_>>(),
            vec!["scope", "checks", "ship", "limit"]
        );
        assert_eq!(questions[0].header, "Scope");
        assert!(questions[0].question.contains(message));
        assert!(questions[0].question.contains("Which part should change?"));
        assert_eq!(questions[0].options[0].label, "Keep the review");
        assert_eq!(
            questions[0].options[0].description.as_deref(),
            Some("Leave the findings in place")
        );
        assert!(!questions[0].multi_select);
        assert!(questions[1].multi_select);
        assert_eq!(
            questions[2]
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Yes", "No"]
        );
        assert!(questions[3].options.is_empty());
        assert_eq!(questions[3].question, "Limit");
        assert!(questions[0].required);
        assert!(!questions[0].accepts_custom);
        assert!(questions[1].required && !questions[1].accepts_custom);
        assert!(questions[2].required && !questions[2].accepts_custom);
        assert!(!questions[3].required);
        assert!(questions[3].accepts_custom);

        let response = elicitation_accept_response(
            &request,
            &[
                UserInputAnswer {
                    question_id: "scope".into(),
                    answers: vec!["Keep the review".into()],
                },
                UserInputAnswer {
                    question_id: "checks".into(),
                    answers: vec!["Tests".into(), "Lint".into()],
                },
                UserInputAnswer {
                    question_id: "ship".into(),
                    answers: vec!["Yes".into()],
                },
                UserInputAnswer {
                    question_id: "limit".into(),
                    answers: vec!["12".into()],
                },
            ],
        );
        let value = serde_json::to_value(response).expect("elicitation response");
        assert_eq!(value["action"], "accept");
        assert_eq!(value["content"]["scope"], "keep");
        assert_eq!(value["content"]["checks"], json!(["tests", "lint"]));
        assert_eq!(value["content"]["ship"], true);
        assert_eq!(value["content"]["limit"], 12);

        let omitted = serde_json::to_value(elicitation_accept_response(
            &request,
            &[
                UserInputAnswer {
                    question_id: "scope".into(),
                    answers: vec!["Keep the review".into()],
                },
                UserInputAnswer {
                    question_id: "checks".into(),
                    answers: vec!["Tests".into(), "Lint".into()],
                },
                UserInputAnswer {
                    question_id: "ship".into(),
                    answers: vec!["Yes".into()],
                },
            ],
        ))
        .expect("elicitation response");
        assert!(omitted["content"].get("limit").is_none());
    }

    #[test]
    fn grok_question_response_keeps_native_labels_and_annotates_custom_text() {
        let params = json!({
            "sessionId": "session-1",
            "toolCallId": "tool-1",
            "mode": "default",
            "questions": [
                {
                    "id": "environment",
                    "question": "Where should this deploy?",
                    "options": [{"label": "Preview", "preview": "Deploy to preview"}],
                    "multiSelect": false
                },
                {
                    "id": "notes",
                    "question": "Anything else?",
                    "options": [{"label": "No"}],
                    "multiSelect": false
                }
            ]
        });
        let response = xai_user_input_response(
            &params,
            &[
                UserInputAnswer {
                    question_id: "environment".into(),
                    answers: vec!["Preview".into()],
                },
                UserInputAnswer {
                    question_id: "notes".into(),
                    answers: vec!["Use the EU region".into()],
                },
            ],
        );

        assert_eq!(response["outcome"], "accepted");
        assert_eq!(
            response.pointer("/answers/Where should this deploy?/0"),
            Some(&json!("Preview"))
        );
        assert_eq!(
            response.pointer("/answers/Anything else?/0"),
            Some(&json!("Other"))
        );
        assert_eq!(
            response.pointer("/annotations/Where should this deploy?/preview"),
            Some(&json!("Deploy to preview"))
        );
        assert_eq!(
            response.pointer("/annotations/Anything else?/notes"),
            Some(&json!("Use the EU region"))
        );
    }

    #[test]
    fn legacy_read_only_sessions_return_to_the_advertised_agent_mode() {
        let modes = SessionModeState::new(
            "plan",
            vec![
                SessionMode::new("agent", "Agent"),
                SessionMode::new("plan", "Plan"),
            ],
        );

        assert_eq!(
            desired_access_mode(ProviderKind::Cursor, Some(&modes), RuntimeMode::FullAccess)
                .map(|mode| mode.to_string()),
            Some("agent".to_owned())
        );
    }

    #[test]
    fn fx_access_mode_selects_ask_or_code() {
        let modes = SessionModeState::new(
            "code",
            vec![
                SessionMode::new("ask", "Ask before sensitive actions"),
                SessionMode::new("code", "Review sensitive actions automatically"),
            ],
        );
        assert_eq!(
            desired_access_mode(ProviderKind::Fx, Some(&modes), RuntimeMode::Ask)
                .map(|mode| mode.to_string()),
            Some("ask".to_owned())
        );
        assert!(
            desired_access_mode(ProviderKind::Fx, Some(&modes), RuntimeMode::FullAccess).is_none()
        );
    }

    #[test]
    fn fx_launches_its_documented_acp_subcommand() {
        let launch = launch_for(ProviderKind::Fx, None, None).unwrap();
        assert_eq!(launch.args, ["acp"]);
        assert!(launch.env.is_empty());
    }

    #[test]
    fn recognizes_antigravity_lost_trajectory_errors() {
        // Verbatim from `agy acp` after a turn that never persisted.
        assert!(is_lost_native_session(
            "Failed to send user message: not_found: trajectory not found: \
             ea82b742-509e-420b-9367-b85b48b375fa: not found"
        ));
        assert!(!is_lost_native_session("Internal error: quota exceeded"));
    }

    #[test]
    fn antigravity_launches_its_acp_server() {
        let launch = launch_for(ProviderKind::Antigravity, Some("high"), None).unwrap();
        assert_eq!(launch.args, ["acp"]);
        assert!(launch.env.is_empty());
        assert_eq!(
            reasoning_effort_config_id(ProviderKind::Antigravity),
            "thought_level"
        );
    }

    #[test]
    fn antigravity_launches_with_config_dir_isolation_and_account_env() {
        let root = std::env::temp_dir().join(format!(
            "waku-acp-antigravity-launch-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let adc = root.join("application_default_credentials.json");
        std::fs::write(&adc, "{}").unwrap();
        std::fs::write(root.join(".env"), "GEMINI_API_KEY=test-key\n").unwrap();

        let launch = launch_for(ProviderKind::Antigravity, None, Some(&root)).unwrap();
        assert_eq!(
            launch.args,
            vec![
                "acp".to_owned(),
                "--gemini-dir".to_owned(),
                root.to_string_lossy().into_owned(),
                "--app-data-dir".to_owned(),
                ".".to_owned(),
            ]
        );
        assert!(
            launch
                .env
                .contains(&("AGY_ADC_AUTH".to_owned(), "true".to_owned()))
        );
        assert!(launch.env.contains(&(
            "GOOGLE_APPLICATION_CREDENTIALS".to_owned(),
            adc.to_string_lossy().into_owned(),
        )));
        assert!(
            launch
                .env
                .contains(&("GEMINI_API_KEY".to_owned(), "test-key".to_owned()))
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn antigravity_access_mode_maps_auto_and_falls_back_to_default() {
        let modes_with_auto = SessionModeState::new(
            "default",
            vec![
                SessionMode::new("default", "Default"),
                SessionMode::new("accept-edits", "Accept edits"),
                SessionMode::new("auto", "Auto"),
                SessionMode::new("plan", "Plan"),
            ],
        );
        let mode_with_auto = |runtime| {
            desired_access_mode(ProviderKind::Antigravity, Some(&modes_with_auto), runtime)
                .map(|mode| mode.to_string())
        };
        assert_eq!(mode_with_auto(RuntimeMode::Ask), None);
        assert_eq!(mode_with_auto(RuntimeMode::Auto), Some("auto".to_owned()));
        assert_eq!(
            mode_with_auto(RuntimeMode::AutoAcceptEdits),
            Some("accept-edits".to_owned())
        );
        assert_eq!(mode_with_auto(RuntimeMode::FullAccess), None);

        // Older `agy` versions without `auto` fall back to `default`.
        let modes_without_auto = SessionModeState::new(
            "accept-edits",
            vec![
                SessionMode::new("default", "Default"),
                SessionMode::new("accept-edits", "Accept edits"),
                SessionMode::new("plan", "Plan"),
            ],
        );
        assert_eq!(
            desired_access_mode(
                ProviderKind::Antigravity,
                Some(&modes_without_auto),
                RuntimeMode::Auto
            )
            .map(|mode| mode.to_string()),
            Some("default".to_owned())
        );
        let modes_already_default = SessionModeState::new(
            "default",
            vec![
                SessionMode::new("default", "Default"),
                SessionMode::new("accept-edits", "Accept edits"),
                SessionMode::new("plan", "Plan"),
            ],
        );
        assert_eq!(
            desired_access_mode(
                ProviderKind::Antigravity,
                Some(&modes_already_default),
                RuntimeMode::Auto
            ),
            None
        );
    }

    #[test]
    fn antigravity_live_mode_switching_sends_set_mode_and_handles_noop_and_rejection() {
        use agent_client_protocol::schema::v1::SetSessionModeResponse;

        let requested_modes = Arc::new(Mutex::new(Vec::<String>::new()));
        let requested_for_agent = Arc::clone(&requested_modes);
        let mock_agent = Agent.builder().on_receive_request(
            async move |req: SetSessionModeRequest, responder, _cx| {
                let id = req.mode_id.to_string();
                requested_for_agent.lock().push(id.clone());
                if id == "accept-edits" {
                    responder.respond_with_error(agent_client_protocol::util::internal_error(
                        "mode rejected",
                    ))
                } else {
                    responder.respond(SetSessionModeResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        );

        let initial_modes = SessionModeState::new(
            "default",
            vec![
                SessionMode::new("default", "Default"),
                SessionMode::new("accept-edits", "Accept edits"),
                SessionMode::new("auto", "Auto"),
                SessionMode::new("plan", "Plan"),
            ],
        );
        smol::block_on(Client.builder().connect_with(
            mock_agent,
            async move |connection: ConnectionTo<Agent>| {
                let session_id = SessionId::new("agy-sess-1");
                // Ask <-> FullAccess both map to `default`: live no-op without RPC.
                assert_eq!(
                    apply_antigravity_mode(
                        &connection,
                        &session_id,
                        Some(&initial_modes),
                        RuntimeMode::FullAccess,
                        ACP_REQUEST_TIMEOUT,
                    )
                    .await
                    .unwrap(),
                    None
                );
                // Switching to Auto calls `session/set_mode` with `"auto"`.
                assert_eq!(
                    apply_antigravity_mode(
                        &connection,
                        &session_id,
                        Some(&initial_modes),
                        RuntimeMode::Auto,
                        ACP_REQUEST_TIMEOUT,
                    )
                    .await
                    .unwrap()
                    .map(|id| id.to_string()),
                    Some("auto".to_owned())
                );
                // A rejected transition returns Err so the caller fails closed.
                assert!(
                    apply_antigravity_mode(
                        &connection,
                        &session_id,
                        Some(&initial_modes),
                        RuntimeMode::AutoAcceptEdits,
                        ACP_REQUEST_TIMEOUT,
                    )
                    .await
                    .is_err()
                );
                // Missing mode advertisement returns Err.
                assert!(
                    apply_antigravity_mode(
                        &connection,
                        &session_id,
                        None,
                        RuntimeMode::Auto,
                        ACP_REQUEST_TIMEOUT,
                    )
                    .await
                    .is_err()
                );
                Ok(())
            },
        ))
        .unwrap();

        assert_eq!(*requested_modes.lock(), vec!["auto", "accept-edits"]);
    }

    #[test]
    fn antigravity_unsolicited_turn_lifecycle_handles_content_tools_usage_and_settlement() {
        let mut state = UnsolicitedTurnState::default();
        let now = Instant::now();

        // First content opens a reactive turn; subsequent content does not re-open.
        assert!(state.on_content_update());
        assert!(!state.on_content_update());
        assert!(state.active);

        // In-flight tool pauses settlement.
        assert!(!state.schedule_settle(now, Duration::from_secs(6), true));
        assert!(state.settle_at.is_none());

        // Tool completes -> arms idle settle timer.
        assert!(state.schedule_settle(now, Duration::from_secs(6), false));
        let (idle_deadline, idle_gen) = state.settle_at.expect("idle settle deadline");
        assert_eq!(idle_deadline, now + Duration::from_secs(6));

        // Trailing usage_update tightens settle deadline and bumps generation.
        let t1 = now + Duration::from_millis(50);
        assert!(state.schedule_settle(t1, Duration::from_secs(2), false));
        let (usage_deadline, usage_gen) = state.settle_at.expect("usage settle deadline");
        assert_eq!(usage_deadline, t1 + Duration::from_secs(2));
        assert_ne!(usage_gen, idle_gen);

        // Stale generation cannot settle.
        assert!(!state.try_settle(idle_gen, false));
        assert!(state.active);

        // In-flight work blocks even the current generation.
        assert!(!state.try_settle(usage_gen, true));
        assert!(state.active);

        // Re-arm and settle cleanly.
        assert!(state.schedule_settle(t1, Duration::from_secs(2), false));
        let (_, settle_gen) = state.settle_at.unwrap();
        assert!(state.try_settle(settle_gen, false));
        assert!(!state.active);
        assert!(state.settle_at.is_none());

        // User prompt/steer/cancel cleanly takes ownership of an active turn.
        assert!(state.on_content_update());
        assert!(state.take_active());
        assert!(!state.active);
        assert!(!state.take_active());

        // UserMessageChunk and metadata must never trigger unsolicited turns.
        let user_chunk: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "user_message_chunk",
            "content": {"type": "text", "text": "echo"}
        }))
        .unwrap();
        let usage_update: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "usage_update",
            "used": 100,
            "size": 1000
        }))
        .unwrap();
        let agent_chunk: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "woke up"}
        }))
        .unwrap();
        assert!(!is_unsolicited_turn_content(&user_chunk));
        assert!(!is_unsolicited_turn_content(&usage_update));
        assert!(is_unsolicited_turn_content(&agent_chunk));

        // Settle delay selection: post-tool thinking uses 25s, pre-tool usage_update
        // does not shrink to 2s, swarm_wait completion uses 2s, and
        // AgentMessageChunk + UsageUpdate uses 2s.
        let tool_call: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "c1",
            "title": "Run read_board",
            "status": "in_progress"
        }))
        .unwrap();
        let tool_done: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "c1",
            "status": "completed"
        }))
        .unwrap();
        let thought_chunk: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "thinking after tool"}
        }))
        .unwrap();

        let mut state = UnsolicitedTurnState::default();
        assert!(state.on_content_update());
        // Pre-tool usage_update before any message must NOT shrink settle delay to 2s.
        state.on_stream_update(&usage_update, None);
        assert_eq!(
            state.settle_delay(&usage_update),
            ANTIGRAVITY_UNSOLICITED_THINKING_SETTLE_DELAY
        );

        // Tool runs and finishes -> post-tool thinking gets 25s even on usage_update.
        state.on_stream_update(&tool_call, None);
        state.on_stream_update(&tool_done, Some("read_board"));
        assert_eq!(
            state.settle_delay(&tool_done),
            ANTIGRAVITY_UNSOLICITED_THINKING_SETTLE_DELAY
        );
        state.on_stream_update(&thought_chunk, None);
        assert_eq!(
            state.settle_delay(&thought_chunk),
            ANTIGRAVITY_UNSOLICITED_THINKING_SETTLE_DELAY
        );
        state.on_stream_update(&usage_update, None);
        assert_eq!(
            state.settle_delay(&usage_update),
            ANTIGRAVITY_UNSOLICITED_THINKING_SETTLE_DELAY
        );

        // AgentMessageChunk gets 6s idle, then trailing UsageUpdate shrinks to 2s.
        state.on_stream_update(&agent_chunk, None);
        assert_eq!(
            state.settle_delay(&agent_chunk),
            ANTIGRAVITY_UNSOLICITED_IDLE_SETTLE_DELAY
        );
        state.on_stream_update(&usage_update, None);
        assert_eq!(
            state.settle_delay(&usage_update),
            ANTIGRAVITY_UNSOLICITED_USAGE_SETTLE_DELAY
        );

        // Completing swarm_wait settles quickly in 2s.
        state.on_stream_update(&tool_call, None);
        state.on_stream_update(&tool_done, Some("swarm_wait"));
        assert_eq!(
            state.settle_delay(&tool_done),
            ANTIGRAVITY_UNSOLICITED_USAGE_SETTLE_DELAY
        );
    }

    #[test]
    fn antigravity_session_load_drains_post_response_replay_while_delivering_metadata() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, LoadSessionResponse,
            SessionInfoUpdate,
        };

        let suppress = Arc::new(ReplaySuppression::default());
        let suppress_notif = Arc::clone(&suppress);
        let (events, event_rx) = crossbeam_channel::unbounded();
        let events_notif = events.clone();
        let stream_state = Arc::new(Mutex::new(AcpStreamState::default()));
        let stream_notif = Arc::clone(&stream_state);

        let mock_agent = Agent.builder().on_receive_request(
            async move |req: LoadSessionRequest, responder, cx| {
                // Pre-response replay chunk.
                let pre_chunk: SessionUpdate = serde_json::from_value(json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "historical-before"}
                }))
                .unwrap();
                cx.send_notification(SessionNotification::new(
                    req.session_id.clone(),
                    pre_chunk,
                ))?;
                responder.respond(LoadSessionResponse::new())?;
                // Post-response trailing replay + metadata (matching live `agy acp` behavior).
                let post_chunk: SessionUpdate = serde_json::from_value(json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "historical-after"}
                }))
                .unwrap();
                cx.send_notification(SessionNotification::new(
                    req.session_id.clone(),
                    post_chunk,
                ))?;
                cx.send_notification(SessionNotification::new(
                    req.session_id.clone(),
                    SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                        AvailableCommand::new("boost", "Boost quality"),
                        AvailableCommand::new("swarm", "Run a swarm"),
                    ])),
                ))?;
                cx.send_notification(SessionNotification::new(
                    req.session_id.clone(),
                    SessionUpdate::SessionInfoUpdate(
                        SessionInfoUpdate::new().title("Loaded Antigravity"),
                    ),
                ))?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        );

        let init = InitializeResponse::new(ProtocolVersion::V1)
            .agent_capabilities(AgentCapabilities::new().load_session(true));
        let cwd = std::env::temp_dir();
        let suppress_conn = Arc::clone(&suppress);
        smol::block_on(
            Client
                .builder()
                .on_receive_notification(
                    async move |notification: SessionNotification, _cx| {
                        if is_replay_transcript_update(&notification.update)
                            && suppress_notif.should_suppress_replay(
                                Instant::now(),
                                ANTIGRAVITY_POST_LOAD_GUARD_WINDOW,
                            )
                        {
                            return Ok(());
                        }
                        handle_session_update(
                            ProviderKind::Antigravity,
                            notification,
                            &events_notif,
                            &mut stream_notif.lock(),
                        )
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(mock_agent, async move |connection: ConnectionTo<Agent>| {
                    let (session_id, _, _) = establish_session(
                        &connection,
                        ProviderKind::Antigravity,
                        &init,
                        Some("agy-loaded-1"),
                        &cwd,
                        &[],
                        &suppress_conn,
                        ACP_REQUEST_TIMEOUT,
                    )
                    .await?;
                    assert_eq!(session_id.to_string(), "agy-loaded-1");
                    Ok(())
                }),
        )
        .unwrap();

        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, DriverEvent::TextDelta(_))),
            "both pre- and post-response replay text chunks must be suppressed: {seen:?}"
        );
        assert!(seen.iter().any(|event| matches!(
            event,
            DriverEvent::AvailableCommands(commands)
                if commands.iter().any(|c| c.name == "boost")
                    && commands.iter().any(|c| c.name == "swarm")
        )));
        assert!(seen.iter().any(|event| matches!(
            event,
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Loaded Antigravity"
        )));
    }

    #[test]
    fn antigravity_reduces_variant_ids_to_advertised_families() {
        let advertised = ["gemini-4-carbon", "claude-opus-5-5", "gpt-oss-120b-medium"];
        assert_eq!(
            antigravity_model_selection(&advertised, "gemini-4-carbon"),
            Some(("gemini-4-carbon".to_owned(), None))
        );
        assert_eq!(
            antigravity_model_selection(&advertised, "gemini-4-carbon-medium"),
            Some(("gemini-4-carbon".to_owned(), Some("medium".to_owned())))
        );
        assert_eq!(
            antigravity_model_selection(&advertised, "gpt-oss-120b-medium"),
            Some(("gpt-oss-120b-medium".to_owned(), None)),
            "an advertised family is used as-is; its suffix is part of the id"
        );
        assert_eq!(
            antigravity_model_selection(&advertised, "claude-opus-5-5-high"),
            Some(("claude-opus-5-5".to_owned(), Some("high".to_owned())))
        );
        assert!(
            antigravity_model_selection(&advertised, "gemini-4-carbon-fast").is_some(),
            "an unknown suffix keeps the family without inventing an effort"
        );
        assert_eq!(
            antigravity_model_selection(&advertised, "gemini-unknown"),
            None
        );
    }

    #[test]
    fn antigravity_questions_use_permission_options_as_choices() {
        use agent_client_protocol::schema::v1::PermissionOption as AcpPermissionOption;
        let request = RequestPermissionRequest::new(
            "session-1",
            ToolCallUpdate::new(
                "interaction_abc",
                ToolCallUpdateFields::new().title("Which environment?"),
            ),
            vec![
                AcpPermissionOption::new("staging", "Staging", PermissionOptionKind::AllowOnce),
                AcpPermissionOption::new(
                    "production",
                    "Production",
                    PermissionOptionKind::AllowOnce,
                ),
            ],
        );

        let questions = antigravity_user_input_questions(&request).expect("question");
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].id, "interaction_abc");
        assert_eq!(questions[0].question, "Which environment?");
        assert!(!questions[0].multi_select);
        assert!(!questions[0].accepts_custom);
        assert_eq!(
            questions[0]
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["Staging", "Production"]
        );

        let response = antigravity_question_response(
            &request,
            &[UserInputAnswer {
                question_id: "interaction_abc".into(),
                answers: vec!["Production".into()],
            }],
        );
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Selected(choice)
                if choice.option_id.to_string() == "production"
        ));
    }

    #[test]
    fn antigravity_exposes_the_allow_always_security_warning() {
        use agent_client_protocol::schema::v1::PermissionOption as AcpPermissionOption;
        let mut meta = serde_json::Map::new();
        meta.insert(
            "agy.security.warning".into(),
            json!({"title": "Runs outside the sandbox", "message": "This command can modify the host."}),
        );
        let request = RequestPermissionRequest::new(
            "session-1",
            ToolCallUpdate::new("call-1", ToolCallUpdateFields::new().title("Run a command")),
            vec![
                AcpPermissionOption::new(
                    "allow",
                    "Allow for this thread",
                    PermissionOptionKind::AllowAlways,
                )
                .meta(meta),
            ],
        );
        assert_eq!(
            antigravity_security_warning(&request).as_deref(),
            Some("This command can modify the host.")
        );
    }

    #[test]
    fn antigravity_questions_are_not_approval_requests() {
        use agent_client_protocol::schema::v1::PermissionOption as AcpPermissionOption;
        let request = RequestPermissionRequest::new(
            "session-1",
            ToolCallUpdate::new("call-1", ToolCallUpdateFields::new().title("Run the tests")),
            vec![AcpPermissionOption::new(
                "allow",
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
        );
        assert!(antigravity_user_input_questions(&request).is_none());
    }

    #[test]
    fn session_load_suppression_drops_replay_transcript_but_keeps_metadata() {
        let replay_updates = [
            json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old prompt"}}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old answer"}}),
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"old thought"}}),
            json!({"sessionUpdate":"tool_call","toolCallId":"c1","title":"read_file","kind":"read","status":"completed"}),
            json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed"}),
            json!({"sessionUpdate":"plan","entries":[{"content":"Step 1","priority":"high","status":"completed"}]}),
        ];
        for raw in replay_updates {
            let update: SessionUpdate = serde_json::from_value(raw).unwrap();
            assert!(
                is_replay_transcript_update(&update),
                "expected replay transcript update to be suppressed: {update:?}"
            );
        }

        let metadata_updates = [
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [{"name": "swarm", "description": "Run a swarm"}]
            }),
            json!({"sessionUpdate": "usage_update", "used": 4096, "size": 200_000}),
            json!({"sessionUpdate": "session_info_update", "title": "Resumed Antigravity task"}),
        ];
        let (events, event_rx) = crossbeam_channel::unbounded();
        let mut state = AcpStreamState::default();
        for raw in metadata_updates {
            let update: SessionUpdate = serde_json::from_value(raw).unwrap();
            assert!(
                !is_replay_transcript_update(&update),
                "metadata update must pass through during session/load: {update:?}"
            );
            handle_session_update(
                ProviderKind::Antigravity,
                SessionNotification::new("s-load", update),
                &events,
                &mut state,
            )
            .unwrap();
        }
        assert!(
            !state.produced_content,
            "metadata updates must not mark produced_content"
        );
        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 3);
        assert!(matches!(
            &seen[0],
            DriverEvent::AvailableCommands(commands)
                if commands.len() == 1 && commands[0].name == "swarm"
        ));
        assert!(matches!(
            &seen[1],
            DriverEvent::UsageUpdated {
                context_tokens: Some(4096),
                context_window: Some(200_000),
            }
        ));
        assert!(matches!(
            &seen[2],
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Resumed Antigravity task"
        ));
    }

    #[test]
    fn antigravity_swarm_subagent_and_boost_tools_have_readable_presentations() {
        let mut tools = HashMap::new();

        // 1. swarm add with live `agy acp` "Run swarm" title + update omitting title/rawInput
        let add_start = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-swarm-add",
                "title": "Run swarm",
                "kind": "other",
                "status": "in_progress",
                "rawInput": {
                    "action": "add",
                    "name": "acp-core",
                    "brief": "Own ACP driver updates.\nSecond line."
                }
            }),
            &mut tools,
            None,
        );
        assert_eq!(add_start.kind, ActivityKind::Tool);
        assert_eq!(add_start.tool_name.as_deref(), Some("swarm"));
        assert_eq!(add_start.title, "Swarm: add acp-core");
        assert_eq!(
            add_start.display_target.as_deref(),
            Some("Swarm: add acp-core")
        );
        assert_eq!(add_start.detail.as_deref(), Some("Own ACP driver updates."));

        let add_done = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-swarm-add",
                "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "spawned"}}]
            }),
            &mut tools,
            None,
        );
        assert_eq!(add_done.tool_name.as_deref(), Some("swarm"));
        assert_eq!(add_done.title, "Swarm: add acp-core");
        assert_eq!(
            add_done.display_target.as_deref(),
            Some("Swarm: add acp-core")
        );

        // 2. swarm title must not collide with generic input_title("title")
        let title_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-swarm-title",
                "title": "swarm",
                "status": "completed",
                "rawInput": {"action": "title", "title": "Antigravity integration"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(title_item.tool_name.as_deref(), Some("swarm"));
        assert_eq!(title_item.title, "Swarm: title \"Antigravity integration\"");

        // 3. post_message with live "Run post_message" title and leading @mention
        let post_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-post",
                "title": "Run post_message",
                "kind": "other",
                "status": "completed",
                "rawInput": {"text": "@verifier Ready for review\nAll tests pass."}
            }),
            &mut tools,
            None,
        );
        assert_eq!(post_item.tool_name.as_deref(), Some("post_message"));
        assert_eq!(post_item.title, "Post to swarm board (@verifier)");
        assert_eq!(
            post_item.detail.as_deref(),
            Some("@verifier Ready for review")
        );

        // 4. read_board (even when agent sends "Run read_board" or kind="read")
        let board_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-read-board",
                "title": "Run read_board",
                "kind": "read",
                "status": "completed",
                "rawInput": {"since": 7, "limit": 20}
            }),
            &mut tools,
            None,
        );
        assert_eq!(board_item.kind, ActivityKind::Tool);
        assert_eq!(board_item.tool_name.as_deref(), Some("read_board"));
        assert_eq!(board_item.title, "Read swarm board (since #7)");
        assert_eq!(board_item.detail.as_deref(), Some("limit 20"));

        let roster_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-read-roster",
                "title": "read_board",
                "status": "completed",
                "rawInput": {"status": true}
            }),
            &mut tools,
            None,
        );
        assert_eq!(roster_item.title, "Read swarm roster");

        // 5. swarm_wait
        let wait_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-wait",
                "title": "Run swarm_wait",
                "kind": "other",
                "status": "in_progress",
                "rawInput": {"since": 12, "mentions_only": false}
            }),
            &mut tools,
            None,
        );
        assert_eq!(wait_item.tool_name.as_deref(), Some("swarm_wait"));
        assert_eq!(wait_item.title, "Wait on swarm board (since #12)");
        assert_eq!(
            wait_item.detail.as_deref(),
            Some("Waiting for any board message")
        );

        // 6. invoke_subagent
        let subagent_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-subagent",
                "title": "Run invoke_subagent",
                "kind": "other",
                "status": "completed",
                "rawInput": {"subagent_type": "code-reviewer", "description": "Audit ACP changes"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(subagent_item.tool_name.as_deref(), Some("invoke_subagent"));
        assert_eq!(subagent_item.title, "Subagent: code-reviewer");
        assert_eq!(subagent_item.detail.as_deref(), Some("Audit ACP changes"));

        // 7. boost + failure detail preservation
        let boost_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-boost",
                "title": "Run boost",
                "kind": "other",
                "status": "failed",
                "rawInput": {"mode": "deep", "prompt": "Analyze race condition"},
                "content": [{"type": "content", "content": {"type": "text", "text": "Quota exceeded"}}]
            }),
            &mut tools,
            None,
        );
        assert_eq!(boost_item.tool_name.as_deref(), Some("boost"));
        assert_eq!(boost_item.title, "Boost: deep");
        assert!(boost_item.failed);
        assert_eq!(boost_item.detail.as_deref(), Some("Quota exceeded"));

        // 8. PascalCase invoke_subagent, define_subagent, send_message, manage_subagents,
        // schedule, manage_task, run_workflow, swarm channel, and exit_plan_mode.
        let pascal_subagent = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-pascal-subagent",
                "title": "Run invoke_subagent",
                "status": "completed",
                "rawInput": {
                    "Role": "Codebase Researcher",
                    "TypeName": "research",
                    "Prompt": "Survey the ACP driver\nReturn file paths",
                    "Model": "inherit"
                }
            }),
            &mut tools,
            None,
        );
        assert_eq!(pascal_subagent.tool_name.as_deref(), Some("invoke_subagent"));
        assert_eq!(pascal_subagent.title, "Subagent: Codebase Researcher");
        assert_eq!(
            pascal_subagent.detail.as_deref(),
            Some("Survey the ACP driver")
        );

        let define_subagent = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-define-subagent",
                "title": "Run define_subagent",
                "status": "completed",
                "rawInput": {"Name": "auditor", "Description": "Security review agent"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(define_subagent.tool_name.as_deref(), Some("define_subagent"));
        assert_eq!(define_subagent.title, "Define subagent: auditor");
        assert_eq!(
            define_subagent.detail.as_deref(),
            Some("Security review agent")
        );

        let send_msg = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-send-msg",
                "title": "Run send_message",
                "status": "completed",
                "rawInput": {"Recipient": "auditor-1", "Message": "Check auth flow\nThanks"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(send_msg.tool_name.as_deref(), Some("send_message"));
        assert_eq!(send_msg.title, "Message subagent (auditor-1)");
        assert_eq!(send_msg.detail.as_deref(), Some("Check auth flow"));

        let manage_subs = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-manage-subs",
                "title": "Run manage_subagents",
                "status": "completed",
                "rawInput": {"Action": "list"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(manage_subs.tool_name.as_deref(), Some("manage_subagents"));
        assert_eq!(manage_subs.title, "Subagents: list");

        let sched = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-schedule",
                "title": "Run schedule",
                "status": "completed",
                "rawInput": {"CronExpression": "*/5 * * * *", "Prompt": "Check CI status"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(sched.tool_name.as_deref(), Some("schedule"));
        assert_eq!(sched.title, "Schedule: */5 * * * *");
        assert_eq!(sched.detail.as_deref(), Some("Check CI status"));

        let task_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-manage-task",
                "title": "Run manage_task",
                "status": "completed",
                "rawInput": {"Action": "status", "TaskId": "task-42"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(task_item.tool_name.as_deref(), Some("manage_task"));
        assert_eq!(task_item.title, "Task: status");
        assert_eq!(task_item.detail.as_deref(), Some("task-42"));

        let workflow_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-workflow",
                "title": "Run run_workflow",
                "status": "completed",
                "rawInput": {"Description": "Fan out lint checks", "ScriptPath": "scripts/lint.py"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(workflow_item.tool_name.as_deref(), Some("run_workflow"));
        assert_eq!(workflow_item.title, "Workflow: Fan out lint checks");

        let channel_item = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-swarm-channel",
                "title": "Run swarm",
                "status": "completed",
                "rawInput": {"action": "channel", "op": "create", "channel": "parser", "topic": "Lexer and parser"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(channel_item.tool_name.as_deref(), Some("swarm"));
        assert_eq!(channel_item.title, "Swarm: channel create #parser");
        assert_eq!(channel_item.detail.as_deref(), Some("Lexer and parser"));

        let exit_plan = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-exit-plan",
                "title": "Run exit_plan_mode",
                "kind": "other",
                "status": "completed",
                "rawInput": {"plan": "# Plan\n- Step 1\n- Step 2"}
            }),
            &mut tools,
            None,
        );
        assert_eq!(exit_plan.kind, ActivityKind::Plan);
        assert_eq!(exit_plan.tool_name.as_deref(), Some("exit_plan_mode"));
        assert_eq!(exit_plan.title, "Plan ready");
        assert_eq!(exit_plan.detail.as_deref(), Some("# Plan"));
        assert_eq!(exit_plan.output.as_deref(), Some("# Plan\n- Step 1\n- Step 2"));

        // 9. Antigravity /plan write_file with ArtifactMetadata and /C:/... location normalization.
        let plan_artifact = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-plan-artifact",
                "title": "File ../../.gemini/antigravity-cli/brain/123/plan.md",
                "kind": "edit",
                "status": "completed",
                "locations": [{"path": "/C:/Users/gaoxi/.gemini/antigravity-cli/brain/123/plan.md"}],
                "rawInput": {
                    "ArtifactMetadata": {
                        "RequestFeedback": true,
                        "Summary": "2-bullet implementation plan",
                        "UserFacing": true
                    },
                    "Content": "# Plan\n- Add blank line",
                    "Path": "C:\\Users\\gaoxi\\.gemini\\antigravity-cli\\brain\\123\\plan.md"
                }
            }),
            &mut tools,
            None,
        );
        assert_eq!(plan_artifact.kind, ActivityKind::Plan);
        assert_eq!(plan_artifact.title, "Plan: plan.md");
        assert_eq!(plan_artifact.display_target.as_deref(), Some("Plan: plan.md"));
        assert_eq!(
            plan_artifact.detail.as_deref(),
            Some("2-bullet implementation plan")
        );
        assert_eq!(
            plan_artifact.output.as_deref(),
            Some("# Plan\n- Add blank line")
        );
        let normalized_locations = normalize_acp_locations(&json!([
            {"path": "/C:/Users/gaoxi/.gemini/antigravity-cli/brain/123/plan.md"},
            {"path": "/tmp/plan.md"}
        ]));
        assert_eq!(
            normalized_locations.pointer("/0/path").and_then(Value::as_str),
            Some("C:/Users/gaoxi/.gemini/antigravity-cli/brain/123/plan.md")
        );
        assert_eq!(
            normalized_locations.pointer("/1/path").and_then(Value::as_str),
            Some("/tmp/plan.md")
        );

        let user_artifact = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-user-artifact",
                "title": "File report.md",
                "kind": "edit",
                "status": "completed",
                "locations": [{"path": "/C:/Users/gaoxi/report.md"}],
                "rawInput": {
                    "ArtifactMetadata": {
                        "RequestFeedback": false,
                        "Summary": "Audit report",
                        "UserFacing": true
                    },
                    "Path": "/C:/Users/gaoxi/report.md"
                }
            }),
            &mut tools,
            None,
        );
        assert_eq!(user_artifact.kind, ActivityKind::FileChange);
        assert_eq!(user_artifact.title, "Artifact: report.md");
        assert_eq!(user_artifact.detail.as_deref(), Some("Audit report"));
        assert_eq!(
            user_artifact
                .file_changes
                .first()
                .map(|change| change.path.as_str()),
            Some("C:/Users/gaoxi/report.md")
        );
    }

    #[test]
    fn droid_launches_the_registry_acp_daemon() {
        let launch = launch_for(ProviderKind::Droid, None, None).unwrap();
        assert_eq!(launch.args, ["exec", "--output-format", "acp-daemon"]);
        assert!(
            launch
                .env
                .contains(&("DROID_DISABLE_AUTO_UPDATE".into(), "true".into()))
        );
        assert!(
            launch
                .env
                .contains(&("FACTORY_DROID_AUTO_UPDATE_ENABLED".into(), "false".into()))
        );
    }

    #[test]
    fn droid_auto_approval_answers_once_without_raising_autonomy() {
        use agent_client_protocol::schema::v1::PermissionOption as AcpPermissionOption;
        let options = [
            AcpPermissionOption::new("proceed_once", "Allow", PermissionOptionKind::AllowOnce),
            AcpPermissionOption::new(
                "proceed_always",
                "Allow & auto-run low risk commands",
                PermissionOptionKind::AllowAlways,
            ),
            AcpPermissionOption::new("cancel", "No, cancel", PermissionOptionKind::RejectOnce),
        ];
        let id = |prefer_once| {
            auto_permission_choice(&options, prefer_once).map(|option| option.option_id.to_string())
        };
        assert_eq!(id(true).as_deref(), Some("proceed_once"));
        assert_eq!(id(false).as_deref(), Some("proceed_always"));
        assert!(auto_permission_choice(&options[2..], true).is_none());
    }

    #[test]
    fn droid_autonomy_follows_the_access_mode() {
        assert_eq!(droid_autonomy_level(RuntimeMode::Ask), "normal");
        assert_eq!(
            droid_autonomy_level(RuntimeMode::AutoAcceptEdits),
            "auto-low"
        );
        assert_eq!(droid_autonomy_level(RuntimeMode::Auto), "auto-medium");
        assert_eq!(droid_autonomy_level(RuntimeMode::FullAccess), "auto-high");
    }

    #[test]
    fn droid_leaves_requests_beyond_its_autonomy_to_the_user() {
        for mode in [
            RuntimeMode::Ask,
            RuntimeMode::AutoAcceptEdits,
            RuntimeMode::Auto,
        ] {
            assert!(!auto_approves_permissions(ProviderKind::Droid, mode));
        }
        assert!(auto_approves_permissions(
            ProviderKind::Droid,
            RuntimeMode::FullAccess
        ));
        assert!(!auto_approves_permissions(
            ProviderKind::Cursor,
            RuntimeMode::Ask
        ));
        assert!(auto_approves_permissions(
            ProviderKind::Cursor,
            RuntimeMode::AutoAcceptEdits
        ));
    }

    #[test]
    fn droid_permission_prompt_omits_the_autonomy_raising_answer() {
        use agent_client_protocol::schema::v1::PermissionOption as AcpPermissionOption;
        let options = [
            AcpPermissionOption::new("proceed_once", "Allow", PermissionOptionKind::AllowOnce),
            AcpPermissionOption::new(
                "proceed_always",
                "Allow & auto-run low risk commands",
                PermissionOptionKind::AllowAlways,
            ),
            AcpPermissionOption::new("cancel", "No, cancel", PermissionOptionKind::RejectOnce),
        ];
        let ids = |options: &[AcpPermissionOption], once_only| {
            offered_permission_options(options, once_only)
                .into_iter()
                .map(|option| option.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&options, true), ["proceed_once", "cancel"]);
        assert_eq!(
            ids(&options, false),
            ["proceed_once", "proceed_always", "cancel"]
        );
        // Without a one-time allow, "always" is the only way to proceed.
        assert_eq!(ids(&options[1..], true), ["proceed_always", "cancel"]);
    }

    #[test]
    fn config_updates_refresh_the_cache_and_flag_droid_autonomy_drift() {
        let cache: SharedConfigOptions = Arc::new(Mutex::new(None));
        let update = |current| {
            SessionUpdate::ConfigOptionUpdate(
                agent_client_protocol::schema::v1::ConfigOptionUpdate::new(vec![
                    select_config_option(
                        "autonomy_level",
                        SessionConfigOptionCategory::Mode,
                        current,
                        &DROID_AUTONOMY_LEVELS,
                    ),
                ]),
            )
        };

        assert!(track_session_state(
            ProviderKind::Droid,
            &update("auto-low"),
            RuntimeMode::Ask,
            &cache,
        ));
        let cached = cache.lock().clone().unwrap();
        assert_eq!(
            session_config_current_value(&cached[0]),
            Some("auto-low"),
            "the cache holds what the agent reported"
        );
        assert!(!track_session_state(
            ProviderKind::Droid,
            &update("normal"),
            RuntimeMode::Ask,
            &cache,
        ));
        // Other agents get the cache refresh but no autonomy correction.
        assert!(!track_session_state(
            ProviderKind::Cursor,
            &update("auto-low"),
            RuntimeMode::Ask,
            &cache,
        ));
    }

    #[test]
    fn config_updates_exempt_droid_spec_mode_from_autonomy_drift() {
        let cache: SharedConfigOptions = Arc::new(Mutex::new(None));
        let update = |current| {
            SessionUpdate::ConfigOptionUpdate(
                agent_client_protocol::schema::v1::ConfigOptionUpdate::new(vec![
                    select_config_option(
                        "autonomy_level",
                        SessionConfigOptionCategory::Mode,
                        current,
                        &["normal", "spec", "auto-low", "auto-medium", "auto-high"],
                    ),
                ]),
            )
        };

        assert!(!track_session_state(
            ProviderKind::Droid,
            &update("spec"),
            RuntimeMode::Ask,
            &cache,
        ));
        assert!(!track_session_state(
            ProviderKind::Droid,
            &update("spec"),
            RuntimeMode::AutoAcceptEdits,
            &cache,
        ));
        assert!(!track_session_state(
            ProviderKind::Droid,
            &update("spec"),
            RuntimeMode::FullAccess,
            &cache,
        ));
        let cached = cache.lock().clone().unwrap();
        assert_eq!(
            session_config_current_value(&cached[0]),
            Some("spec"),
            "spec mode config update still refreshes the cache without flagging drift"
        );
    }

    #[test]
    fn completed_tool_call_retains_metadata_for_sparse_follow_up_update() {
        let mut state = AcpStreamState::default();
        let initial = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-droid-1",
                "status": "completed",
                "kind": "read",
                "title": "Read crates/waku-core/src/lib.rs",
                "rawInput": {
                    "file_path": "crates/waku-core/src/lib.rs"
                },
                "locations": [
                    { "path": "crates/waku-core/src/lib.rs", "line": 1 }
                ]
            }),
            &mut state.tools,
            None,
        );
        assert_eq!(initial.kind, ActivityKind::FileRead);
        assert_eq!(initial.title, "Read crates/waku-core/src/lib.rs");
        assert!(initial.complete);
        assert!(
            !state.has_in_flight_tools(),
            "retaining completed tool metadata must not mark the tool as in-flight"
        );

        let follow_up = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-droid-1",
                "status": "completed",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "pub mod driver;" } }
                ],
                "rawOutput": "pub mod driver;"
            }),
            &mut state.tools,
            None,
        );
        assert_eq!(follow_up.kind, ActivityKind::FileRead);
        assert_eq!(follow_up.title, "Read crates/waku-core/src/lib.rs");
        assert_eq!(follow_up.output.as_deref(), Some("pub mod driver;"));
        assert!(follow_up.arguments.is_some());
        assert!(follow_up.complete);
        assert!(!follow_up.failed);
        assert!(!state.has_in_flight_tools());
        assert!(
            state.tools["call-droid-1"].content.is_none(),
            "completed tools must not hold onto full content buffers"
        );
    }

    #[test]
    fn failed_tool_call_retains_failed_kind_and_title_across_sparse_generic_update() {
        let mut state = AcpStreamState::default();
        let initial = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-fail-1",
                "status": "failed",
                "kind": "execute",
                "title": "Run cargo test",
                "rawInput": { "command": "cargo test" }
            }),
            &mut state.tools,
            None,
        );
        assert!(initial.complete);
        assert!(initial.failed);
        assert_eq!(initial.kind, ActivityKind::Command);
        assert_eq!(initial.title, "Run cargo test");

        // Follow-up omits `status` and sends generic `"title": "Tool"`.
        let sparse = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-fail-1",
                "title": "Tool",
                "rawOutput": "exit code 101"
            }),
            &mut state.tools,
            None,
        );
        assert!(sparse.complete);
        assert!(sparse.failed);
        assert_eq!(sparse.kind, ActivityKind::Command);
        assert_eq!(sparse.title, "Run cargo test");
        assert_eq!(sparse.output.as_deref(), Some("exit code 101"));
    }

    #[test]
    fn droid_mode_updates_flag_only_autonomy_levels() {
        let cache: SharedConfigOptions = Arc::new(Mutex::new(None));
        let mode = |id: &str| {
            SessionUpdate::CurrentModeUpdate(
                agent_client_protocol::schema::v1::CurrentModeUpdate::new(id.to_owned()),
            )
        };
        assert!(track_session_state(
            ProviderKind::Droid,
            &mode("auto-medium"),
            RuntimeMode::AutoAcceptEdits,
            &cache,
        ));
        assert!(!track_session_state(
            ProviderKind::Droid,
            &mode("auto-low"),
            RuntimeMode::AutoAcceptEdits,
            &cache,
        ));
        assert!(!track_session_state(
            ProviderKind::Droid,
            &mode("spec"),
            RuntimeMode::AutoAcceptEdits,
            &cache,
        ));
    }

    #[test]
    fn plan_updates_carry_their_steps_and_settle() {
        let (events, event_rx) = crossbeam_channel::unbounded();
        let mut state = AcpStreamState::default();
        let update = serde_json::from_value(json!({
            "sessionUpdate": "plan",
            "entries": [
                {"content": "Read the driver", "priority": "high", "status": "completed"},
                {"content": "Fix approvals", "priority": "high", "status": "in_progress"},
                {"content": "Update docs", "priority": "low", "status": "pending"},
            ]
        }))
        .unwrap();
        handle_session_update(
            ProviderKind::Droid,
            SessionNotification::new("s", update),
            &events,
            &mut state,
        )
        .unwrap();

        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 1);
        let DriverEvent::RichActivity(item) = &seen[0] else {
            panic!("a plan becomes a rich activity");
        };
        assert_eq!(item.kind, ActivityKind::Plan);
        assert!(item.complete);
        assert_eq!(
            item.output.as_deref(),
            Some("[x] Read the driver\n[~] Fix approvals\n[ ] Update docs")
        );
    }

    #[test]
    fn fx_model_option_ignores_provider_selector_in_same_category() {
        let provider = select_config_option(
            "provider",
            SessionConfigOptionCategory::Model,
            "gateway",
            &["gateway", "codex", "grok"],
        );
        let model = select_config_option(
            "model",
            SessionConfigOptionCategory::Model,
            "openai/gpt-5.6-sol",
            &["openai/gpt-5.6-sol", "anthropic/claude-sonnet-5"],
        );

        assert_eq!(
            fx_model_option(&[provider, model]).map(|option| option.id.to_string()),
            Some("model".to_owned())
        );
    }

    #[test]
    fn fx_gateway_model_selects_the_gateway_route_first() {
        let provider = select_config_option(
            "provider",
            SessionConfigOptionCategory::Model,
            "codex",
            &["gateway", "codex", "grok"],
        );
        let model = select_config_option(
            "model",
            SessionConfigOptionCategory::Model,
            "gpt-5.6-luna",
            &["gpt-5.6-sol", "gpt-5.6-luna"],
        );
        let options = [provider, model];

        let (option, value) =
            fx_model_provider_switch(&options, "openai/gpt-5.6-luna-fast").unwrap();
        assert_eq!(option.id.to_string(), "provider");
        assert_eq!(value, "gateway");
    }

    #[test]
    fn cursor_model_aliases_resolve_to_advertised_parameterized_picker_values() {
        let option = select_config_option(
            "model",
            SessionConfigOptionCategory::Model,
            "default",
            &["default", "grok-4.6", "composer-2.5", "claude-sonnet-4-6"],
        );

        assert_eq!(
            cursor_model_selection(&option, "auto"),
            Some(CursorModelSelection {
                value: "default".into(),
                suffix: String::new(),
            })
        );
        assert_eq!(
            cursor_model_selection(&option, "composer-2.5"),
            Some(CursorModelSelection {
                value: "composer-2.5".into(),
                suffix: String::new(),
            })
        );
        assert_eq!(
            cursor_model_selection(&option, "cursor-grok-4.6-xhigh-fast"),
            Some(CursorModelSelection {
                value: "grok-4.6".into(),
                suffix: "xhigh-fast".into(),
            })
        );
        assert_eq!(
            cursor_model_selection(&option, "claude-4.6-sonnet-medium-thinking"),
            Some(CursorModelSelection {
                value: "claude-sonnet-4-6".into(),
                suffix: "medium-thinking".into(),
            })
        );
    }

    #[test]
    fn cursor_model_suffix_selects_dynamic_effort_thinking_and_fast_options() {
        let selection = CursorModelSelection {
            value: "claude-opus-5".into(),
            suffix: "thinking-extra-high-fast".into(),
        };
        let effort = select_config_option(
            "effort",
            SessionConfigOptionCategory::ThoughtLevel,
            "high",
            &["low", "medium", "high", "xhigh"],
        );
        let extra_high = select_config_option(
            "reasoning",
            SessionConfigOptionCategory::ThoughtLevel,
            "high",
            &["low", "medium", "high", "extra-high"],
        );
        let context = select_config_option(
            "context",
            SessionConfigOptionCategory::ModelConfig,
            "272k",
            &["272k", "1m"],
        );

        assert_eq!(
            cursor_desired_effort_value(&effort, &selection, None).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            cursor_desired_effort_value(&extra_high, &selection, Some("xhigh")).as_deref(),
            Some("extra-high")
        );
        assert_eq!(cursor_desired_thinking(&selection, None), Some(true));
        assert_eq!(cursor_desired_fast(&selection, None), Some(true));
        assert_eq!(
            cursor_desired_effort_value(&effort, &selection, Some("low")).as_deref(),
            Some("low")
        );
        assert_eq!(
            cursor_desired_fast(&selection, Some("default")),
            Some(false)
        );
        assert_eq!(
            cursor_desired_context_value(&context, Some("1m")).as_deref(),
            Some("1m")
        );
        assert_eq!(
            cursor_desired_thinking(&selection, Some("none")),
            Some(false)
        );
    }

    #[test]
    fn cursor_prefers_model_option_effort_over_thought_level() {
        let thought = select_config_option(
            "reasoning",
            SessionConfigOptionCategory::ThoughtLevel,
            "high",
            &["low", "medium", "high"],
        );
        let effort = select_config_option(
            "effort",
            SessionConfigOptionCategory::Other("model_option".into()),
            "max",
            &["low", "medium", "high", "max"],
        );
        let options = [thought, effort];
        let selected = find_cursor_effort_option(&options).unwrap();
        assert_eq!(selected.id.to_string(), "effort");
    }

    #[test]
    fn a_steer_only_settles_when_the_last_sdk_request_finishes() {
        let requests = Mutex::new(PendingPrompts::default());
        requests
            .lock()
            .insert(RequestId::Str("first".into()), None, "session".into());
        requests
            .lock()
            .insert(RequestId::Str("steer".into()), None, "session".into());
        assert!(!settle_prompt_request(
            &requests,
            &RequestId::Str("first".into())
        ));
        assert!(settle_prompt_request(
            &requests,
            &RequestId::Str("steer".into())
        ));
        assert!(!settle_prompt_request(
            &requests,
            &RequestId::Str("steer".into())
        ));
    }

    #[test]
    fn xai_prompt_complete_settles_a_missing_standard_response_once() {
        let requests = Mutex::new(PendingPrompts::default());
        let request_id = RequestId::Str("sdk-request".into());
        requests.lock().insert(
            request_id.clone(),
            Some("waku-prompt".into()),
            "grok-session".into(),
        );
        let stream_state = Mutex::new(AcpStreamState::default());
        stream_state.lock().tools.insert(
            "unfinished-call".into(),
            StoredToolCall {
                kind: ActivityKind::Tool,
                title: "unfinished".into(),
                raw_input: None,
                locations: None,
                content: None,
                tool_name: None,
                mcp_server: None,
                complete: false,
                failed: false,
            },
        );
        let (events, event_rx) = crate::driver::test_event_channel();

        assert_eq!(
            finish_xai_prompt_complete(
                &json!({
                    "sessionId": "grok-session",
                    "promptId": "waku-prompt",
                    "stopReason": "end_turn"
                }),
                &requests,
                &stream_state,
                &events,
            ),
            Some("grok-session".into())
        );
        assert!(
            stream_state.lock().tools.is_empty(),
            "unfinished tool calls must be cleared when a turn ends"
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::TurnFinished {
                success: true,
                summary: None
            }
        ));
        assert!(!settle_prompt_request(&requests, &request_id));
        assert!(event_rx.try_recv().is_err());
    }

    /// Kimi ends a failed turn with `end_turn` and no content at all, so the
    /// provider's own record is the only thing that can name the cause.
    #[test]
    fn a_recovered_provider_failure_overrides_a_clean_stop_reason() {
        let (events, event_rx) = crossbeam_channel::unbounded();

        assert!(!finish_prompt(
            Ok(PromptResponse::new(StopReason::EndTurn)),
            Some("402 membership inactive".to_owned()),
            &events
        ));

        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::Error(message) if message == "402 membership inactive"
        ));
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::TurnFinished {
                success: false,
                summary: None
            }
        ));
    }

    #[test]
    fn typed_prompt_response_settles_the_turn() {
        let (events, event_rx) = crossbeam_channel::unbounded();
        assert!(finish_prompt(
            Ok(PromptResponse::new(StopReason::EndTurn)),
            None,
            &events
        ));
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::TurnFinished {
                success: true,
                summary: None
            }
        ));
    }

    #[test]
    fn typed_updates_preserve_text_reasoning_and_correlated_tools() {
        let (events, event_rx) = crossbeam_channel::unbounded();
        let mut state = AcpStreamState::default();
        let updates = [
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}),
            json!({"sessionUpdate":"tool_call","toolCallId":"call_1","title":"read","kind":"read","status":"pending","rawInput":{}}),
            json!({"sessionUpdate":"tool_call_update","toolCallId":"call_1","status":"completed","title":"fixture.txt","content":[{"type":"content","content":{"type":"text","text":"waku probe fixture"}}]}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"OK"}}),
            json!({"sessionUpdate":"usage_update","used":9677,"size":500000}),
        ];
        for update in updates {
            let update = serde_json::from_value(update).unwrap();
            handle_session_update(
                ProviderKind::Cursor,
                SessionNotification::new("s", update),
                &events,
                &mut state,
            )
            .unwrap();
        }

        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert!(matches!(&seen[0], DriverEvent::ReasoningDelta(text) if text == "thinking"));
        assert!(matches!(&seen[1], DriverEvent::RichActivity(item)
                if item.kind == ActivityKind::FileRead && !item.complete));
        assert!(matches!(&seen[2], DriverEvent::RichActivity(item)
                if item.complete
                    && item.title == "fixture.txt"
                    && item.output.as_deref().is_some_and(|output| output.contains("waku probe fixture"))));
        assert!(matches!(&seen[3], DriverEvent::TextDelta(text) if text == "OK"));
        assert!(matches!(
            &seen[4],
            DriverEvent::UsageUpdated {
                context_tokens: Some(9677),
                context_window: Some(500000),
            }
        ));
    }

    #[test]
    fn fx_context_notices_do_not_become_assistant_text() {
        let (events, event_rx) = crossbeam_channel::unbounded();
        let mut state = AcpStreamState::default();
        for text in [
            "[context] skill catalog omitted 19 entries",
            "skill discovery warning: candidate was skipped",
            "Hi! How can I help?",
            "[context] is ordinary text after the answer starts",
        ] {
            let update = serde_json::from_value(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text}
            }))
            .unwrap();
            handle_session_update(
                ProviderKind::Fx,
                SessionNotification::new("s", update),
                &events,
                &mut state,
            )
            .unwrap();
        }

        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 2);
        assert!(matches!(&seen[0], DriverEvent::TextDelta(text) if text == "Hi! How can I help?"));
        assert!(matches!(&seen[1], DriverEvent::TextDelta(text) if text.starts_with("[context]")));
        assert!(state.produced_content);
    }

    #[test]
    fn grok_launch_passes_reasoning_effort_before_stdio() {
        let launch = launch_for(ProviderKind::Grok, Some("xhigh"), None).unwrap();
        assert_eq!(
            launch.args,
            ["agent", "--reasoning-effort", "xhigh", "stdio"]
        );
        let bare = launch_for(ProviderKind::Grok, None, None).unwrap();
        assert_eq!(bare.args, ["agent", "stdio"]);
    }

    #[test]
    fn grok_set_model_includes_reasoning_effort_meta() {
        let params = set_model_params(
            &SessionId::new("sess"),
            "grok-4.6",
            Some("xhigh"),
            ProviderKind::Grok,
        );
        assert_eq!(params["modelId"], "grok-4.6");
        assert_eq!(params["_meta"]["reasoningEffort"], "xhigh");
    }

    #[test]
    fn permission_reason_preserves_the_agents_explanation() {
        let tool_call = ToolCallUpdate::new(
            "tool-1",
            serde_json::from_value::<ToolCallUpdateFields>(json!({
                "title": "rm -rf build",
                "kind": "execute",
                "content": [
                    {"type":"content","content":{"type":"text","text":"Not in allowlist: rm"}}
                ]
            }))
            .unwrap(),
        );
        let request = RequestPermissionRequest::new("s", tool_call, Vec::new());
        let params = serde_json::to_value(request).unwrap();
        assert_eq!(
            permission_reason(&params).as_deref(),
            Some("Not in allowlist: rm")
        );
    }

    #[test]
    fn tool_call_update_preserves_raw_input_and_file_changes_from_initial_tool_call() {
        let (events, event_rx) = crossbeam_channel::unbounded();
        let mut state = AcpStreamState::default();

        tool_activity(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-edit-1",
                "title": "str_replace_editor",
                "status": "in_progress",
                "rawInput": {
                    "file_path": "src/main.rs",
                    "old_str": "fn old() {}",
                    "new_str": "fn updated() {}\nfn extra() {}"
                }
            }),
            &events,
            &mut state,
        );
        tool_activity(
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "droid-edit-1",
                "status": "completed",
                "content": [
                    {"type": "content", "content": {"type": "text", "text": "Applied edit"}}
                ]
            }),
            &events,
            &mut state,
        );

        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 2);
        let DriverEvent::RichActivity(completed) = &seen[1] else {
            panic!("expected completed RichActivity");
        };
        assert_eq!(completed.kind, ActivityKind::FileChange);
        assert!(completed.complete);
        assert!(!completed.failed);
        assert_eq!(completed.file_changes.len(), 1);
        assert_eq!(completed.file_changes[0].path, "src/main.rs");
        assert_eq!(completed.file_changes[0].additions, Some(2));
        assert_eq!(completed.file_changes[0].deletions, Some(1));
        assert!(completed.file_changes[0].diff.is_some());
    }

    #[test]
    fn tool_activity_handles_acp_diff_content_and_locations() {
        let (events, event_rx) = crossbeam_channel::unbounded();
        let mut state = AcpStreamState::default();

        tool_activity(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "acp-edit-diff",
                "title": "apply_patch",
                "status": "in_progress",
                "locations": [{"path": "src/lib.rs", "line": 10}]
            }),
            &events,
            &mut state,
        );
        tool_activity(
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "acp-edit-diff",
                "status": "completed",
                "content": [
                    {
                        "type": "diff",
                        "path": "src/lib.rs",
                        "oldText": "fn old() {}\n",
                        "newText": "fn new_one() {}\nfn new_two() {}\n"
                    }
                ]
            }),
            &events,
            &mut state,
        );

        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 2);
        let DriverEvent::RichActivity(completed) = &seen[1] else {
            panic!("expected completed RichActivity");
        };
        assert_eq!(completed.kind, ActivityKind::FileChange);
        assert!(completed.complete);
        assert_eq!(completed.file_changes.len(), 1);
        assert_eq!(completed.file_changes[0].path, "src/lib.rs");
        assert_eq!(completed.file_changes[0].additions, Some(2));
        assert_eq!(completed.file_changes[0].deletions, Some(1));
        // ToolCallContent::Diff should never leak raw JSON into output
        assert_eq!(completed.output, None);
    }

    #[test]
    fn tool_activity_handles_acp_diff_for_newly_added_file() {
        let (events, event_rx) = crossbeam_channel::unbounded();
        let mut state = AcpStreamState::default();

        tool_activity(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "acp-new-diff",
                "title": "create_file",
                "status": "completed",
                "content": [
                    {
                        "type": "diff",
                        "path": "src/new.rs",
                        "oldText": null,
                        "newText": "line 1\nline 2\nline 3\n"
                    }
                ]
            }),
            &events,
            &mut state,
        );

        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 1);
        let DriverEvent::RichActivity(completed) = &seen[0] else {
            panic!("expected completed RichActivity");
        };
        assert_eq!(completed.kind, ActivityKind::FileChange);
        assert_eq!(completed.file_changes.len(), 1);
        assert_eq!(completed.file_changes[0].path, "src/new.rs");
        assert_eq!(
            completed.file_changes[0].status,
            Some(crate::model::ActivityFileChangeStatus::Added)
        );
        assert_eq!(completed.file_changes[0].additions, Some(3));
        assert_eq!(completed.file_changes[0].deletions, Some(0));
    }

    #[test]
    fn acp_fs_read_and_write_text_file_handlers() {
        let temp_dir =
            std::env::temp_dir().join(format!("waku-acp-fs-test-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&temp_dir);

        let write_req = WriteTextFileRequest::new("session-1", "nested/test.txt", "1\n2\n3\n4\n5");
        let write_res = handle_write_text_file(write_req, &temp_dir);
        assert!(write_res.is_ok());

        let read_req = ReadTextFileRequest::new("session-1", "nested/test.txt")
            .line(2)
            .limit(3);
        let read_res = handle_read_text_file(read_req, &temp_dir).unwrap();
        assert_eq!(read_res.content, "2\n3\n4");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn acp_prompt_content_blocks_attaches_images_and_resources() {
        let temp_dir =
            std::env::temp_dir().join(format!("waku-acp-prompt-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let image_file = temp_dir.join("test.png");
        std::fs::write(&image_file, b"\x89PNG\r\n\x1a\nfakeimagebytes").unwrap();
        let doc_file = temp_dir.join("notes.txt");
        std::fs::write(&doc_file, "Project documentation").unwrap();

        let prompt = "Please check @test.png and @notes.txt";
        let capabilities = PromptCapabilities::new().image(true).embedded_context(true);
        let blocks = build_prompt_content_blocks(prompt, &temp_dir, &capabilities);

        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], ContentBlock::Text(_)));
        assert!(matches!(&blocks[1], ContentBlock::Image(_)));
        assert!(matches!(&blocks[2], ContentBlock::Resource(_)));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    fn create_test_dir_link(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_dir(target, link).is_ok() {
                return true;
            }
            // Directory junctions work without elevated privileges on Windows
            // and test the exact same reparse-point escape class.
            std::process::Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }
    }

    fn create_test_file_link(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }

    #[test]
    fn acp_fs_handlers_reject_workspace_escapes_and_symlinks() {
        let outer_dir =
            std::env::temp_dir().join(format!("waku-acp-fs-sec-{}", uuid::Uuid::new_v4()));
        let workspace_dir = outer_dir.join("workspace");
        std::fs::create_dir_all(workspace_dir.join("sub")).unwrap();
        let outside_file = outer_dir.join("secret.txt");
        std::fs::write(&outside_file, "top-secret").unwrap();
        let inside_file = workspace_dir.join("sub/ok.txt");
        std::fs::write(&inside_file, "inside-ok").unwrap();

        // In-workspace relative and absolute reads/writes succeed.
        let read_rel =
            handle_read_text_file(ReadTextFileRequest::new("s", "sub/ok.txt"), &workspace_dir)
                .unwrap();
        assert_eq!(read_rel.content, "inside-ok");
        let read_abs = handle_read_text_file(
            ReadTextFileRequest::new("s", inside_file.clone()),
            &workspace_dir,
        )
        .unwrap();
        assert_eq!(read_abs.content, "inside-ok");

        assert!(
            handle_write_text_file(
                WriteTextFileRequest::new("s", "new_dir/written.txt", "safe"),
                &workspace_dir
            )
            .is_ok()
        );
        assert_eq!(
            std::fs::read_to_string(workspace_dir.join("new_dir/written.txt")).unwrap(),
            "safe"
        );

        // Relative traversal and out-of-workspace absolute paths are rejected.
        for escape in [
            std::path::PathBuf::from("../secret.txt"),
            std::path::PathBuf::from("sub/../../secret.txt"),
            outside_file.clone(),
            std::path::PathBuf::from("."),
            std::path::PathBuf::from(""),
        ] {
            assert!(
                handle_read_text_file(
                    ReadTextFileRequest::new("s", escape.clone()),
                    &workspace_dir
                )
                .is_err(),
                "read should reject `{}`",
                escape.display()
            );
        }

        let escaped_write_target = outer_dir.join("pwned.txt");
        for escape in [
            std::path::PathBuf::from("../pwned.txt"),
            std::path::PathBuf::from("nonexistent/../../pwned.txt"),
            escaped_write_target.clone(),
            std::path::PathBuf::from("."),
            std::path::PathBuf::from(""),
        ] {
            assert!(
                handle_write_text_file(
                    WriteTextFileRequest::new("s", escape.clone(), "bad"),
                    &workspace_dir
                )
                .is_err(),
                "write should reject `{}`",
                escape.display()
            );
        }
        assert!(!escaped_write_target.exists());

        // Symlink / Windows directory junction escapes are rejected for both reads and writes.
        let dir_link = workspace_dir.join("escape_dir");
        if create_test_dir_link(&dir_link, &outer_dir) {
            assert!(
                handle_read_text_file(
                    ReadTextFileRequest::new("s", "escape_dir/secret.txt"),
                    &workspace_dir
                )
                .is_err(),
                "read should reject directory symlink/junction escape"
            );
            assert!(
                handle_write_text_file(
                    WriteTextFileRequest::new("s", "escape_dir/via_link.txt", "bad"),
                    &workspace_dir
                )
                .is_err(),
                "write should reject directory symlink/junction escape"
            );
            assert!(!outer_dir.join("via_link.txt").exists());
            let _ = std::fs::remove_dir(&dir_link);
        }

        let file_link = workspace_dir.join("escape_file.txt");
        if create_test_file_link(&file_link, &outside_file) {
            assert!(
                handle_read_text_file(
                    ReadTextFileRequest::new("s", "escape_file.txt"),
                    &workspace_dir
                )
                .is_err(),
                "read should reject file symlink escape"
            );
            assert!(
                handle_write_text_file(
                    WriteTextFileRequest::new("s", "escape_file.txt", "bad"),
                    &workspace_dir
                )
                .is_err(),
                "write should reject file symlink escape"
            );
            assert_eq!(
                std::fs::read_to_string(&outside_file).unwrap(),
                "top-secret"
            );
        }

        let dangling_target = outer_dir.join("dangling_out.txt");
        let dangling_link = workspace_dir.join("dangling_link.txt");
        if create_test_file_link(&dangling_link, &dangling_target) {
            assert!(
                handle_write_text_file(
                    WriteTextFileRequest::new("s", "dangling_link.txt", "bad"),
                    &workspace_dir
                )
                .is_err(),
                "write should reject dangling symlink escape"
            );
            assert!(!dangling_target.exists());
        }

        let _ = std::fs::remove_dir_all(&outer_dir);
    }

    #[test]
    fn acp_prompt_content_blocks_rejects_out_of_workspace_mentions() {
        let outer_dir =
            std::env::temp_dir().join(format!("waku-acp-prompt-sec-{}", uuid::Uuid::new_v4()));
        let workspace_dir = outer_dir.join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let outside_file = outer_dir.join("secret.txt");
        std::fs::write(&outside_file, "secret-data").unwrap();
        let inside_file = workspace_dir.join("inside.txt");
        std::fs::write(&inside_file, "inside-data").unwrap();

        let dir_link = workspace_dir.join("link_out");
        let has_dir_link = create_test_dir_link(&dir_link, &outer_dir);

        let prompt = format!(
            "Check @inside.txt @../secret.txt @{} @link_out/secret.txt",
            outside_file.display()
        );
        let capabilities = PromptCapabilities::new().image(true).embedded_context(true);
        let blocks = build_prompt_content_blocks(&prompt, &workspace_dir, &capabilities);

        assert_eq!(
            blocks.len(),
            2,
            "only the text block and in-workspace @inside.txt should be attached (has_dir_link={has_dir_link})"
        );
        assert!(matches!(&blocks[0], ContentBlock::Text(_)));
        let ContentBlock::Resource(embedded) = &blocks[1] else {
            panic!("expected embedded resource for inside.txt");
        };
        let EmbeddedResourceResource::TextResourceContents(contents) = &embedded.resource else {
            panic!("expected text resource contents");
        };
        assert_eq!(contents.text, "inside-data");

        if has_dir_link {
            let _ = std::fs::remove_dir(&dir_link);
        }
        let _ = std::fs::remove_dir_all(&outer_dir);
    }

    #[test]
    fn acp_prompt_content_blocks_attaches_space_paths_and_daemon_blobs_while_guarding_escapes() {
        let outer_dir =
            std::env::temp_dir().join(format!("waku-acp-prompt-spaces-{}", uuid::Uuid::new_v4()));
        let workspace_dir = outer_dir.join("my workspace");
        let data_dir = outer_dir.join("Application Support").join("Waku");
        let blobs_dir = data_dir.join("blobs");
        let attachments_dir = data_dir.join("attachments");
        std::fs::create_dir_all(workspace_dir.join("docs with spaces")).unwrap();
        std::fs::create_dir_all(blobs_dir.join("ab")).unwrap();
        std::fs::create_dir_all(attachments_dir.join("att-1")).unwrap();

        let outside_file = outer_dir.join("outside secret.txt");
        std::fs::write(&outside_file, "secret").unwrap();
        let outside_image = outer_dir.join("outside image.png");
        std::fs::write(&outside_image, b"\x89PNG\r\n\x1a\nsecretimg").unwrap();

        let ws_doc = workspace_dir.join("docs with spaces/my notes.md");
        std::fs::write(&ws_doc, "workspace notes").unwrap();
        let pasted_blob = blobs_dir.join("ab/0123456789abcdef.png");
        std::fs::write(&pasted_blob, b"\x89PNG\r\n\x1a\npastedblob").unwrap();
        let dropped_doc = attachments_dir.join("att-1/dropped file.txt");
        std::fs::write(&dropped_doc, "dropped text").unwrap();

        let blob_link = blobs_dir.join("ab/escape.txt");
        let has_blob_link = create_test_file_link(&blob_link, &outside_file);

        let doc_with_at = workspace_dir.join("dir with @ sign/file.txt");
        std::fs::create_dir_all(doc_with_at.parent().unwrap()).unwrap();
        std::fs::write(&doc_with_at, "at-sign doc").unwrap();

        let prompt = format!(
            "Check @docs with spaces/my notes.md, @\"dir with @ sign/file.txt\", (@{}), @{}, @../outside secret.txt, @{}, and @{}!",
            pasted_blob.display(),
            dropped_doc.display(),
            outside_image.display(),
            blob_link.display(),
        );
        let capabilities = PromptCapabilities::new().image(true).embedded_context(true);
        let blocks = build_prompt_content_blocks_with_roots(
            &prompt,
            &workspace_dir,
            &[blobs_dir.clone(), attachments_dir.clone()],
            &capabilities,
        );

        assert_eq!(
            blocks.len(),
            5,
            "expected text + workspace doc + quoted @ doc + pasted blob image + dropped doc (has_blob_link={has_blob_link})"
        );
        assert!(matches!(&blocks[0], ContentBlock::Text(_)));
        let ContentBlock::Resource(ws_res) = &blocks[1] else {
            panic!("expected workspace text resource");
        };
        let EmbeddedResourceResource::TextResourceContents(ws_text) = &ws_res.resource else {
            panic!("expected text contents for workspace doc");
        };
        assert_eq!(ws_text.text, "workspace notes");

        let ContentBlock::Resource(at_res) = &blocks[2] else {
            panic!("expected quoted @-dir text resource");
        };
        let EmbeddedResourceResource::TextResourceContents(at_text) = &at_res.resource else {
            panic!("expected text contents for quoted @-dir doc");
        };
        assert_eq!(at_text.text, "at-sign doc");

        let ContentBlock::Image(blob_img) = &blocks[3] else {
            panic!("expected pasted blob image block");
        };
        assert_eq!(blob_img.mime_type, "image/png");

        let ContentBlock::Resource(att_res) = &blocks[4] else {
            panic!("expected dropped attachment resource block");
        };
        let EmbeddedResourceResource::TextResourceContents(att_text) = &att_res.resource else {
            panic!("expected text contents for dropped attachment");
        };
        assert_eq!(att_text.text, "dropped text");

        let _ = std::fs::remove_dir_all(&outer_dir);
    }

    #[test]
    fn acp_terminal_confines_cwd_checks_access_mode_and_sanitizes_env() {
        use agent_client_protocol::schema::v1::EnvVariable;

        let outer_dir =
            std::env::temp_dir().join(format!("waku-acp-term-sec-{}", uuid::Uuid::new_v4()));
        let workspace_dir = outer_dir.join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        let dir_link = workspace_dir.join("escape_cwd");
        let has_dir_link = create_test_dir_link(&dir_link, &outer_dir);

        let registry = AcpTerminalRegistry::default();
        let (events, _event_rx) = crate::driver::test_event_channel();

        // Out-of-workspace cwd rejected even in FullAccess.
        for bad_cwd in [std::path::PathBuf::from(".."), outer_dir.clone()] {
            let req = CreateTerminalRequest::new("s", "echo").cwd(bad_cwd.clone());
            assert!(
                registry
                    .create_terminal(req, &workspace_dir, RuntimeMode::FullAccess, &events)
                    .is_err(),
                "create_terminal should reject out-of-workspace cwd `{}`",
                bad_cwd.display()
            );
        }
        if has_dir_link {
            let req = CreateTerminalRequest::new("s", "echo").cwd("escape_cwd");
            assert!(
                registry
                    .create_terminal(req, &workspace_dir, RuntimeMode::FullAccess, &events)
                    .is_err(),
                "create_terminal should reject symlink/junction cwd escape"
            );
            let _ = std::fs::remove_dir(&dir_link);
        }

        // Access mode enforcement on write and terminal policy helpers and create_terminal.
        assert!(!auto_allows_client_write(RuntimeMode::Ask));
        assert!(auto_allows_client_write(RuntimeMode::AutoAcceptEdits));
        assert!(auto_allows_client_write(RuntimeMode::Auto));
        assert!(auto_allows_client_write(RuntimeMode::FullAccess));

        assert!(!auto_allows_client_terminal(RuntimeMode::Ask));
        assert!(!auto_allows_client_terminal(RuntimeMode::AutoAcceptEdits));
        assert!(auto_allows_client_terminal(RuntimeMode::Auto));
        assert!(auto_allows_client_terminal(RuntimeMode::FullAccess));

        for gated_mode in [RuntimeMode::Ask, RuntimeMode::AutoAcceptEdits] {
            let req = CreateTerminalRequest::new("s", "echo");
            assert!(
                registry
                    .create_terminal(req, &workspace_dir, gated_mode, &events)
                    .is_err(),
                "create_terminal should require approval in {gated_mode:?}"
            );
        }

        // Env sanitization strips loader/shell/interpreter injection vectors while keeping benign vars.
        let raw_env = vec![
            EnvVariable::new("LD_PRELOAD", "/tmp/evil.so"),
            EnvVariable::new("LD_LIBRARY_PATH", "/tmp/evil"),
            EnvVariable::new("dyld_insert_libraries", "/tmp/evil.dylib"),
            EnvVariable::new("DYLD_LIBRARY_PATH", "/tmp/evil"),
            EnvVariable::new("PATH", "/tmp/evil-bin"),
            EnvVariable::new("BASH_ENV", "/tmp/evil.sh"),
            EnvVariable::new("BASH_FUNC_ls%%", "() { evil; }"),
            EnvVariable::new("NODE_OPTIONS", "--require=/tmp/evil.js"),
            EnvVariable::new("PYTHONSTARTUP", "/tmp/evil.py"),
            EnvVariable::new("GIT_SSH_COMMAND", "sh -c evil"),
            EnvVariable::new("CI", "1"),
            EnvVariable::new("NO_COLOR", "1"),
            EnvVariable::new("RUST_LOG", "debug"),
        ];
        let filtered = sanitized_terminal_env(&raw_env);
        assert_eq!(
            filtered,
            vec![("CI", "1"), ("NO_COLOR", "1"), ("RUST_LOG", "debug")]
        );

        let _ = std::fs::remove_dir_all(&outer_dir);
    }

    #[test]
    fn acp_terminal_kill_and_stop_skip_already_exited_process() {
        let registry = AcpTerminalRegistry::default();
        let (events, event_rx) = crate::driver::test_event_channel();
        let id = "term-already-exited".to_owned();
        let entry = Arc::new(Mutex::new(AcpTerminalProcess {
            id: id.clone(),
            title: "echo ok".to_owned(),
            command_display: "echo ok".to_owned(),
            cwd_display: ".".to_owned(),
            started_at_ms: 100,
            updated_at_ms: 200,
            completed_seq: Some(1),
            output: "ok\n".to_owned(),
            output_truncated: false,
            output_byte_limit: DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT,
            exit_status: Some(TerminalExitStatus::new().exit_code(Some(0))),
            killed: false,
            // Sentinel non-zero PID: if kill_process_tree were wrongly called,
            // it would flip `killed` and try to signal this PID.
            child_pid: u32::MAX,
            #[cfg(windows)]
            job: None,
            waiters: Vec::new(),
        }));
        registry.terminals.lock().insert(id.clone(), entry.clone());

        registry
            .kill_terminal(KillTerminalRequest::new("s", id.clone()), &events)
            .unwrap();
        assert!(
            !entry.lock().killed,
            "kill_terminal must not kill an already-exited child"
        );
        assert_eq!(
            entry.lock().background_work_status(),
            BackgroundWorkStatus::Completed
        );

        let key = BackgroundWorkKey::new(BackgroundWorkKind::Process, id.clone());
        registry.stop_by_id(&id, &key, &events);
        assert!(
            !entry.lock().killed,
            "stop_by_id must not kill an already-exited child"
        );
        assert_eq!(
            entry.lock().background_work_status(),
            BackgroundWorkStatus::Completed
        );

        // Drain events and confirm upserts kept Completed status.
        let mut upserts = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(item)) = event {
                upserts.push(item.status);
            }
        }
        assert_eq!(
            upserts,
            vec![
                BackgroundWorkStatus::Completed,
                BackgroundWorkStatus::Completed
            ]
        );
    }

    #[test]
    fn available_commands_update_captures_input_argument_hint() {
        let (events, event_rx) = crossbeam_channel::unbounded();
        let mut state = AcpStreamState::default();
        let update = serde_json::from_value(json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {
                    "name": "review",
                    "description": "Code review",
                    "input": {
                        "hint": "[branch]"
                    }
                }
            ]
        }))
        .unwrap();

        handle_session_update(
            ProviderKind::Droid,
            SessionNotification::new("s", update),
            &events,
            &mut state,
        )
        .unwrap();

        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 1);
        let DriverEvent::AvailableCommands(commands) = &seen[0] else {
            panic!("expected AvailableCommands event");
        };
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "review");
        assert_eq!(commands[0].description, "Code review");
        assert_eq!(commands[0].argument_hint.as_deref(), Some("[branch]"));
    }

    #[test]
    fn available_commands_update_strips_slashes_rejects_whitespace_and_emits_empty() {
        let (events, event_rx) = crossbeam_channel::unbounded();
        let mut state = AcpStreamState::default();
        let update = serde_json::from_value(json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": " /review ", "description": "  Code review  " },
                { "name": "///simplify", "description": "Simplify code" },
                { "name": "bad command", "description": "Must be dropped" },
                { "name": "/", "description": "Empty after slash strip" },
                { "name": "   ", "description": "Blank" }
            ]
        }))
        .unwrap();

        handle_session_update(
            ProviderKind::Droid,
            SessionNotification::new("s", update),
            &events,
            &mut state,
        )
        .unwrap();

        let empty_update = serde_json::from_value(json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": []
        }))
        .unwrap();

        handle_session_update(
            ProviderKind::Droid,
            SessionNotification::new("s", empty_update),
            &events,
            &mut state,
        )
        .unwrap();

        let seen = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 2);
        let DriverEvent::AvailableCommands(first) = &seen[0] else {
            panic!("expected first AvailableCommands event");
        };
        assert_eq!(
            first
                .iter()
                .map(|cmd| (cmd.name.as_str(), cmd.description.as_str()))
                .collect::<Vec<_>>(),
            vec![("review", "Code review"), ("simplify", "Simplify code")]
        );
        let DriverEvent::AvailableCommands(second) = &seen[1] else {
            panic!("expected empty AvailableCommands event");
        };
        assert!(second.is_empty());
    }

    #[test]
    fn droid_tool_classification_and_metadata_cover_grep_glob_ls_execute_fetch_spec_and_mcp() {
        let mut tools = HashMap::new();

        // 1. Grep (kind: "search", human title, rawInput.pattern) -> FileSearch
        let grep = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-grep",
                "kind": "search",
                "title": "Search \"swarm\" in .",
                "status": "completed",
                "rawInput": { "pattern": "swarm", "path": "." }
            }),
            &mut tools,
            None,
        );
        assert_eq!(grep.kind, ActivityKind::FileSearch);
        assert_eq!(grep.display_target.as_deref(), Some("swarm"));

        // 2. Glob (kind: "search", human title, rawInput.{patterns, folder}) -> FileSearch
        let glob = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-glob",
                "kind": "search",
                "title": "Find icons/chevron*.svg in assets",
                "status": "completed",
                "rawInput": { "patterns": ["icons/chevron*.svg"], "folder": "assets" }
            }),
            &mut tools,
            None,
        );
        assert_eq!(glob.kind, ActivityKind::FileSearch);
        assert_eq!(glob.display_target.as_deref(), Some("icons/chevron*.svg"));

        // 3. WebSearch (kind: "fetch") stays web Search; FetchUrl extracts url
        let web_search = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-web-search",
                "kind": "fetch",
                "title": "Search web: \"rust acp\"",
                "status": "completed",
                "rawInput": { "query": "rust acp" }
            }),
            &mut tools,
            None,
        );
        assert_eq!(web_search.kind, ActivityKind::Search);
        assert_eq!(web_search.display_target.as_deref(), Some("rust acp"));

        let fetch_url = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-fetch",
                "kind": "fetch",
                "title": "Fetch https://docs.factory.ai/droid",
                "status": "completed",
                "rawInput": { "url": "https://docs.factory.ai/droid" }
            }),
            &mut tools,
            None,
        );
        assert_eq!(fetch_url.kind, ActivityKind::Search);
        assert_eq!(
            fetch_url.display_target.as_deref(),
            Some("https://docs.factory.ai/droid")
        );

        // 4. LS (kind: "read", title "List <dir>") -> FileList
        let ls = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-ls",
                "kind": "read",
                "title": "List crates/waku-core",
                "status": "completed",
                "rawInput": { "directory_path": "crates/waku-core" }
            }),
            &mut tools,
            None,
        );
        assert_eq!(ls.kind, ActivityKind::FileList);
        assert_eq!(ls.display_target.as_deref(), Some("crates/waku-core"));

        // 5. Execute with rawInput.summary
        let execute = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-exec",
                "kind": "execute",
                "title": "Run cargo test",
                "status": "completed",
                "rawInput": {
                    "command": "cargo test -p waku-core",
                    "summary": "Run core unit tests"
                }
            }),
            &mut tools,
            None,
        );
        assert_eq!(execute.kind, ActivityKind::Command);
        assert_eq!(
            execute.display_description.as_deref(),
            Some("Run core unit tests")
        );

        // 6. ExitSpecMode (kind: "switch_mode", title: "Approve Spec") -> Plan with preserved title & markdown
        let spec = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-spec",
                "kind": "switch_mode",
                "title": "Approve Spec",
                "status": "completed",
                "rawInput": { "plan": "# Auth Refactor\n\n- Step 1" },
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "# Auth Refactor\n\n- Step 1" } }
                ]
            }),
            &mut tools,
            None,
        );
        assert_eq!(spec.kind, ActivityKind::Plan);
        assert_eq!(spec.title, "Approve Spec");
        assert_eq!(spec.output.as_deref(), Some("# Auth Refactor\n\n- Step 1"));

        // 7. MCP tool via _meta.factory.{source: "mcp", server, tool} and sparse follow-up update
        let mcp_start = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-mcp-1",
                "kind": "other",
                "title": "github: search_issues",
                "status": "in_progress",
                "_meta": {
                    "factory": {
                        "source": "mcp",
                        "server": "github",
                        "tool": "search_issues"
                    }
                },
                "rawInput": { "query": "is:open label:bug" }
            }),
            &mut tools,
            None,
        );
        assert_eq!(mcp_start.mcp_server.as_deref(), Some("github"));
        assert_eq!(mcp_start.tool_name.as_deref(), Some("search_issues"));

        let mcp_done = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "droid-mcp-1",
                "status": "completed",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "3 issues found" } }
                ]
            }),
            &mut tools,
            None,
        );
        assert_eq!(mcp_done.mcp_server.as_deref(), Some("github"));
        assert_eq!(mcp_done.tool_name.as_deref(), Some("search_issues"));
        assert_eq!(mcp_done.output.as_deref(), Some("3 issues found"));

        // 8. MCP tool with _meta.factory.source == "mcp" falling back to "<server>: <Tool>" title
        let mcp_title_fallback = build_tool_activity_item(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "droid-mcp-2",
                "kind": "other",
                "title": "linear: list_issues",
                "status": "completed",
                "_meta": { "factory": { "source": "mcp" } }
            }),
            &mut tools,
            None,
        );
        assert_eq!(mcp_title_fallback.mcp_server.as_deref(), Some("linear"));
        assert_eq!(
            mcp_title_fallback.tool_name.as_deref(),
            Some("list_issues")
        );
    }

    #[test]
    fn acp_prompt_content_blocks_handles_single_quoted_and_autocomplete_space_mentions() {
        let temp_dir =
            std::env::temp_dir().join(format!("waku-acp-mention-spaces-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(temp_dir.join("folder with spaces")).unwrap();
        let first = temp_dir.join("folder with spaces/first note.md");
        let second = temp_dir.join("folder with spaces/second note.md");
        std::fs::write(&first, "first body").unwrap();
        std::fs::write(&second, "second body").unwrap();

        // Autocomplete inserts `@<path> ` unquoted; users may also single-quote paths.
        let prompt = "Compare @folder with spaces/first note.md against @'folder with spaces/second note.md' please";
        let capabilities = PromptCapabilities::new().image(true).embedded_context(true);
        let blocks = build_prompt_content_blocks(prompt, &temp_dir, &capabilities);

        assert_eq!(blocks.len(), 3);
        let ContentBlock::Resource(res1) = &blocks[1] else {
            panic!("expected first embedded resource");
        };
        let EmbeddedResourceResource::TextResourceContents(text1) = &res1.resource else {
            panic!("expected first text resource");
        };
        assert_eq!(text1.text, "first body");

        let ContentBlock::Resource(res2) = &blocks[2] else {
            panic!("expected second embedded resource");
        };
        let EmbeddedResourceResource::TextResourceContents(text2) = &res2.resource else {
            panic!("expected second text resource");
        };
        assert_eq!(text2.text, "second body");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn sdk_agent_and_terminal_path_include_binary_parent_and_deduplicate_path() {
        let base_dir = std::env::temp_dir().join("waku-path-base");
        let bin_dir = std::env::temp_dir().join("waku-bun-bin");
        let binary = bin_dir.join("cursor-agent.cmd");
        let base_path = std::env::join_paths([&base_dir]).unwrap();

        let merged = child_search_path_with_base(Some(base_path.as_os_str()), &binary).unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&merged).collect();
        assert_eq!(dirs, vec![base_dir.clone(), bin_dir.clone()]);

        // sdk_agent_environment strips stale PATH / Path entries and installs the merged PATH.
        let env = sdk_agent_environment(
            &binary,
            vec![
                (OsString::from("Path"), OsString::from("/stale/lower")),
                (OsString::from("PATH"), OsString::from("/stale/upper")),
                (OsString::from("CUSTOM"), OsString::from("keep")),
            ],
            Some(base_path.as_os_str()),
            vec![("LAUNCH".into(), "1".into())],
            vec![("CU".into(), "2".into())],
        );
        let path_entries: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.as_str())
            .collect();
        #[cfg(windows)]
        assert_eq!(path_entries.len(), 1);
        assert!(path_entries.iter().any(|value| {
            std::env::split_paths(value).any(|dir| dir == bin_dir)
                && std::env::split_paths(value).any(|dir| dir == base_dir)
        }));
        assert!(env.contains(&("CUSTOM".into(), "keep".into())));
        assert!(env.contains(&("LAUNCH".into(), "1".into())));
        assert!(env.contains(&("CU".into(), "2".into())));

        // Terminal command search path includes binary parent for both absolute and relative paths.
        let work_dir = std::env::temp_dir().join("waku-term-wd");
        let term_path = terminal_command_search_path("./tools/run.cmd", &work_dir).unwrap();
        let expected_parent = work_dir.join("./tools");
        assert!(
            std::env::split_paths(&term_path).any(|dir| dir == expected_parent),
            "terminal PATH must include relative command's resolved parent dir"
        );
    }

    #[test]
    fn establish_session_surfaces_auth_and_transport_errors_instead_of_creating_blank_session() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, LoadSessionRequest, NewSessionRequest, NewSessionResponse,
            ResumeSessionRequest, SessionCapabilities, SessionResumeCapabilities,
        };
        use std::sync::atomic::AtomicUsize;

        let cwd = std::env::temp_dir();
        let init = InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .session_capabilities(
                    SessionCapabilities::new().resume(SessionResumeCapabilities::new()),
                ),
        );

        // Case 1: AuthRequired surfaces immediately without calling session/new.
        let new_calls = Arc::new(AtomicUsize::new(0));
        let new_calls_agent = Arc::clone(&new_calls);
        let auth_agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: ResumeSessionRequest, responder, _cx| {
                    responder.respond_with_error(agent_client_protocol::Error::auth_required())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: LoadSessionRequest, responder, _cx| {
                    responder.respond_with_error(agent_client_protocol::Error::auth_required())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    new_calls_agent.fetch_add(1, Ordering::Relaxed);
                    responder.respond(NewSessionResponse::new("blank-new"))
                },
                agent_client_protocol::on_receive_request!(),
            );
        let init_clone = init.clone();
        let cwd_clone = cwd.clone();
        let auth_outcome = smol::block_on(Client.builder().connect_with(
            auth_agent,
            async move |connection: ConnectionTo<Agent>| {
                let suppress = ReplaySuppression::default();
                let result = establish_session(
                    &connection,
                    ProviderKind::Cursor,
                    &init_clone,
                    Some("existing-cursor"),
                    &cwd_clone,
                    &[],
                    &suppress,
                    ACP_REQUEST_TIMEOUT,
                )
                .await;
                assert!(result.is_err());
                assert_eq!(
                    result.unwrap_err().code,
                    agent_client_protocol::ErrorCode::AuthRequired
                );
                Ok(())
            },
        ));
        auth_outcome.unwrap();
        assert_eq!(new_calls.load(Ordering::Relaxed), 0);

        // Case 2: Transport/server internal error on both resume and load surfaces cleanly.
        let new_calls = Arc::new(AtomicUsize::new(0));
        let new_calls_agent = Arc::clone(&new_calls);
        let transport_agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: ResumeSessionRequest, responder, _cx| {
                    responder.respond_with_error(agent_client_protocol::util::internal_error(
                        "connection closed unexpectedly",
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: LoadSessionRequest, responder, _cx| {
                    responder.respond_with_error(agent_client_protocol::Error::method_not_found())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    new_calls_agent.fetch_add(1, Ordering::Relaxed);
                    responder.respond(NewSessionResponse::new("blank-new"))
                },
                agent_client_protocol::on_receive_request!(),
            );
        let init_clone = init.clone();
        let cwd_clone = cwd.clone();
        let transport_outcome = smol::block_on(Client.builder().connect_with(
            transport_agent,
            async move |connection: ConnectionTo<Agent>| {
                let suppress = ReplaySuppression::default();
                let result = establish_session(
                    &connection,
                    ProviderKind::Cursor,
                    &init_clone,
                    Some("existing-cursor"),
                    &cwd_clone,
                    &[],
                    &suppress,
                    ACP_REQUEST_TIMEOUT,
                )
                .await;
                let err = result.unwrap_err();
                assert!(err.to_string().contains("connection closed"));
                Ok(())
            },
        ));
        transport_outcome.unwrap();
        assert_eq!(new_calls.load(Ordering::Relaxed), 0);

        // Case 3: Genuine ResourceNotFound on both resume and load still falls back to session/new.
        let new_calls = Arc::new(AtomicUsize::new(0));
        let new_calls_agent = Arc::clone(&new_calls);
        let missing_agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: ResumeSessionRequest, responder, _cx| {
                    responder
                        .respond_with_error(agent_client_protocol::Error::resource_not_found(None))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: LoadSessionRequest, responder, _cx| {
                    responder.respond_with_error(agent_client_protocol::util::internal_error(
                        "session not found",
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    new_calls_agent.fetch_add(1, Ordering::Relaxed);
                    responder.respond(NewSessionResponse::new("fresh-session"))
                },
                agent_client_protocol::on_receive_request!(),
            );
        let init_clone = init;
        let cwd_clone = cwd;
        let missing_outcome = smol::block_on(Client.builder().connect_with(
            missing_agent,
            async move |connection: ConnectionTo<Agent>| {
                let suppress = ReplaySuppression::default();
                let (session_id, _, _) = establish_session(
                    &connection,
                    ProviderKind::Cursor,
                    &init_clone,
                    Some("deleted-session"),
                    &cwd_clone,
                    &[],
                    &suppress,
                    ACP_REQUEST_TIMEOUT,
                )
                .await?;
                assert_eq!(session_id.to_string(), "fresh-session");
                Ok(())
            },
        ));
        missing_outcome.unwrap();
        assert_eq!(new_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn apply_model_updates_shared_config_options_for_cursor_fx_and_none_model() {
        use agent_client_protocol::schema::v1::SetSessionConfigOptionResponse;

        // Cursor: model == None still applies effort/fast/context and refreshes SharedConfigOptions.
        let initial_cursor = vec![
            select_config_option(
                "model",
                SessionConfigOptionCategory::Model,
                "claude-sonnet-4-6",
                &["claude-sonnet-4-6"],
            ),
            select_config_option(
                "effort",
                SessionConfigOptionCategory::ThoughtLevel,
                "medium",
                &["low", "medium", "high"],
            ),
            select_config_option(
                "context",
                SessionConfigOptionCategory::ModelConfig,
                "200k",
                &["200k", "1m"],
            ),
            select_config_option(
                "fast",
                SessionConfigOptionCategory::ModelConfig,
                "false",
                &["false", "true"],
            ),
        ];
        let state = Arc::new(Mutex::new(initial_cursor.clone()));
        let state_agent = Arc::clone(&state);
        let sent_updates = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let sent_updates_agent = Arc::clone(&sent_updates);

        let cursor_agent = Agent.builder().on_receive_request(
            async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                let value_str = match &req.value {
                    SessionConfigOptionValue::ValueId { value } => value.0.to_string(),
                    SessionConfigOptionValue::Boolean { value } => value.to_string(),
                    _ => String::new(),
                };
                sent_updates_agent
                    .lock()
                    .push((req.config_id.to_string(), value_str.clone()));
                let mut opts = state_agent.lock();
                for opt in opts.iter_mut() {
                    if opt.id == req.config_id
                        && let SessionConfigKind::Select(select) = &mut opt.kind
                    {
                        select.current_value = value_str.clone().into();
                    }
                }
                responder.respond(SetSessionConfigOptionResponse::new(opts.clone()))
            },
            agent_client_protocol::on_receive_request!(),
        );

        let cache: SharedConfigOptions = Arc::new(Mutex::new(Some(initial_cursor)));
        let cache_clone = Arc::clone(&cache);
        let (events, _rx) = crate::driver::test_event_channel();
        smol::block_on(Client.builder().connect_with(
            cursor_agent,
            async move |connection: ConnectionTo<Agent>| {
                sync_cached_session_model(
                    &connection,
                    ProviderKind::Cursor,
                    &SessionId::new("s-cursor"),
                    &cache_clone,
                    None,
                    Some("high"),
                    Some("fast"),
                    Some("1m"),
                    &events,
                    ACP_REQUEST_TIMEOUT,
                )
                .await;
                Ok(())
            },
        ))
        .unwrap();

        assert_eq!(
            *sent_updates.lock(),
            vec![
                ("effort".to_owned(), "high".to_owned()),
                ("context".to_owned(), "1m".to_owned()),
                ("fast".to_owned(), "true".to_owned()),
            ]
        );
        let updated = cache.lock().clone().unwrap();
        assert_eq!(
            session_config_current_value(config_option_by_id(&updated, "effort").unwrap()),
            Some("high")
        );
        assert_eq!(
            session_config_current_value(config_option_by_id(&updated, "context").unwrap()),
            Some("1m")
        );
        assert_eq!(
            session_config_current_value(config_option_by_id(&updated, "fast").unwrap()),
            Some("true")
        );

        // Fx: provider switch + model selection updates SharedConfigOptions.
        let initial_fx = vec![
            select_config_option(
                "provider",
                SessionConfigOptionCategory::Model,
                "codex",
                &["gateway", "codex"],
            ),
            select_config_option(
                "model",
                SessionConfigOptionCategory::Model,
                "gpt-5",
                &["gpt-5"],
            ),
        ];
        let fx_agent = Agent.builder().on_receive_request(
            async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                let response = if req.config_id.to_string() == "provider" {
                    vec![
                        select_config_option(
                            "provider",
                            SessionConfigOptionCategory::Model,
                            "gateway",
                            &["gateway", "codex"],
                        ),
                        select_config_option(
                            "model",
                            SessionConfigOptionCategory::Model,
                            "openai/gpt-5",
                            &["openai/gpt-5", "anthropic/claude-sonnet-5"],
                        ),
                    ]
                } else {
                    vec![
                        select_config_option(
                            "provider",
                            SessionConfigOptionCategory::Model,
                            "gateway",
                            &["gateway", "codex"],
                        ),
                        select_config_option(
                            "model",
                            SessionConfigOptionCategory::Model,
                            "anthropic/claude-sonnet-5",
                            &["openai/gpt-5", "anthropic/claude-sonnet-5"],
                        ),
                    ]
                };
                responder.respond(SetSessionConfigOptionResponse::new(response))
            },
            agent_client_protocol::on_receive_request!(),
        );
        let fx_cache: SharedConfigOptions = Arc::new(Mutex::new(Some(initial_fx)));
        let fx_cache_clone = Arc::clone(&fx_cache);
        let (events, _rx) = crate::driver::test_event_channel();
        smol::block_on(Client.builder().connect_with(
            fx_agent,
            async move |connection: ConnectionTo<Agent>| {
                sync_cached_session_model(
                    &connection,
                    ProviderKind::Fx,
                    &SessionId::new("s-fx"),
                    &fx_cache_clone,
                    Some("anthropic/claude-sonnet-5"),
                    None,
                    None,
                    None,
                    &events,
                    ACP_REQUEST_TIMEOUT,
                )
                .await;
                Ok(())
            },
        ))
        .unwrap();
        let fx_updated = fx_cache.lock().clone().unwrap();
        assert_eq!(
            session_config_current_value(config_option_by_id(&fx_updated, "provider").unwrap()),
            Some("gateway")
        );
        assert_eq!(
            session_config_current_value(config_option_by_id(&fx_updated, "model").unwrap()),
            Some("anthropic/claude-sonnet-5")
        );

        // Kimi: model == None still applies reasoning_effort (`thinking`) and refreshes cache.
        let kimi_sent = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let kimi_sent_agent = Arc::clone(&kimi_sent);
        let kimi_agent = Agent.builder().on_receive_request(
            async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                let val = match &req.value {
                    SessionConfigOptionValue::ValueId { value } => value.0.to_string(),
                    _ => String::new(),
                };
                kimi_sent_agent
                    .lock()
                    .push((req.config_id.to_string(), val.clone()));
                responder.respond(SetSessionConfigOptionResponse::new(vec![
                    select_config_option(
                        "thinking",
                        SessionConfigOptionCategory::ThoughtLevel,
                        &val,
                        &["off", "high"],
                    ),
                ]))
            },
            agent_client_protocol::on_receive_request!(),
        );
        let kimi_cache: SharedConfigOptions = Arc::new(Mutex::new(None));
        let kimi_cache_clone = Arc::clone(&kimi_cache);
        let (events, _rx) = crate::driver::test_event_channel();
        smol::block_on(Client.builder().connect_with(
            kimi_agent,
            async move |connection: ConnectionTo<Agent>| {
                sync_cached_session_model(
                    &connection,
                    ProviderKind::Kimi,
                    &SessionId::new("s-kimi"),
                    &kimi_cache_clone,
                    None,
                    Some("high"),
                    None,
                    None,
                    &events,
                    ACP_REQUEST_TIMEOUT,
                )
                .await;
                Ok(())
            },
        ))
        .unwrap();
        assert_eq!(
            *kimi_sent.lock(),
            vec![("thinking".to_owned(), "high".to_owned())]
        );
        assert_eq!(
            session_config_current_value(
                config_option_by_id(kimi_cache.lock().as_deref().unwrap(), "thinking").unwrap()
            ),
            Some("high")
        );
    }

    #[test]
    fn send_prompt_steer_preserves_produced_content_and_turn_end_clears_unfinished_tools() {
        let prompt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let prompt_count_agent = Arc::clone(&prompt_count);
        let mock_agent = Agent.builder().on_receive_request(
            async move |_req: PromptRequest, responder, _cx| {
                prompt_count_agent.fetch_add(1, Ordering::Relaxed);
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        );

        let stream_state = Arc::new(Mutex::new(AcpStreamState::default()));
        stream_state.lock().produced_content = true;
        stream_state.lock().tools.insert(
            "dangling-tool".into(),
            StoredToolCall {
                kind: ActivityKind::Tool,
                title: "hanging".into(),
                raw_input: None,
                locations: None,
                content: None,
                tool_name: None,
                mcp_server: None,
                complete: false,
                failed: false,
            },
        );
        let stream_state_clone = Arc::clone(&stream_state);
        let (events, event_rx) = crate::driver::test_event_channel();

        smol::block_on(Client.builder().connect_with(
            mock_agent,
            async move |connection: ConnectionTo<Agent>| {
                let prompt_requests = Arc::new(Mutex::new(PendingPrompts::default()));
                // Simulate an active turn already registered before the steer arrives.
                prompt_requests.lock().insert(
                    RequestId::Str("active-turn".into()),
                    None,
                    "sess-1".into(),
                );
                let title_refresh = super::super::title_refresh::NativeTitleRefresh::default();
                let cwd = std::env::temp_dir();
                send_prompt(
                    &connection,
                    &SessionId::new("sess-1"),
                    "steer message".into(),
                    &cwd,
                    &[],
                    &PromptCapabilities::new(),
                    &prompt_requests,
                    &events,
                    ProviderKind::Cursor,
                    "sess-1",
                    None,
                    None,
                    title_refresh.clone(),
                    stream_state_clone.clone(),
                    None,
                    PromptKind::Steer,
                    None,
                )?;
                assert!(
                    stream_state_clone.lock().produced_content,
                    "Steer must not reset produced_content to false"
                );
                // Now settle the original turn request and send a normal prompt that settles.
                prompt_requests
                    .lock()
                    .settle_request(&RequestId::Str("active-turn".into()));
                // Wait briefly for the steer response to clear its request id.
                for _ in 0..50 {
                    if prompt_requests.lock().is_empty() {
                        break;
                    }
                    smol::Timer::after(Duration::from_millis(10)).await;
                }
                assert!(
                    stream_state_clone.lock().tools.is_empty(),
                    "unfinished tools must be cleared when turn settles"
                );
                Ok(())
            },
        ))
        .unwrap();

        assert_eq!(prompt_count.load(Ordering::Relaxed), 1);
        let events_seen: Vec<_> = event_rx.try_iter().collect();
        assert!(events_seen.iter().any(|e| matches!(
            e,
            DriverEvent::TurnFinished {
                success: true,
                summary: None
            }
        )));
    }

    #[test]
    fn format_prompt_with_seed_keeps_seed_reusable_until_prompt_succeeds() {
        let mut fork_context = Some("[{\"role\":\"user\",\"content\":\"hi\"}]".to_owned());
        let mut cu_instructions = Some("CU RULES".to_owned());

        let first = format_prompt_with_seed(
            ProviderKind::Cursor,
            fork_context.as_deref(),
            cu_instructions.as_deref(),
            "first try".to_owned(),
        );
        assert!(first.starts_with("CU RULES\n\nWAKU_CURSOR_BRANCH_CONTEXT_V1 "));
        assert!(first.ends_with("first try"));
        // Simulating a failed send_prompt: seed state is NOT cleared yet, so retry gets it.
        assert!(fork_context.is_some());
        assert!(cu_instructions.is_some());

        let retry = format_prompt_with_seed(
            ProviderKind::Cursor,
            fork_context.as_deref(),
            cu_instructions.as_deref(),
            "second try".to_owned(),
        );
        assert!(retry.starts_with("CU RULES\n\nWAKU_CURSOR_BRANCH_CONTEXT_V1 "));
        assert!(retry.ends_with("second try"));

        // After send_prompt succeeds, both are consumed.
        fork_context = None;
        cu_instructions = None;
        let next = format_prompt_with_seed(
            ProviderKind::Cursor,
            fork_context.as_deref(),
            cu_instructions.as_deref(),
            "later turn".to_owned(),
        );
        assert_eq!(next, "later turn");
    }

    #[test]
    fn format_prompt_with_seed_frames_antigravity_with_antigravity_envelope() {
        let seeded = format_prompt_with_seed(
            ProviderKind::Antigravity,
            Some("[{\"role\":\"user\",\"content\":\"hi\"}]"),
            None,
            "follow up".to_owned(),
        );
        assert!(seeded.starts_with("WAKU_ANTIGRAVITY_BRANCH_CONTEXT_V1 "));
        assert!(seeded.contains("WAKU_ANTIGRAVITY_CURRENT_PROMPT_V1 "));
        assert!(seeded.ends_with("follow up"));
    }

    #[test]
    fn acp_terminal_registry_evicts_oldest_completed_entries_and_keeps_running() {
        let registry = AcpTerminalRegistry::default();
        let make_proc = |id: &str, completed_seq: Option<u64>, updated_at_ms: u64| {
            Arc::new(Mutex::new(AcpTerminalProcess {
                id: id.to_owned(),
                title: id.to_owned(),
                command_display: id.to_owned(),
                cwd_display: ".".to_owned(),
                started_at_ms: updated_at_ms,
                updated_at_ms,
                completed_seq,
                output: String::new(),
                output_truncated: false,
                output_byte_limit: DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT,
                exit_status: completed_seq.map(|_| TerminalExitStatus::new().exit_code(Some(0))),
                killed: false,
                child_pid: 0,
                #[cfg(windows)]
                job: None,
                waiters: Vec::new(),
            }))
        };

        {
            let mut map = registry.terminals.lock();
            map.insert("running-1".into(), make_proc("running-1", None, 10));
            map.insert("done-1".into(), make_proc("done-1", Some(1), 100));
            map.insert("done-2".into(), make_proc("done-2", Some(2), 200));
            map.insert("done-3".into(), make_proc("done-3", Some(3), 300));
            map.insert("done-4".into(), make_proc("done-4", Some(4), 400));
        }

        evict_completed_terminals_in(&registry.terminals, 2);

        let map = registry.terminals.lock();
        assert!(
            map.contains_key("running-1"),
            "running terminals must not be evicted"
        );
        assert!(
            !map.contains_key("done-1"),
            "oldest completed terminal should be evicted"
        );
        assert!(
            !map.contains_key("done-2"),
            "second oldest completed terminal should be evicted"
        );
        assert!(
            map.contains_key("done-3"),
            "newer completed terminal should be retained"
        );
        assert!(
            map.contains_key("done-4"),
            "newest completed terminal should be retained"
        );
    }

    #[test]
    fn native_turn_failure_offloads_blocking_poll_off_single_threaded_executor() {
        // Kimi's turn_failure polls for ~1s when the wire log does not exist.
        // Because `resolve_native_turn_failure` runs it on `smol::unblock`, a
        // short timer racing on the same single-threaded `smol::block_on`
        // executor must win promptly instead of being starved for 1s.
        let started = std::time::Instant::now();
        let winner = smol::block_on(smol::future::race(
            async {
                let _ = resolve_native_turn_failure(
                    Some(0),
                    None,
                    format!("nonexistent-session-{}", uuid::Uuid::new_v4()),
                    None,
                    false,
                    Some(StopReason::EndTurn),
                )
                .await;
                "lookup"
            },
            async {
                smol::Timer::after(Duration::from_millis(50)).await;
                "timer"
            },
        ));
        let elapsed = started.elapsed();
        assert_eq!(
            winner, "timer",
            "blocking failure lookup starved the single-threaded smol executor"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "timer took {elapsed:?} instead of ~50ms"
        );
    }

    #[test]
    fn acp_connection_enforces_timeouts_on_initialize_establish_and_cancel_not_prompt() {
        use agent_client_protocol::schema::v1::{AgentCapabilities, InitializeResponse, NewSessionResponse};

        let short_timeouts = AcpTimeouts {
            request: Duration::from_millis(50),
            cancel: Duration::from_millis(50),
        };

        // 1. Hanging initialize times out.
        let hang_init = Agent.builder().on_receive_request(
            async move |_req: InitializeRequest, _responder, _cx| {
                std::future::pending::<agent_client_protocol::Result<()>>().await
            },
            agent_client_protocol::on_receive_request!(),
        );
        let (events, _rx) = crate::driver::test_event_channel();
        let (_cmd_tx, cmd_rx) = smol::channel::unbounded();
        let init_err = smol::block_on(run_sdk_connection_with(
            hang_init,
            ProviderKind::Cursor,
            std::env::temp_dir(),
            Vec::new(),
            RuntimeMode::Ask,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            cmd_rx,
            events,
            short_timeouts,
        ))
        .unwrap_err();
        assert!(init_err.to_string().contains("ACP initialize timed out"));

        // 2. Hanging session/new times out.
        let hang_new = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, _responder, _cx| {
                    std::future::pending::<agent_client_protocol::Result<()>>().await
                },
                agent_client_protocol::on_receive_request!(),
            );
        let (events, _rx) = crate::driver::test_event_channel();
        let (_cmd_tx, cmd_rx) = smol::channel::unbounded();
        let new_err = smol::block_on(run_sdk_connection_with(
            hang_new,
            ProviderKind::Cursor,
            std::env::temp_dir(),
            Vec::new(),
            RuntimeMode::Ask,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            cmd_rx,
            events,
            short_timeouts,
        ))
        .unwrap_err();
        assert!(new_err.to_string().contains("ACP session/new timed out"));

        // 3. Long prompt outlives `timeouts.request`, and hanging cancel settles at `timeouts.cancel`
        // and tears down the wedged connection cleanly without needing `Shutdown`.
        let prompt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let prompt_count_agent = Arc::clone(&prompt_count);
        let cancel_agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    responder.respond(NewSessionResponse::new("sess-cancel"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: PromptRequest, responder, _cx| {
                    let count = prompt_count_agent.fetch_add(1, Ordering::Relaxed);
                    if count == 0 {
                        // Longer than `timeouts.request` (50ms) — must NOT time out.
                        smol::Timer::after(Duration::from_millis(120)).await;
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    } else {
                        // Ignore cancel and hang forever.
                        std::future::pending::<agent_client_protocol::Result<()>>().await
                    }
                },
                agent_client_protocol::on_receive_request!(),
            );
        let (events, event_rx) = crate::driver::test_event_channel();
        let (cmd_tx, cmd_rx) = smol::channel::unbounded();
        let handle = std::thread::spawn(move || {
            smol::block_on(run_sdk_connection_with(
                cancel_agent,
                ProviderKind::Cursor,
                std::env::temp_dir(),
                Vec::new(),
                RuntimeMode::Ask,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                None,
                cmd_rx,
                events,
                short_timeouts,
            ))
        });

        // Wait for Connected.
        loop {
            let ev = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if matches!(ev, DriverEvent::Connected { .. }) {
                break;
            }
        }
        // First prompt takes 120ms (> 50ms request timeout) and succeeds.
        cmd_tx.try_send(CommandMessage::Prompt("slow turn".into())).unwrap();
        loop {
            let ev = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if let DriverEvent::TurnFinished { success, .. } = ev {
                assert!(success, "long prompt must not hit short request timeout");
                break;
            }
        }
        // Second prompt hangs; Cancel must settle after `timeouts.cancel` and exit the loop.
        cmd_tx.try_send(CommandMessage::Prompt("hung turn".into())).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        cmd_tx.try_send(CommandMessage::Cancel).unwrap();
        let mut saw_cancel_error = false;
        loop {
            let ev = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            match ev {
                DriverEvent::Error(msg) if msg.contains("session/cancel timed out") => {
                    saw_cancel_error = true;
                }
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(!success);
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_cancel_error);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn acp_cancel_then_unsolicited_tool_and_steer_does_not_trip_stale_cancel_timeout() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, InitializeResponse, NewSessionResponse, ToolCall, ToolCallStatus,
            ToolCallUpdate, ToolCallUpdateFields,
        };

        let short_timeouts = AcpTimeouts {
            request: Duration::from_secs(1),
            cancel: Duration::from_millis(80),
        };
        let prompt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let prompt_count_agent = Arc::clone(&prompt_count);
        let agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    responder.respond(NewSessionResponse::new("sess-stale-cancel"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: PromptRequest, responder, cx| {
                    let idx = prompt_count_agent.fetch_add(1, Ordering::Relaxed);
                    if idx == 0 {
                        // Settle slightly after Cancel arrives, then kick off an unsolicited
                        // turn starting with an in-flight tool call (no wake signal).
                        smol::Timer::after(Duration::from_millis(25)).await;
                        responder.respond(PromptResponse::new(StopReason::Cancelled))?;
                        let cx_bg = cx.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(15));
                            let _ = cx_bg.send_notification(SessionNotification::new(
                                "sess-stale-cancel",
                                SessionUpdate::ToolCall(
                                    ToolCall::new("unsol-tool-1", "Working")
                                        .status(ToolCallStatus::InProgress),
                                ),
                            ));
                        });
                        Ok(())
                    } else {
                        // Steer outlives `timeouts.cancel` (80ms) — must NOT be killed by
                        // the stale cancel_deadline from Turn 0.
                        smol::Timer::after(Duration::from_millis(150)).await;
                        let _ = cx.send_notification(SessionNotification::new(
                            "sess-stale-cancel",
                            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                "unsol-tool-1",
                                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                            )),
                        ));
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            );

        let (events, event_rx) = crate::driver::test_event_channel();
        let (cmd_tx, cmd_rx) = smol::channel::unbounded();
        let handle = std::thread::spawn(move || {
            smol::block_on(run_sdk_connection_with(
                agent,
                ProviderKind::Antigravity,
                std::env::temp_dir(),
                Vec::new(),
                RuntimeMode::Ask,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                None,
                cmd_rx,
                events,
                short_timeouts,
            ))
        });

        loop {
            let ev = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if matches!(ev, DriverEvent::Connected { .. }) {
                break;
            }
        }
        // Prompt 1 -> Cancel -> Prompt 1 settles normally before cancel timeout.
        cmd_tx.try_send(CommandMessage::Prompt("first".into())).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        cmd_tx.try_send(CommandMessage::Cancel).unwrap();
        loop {
            let ev = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if let DriverEvent::TurnFinished { success, .. } = ev {
                assert!(success);
                break;
            }
        }
        // Wait for unsolicited tool turn to open, then Steer.
        loop {
            let ev = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if matches!(ev, DriverEvent::TurnStarted) {
                break;
            }
        }
        cmd_tx.try_send(CommandMessage::Steer("steer it".into())).unwrap();
        let mut steer_finished_ok = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(ev) = event_rx.recv_timeout(Duration::from_millis(200)) {
                match ev {
                    DriverEvent::Error(msg) => panic!("unexpected error: {msg}"),
                    DriverEvent::TurnFinished { success, .. } => {
                        steer_finished_ok = success;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(steer_finished_ok);
        let _ = cmd_tx.try_send(CommandMessage::Shutdown);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn acp_antigravity_unsolicited_turn_checks_native_failure_on_settle() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, ContentChunk, InitializeResponse, NewSessionResponse,
            UsageUpdate,
        };

        let root =
            std::env::temp_dir().join(format!("waku-acp-unsolicited-err-{}", uuid::Uuid::new_v4()));
        let config_dir = root.join("agy-config");
        std::fs::create_dir_all(config_dir.join("conversations")).unwrap();
        let session_id = "unsolicited-err-1";
        let db_path = config_dir
            .join("conversations")
            .join(format!("{session_id}.db"));
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (idx INTEGER NOT NULL, step_type INTEGER NOT NULL, step_payload BLOB);
             INSERT INTO steps (idx, step_type, step_payload) VALUES (0, 1, X'00');",
        )
        .unwrap();
        drop(conn);

        let db_path_bg = db_path.clone();
        let agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, cx| {
                    responder.respond(NewSessionResponse::new(session_id))?;
                    let cx_bg = cx.clone();
                    let db_path = db_path_bg.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(50));
                        // Write a silent native quota error step during the unsolicited turn.
                        let conn = rusqlite::Connection::open(&db_path).unwrap();
                        conn.execute(
                            "INSERT INTO steps (idx, step_type, step_payload) VALUES (1, 17, CAST('Error 429, Message: quota, Status: RESOURCE_EXHAUSTED' AS BLOB))",
                            [],
                        )
                        .unwrap();
                        let _ = cx_bg.send_notification(SessionNotification::new(
                            session_id,
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new("Model quota exceeded")),
                            )),
                        ));
                        let _ = cx_bg.send_notification(SessionNotification::new(
                            session_id,
                            SessionUpdate::UsageUpdate(UsageUpdate::new(100, 1000)),
                        ));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            );

        let (events, event_rx) = crate::driver::test_event_channel();
        let (cmd_tx, cmd_rx) = smol::channel::unbounded();
        let cfg = config_dir.clone();
        let handle = std::thread::spawn(move || {
            smol::block_on(run_sdk_connection_with(
                agent,
                ProviderKind::Antigravity,
                std::env::temp_dir(),
                Vec::new(),
                RuntimeMode::Ask,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(cfg),
                Vec::new(),
                None,
                None,
                cmd_rx,
                events,
                AcpTimeouts::default(),
            ))
        });

        let mut saw_error = false;
        let mut saw_failed_finish = false;
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            if let Ok(ev) = event_rx.recv_timeout(Duration::from_millis(200)) {
                match ev {
                    DriverEvent::Error(_) => saw_error = true,
                    DriverEvent::TurnFinished { success, .. } => {
                        assert!(!success, "unsolicited turn with native error must fail");
                        saw_failed_finish = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_error, "expected DriverEvent::Error from native failure");
        assert!(saw_failed_finish);
        let _ = cmd_tx.try_send(CommandMessage::Shutdown);
        handle.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn acp_antigravity_steer_recovers_lost_session() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, ContentChunk, InitializeResponse, NewSessionResponse,
        };

        let new_session_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let new_session_count_agent = Arc::clone(&new_session_count);
        let steered_on_fresh = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let steered_on_fresh_agent = Arc::clone(&steered_on_fresh);
        let agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, cx| {
                    let idx = new_session_count_agent.fetch_add(1, Ordering::Relaxed);
                    let id = if idx == 0 { "lost-sess" } else { "fresh-sess" };
                    responder.respond(NewSessionResponse::new(id))?;
                    if idx == 0 {
                        // Emit an unsolicited update so Steer can target the session immediately.
                        let cx_bg = cx.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(30));
                            let _ = cx_bg.send_notification(SessionNotification::new(
                                "lost-sess",
                                SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                    ContentBlock::Text(TextContent::new("working...")),
                                )),
                            ));
                        });
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: PromptRequest, responder, cx| {
                    let sid = req.session_id.to_string();
                    if sid == "lost-sess" {
                        return responder.respond_with_error(
                            agent_client_protocol::util::internal_error(
                                "not_found: trajectory not found: lost-sess",
                            ),
                        );
                    }
                    let text = req
                        .prompt
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    steered_on_fresh_agent.lock().push((sid.clone(), text));
                    let _ = cx.send_notification(SessionNotification::new(
                        sid,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(
                            ContentBlock::Text(TextContent::new("steered ok")),
                        )),
                    ));
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                agent_client_protocol::on_receive_request!(),
            );

        let (events, event_rx) = crate::driver::test_event_channel();
        let (cmd_tx, cmd_rx) = smol::channel::unbounded();
        let handle = std::thread::spawn(move || {
            smol::block_on(run_sdk_connection_with(
                agent,
                ProviderKind::Antigravity,
                std::env::temp_dir(),
                Vec::new(),
                RuntimeMode::Ask,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                None,
                cmd_rx,
                events,
                AcpTimeouts::default(),
            ))
        });

        // Wait for the unsolicited turn to open, then steer it.
        loop {
            let ev = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if matches!(ev, DriverEvent::TurnStarted) {
                break;
            }
        }
        cmd_tx.try_send(CommandMessage::Steer("steer payload".into())).unwrap();

        let mut recovered_cursor = None;
        let mut finished_ok = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(ev) = event_rx.recv_timeout(Duration::from_millis(200)) {
                match ev {
                    DriverEvent::Connected {
                        provider_cursor: Some(cursor),
                    } => {
                        recovered_cursor = Some(cursor.native_id().to_owned());
                    }
                    DriverEvent::TurnFinished { success, .. } => {
                        finished_ok = success;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(recovered_cursor.as_deref(), Some("fresh-sess"));
        assert!(finished_ok);
        assert_eq!(
            *steered_on_fresh.lock(),
            vec![("fresh-sess".to_owned(), "steer payload".to_owned())]
        );
        let _ = cmd_tx.try_send(CommandMessage::Shutdown);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn acp_antigravity_concurrent_turn_and_steer_both_recover_on_lost_session() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, InitializeResponse, NewSessionResponse,
        };

        let new_session_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let new_session_count_agent = Arc::clone(&new_session_count);
        let delivered_on_fresh = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let delivered_on_fresh_agent = Arc::clone(&delivered_on_fresh);
        let agent = Agent
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    let idx = new_session_count_agent.fetch_add(1, Ordering::Relaxed);
                    let id = if idx == 0 { "lost-sess" } else { "fresh-sess" };
                    responder.respond(NewSessionResponse::new(id))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: PromptRequest, responder, cx| {
                    let sid = req.session_id.to_string();
                    let text = req
                        .prompt
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if sid == "lost-sess" {
                        // Make the primary Turn fail slightly before the Steer so
                        // `settle_prompt_request` returns `false` for the Turn.
                        if text != "primary turn" {
                            smol::Timer::after(Duration::from_millis(25)).await;
                        }
                        return responder.respond_with_error(
                            agent_client_protocol::util::internal_error(
                                "not_found: trajectory not found: lost-sess",
                            ),
                        );
                    }
                    delivered_on_fresh_agent.lock().push((sid.clone(), text));
                    smol::Timer::after(Duration::from_millis(15)).await;
                    let _ = cx.send_notification(SessionNotification::new(
                        sid,
                        SessionUpdate::AgentMessageChunk(
                            agent_client_protocol::schema::v1::ContentChunk::new(
                                ContentBlock::Text(TextContent::new("recovered ok")),
                            ),
                        ),
                    ));
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                agent_client_protocol::on_receive_request!(),
            );

        let (events, event_rx) = crate::driver::test_event_channel();
        let (cmd_tx, cmd_rx) = smol::channel::unbounded();
        let handle = std::thread::spawn(move || {
            smol::block_on(run_sdk_connection_with(
                agent,
                ProviderKind::Antigravity,
                std::env::temp_dir(),
                Vec::new(),
                RuntimeMode::Ask,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                None,
                cmd_rx,
                events,
                AcpTimeouts::default(),
            ))
        });

        loop {
            let ev = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if matches!(ev, DriverEvent::Connected { .. }) {
                break;
            }
        }
        cmd_tx.try_send(CommandMessage::Prompt("primary turn".into())).unwrap();
        cmd_tx.try_send(CommandMessage::Steer("follow-up steer".into())).unwrap();

        let mut finished_ok = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(ev) = event_rx.recv_timeout(Duration::from_millis(200))
                && let DriverEvent::TurnFinished { success, .. } = ev
            {
                finished_ok = success;
                break;
            }
        }
        assert!(finished_ok);
        assert_eq!(new_session_count.load(Ordering::Relaxed), 2);
        assert_eq!(
            *delivered_on_fresh.lock(),
            vec![
                ("fresh-sess".to_owned(), "primary turn".to_owned()),
                ("fresh-sess".to_owned(), "follow-up steer".to_owned()),
            ]
        );
        let _ = cmd_tx.try_send(CommandMessage::Shutdown);
        handle.join().unwrap().unwrap();
    }

    /// Drives a real agent through the SDK-backed driver. Ignored by default:
    /// it needs the CLI installed, credentials, and the network.
    #[test]
    #[ignore = "requires an installed, authenticated grok"]
    fn grok_prompt_response_from_the_sdk_finishes_the_turn() {
        let binary = crate::command_env::find_executable("grok").expect("grok is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = AcpDriver::start(
            ProviderKind::Grok,
            DriverStartOptions {
                binary,
                cwd: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
                mode: RuntimeMode::FullAccess,
                model: Some("grok-4.5".into()),
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                config_dir: None,
            },
            events,
        )
        .expect("the ACP session should open");

        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the agent should report its session");
            match event {
                DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Grok { .. }),
                } => break,
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        driver.prompt("hi".into());
        let mut finished = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(120)) {
            match event {
                DriverEvent::TurnFinished { success, .. } => {
                    finished = Some(success);
                    break;
                }
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        assert_eq!(finished, Some(true));
    }

    /// Covers Cursor's provider-private parameterized picker with a model id
    /// whose CLI alias carries both effort and fast-mode values.
    #[test]
    #[ignore = "requires an installed, authenticated cursor-agent"]
    fn cursor_parameterized_model_selection_finishes_a_real_turn() {
        let binary = crate::command_env::find_executable("cursor-agent")
            .expect("cursor-agent is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = AcpDriver::start(
            ProviderKind::Cursor,
            DriverStartOptions {
                binary,
                cwd: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
                mode: RuntimeMode::FullAccess,
                model: Some("cursor-grok-4.6-xhigh".into()),
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                config_dir: None,
            },
            events,
        )
        .expect("the ACP session should open");

        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the agent should report its session");
            match event {
                DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Cursor { .. }),
                } => break,
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        driver.prompt("Reply exactly OK.".into());

        let mut produced_text = false;
        let mut finished = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(120)) {
            match event {
                DriverEvent::TextDelta(text) => produced_text |= !text.is_empty(),
                DriverEvent::TurnFinished { success, .. } => {
                    finished = Some(success);
                    break;
                }
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        assert!(produced_text, "the Cursor turn produced no text");
        assert_eq!(finished, Some(true));
    }

    /// The invariant Kimi's silent failures break: a turn may finish
    /// successfully or report why it did not, but it must never claim success
    /// having produced nothing at all. Holds whether or not the account is
    /// currently able to serve the request.
    #[test]
    #[ignore = "requires an installed, authenticated kimi"]
    fn kimi_never_reports_an_empty_turn_as_a_success() {
        let binary = crate::command_env::find_executable("kimi").expect("kimi is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = AcpDriver::start(
            ProviderKind::Kimi,
            DriverStartOptions {
                binary,
                cwd: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                config_dir: None,
            },
            events,
        )
        .expect("the ACP session should open");

        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the agent should report its session");
            match event {
                DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Kimi { .. }),
                } => break,
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        driver.prompt("Say hi in three words.".into());

        let mut produced_content = false;
        let mut reported_error = None;
        let mut finished = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(120)) {
            match event {
                DriverEvent::TextDelta(_) | DriverEvent::ReasoningDelta(_) => {
                    produced_content = true;
                }
                DriverEvent::Error(error) => reported_error = Some(error),
                DriverEvent::TurnFinished { success, .. } => {
                    finished = Some(success);
                    break;
                }
                _ => {}
            }
        }

        match finished.expect("the turn should settle") {
            true => assert!(
                produced_content,
                "the turn was reported successful without producing anything"
            ),
            false => assert!(
                reported_error.is_some_and(|error| !error.trim().is_empty()),
                "the turn failed without naming a reason"
            ),
        }
    }

    /// Factory Droid's ACP daemon must accept a session and settle a turn.
    /// Autonomy `auto-high` is Full access; the test does not pass
    /// `--skip-permissions-unsafe`.
    #[test]
    #[ignore = "requires an installed, authenticated droid"]
    fn droid_acp_daemon_finishes_a_real_turn() {
        let binary = crate::command_env::find_executable("droid").expect("droid is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = AcpDriver::start(
            ProviderKind::Droid,
            DriverStartOptions {
                binary,
                cwd: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                config_dir: None,
            },
            events,
        )
        .expect("the ACP session should open");

        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the agent should report its session");
            match event {
                DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Droid { .. }),
                } => break,
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        driver.prompt("Reply exactly OK.".into());

        let mut produced_text = false;
        let mut finished = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(120)) {
            match event {
                DriverEvent::TextDelta(text) => produced_text |= !text.is_empty(),
                DriverEvent::TurnFinished { success, .. } => {
                    finished = Some(success);
                    break;
                }
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        assert!(produced_text, "the Droid turn produced no text");
        assert_eq!(finished, Some(true));
    }

    /// Antigravity's `agy acp` server must accept a session and settle a turn.
    #[test]
    #[ignore = "requires an installed, authenticated agy"]
    fn antigravity_acp_server_finishes_a_real_turn() {
        let binary = crate::command_env::find_executable("agy").expect("agy is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = AcpDriver::start(
            ProviderKind::Antigravity,
            DriverStartOptions {
                binary,
                cwd: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
                mode: RuntimeMode::AutoAcceptEdits,
                model: Some("gemini-4-carbon".into()),
                reasoning_effort: Some("low".into()),
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                config_dir: None,
            },
            events,
        )
        .expect("the ACP session should open");

        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the agent should report its session");
            match event {
                DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Antigravity { .. }),
                } => break,
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        driver.prompt("Reply exactly OK.".into());

        let mut produced_text = false;
        let mut finished = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(120)) {
            match event {
                DriverEvent::TextDelta(text) => produced_text |= !text.is_empty(),
                DriverEvent::TurnFinished { success, .. } => {
                    finished = Some(success);
                    break;
                }
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        assert!(produced_text, "the Antigravity turn produced no text");
        assert_eq!(finished, Some(true));
    }

    /// Antigravity advertises `session/resume`, but only `session/load`
    /// restores the conversation; a resumed process otherwise starts empty.
    #[test]
    #[ignore = "requires an installed, authenticated agy"]
    fn antigravity_resume_keeps_the_conversation() {
        let binary = crate::command_env::find_executable("agy").expect("agy is not installed");
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let options = || DriverStartOptions {
            binary: binary.clone(),
            cwd: cwd.clone(),
            mode: RuntimeMode::AutoAcceptEdits,
            model: Some("gemini-4-carbon".into()),
            reasoning_effort: Some("low".into()),
            service_tier: None,
            context_window: None,
            agent_preset: None,
            computer_use_enabled: false,
            provider_cursor: None,
            config_dir: None,
        };

        let (events, event_rx) = crate::driver::test_event_channel();
        let driver =
            AcpDriver::start(ProviderKind::Antigravity, options(), events).expect("session opens");
        let cursor = loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the agent should report its session");
            match event {
                DriverEvent::Connected {
                    provider_cursor: Some(cursor),
                } => break cursor,
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        };
        driver.prompt("Remember the word PINEAPPLE. Reply exactly STORED.".into());
        loop {
            match event_rx
                .recv_timeout(Duration::from_secs(120))
                .expect("the first turn should finish")
            {
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "the first turn failed");
                    break;
                }
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        drop(driver);
        std::thread::sleep(Duration::from_secs(1));

        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = AcpDriver::start(
            ProviderKind::Antigravity,
            DriverStartOptions {
                provider_cursor: Some(cursor),
                ..options()
            },
            events,
        )
        .expect("the session should resume");
        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the agent should report its session");
            match event {
                DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Antigravity { .. }),
                } => break,
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        driver.prompt("What word were you asked to remember? Reply with exactly that word.".into());

        let mut reply = String::new();
        let mut finished = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(120)) {
            match event {
                DriverEvent::TextDelta(text) => reply.push_str(&text),
                DriverEvent::TurnFinished { success, .. } => {
                    finished = Some(success);
                    break;
                }
                DriverEvent::Error(error) => panic!("the agent reported: {error}"),
                _ => {}
            }
        }
        assert_eq!(finished, Some(true));
        assert!(
            reply.to_ascii_uppercase().contains("PINEAPPLE"),
            "the resumed turn lost the conversation: {reply:?}"
        );
    }
}
