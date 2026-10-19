//! Agent Client Protocol transport backed by the official Rust SDK.
//!
//! The SDK owns JSON-RPC framing, request IDs, response routing, cancellation,
//! unknown-method errors, stdio lifetime, and protocol type validation. Waku
//! only adapts typed ACP messages to its provider-neutral [`DriverEvent`]s.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, CreateElicitationRequest,
    CreateElicitationResponse, ElicitationAcceptAction, ElicitationAction, ElicitationCapabilities,
    ElicitationContentValue, ElicitationFormCapabilities, ElicitationMode,
    ElicitationPropertySchema, EnumOption, Implementation, InitializeRequest, InitializeResponse,
    LoadSessionRequest, MultiSelectItems, NewSessionRequest, PermissionOptionKind, PromptRequest,
    PromptResponse, RequestId, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOptions, SessionId, SessionModeId, SessionModeState, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason, TextContent,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo, Handled, LineDirection, Responder,
    UntypedMessage,
};
use anyhow::{Context as _, anyhow};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use super::activity;
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityKind, DriverEvent, PermissionOption, ProviderKind, ProviderResumeCursor, RuntimeMode,
    UserInputAnswer, UserInputOption, UserInputQuestion,
};
use waku_protocol::model_catalog::{
    CursorModelSelection, cursor_suffix_has, normalize_cursor_reasoning_effort,
    resolve_cursor_model,
};

enum CommandMessage {
    Prompt(String),
    Steer(String),
    Cancel,
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
    Shutdown,
}

/// How long `apply_options` waits for the agent to accept a live access-mode
/// switch before asking the app to restart the session instead.
const MODE_SWITCH_TIMEOUT: Duration = Duration::from_secs(10);

pub struct AcpDriver {
    commands: smol::channel::Sender<CommandMessage>,
    supports_steer: bool,
    mode: Mutex<RuntimeMode>,
    /// Factory Droid's access mode is a session config option, so it switches
    /// in place; every other ACP agent restarts to change it.
    switches_mode_live: bool,
    computer_use: Option<super::support::HeadlessComputerUseRuntime>,
}

/// Per-provider launch details. Everything after process launch is ACP.
struct AcpLaunch {
    args: Vec<String>,
    env: Vec<(String, String)>,
}

fn launch_for(provider: ProviderKind, reasoning_effort: Option<&str>) -> anyhow::Result<AcpLaunch> {
    match provider {
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
        } = options;
        let fork_context = match &provider_cursor {
            Some(ProviderResumeCursor::Cursor { fork_context, .. }) => fork_context.clone(),
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

        let launch = launch_for(provider, reasoning_effort.as_deref())?;
        let computer_use = (provider == ProviderKind::Grok && computer_use_enabled)
            .then(|| super::support::HeadlessComputerUseRuntime::start(provider, events.clone()))
            .transpose()?;
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
                    mode,
                    model,
                    reasoning_effort,
                    service_tier,
                    context_window,
                    resume_session_id,
                    fork_context,
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
            // Both reject a second `session/prompt` while one is running.
            supports_steer: !matches!(provider, ProviderKind::Fx | ProviderKind::Droid),
            mode: Mutex::new(mode),
            switches_mode_live: provider == ProviderKind::Droid,
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

fn sdk_agent(
    binary: &Path,
    cwd: &Path,
    mut launch: AcpLaunch,
    computer_use: Option<&super::support::HeadlessComputerUseConfig>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
) -> anyhow::Result<ProviderAgent> {
    let binary = binary
        .to_str()
        .ok_or_else(|| anyhow!("the ACP executable path is not valid UTF-8"))?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow!("the ACP working directory is not valid UTF-8"))?;
    let (computer_args, computer_env) =
        super::support::grok_computer_use_launch_configuration(computer_use);
    launch.args.extend(computer_args);
    let mut environment = crate::command_env::shell_environment()
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect::<Vec<_>>();
    environment.append(&mut launch.env);
    environment.extend(computer_env);

    // `AcpAgentConfig` deliberately contains only argv and environment. macOS
    // `env -C` supplies the session cwd without a shell, preserving exact
    // argument boundaries and the SDK's process-group lifecycle management.
    // Windows has no `/usr/bin/env`: the resolved agent is spawned directly —
    // `Command` runs `.cmd`/`.bat` shims through `cmd.exe` itself — and the
    // session cwd reaches the agent through the ACP `session/new`, `resume`,
    // and `load` requests instead.
    let (program, args) = if cfg!(windows) {
        (binary, launch.args)
    } else {
        let mut args = vec!["-C".to_owned(), cwd.to_owned(), binary.to_owned()];
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
    let launch = launch_for(provider, None)?;
    sdk_agent(binary, cwd, launch, None, Arc::new(Mutex::new(Vec::new())))
}

type PermissionResponder = Responder<RequestPermissionResponse>;
type PendingPermissions = Arc<Mutex<HashMap<String, PermissionResponder>>>;

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
}

type PendingPromptRequests = Arc<Mutex<PendingPrompts>>;

#[allow(clippy::too_many_arguments)]
async fn run_sdk_connection(
    agent: ProviderAgent,
    provider: ProviderKind,
    cwd: std::path::PathBuf,
    mode: RuntimeMode,
    model: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    context_window: Option<String>,
    resume_session_id: Option<String>,
    fork_context: Option<String>,
    grok_title_home: Option<std::path::PathBuf>,
    commands: smol::channel::Receiver<CommandMessage>,
    events: DriverEventSender,
) -> agent_client_protocol::Result<()> {
    let suppress_session_updates = Arc::new(AtomicBool::new(false));
    let stream_state = Arc::new(Mutex::new(AcpStreamState::default()));
    let pending_permissions: PendingPermissions = Arc::new(Mutex::new(HashMap::new()));
    let pending_user_inputs: PendingAcpUserInputs = Arc::new(Mutex::new(HashMap::new()));
    let prompt_requests = Arc::new(Mutex::new(PendingPrompts::default()));
    let title_refresh = super::title_refresh::NativeTitleRefresh::default();
    // The access mode can change in place (Droid), so every reader goes
    // through this cell rather than the mode the session started with.
    let access_mode = Arc::new(Mutex::new(mode));
    let config_options: SharedConfigOptions = Arc::new(Mutex::new(None));
    // Set once the session's own autonomy has been written, so drift
    // correction never races the initial write.
    let droid_autonomy_ready = Arc::new(AtomicBool::new(false));

    Client
        .builder()
        .name("waku")
        .on_receive_notification(
            {
                let events = events.clone();
                let suppress_session_updates = suppress_session_updates.clone();
                let stream_state = stream_state.clone();
                let access_mode = access_mode.clone();
                let config_options = config_options.clone();
                let droid_autonomy_ready = droid_autonomy_ready.clone();
                async move |notification: SessionNotification, connection: ConnectionTo<Agent>| {
                    if !suppress_session_updates.load(Ordering::Acquire) {
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
                        handle_session_update(
                            provider,
                            notification,
                            &events,
                            &mut stream_state.lock(),
                        )?;
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
                let grok_title_home = grok_title_home.clone();
                let title_refresh = title_refresh.clone();
                async move |notification: UntypedMessage, _connection| {
                    if notification.method() == "_x.ai/session/prompt_complete" {
                        if let Some(session_id) = finish_xai_prompt_complete(
                            notification.params(),
                            &prompt_requests,
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
                let pending_permissions = pending_permissions.clone();
                let access_mode = access_mode.clone();
                async move |request: RequestPermissionRequest, responder, _connection| {
                    handle_permission_request(
                        request,
                        responder,
                        auto_approves_permissions(provider, *access_mode.lock()),
                        provider == ProviderKind::Droid,
                        &pending_permissions,
                        &events,
                    )
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
            let mut client_capabilities = ClientCapabilities::new().terminal(false).elicitation(
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
            let initialize = connection
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(client_capabilities)
                        .client_info(Implementation::new("waku", env!("CARGO_PKG_VERSION"))),
                )
                .block_task()
                .await?;
            let (session_id, modes, initial_config_options) = establish_session(
                &connection,
                &initialize,
                resume_session_id.as_deref(),
                &cwd,
                &suppress_session_updates,
            )
            .await?;
            *config_options.lock() = initial_config_options;

            if let Some(mode_id) = desired_access_mode(provider, modes.as_ref(), mode) {
                // Mode selection is opportunistic: an agent can advertise a
                // mode but reject a later transition without invalidating the
                // session itself.
                let _ = connection
                    .send_request(SetSessionModeRequest::new(session_id.clone(), mode_id))
                    .block_task()
                    .await;
            }
            if provider == ProviderKind::Droid {
                let current = config_options.lock().clone();
                match apply_droid_autonomy(&connection, &session_id, current.as_deref(), mode).await
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
            let native_session_id = session_id.to_string();
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
            )
            .await;
            let mut fork_context = fork_context;

            while let Ok(command) = commands.recv().await {
                match command {
                    CommandMessage::Prompt(text) => {
                        let text = fork_context
                            .take()
                            .map(|context| {
                                crate::cursor_session::prompt_with_fork_context(&context, &text)
                            })
                            .unwrap_or(text);
                        let _ = events.send(DriverEvent::TurnStarted);
                        if let Err(error) = send_prompt(
                            &connection,
                            &session_id,
                            text,
                            &prompt_requests,
                            &events,
                            provider,
                            &native_session_id,
                            grok_title_home.clone(),
                            title_refresh.clone(),
                            stream_state.clone(),
                        ) {
                            let _ = events.send(DriverEvent::Error(error.to_string()));
                            let _ = events.send(DriverEvent::TurnFinished {
                                success: false,
                                summary: None,
                            });
                        }
                    }
                    CommandMessage::Steer(text) => {
                        if prompt_requests.lock().is_empty() {
                            let _ = events.send(DriverEvent::SteerRejected {
                                message: text,
                                reason: format!(
                                    "{} has no active turn to steer.",
                                    provider.display_name()
                                ),
                            });
                            continue;
                        }
                        match send_prompt(
                            &connection,
                            &session_id,
                            text.clone(),
                            &prompt_requests,
                            &events,
                            provider,
                            &native_session_id,
                            grok_title_home.clone(),
                            title_refresh.clone(),
                            stream_state.clone(),
                        ) {
                            Ok(()) => {
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
                    }
                    CommandMessage::Respond {
                        request_id,
                        option_id,
                    } => {
                        if let Some(responder) = pending_permissions.lock().remove(&request_id) {
                            let _ = responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                    option_id,
                                )),
                            ));
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
                            *access_mode.lock() = options.mode;
                            let current = config_options.lock().clone();
                            let applied = match apply_droid_autonomy(
                                &connection,
                                &session_id,
                                current.as_deref(),
                                options.mode,
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
                            )
                            .await;
                        }
                    }
                    CommandMessage::Shutdown => break,
                }
            }
            cancel_pending_permissions(&pending_permissions);
            cancel_pending_user_inputs(&pending_user_inputs);
            Ok(())
        })
        .await
}

async fn establish_session(
    connection: &ConnectionTo<Agent>,
    initialize: &InitializeResponse,
    resume_session_id: Option<&str>,
    cwd: &Path,
    suppress_session_updates: &AtomicBool,
) -> agent_client_protocol::Result<(
    SessionId,
    Option<SessionModeState>,
    Option<Vec<SessionConfigOption>>,
)> {
    if let Some(existing) = resume_session_id {
        if initialize
            .agent_capabilities
            .session_capabilities
            .resume
            .is_some()
            && let Ok(response) = connection
                .send_request(ResumeSessionRequest::new(existing.to_owned(), cwd))
                .block_task()
                .await
        {
            return Ok((
                SessionId::new(existing.to_owned()),
                response.modes,
                response.config_options,
            ));
        }

        if initialize.agent_capabilities.load_session {
            suppress_session_updates.store(true, Ordering::Release);
            let response = connection
                .send_request(LoadSessionRequest::new(existing.to_owned(), cwd))
                .block_task()
                .await;
            suppress_session_updates.store(false, Ordering::Release);
            if let Ok(response) = response {
                return Ok((
                    SessionId::new(existing.to_owned()),
                    response.modes,
                    response.config_options,
                ));
            }
        }
    }

    let response = connection
        .send_request(NewSessionRequest::new(cwd))
        .block_task()
        .await?;
    Ok((response.session_id, response.modes, response.config_options))
}

fn desired_access_mode(
    provider: ProviderKind,
    modes: Option<&SessionModeState>,
    mode: RuntimeMode,
) -> Option<SessionModeId> {
    let modes = modes?;
    let desired = if provider == ProviderKind::Fx {
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

async fn apply_cursor_variant_configs(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    mut options: Vec<SessionConfigOption>,
    selection: &CursorModelSelection,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
    context_window: Option<&str>,
) -> agent_client_protocol::Result<()> {
    // Thinking can reveal a thought-level option, so apply it first and use
    // each response's refreshed option set for the next selection.
    for target in ["thinking", "effort", "context", "fast"] {
        let Some(option) = (match target {
            "thinking" => options
                .iter()
                .find(|option| is_cursor_thinking_option(option)),
            "effort" => find_cursor_effort_option(&options),
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
        options = connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id,
                value,
            ))
            .block_task()
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

/// Writes Droid's autonomy level for `mode`. `Ok(None)` means the session
/// already had it; `Err` carries a user-facing reason.
async fn apply_droid_autonomy(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    config_options: Option<&[SessionConfigOption]>,
    mode: RuntimeMode,
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
    connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id.clone(),
            option.id.clone(),
            desired,
        ))
        .block_task()
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
        SessionUpdate::ConfigOptionUpdate(update) => {
            *config_options.lock() = Some(update.config_options.clone());
            provider == ProviderKind::Droid
                && config_option_by_id(&update.config_options, "autonomy_level")
                    .and_then(session_config_current_value)
                    .is_some_and(|current| current != desired)
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
) -> Option<Vec<SessionConfigOption>> {
    let model = model?;
    let mut options = config_options.unwrap_or_default().to_vec();
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
        match connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id.clone(),
                model,
            ))
            .block_task()
            .await
        {
            Ok(response) => options = response.config_options,
            Err(error) => {
                let _ = events.send(DriverEvent::Error(tr!(
                    "errors.select_model",
                    error = error
                )));
                return None;
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
        if let Ok(response) = connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                config_id,
                effort,
            ))
            .block_task()
            .await
        {
            options = response.config_options;
        }
    }
    Some(options)
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
) {
    if provider == ProviderKind::Droid {
        if let Some(updated) = apply_droid_model(
            connection,
            session_id,
            config_options.as_deref(),
            model,
            reasoning_effort,
            events,
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
        config_options.as_deref(),
        model,
        reasoning_effort,
        service_tier,
        context_window,
        events,
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

#[allow(clippy::too_many_arguments)]
async fn apply_model(
    connection: &ConnectionTo<Agent>,
    provider: ProviderKind,
    session_id: &SessionId,
    config_options: Option<&[SessionConfigOption]>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
    context_window: Option<&str>,
    events: &DriverEventSender,
) {
    let Some(model) = model else {
        return;
    };
    let cursor_model_option = (provider == ProviderKind::Cursor)
        .then_some(config_options)
        .flatten()
        .and_then(|options| find_config_option(options, SessionConfigOptionCategory::Model));
    if let Some(option) = cursor_model_option
        && let Some(selection) = cursor_model_selection(option, model)
    {
        match connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id.clone(),
                selection.value.as_str(),
            ))
            .block_task()
            .await
        {
            Ok(response) => {
                if let Err(error) = apply_cursor_variant_configs(
                    connection,
                    session_id,
                    response.config_options,
                    &selection,
                    reasoning_effort,
                    service_tier,
                    context_window,
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

    if provider == ProviderKind::Fx {
        let mut options = config_options.unwrap_or_default().to_vec();
        if let Some((provider_option, value)) = fx_model_provider_switch(&options, model) {
            let config_id = provider_option.id.clone();
            match connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    config_id,
                    value,
                ))
                .block_task()
                .await
            {
                Ok(response) => options = response.config_options,
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
        if let Err(error) = connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id.clone(),
                model,
            ))
            .block_task()
            .await
        {
            let _ = events.send(DriverEvent::Error(tr!(
                "errors.select_model",
                error = error
            )));
        }
        return;
    }

    // Grok, Kimi, and Cursor agents that do not advertise a model config
    // option retain the legacy request unchanged. Fx intentionally
    // stays on session/set_config_option, its documented model API.
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
    if let Err(error) = connection.send_request(request).block_task().await {
        let _ = events.send(DriverEvent::Error(tr!(
            "errors.select_model",
            error = error
        )));
        return;
    }
    if provider != ProviderKind::Grok
        && let Some(effort) = reasoning_effort
    {
        // Reasoning effort is an optional config extension and is deliberately
        // non-fatal when an agent does not expose it.
        let _ = connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                reasoning_effort_config_id(provider),
                effort,
            ))
            .block_task()
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
fn send_prompt(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    text: String,
    prompt_requests: &PendingPromptRequests,
    events: &DriverEventSender,
    provider: ProviderKind,
    native_session_id: &str,
    grok_title_home: Option<std::path::PathBuf>,
    title_refresh: super::title_refresh::NativeTitleRefresh,
    stream_state: Arc<Mutex<AcpStreamState>>,
) -> agent_client_protocol::Result<()> {
    stream_state.lock().produced_content = false;
    // Read before the turn runs, so the failure lookup cannot mistake an
    // earlier turn's record for this one's.
    let wire_offset = (provider == ProviderKind::Kimi)
        .then(|| crate::kimi_session::wire_offset(native_session_id));
    let extension_id =
        (provider == ProviderKind::Grok).then(|| format!("waku-{}", uuid::Uuid::new_v4()));
    let mut request = PromptRequest::new(
        session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(text))],
    );
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
        if settle_prompt_request(&callback_requests, &callback_request_id) {
            // Only an empty turn pays for this lookup, so a healthy turn never
            // waits on Kimi's records.
            let native_failure = wire_offset
                .filter(|_| !stream_state.lock().produced_content)
                .and_then(|offset| crate::kimi_session::turn_failure(&native_session_id, offset));
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

fn settle_prompt_request(prompt_requests: &Mutex<PendingPrompts>, request_id: &RequestId) -> bool {
    prompt_requests.lock().settle_request(request_id)
}

fn finish_xai_prompt_complete(
    params: &Value,
    prompt_requests: &Mutex<PendingPrompts>,
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
    for (_, responder) in pending.lock().drain() {
        let _ = responder.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
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

/// `allow_once_only` is Droid's: its "always" answer raises the session's
/// autonomy level, so it is neither chosen automatically nor offered, and the
/// access mode stays the one control over what runs unasked.
fn handle_permission_request(
    request: RequestPermissionRequest,
    responder: PermissionResponder,
    auto_approve: bool,
    allow_once_only: bool,
    pending: &PendingPermissions,
    events: &impl DriverEventSink,
) -> agent_client_protocol::Result<()> {
    let request_id = responder.id().to_string();
    let params = serde_json::to_value(&request)?;
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
    pending.lock().insert(request_id.clone(), responder);
    if events
        .send(DriverEvent::Permission {
            request_id: request_id.clone(),
            title,
            detail,
            options,
        })
        .is_err()
        && let Some(responder) = pending.lock().remove(&request_id)
    {
        let _ = responder.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
    }
    Ok(())
}

fn handle_session_update(
    provider: ProviderKind,
    notification: SessionNotification,
    events: &impl DriverEventSink,
    state: &mut AcpStreamState,
) -> agent_client_protocol::Result<()> {
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
            let steps = plan_steps_text(&update);
            let item = activity::tool_activity(
                Some("acp-plan".into()),
                ActivityKind::Plan,
                tr!("activity.plan_updated"),
                None,
                steps.as_ref(),
                None,
                false,
                true,
            );
            let _ = events.send(DriverEvent::RichActivity(item));
        }
        Some("available_commands_update") => {
            let commands = update
                .get("availableCommands")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|command| {
                            let name = command.get("name").and_then(Value::as_str)?;
                            Some(crate::model::ReportedCommand {
                                name: name.to_owned(),
                                description: command
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !commands.is_empty() {
                let _ = events.send(DriverEvent::AvailableCommands(commands));
            }
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
struct StoredToolCall {
    kind: ActivityKind,
    title: String,
    raw_input: Option<Value>,
    tool_name: Option<String>,
}

#[derive(Default)]
struct AcpStreamState {
    tools: HashMap<String, StoredToolCall>,
    /// Whether the running turn has produced anything visible. A turn that
    /// ends having produced nothing is the shape a swallowed provider error
    /// takes, which is what makes a native failure worth looking up.
    produced_content: bool,
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

fn tool_activity(update: &Value, events: &impl DriverEventSink, state: &mut AcpStreamState) {
    let id = update
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let status = update
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending");
    let complete = matches!(status, "completed" | "failed");
    let failed = status == "failed";

    let wire_kind = update.get("kind").and_then(Value::as_str);
    let wire_title = update.get("title").and_then(Value::as_str);
    let stored = id.as_ref().and_then(|id| {
        if complete {
            state.tools.remove(id)
        } else {
            state.tools.get(id).cloned()
        }
    });
    let mut kind = wire_kind
        .map(classify)
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
    let arguments = update
        .get("rawInput")
        .filter(|value| !value.is_null())
        .or_else(|| stored.as_ref().and_then(|tool| tool.raw_input.as_ref()));
    let title = activity::input_title(arguments)
        .or_else(|| {
            wire_title
                .filter(|title| !title.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| stored.as_ref().map(|tool| tool.title.clone()))
        .unwrap_or_else(|| "Tool".to_owned());
    let tool_name = arguments
        .and_then(|input| input.get("tool_name"))
        .and_then(Value::as_str)
        .or_else(|| wire_title.filter(|title| title.starts_with("mcp__")))
        .or_else(|| stored.as_ref().and_then(|tool| tool.tool_name.as_deref()))
        .map(str::to_owned);
    if !complete && let Some(id) = id.as_ref() {
        state.tools.insert(
            id.clone(),
            StoredToolCall {
                kind,
                title: title.clone(),
                raw_input: arguments.cloned(),
                tool_name: tool_name.clone(),
            },
        );
    }

    let output = update
        .get("content")
        .filter(|value| !value.is_null())
        .or_else(|| update.get("rawOutput").filter(|value| !value.is_null()));
    let item =
        activity::tool_activity(id, kind, title, arguments, output, output, failed, complete)
            .with_tool_name(tool_name.as_deref());
    let _ = events.send(DriverEvent::RichActivity(item));
}

fn classify(kind: &str) -> ActivityKind {
    match kind {
        "execute" => ActivityKind::Command,
        "edit" | "delete" | "move" => ActivityKind::FileChange,
        "read" => ActivityKind::FileRead,
        "search" | "fetch" => ActivityKind::Search,
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
        let launch = launch_for(ProviderKind::Fx, None).unwrap();
        assert_eq!(launch.args, ["acp"]);
        assert!(launch.env.is_empty());
    }

    #[test]
    fn droid_launches_the_registry_acp_daemon() {
        let launch = launch_for(ProviderKind::Droid, None).unwrap();
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
        let (events, event_rx) = crate::driver::test_event_channel();

        assert_eq!(
            finish_xai_prompt_complete(
                &json!({
                    "sessionId": "grok-session",
                    "promptId": "waku-prompt",
                    "stopReason": "end_turn"
                }),
                &requests,
                &events,
            ),
            Some("grok-session".into())
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
        let launch = launch_for(ProviderKind::Grok, Some("xhigh")).unwrap();
        assert_eq!(
            launch.args,
            ["agent", "--reasoning-effort", "xhigh", "stdio"]
        );
        let bare = launch_for(ProviderKind::Grok, None).unwrap();
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
}
