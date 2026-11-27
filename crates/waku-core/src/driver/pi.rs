//! Pi RPC transport, shared by Pi and Oh My Pi.
//!
//! Oh My Pi is a fork of Pi that kept the newline-delimited RPC transport but
//! renamed part of the surface: forking is `branch`, a run settles on
//! `agent_end` instead of `agent_settled`, and oversized frames are chunked
//! once protocol v2 is negotiated. [`PiFlavor`] carries those differences so
//! both providers share one transport instead of two near-copies.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Sender, bounded, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{activity, computer_use as computer_use_runtime};
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{ActivityKind, DriverEvent, ProviderResumeCursor, ReportedCommand, RuntimeMode};

const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// The handshake races the agent's own startup, so it needs more headroom than
/// a request against the already-running process. Pi loads extensions and
/// resources and, when model networking is on, refreshes its model catalog
/// before it reads stdin — it budgets 15 s for that refresh alone — and a
/// timed-out `get_state` there fails the whole session.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// Oh My Pi has to start a whole second agent to clone a session, so it needs
/// more headroom than a request against the already-running process.
const CLONE_TIMEOUT: Duration = Duration::from_secs(30);

/// Oh My Pi refuses to reassemble beyond this, so neither should Waku.
const MAX_REASSEMBLED_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Which dialect of the Pi RPC protocol a session speaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiFlavor {
    Pi,
    OhMyPi,
}

impl PiFlavor {
    fn display_name(self) -> &'static str {
        match self {
            Self::Pi => "Pi",
            Self::OhMyPi => "Oh My Pi",
        }
    }

    /// Pi has no permission system and only needs project-local files trusted;
    /// Oh My Pi does have one, and Waku only ever runs these in Full access.
    fn full_access_arg(self) -> &'static str {
        match self {
            Self::Pi => "--approve",
            Self::OhMyPi => "--yolo",
        }
    }

    /// The event that means the run is over and no more work is scheduled.
    fn settled_event(self) -> &'static str {
        match self {
            Self::Pi => "agent_settled",
            Self::OhMyPi => "agent_end",
        }
    }

    fn session_info_event(self) -> &'static str {
        match self {
            Self::Pi => "session_info_changed",
            Self::OhMyPi => "session_info_update",
        }
    }

    fn session_info_title_field(self) -> &'static str {
        match self {
            Self::Pi => "name",
            Self::OhMyPi => "title",
        }
    }

    fn branch_messages_command(self) -> &'static str {
        match self {
            Self::Pi => "get_fork_messages",
            Self::OhMyPi => "get_branch_messages",
        }
    }

    fn branch_command(self) -> &'static str {
        match self {
            Self::Pi => "fork",
            Self::OhMyPi => "branch",
        }
    }

    /// Oh My Pi dropped Pi's opt-out env var; it gates its update check on a
    /// setting instead, and does it off the startup path either way.
    fn skips_version_check_by_env(self) -> bool {
        matches!(self, Self::Pi)
    }

    /// Only Oh My Pi chunks oversized frames, and only after it is asked to.
    fn negotiates_protocol_v2(self) -> bool {
        matches!(self, Self::OhMyPi)
    }

    /// Waku's computer-use bridge is a Pi extension written against Pi's
    /// extension API. Oh My Pi ships its own `/computer` instead.
    fn supports_waku_computer_use(self) -> bool {
        matches!(self, Self::Pi)
    }

    fn cursor(self, session_id: String, session_file: Option<PathBuf>) -> ProviderResumeCursor {
        match self {
            Self::Pi => ProviderResumeCursor::Pi {
                session_id,
                session_file,
            },
            Self::OhMyPi => ProviderResumeCursor::OhMyPi {
                session_id,
                session_file,
            },
        }
    }

    fn session_file_from_cursor(self, cursor: &ProviderResumeCursor) -> Option<&PathBuf> {
        match (self, cursor) {
            (Self::Pi, ProviderResumeCursor::Pi { session_file, .. })
            | (Self::OhMyPi, ProviderResumeCursor::OhMyPi { session_file, .. }) => {
                session_file.as_ref()
            }
            _ => None,
        }
    }

    fn owns_cursor(self, cursor: &ProviderResumeCursor) -> bool {
        matches!(
            (self, cursor),
            (Self::Pi, ProviderResumeCursor::Pi { .. })
                | (Self::OhMyPi, ProviderResumeCursor::OhMyPi { .. })
        )
    }
}

enum CommandMessage {
    Prompt(String),
    Steer(String),
    Cancel,
    CancelExtensionRequest(String),
    Options(SessionOptions),
    Rollback {
        turns: usize,
        response: Sender<Result<ProviderResumeCursor, String>>,
    },
    Fork {
        turns_to_remove: usize,
        response: Sender<Result<ProviderResumeCursor, String>>,
    },
    Shutdown,
}

enum PendingResponse {
    Request(Sender<Result<Value, String>>),
    Prompt,
}

type PendingResponses = Arc<Mutex<HashMap<String, PendingResponse>>>;

struct PiProcessState {
    pid: u32,
    #[cfg(windows)]
    job: Option<crate::process_tree::KillOnCloseJob>,
}

struct PiProcessHandle {
    state: Mutex<PiProcessState>,
    shutting_down: AtomicBool,
}

impl PiProcessHandle {
    fn new(pid: u32, #[cfg(windows)] job: Option<crate::process_tree::KillOnCloseJob>) -> Self {
        Self {
            state: Mutex::new(PiProcessState {
                pid,
                #[cfg(windows)]
                job,
            }),
            shutting_down: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    fn detached() -> Self {
        Self {
            state: Mutex::new(PiProcessState {
                pid: 0,
                #[cfg(windows)]
                job: None,
            }),
            shutting_down: AtomicBool::new(false),
        }
    }

    fn terminate(&self) {
        self.terminate_with_signal(false);
    }

    fn force_terminate(&self) {
        self.terminate_with_signal(true);
    }

    fn terminate_with_signal(&self, force: bool) {
        self.shutting_down.store(true, Ordering::Release);
        let mut state = self.state.lock();
        if state.pid == 0 {
            return;
        }
        let pid = state.pid;
        #[cfg(windows)]
        {
            let _ = force;
            drop(state.job.take());
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
        #[cfg(unix)]
        {
            let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
            unsafe {
                let _ = libc::kill(-(pid as i32), signal);
                let _ = libc::kill(pid as i32, signal);
            }
        }
    }

    fn mark_exited(&self) {
        let mut state = self.state.lock();
        state.pid = 0;
        #[cfg(windows)]
        drop(state.job.take());
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }
}

/// Guards a short-lived child process (such as `omp --fork`) with a Windows
/// kill-on-close job and Unix process group cleanup on drop.
struct ScopedChildProcess {
    child: Child,
    pid: u32,
    #[cfg(windows)]
    job: Option<crate::process_tree::KillOnCloseJob>,
}

impl ScopedChildProcess {
    fn adopt(child: Child) -> Self {
        let pid = child.id();
        #[cfg(windows)]
        let job = crate::process_tree::KillOnCloseJob::adopt(pid).ok();
        Self {
            child,
            pid,
            #[cfg(windows)]
            job,
        }
    }
}

impl Drop for ScopedChildProcess {
    fn drop(&mut self) {
        #[cfg(windows)]
        drop(self.job.take());
        #[cfg(unix)]
        if self.pid != 0 {
            unsafe {
                let _ = libc::kill(-(self.pid as i32), libc::SIGKILL);
                let _ = libc::kill(self.pid as i32, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        self.pid = 0;
        let _ = self.child.wait();
    }
}

pub struct PiDriver {
    flavor: PiFlavor,
    commands: Sender<CommandMessage>,
    process: Arc<PiProcessHandle>,
    computer_use: Option<computer_use_runtime::ComputerUseRuntime>,
}

fn configure_pi_computer_use_command(
    command: &mut std::process::Command,
    config: Option<(&computer_use_runtime::ComputerUseConfig, &Path)>,
) {
    if let Some((config, extension)) = config {
        command
            .arg("--extension")
            .arg(extension)
            .arg("--skill")
            .arg(&config.skill_path)
            .env("WAKU_JS_REPL_SERVER", &config.repl_path)
            .env("WAKU_COMPUTER_USE_SERVER", &config.server_path)
            .env(
                "WAKU_COMPUTER_USE_PROCESS_DIRECTORY",
                &config.process_directory,
            );
    }
}

impl PiDriver {
    pub fn start(
        flavor: PiFlavor,
        options: DriverStartOptions,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            model,
            reasoning_effort,
            service_tier: _,
            context_window: _,
            agent_preset: _,
            computer_use_enabled,
            provider_cursor,
            config_dir: _,
        } = options;
        if mode != RuntimeMode::FullAccess {
            return Err(anyhow!(
                "{} currently supports Full access only",
                flavor.display_name()
            ));
        }
        let resume_session_file = match provider_cursor {
            Some(cursor) if flavor.owns_cursor(&cursor) => {
                let Some(session_file) = flavor.session_file_from_cursor(&cursor).cloned() else {
                    return Err(anyhow!(
                        "cannot resume {} because its native session file is missing",
                        flavor.display_name()
                    ));
                };
                Some(session_file)
            }
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume {} from a {} cursor",
                    flavor.display_name(),
                    cursor.provider().display_name()
                ));
            }
            None => None,
        };
        if let Some(model) = model.as_deref() {
            parse_model_slug(model)?;
        }
        let computer_use_enabled = crate::computer_use::resolve_enabled(computer_use_enabled);

        let computer_use = (computer_use_enabled && flavor.supports_waku_computer_use())
            .then(|| computer_use_runtime::ComputerUseRuntime::start(events.clone()))
            .transpose()?;
        let pi_extension = computer_use
            .as_ref()
            .map(|_| crate::computer_use::pi_extension_path())
            .transpose()?;
        let mut command = crate::command_env::command(&binary);
        command.args(["--mode", "rpc", flavor.full_access_arg()]);
        if flavor.skips_version_check_by_env() {
            command.env("PI_SKIP_VERSION_CHECK", "1");
        }
        configure_pi_computer_use_command(
            &mut command,
            computer_use
                .as_ref()
                .zip(pi_extension.as_deref())
                .map(|(runtime, extension)| (&runtime.config, extension)),
        );
        let command = command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        struct PiStartupChildGuard {
            child: Option<Child>,
            process: Arc<PiProcessHandle>,
        }
        impl Drop for PiStartupChildGuard {
            fn drop(&mut self) {
                if let Some(mut child) = self.child.take() {
                    self.process.force_terminate();
                    let _ = child.kill();
                    let _ = child.wait();
                    self.process.mark_exited();
                }
            }
        }

        let child = crate::command_env::spawn(command)
            .with_context(|| format!("failed to start `{} --mode rpc`", binary.display()))?;
        let child_pid = child.id();
        #[cfg(windows)]
        let job = crate::process_tree::KillOnCloseJob::adopt(child_pid).ok();
        let process = Arc::new(PiProcessHandle::new(
            child_pid,
            #[cfg(windows)]
            job,
        ));
        let mut child_guard = PiStartupChildGuard {
            child: Some(child),
            process: process.clone(),
        };
        let Some(stdin) = child_guard.child.as_mut().and_then(|c| c.stdin.take()) else {
            return Err(anyhow!("{} stdin unavailable", flavor.display_name()));
        };
        let Some(stdout) = child_guard.child.as_mut().and_then(|c| c.stdout.take()) else {
            return Err(anyhow!("{} stdout unavailable", flavor.display_name()));
        };
        let Some(stderr) = child_guard.child.as_mut().and_then(|c| c.stderr.take()) else {
            return Err(anyhow!("{} stderr unavailable", flavor.display_name()));
        };

        let (commands, command_rx) = unbounded();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let turn_active = Arc::new(AtomicBool::new(false));
        let reader_pending = pending.clone();
        let reader_commands = commands.clone();
        let reader_events = events.clone();
        let reader_turn_active = turn_active.clone();
        let reader_thread =
            thread::Builder::new()
                .name("waku-pi-reader".into())
                .spawn(move || {
                    let mut stream_state = PiStreamState {
                        turn_active: reader_turn_active,
                        ..PiStreamState::default()
                    };
                    let mut chunks = ChunkAssembly::default();
                    for line in BufReader::new(stdout).lines() {
                        match line {
                            Ok(line) if !line.trim().is_empty() => {
                                match serde_json::from_str::<Value>(&line) {
                                    Ok(value) => {
                                        // A chunked frame arrives as an
                                        // uninterrupted run of `rpc_chunk`
                                        // envelopes that reassemble into one
                                        // logical message.
                                        match chunks.accept(value) {
                                            Ok(Some(value)) => handle_pi_message(
                                                flavor,
                                                value,
                                                &reader_pending,
                                                &reader_commands,
                                                &reader_events,
                                                &mut stream_state,
                                            ),
                                            Ok(None) => {}
                                            Err(error) => {
                                                let _ =
                                                    reader_events.send(DriverEvent::Error(tr!(
                                                        "errors.provider_transport_read",
                                                        provider = flavor.display_name(),
                                                        error = error
                                                    )));
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        let _ = reader_events.send(DriverEvent::Error(tr!(
                                            "errors.provider_invalid_json",
                                            provider = flavor.display_name(),
                                            error = error
                                        )));
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                let _ = reader_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_transport_read",
                                    provider = flavor.display_name(),
                                    error = error
                                )));
                                break;
                            }
                        }
                    }
                    // Unblock anything waiting on an RPC reply immediately; the
                    // process thread owns the `ProcessExited` announcement so a
                    // non-zero exit can be reported before the runtime is torn down.
                    fail_pending(
                        &reader_pending,
                        &format!("{} RPC process exited", flavor.display_name()),
                    );
                })?;

        let writer_pending = pending;
        let writer_events = events.clone();
        let writer_process = process.clone();
        let writer_turn_active = turn_active;
        thread::Builder::new()
            .name("waku-pi-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                let mut next_request_id = 0_u64;
                let initialize = (|| -> Result<Value, String> {
                    // The agent does not answer until it has finished loading,
                    // so every handshake request gets the startup timeout.
                    // Negotiate before anything else so a large first response
                    // arrives chunked rather than shrunk to an error frame.
                    if flavor.negotiates_protocol_v2() {
                        send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({"type": "negotiate_protocol", "protocolVersion": 2}),
                            INITIALIZE_TIMEOUT,
                        )?;
                    }
                    let _ = send_request_with_timeout(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_state"}),
                        INITIALIZE_TIMEOUT,
                    )?;
                    if let Some(session_file) = resume_session_file {
                        let response = send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({
                                "type": "switch_session",
                                "sessionPath": session_file
                            }),
                            INITIALIZE_TIMEOUT,
                        )?;
                        if response.pointer("/data/cancelled").and_then(Value::as_bool)
                            == Some(true)
                        {
                            return Err(format!(
                                "{} session switch was cancelled",
                                flavor.display_name()
                            ));
                        }
                    }
                    if let Some(model) = model.as_deref() {
                        let (provider, model_id) =
                            parse_model_slug(model).map_err(|error| error.to_string())?;
                        let _ = send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({
                                "type": "set_model",
                                "provider": provider,
                                "modelId": model_id
                            }),
                            INITIALIZE_TIMEOUT,
                        )?;
                    }
                    if let Some(level) = reasoning_effort.as_deref() {
                        let _ = send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({"type": "set_thinking_level", "level": level}),
                            INITIALIZE_TIMEOUT,
                        )?;
                    }
                    send_request_with_timeout(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_state"}),
                        INITIALIZE_TIMEOUT,
                    )
                })();

                let state = match initialize {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = writer_events.send(DriverEvent::Error(tr!(
                            "errors.initialize_provider",
                            provider = flavor.display_name(),
                            error = error
                        )));
                        let _ = writer_events.send(DriverEvent::TurnFinished {
                            success: false,
                            summary: Some(tr!(
                                "errors.provider_initialize_session",
                                provider = flavor.display_name()
                            )),
                        });
                        writer_process.terminate();
                        return;
                    }
                };
                let Some(mut cursor) = cursor_from_state(flavor, &state) else {
                    let _ = writer_events.send(DriverEvent::Error(tr!(
                        "errors.provider_no_session_id",
                        provider = flavor.display_name()
                    )));
                    let _ = writer_events.send(DriverEvent::TurnFinished {
                        success: false,
                        summary: Some(tr!(
                            "errors.provider_initialize_session",
                            provider = flavor.display_name()
                        )),
                    });
                    writer_process.terminate();
                    return;
                };
                let initial_usage = send_request(
                    &mut stdin,
                    &writer_pending,
                    &mut next_request_id,
                    json!({"type": "get_session_stats"}),
                )
                .ok()
                .and_then(|stats| pi_context_usage(&state, Some(&stats)))
                .or_else(|| pi_context_usage(&state, None));
                let _ = writer_events.send(DriverEvent::Connected {
                    provider_cursor: Some(cursor.clone()),
                });
                if let Some((context_tokens, context_window)) = initial_usage {
                    let _ = writer_events.send(DriverEvent::UsageUpdated {
                        context_tokens,
                        context_window,
                    });
                }
                if let Some(title) = state
                    .pointer("/data/sessionName")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                {
                    let _ =
                        writer_events.send(DriverEvent::AutoTitleUpdated(Some(title.to_owned())));
                }

                // Pi reports extension commands, prompts and skills on request;
                // Oh My Pi pushes its registry at startup and after reloads.
                if flavor == PiFlavor::Pi
                    && let Ok(catalog) = send_request(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_commands"}),
                    )
                {
                    publish_commands(
                        crate::slash_command_catalog::parse_pi_commands(&catalog),
                        &writer_events,
                    );
                }

                // Both flavors expose setters for these, so changing either is
                // an RPC on the live session rather than a restart.
                let mut current_model = model;
                let mut current_effort = reasoning_effort;
                while let Ok(message) = command_rx.recv() {
                    match message {
                        CommandMessage::Prompt(prompt) => {
                            let result = send_prompt(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &prompt,
                            );
                            if let Err(error) = result {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_rejected_prompt_detail",
                                    provider = flavor.display_name(),
                                    error = error
                                )));
                                let _ = writer_events.send(DriverEvent::TurnFinished {
                                    success: false,
                                    summary: Some(tr!(
                                        "errors.provider_rejected_prompt",
                                        provider = flavor.display_name()
                                    )),
                                });
                            }
                        }
                        CommandMessage::Steer(prompt) => {
                            if !writer_turn_active.load(Ordering::Acquire) {
                                let _ = writer_events.send(DriverEvent::SteerRejected {
                                    message: prompt,
                                    reason: tr!(
                                        "errors.provider_no_active_turn",
                                        provider = flavor.display_name()
                                    ),
                                });
                                continue;
                            }
                            let result = send_request(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                json!({"type": "steer", "message": prompt}),
                            );
                            match result {
                                Ok(_) => {
                                    let _ = writer_events
                                        .send(DriverEvent::steer_accepted(prompt));
                                }
                                Err(error) => {
                                    let _ = writer_events.send(DriverEvent::SteerRejected {
                                        message: prompt,
                                        reason: error,
                                    });
                                }
                            }
                        }
                        CommandMessage::Cancel => {
                            if let Err(error) = send_request(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                json!({"type": "abort"}),
                            ) {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.stop_provider",
                                    provider = flavor.display_name(),
                                    error = error
                                )));
                                // If the RPC channel cannot honor abort (e.g. a
                                // wedged tool subprocess), tear down the whole
                                // process tree rather than leaving it hung.
                                writer_process.force_terminate();
                                break;
                            }
                        }
                        CommandMessage::Options(options) => {
                            apply_live_options(
                                flavor,
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                options,
                                &mut current_model,
                                &mut current_effort,
                                &writer_events,
                            );
                        }
                        CommandMessage::CancelExtensionRequest(id) => {
                            if write_json_line(
                                &mut stdin,
                                &json!({
                                    "type": "extension_ui_response",
                                    "id": id,
                                    "cancelled": true
                                }),
                            )
                            .is_err()
                            {
                                break;
                            }
                        }
                        CommandMessage::Rollback { turns, response } => {
                            match fork_pi_session(
                                flavor,
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &binary,
                                &cwd,
                                &cursor,
                                turns,
                                false,
                            ) {
                                Ok(next_cursor) => {
                                    cursor = next_cursor.clone();
                                    let _ = response.send(Ok(next_cursor));
                                }
                                Err(ForkError::Recoverable(error)) => {
                                    let _ = response.send(Err(error));
                                }
                                Err(ForkError::SessionDesynced(error)) => {
                                    let _ = response.send(Err(error.clone()));
                                    let _ = writer_events.send(DriverEvent::Error(error));
                                    writer_process.terminate();
                                    break;
                                }
                            }
                        }
                        CommandMessage::Fork {
                            turns_to_remove,
                            response,
                        } => {
                            match fork_pi_session(
                                flavor,
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &binary,
                                &cwd,
                                &cursor,
                                turns_to_remove,
                                true,
                            ) {
                                Ok(fork_cursor) => {
                                    let _ = response.send(Ok(fork_cursor));
                                }
                                Err(ForkError::Recoverable(error)) => {
                                    let _ = response.send(Err(error));
                                }
                                Err(ForkError::SessionDesynced(error)) => {
                                    // The live process already switched to the
                                    // forked session and could not return to
                                    // the source session; shut it down so no
                                    // subsequent prompt mutates the fork.
                                    let _ = response.send(Err(error.clone()));
                                    let _ = writer_events.send(DriverEvent::Error(error));
                                    writer_process.terminate();
                                    break;
                                }
                            }
                        }
                        CommandMessage::Shutdown => break,
                    }
                }
            })?;

        let last_visible_stderr = Arc::new(Mutex::new(None::<String>));
        let stderr_last_error = last_visible_stderr.clone();
        let stderr_events = events.clone();
        let stderr_thread =
            thread::Builder::new()
                .name("waku-pi-stderr".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if let Some(diagnostic) = pi_stderr_error_line(&line) {
                            let error = format!("{}: {diagnostic}", flavor.display_name());
                            *stderr_last_error.lock() = Some(error.clone());
                            let _ = stderr_events.send(DriverEvent::Error(error));
                        }
                    }
                })?;

        // Reap the process, release its Windows job / PID guard before dropping
        // `child` to prevent PID-reuse kills, and suppress expected shutdown
        // exit codes.
        let process_handle = process.clone();
        let mut child = child_guard
            .child
            .take()
            .expect("startup child guard owns child");
        thread::Builder::new()
            .name("waku-pi-process".into())
            .spawn(move || {
                let status = child.wait();
                process_handle.mark_exited();
                drop(child);
                let _ = reader_thread.join();
                let _ = stderr_thread.join();
                let shutting_down = process_handle.is_shutting_down();
                match status {
                    Ok(status)
                        if !shutting_down
                            && !status.success()
                            && last_visible_stderr.lock().is_none() =>
                    {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.provider_rpc_exited",
                            provider = flavor.display_name(),
                            status = status
                        )));
                    }
                    Err(error) if !shutting_down => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.read_provider_exit_status",
                            provider = format!("{} RPC", flavor.display_name()),
                            error = error
                        )));
                    }
                    _ => {}
                }
                let _ = events.send(DriverEvent::ProcessExited);
            })?;

        Ok(Self {
            flavor,
            commands,
            process,
            computer_use,
        })
    }
}

impl DriverControl for PiDriver {
    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    fn supports_steer(&self) -> bool {
        true
    }

    fn steer(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Steer(prompt));
    }

    fn cancel(&self) {
        let _ = self.commands.send(CommandMessage::Cancel);
    }

    fn cancel_computer_use(&self) {
        if let Some(computer_use) = self.computer_use.as_ref() {
            computer_use.stop();
        }
    }

    fn respond(&self, _request_id: String, _option_id: String) {}

    fn apply_options(&self, options: SessionOptions) -> bool {
        // Both flavors have setters for the model and thinking level, so those
        // apply to the live session. Neither exposes one for permissions — and
        // Waku only runs them with Full access anyway, so a mode change asks
        // for a fresh start, which is where that is reported.
        if options.mode != RuntimeMode::FullAccess {
            return false;
        }
        self.commands.send(CommandMessage::Options(options)).is_ok()
    }

    fn rollback(&self, turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        if turns == 0 {
            return Ok(None);
        }
        let (response_tx, response_rx) = bounded(1);
        self.commands
            .send(CommandMessage::Rollback {
                turns,
                response: response_tx,
            })
            .with_context(|| {
                format!(
                    "{} driver stopped before rollback",
                    self.flavor.display_name()
                )
            })?;
        response_rx
            .recv_timeout(Duration::from_secs(60))
            .with_context(|| {
                format!(
                    "timed out waiting for {} conversation rollback",
                    self.flavor.display_name()
                )
            })?
            .map(Some)
            .map_err(anyhow::Error::msg)
    }

    fn fork(&self, turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
        let (response_tx, response_rx) = bounded(1);
        self.commands
            .send(CommandMessage::Fork {
                turns_to_remove,
                response: response_tx,
            })
            .with_context(|| {
                format!(
                    "{} driver stopped before forking",
                    self.flavor.display_name()
                )
            })?;
        response_rx
            .recv_timeout(Duration::from_secs(60))
            .with_context(|| {
                format!(
                    "timed out waiting for {} conversation fork",
                    self.flavor.display_name()
                )
            })?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for PiDriver {
    fn drop(&mut self) {
        self.cancel_computer_use();
        let _ = self.commands.send(CommandMessage::Shutdown);
        self.process.terminate();
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_live_options(
    flavor: PiFlavor,
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    options: SessionOptions,
    current_model: &mut Option<String>,
    current_effort: &mut Option<String>,
    events: &impl DriverEventSink,
) {
    if options.model != *current_model {
        match options.model.as_deref().map(parse_model_slug).transpose() {
            Ok(Some((provider, model_id))) => {
                match send_request(
                    stdin,
                    pending,
                    next_request_id,
                    json!({
                        "type": "set_model",
                        "provider": provider,
                        "modelId": model_id
                    }),
                ) {
                    Ok(response) => {
                        if let Some(window) = response
                            .pointer("/data/contextWindow")
                            .and_then(Value::as_u64)
                            .filter(|window| *window > 0)
                        {
                            let _ = events.send(DriverEvent::UsageUpdated {
                                context_tokens: None,
                                context_window: Some(window),
                            });
                        }
                        *current_model = options.model;
                    }
                    Err(error) => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.switch_provider_model",
                            provider = flavor.display_name(),
                            error = error
                        )));
                    }
                }
            }
            Ok(None) => {
                *current_model = None;
            }
            Err(error) => {
                let _ = events.send(DriverEvent::Error(error.to_string()));
            }
        }
    }
    if options.reasoning_effort != *current_effort {
        match options.reasoning_effort.as_deref() {
            Some(level) => match send_request(
                stdin,
                pending,
                next_request_id,
                json!({"type": "set_thinking_level", "level": level}),
            ) {
                Ok(_) => {
                    *current_effort = options.reasoning_effort;
                }
                Err(error) => {
                    let _ = events.send(DriverEvent::Error(tr!(
                        "errors.change_provider_thinking",
                        provider = flavor.display_name(),
                        error = error
                    )));
                }
            },
            None => {
                *current_effort = None;
            }
        }
    }
}

fn strip_ansi_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if matches!(chars.peek(), Some('[')) {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn is_named_error_token(token: &str) -> bool {
    let base = token.strip_suffix(':').unwrap_or(token);
    let Some(prefix) = base.strip_suffix("error") else {
        return false;
    };
    !prefix.is_empty()
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Filters stderr lines from Pi / Oh My Pi so genuine runtime/CLI errors still
/// surface to the user while benign diagnostics mentioning `"error"` (such as
/// extension filenames, warnings, info/debug logs, or stack frames) do not spam
/// false `DriverEvent::Error` banners.
fn pi_stderr_error_line(raw: &str) -> Option<String> {
    const MAX_STDERR_ERROR_CHARS: usize = 400;

    let cleaned = strip_ansi_sequences(raw);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed.starts_with("at ") {
        return None;
    }

    if trimmed.starts_with('{')
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && value.is_object()
    {
        let is_error_level = value.get("level").is_some_and(|level| {
            level
                .as_str()
                .is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "error" | "fatal"))
                || level.as_u64().is_some_and(|n| n >= 50)
        });
        if !is_error_level {
            return None;
        }
        let message = value
            .get("msg")
            .or_else(|| value.get("message"))
            .or_else(|| value.get("error"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|msg| !msg.is_empty())
            .unwrap_or(trimmed);
        return Some(truncate_stderr_message(message, MAX_STDERR_ERROR_CHARS));
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("warning:")
        || lower.starts_with("warn:")
        || lower.starts_with("[warn]")
        || lower.starts_with("[warning]")
        || lower.starts_with("info:")
        || lower.starts_with("[info]")
        || lower.starts_with("debug:")
        || lower.starts_with("[debug]")
        || lower.starts_with("trace:")
        || lower.starts_with("[trace]")
        || lower.starts_with("note:")
        || lower.starts_with("[note]")
        || lower.starts_with("hint:")
        || lower.starts_with("deprecationwarning:")
        || lower.starts_with("experimentalwarning:")
        || ((lower.starts_with("(node:") || lower.starts_with("(bun:"))
            && lower.contains("warning"))
    {
        return None;
    }

    let after_prefix = strip_diagnostic_prefix(trimmed);
    let after_lower = after_prefix.to_ascii_lowercase();
    let is_structured_error = after_lower.starts_with("error:")
        || after_lower.starts_with("error [")
        || after_lower.starts_with("fatal:")
        || after_lower.starts_with("panic:")
        || after_lower.starts_with("panic(")
        || after_lower.starts_with("[error]")
        || after_lower.starts_with("[fatal]")
        || after_lower.starts_with("uncaught ")
        || after_lower.starts_with("unhandledpromiserejection")
        || after_lower.starts_with("unhandled rejection")
        || after_lower.starts_with("unhandled error")
        || after_prefix.starts_with("ERROR ")
        || after_prefix.starts_with("FATAL ")
        || after_lower.split_whitespace().next().is_some_and(|first| {
            first.ends_with("error:") && is_named_error_token(first)
                || (is_named_error_token(first)
                    && after_lower[first.len()..].trim_start().starts_with('['))
        });

    is_structured_error.then(|| truncate_stderr_message(trimmed, MAX_STDERR_ERROR_CHARS))
}

fn strip_diagnostic_prefix(line: &str) -> &str {
    let mut rest = line;
    // Strip one leading bracketed component tag (e.g. `[pi] `) or ISO timestamp
    // (e.g. `2026-01-01T00:00:00Z `), without stripping `[ERROR]` / `[FATAL]`.
    if rest.starts_with('[')
        && !rest.to_ascii_lowercase().starts_with("[error]")
        && !rest.to_ascii_lowercase().starts_with("[fatal]")
        && let Some(end) = rest.find(']')
    {
        rest = rest[end + 1..].trim_start();
    } else if let Some((first, tail)) = rest.split_once(char::is_whitespace)
        && first.len() >= 10
        && first.as_bytes()[0..4].iter().all(u8::is_ascii_digit)
        && first.as_bytes()[4] == b'-'
    {
        rest = tail.trim_start();
    }
    rest
}

fn truncate_stderr_message(message: &str, max_chars: usize) -> String {
    if message.chars().count() <= max_chars {
        return message.to_owned();
    }
    let mut truncated = message.chars().take(max_chars).collect::<String>();
    truncated.push('…');
    truncated
}

fn send_request(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    request: Value,
) -> Result<Value, String> {
    send_request_with_timeout(stdin, pending, next_request_id, request, RPC_TIMEOUT)
}

fn send_request_with_timeout(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    mut request: Value,
    timeout: Duration,
) -> Result<Value, String> {
    *next_request_id += 1;
    let id = format!("waku-{}", next_request_id);
    request["id"] = Value::String(id.clone());
    let (response_tx, response_rx) = bounded(1);
    pending
        .lock()
        .insert(id.clone(), PendingResponse::Request(response_tx));
    if let Err(error) = write_json_line(stdin, &request) {
        pending.lock().remove(&id);
        return Err(format!("transport write failed: {error}"));
    }
    match response_rx.recv_timeout(timeout) {
        Ok(response) => response,
        Err(_) => {
            pending.lock().remove(&id);
            Err(format!(
                "{} timed out after {}s",
                request["type"].as_str().unwrap_or("request"),
                timeout.as_secs()
            ))
        }
    }
}

fn send_prompt(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    prompt: &str,
) -> Result<(), String> {
    *next_request_id += 1;
    let id = format!("waku-{}", next_request_id);
    {
        let mut pending = pending.lock();
        // A response from an older prompt cannot settle the next turn.
        pending.retain(|_, response| matches!(response, PendingResponse::Request(_)));
        pending.insert(id.clone(), PendingResponse::Prompt);
    }
    // OMP built-ins can hold the prompt response until compaction or another
    // command finishes. Do not apply the short control-RPC timeout or block
    // the writer from sending abort while waiting for that response.
    if let Err(error) = write_json_line(
        stdin,
        &json!({"id": id, "type": "prompt", "message": prompt}),
    ) {
        pending.lock().remove(&id);
        return Err(format!("transport write failed: {error}"));
    }
    Ok(())
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn fail_pending(pending: &PendingResponses, message: &str) {
    for (_, response) in pending.lock().drain() {
        if let PendingResponse::Request(response) = response {
            let _ = response.send(Err(message.to_owned()));
        }
    }
}

fn parse_model_slug(model: &str) -> anyhow::Result<(&str, &str)> {
    let Some((provider, model_id)) = model.trim().split_once('/') else {
        return Err(anyhow!(
            "models must use provider/model format; received `{model}`"
        ));
    };
    if provider.is_empty() || model_id.is_empty() {
        return Err(anyhow!(
            "models must use provider/model format; received `{model}`"
        ));
    }
    Ok((provider, model_id))
}

fn cursor_from_state(flavor: PiFlavor, response: &Value) -> Option<ProviderResumeCursor> {
    let session_id = response
        .pointer("/data/sessionId")
        .and_then(Value::as_str)?;
    let session_file = response
        .pointer("/data/sessionFile")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    Some(flavor.cursor(session_id.to_owned(), session_file))
}

/// Reassembles the `rpc_chunk` runs Oh My Pi emits for frames over its 1 MiB
/// stdout ceiling. Without this a large tool result degrades to an error frame
/// and the activity row renders empty.
#[derive(Default)]
struct ChunkAssembly {
    active: Option<PendingChunks>,
}

struct PendingChunks {
    chunk_id: String,
    count: u64,
    next_index: u64,
    byte_length: usize,
    data: Vec<u8>,
}

impl ChunkAssembly {
    /// Returns the logical message to dispatch, or `None` while a chunked
    /// frame is still arriving.
    fn accept(&mut self, value: Value) -> Result<Option<Value>, String> {
        if value.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
            // The run must be uninterrupted, so anything else invalidates a
            // partial frame rather than silently splicing around it.
            if self.active.take().is_some() {
                return Err("chunked frame was interrupted".to_owned());
            }
            return Ok(Some(value));
        }
        let (chunk_id, index, count, byte_length, data) = (|| {
            Some((
                value.get("chunkId").and_then(Value::as_str)?,
                value.get("index").and_then(Value::as_u64)?,
                value.get("count").and_then(Value::as_u64)?,
                value.get("byteLength").and_then(Value::as_u64)?,
                value.get("data").and_then(Value::as_str)?,
            ))
        })()
        .ok_or_else(|| "chunk frame was malformed".to_owned())?;
        let byte_length = usize::try_from(byte_length)
            .map_err(|_| "chunked frame exceeds the reassembly limit".to_owned())?;
        if count == 0 || index >= count {
            self.active = None;
            return Err("chunk frame was malformed".to_owned());
        }
        if byte_length > MAX_REASSEMBLED_FRAME_BYTES {
            self.active = None;
            return Err("chunked frame exceeds the reassembly limit".to_owned());
        }
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|error| format!("chunk payload was not valid base64: {error}"))?;

        let pending = match self.active.take() {
            Some(pending)
                if pending.chunk_id == chunk_id
                    && pending.count == count
                    && pending.byte_length == byte_length
                    && pending.next_index == index =>
            {
                pending
            }
            Some(_) => {
                return Err("chunked frame was interrupted".to_owned());
            }
            None if index == 0 => PendingChunks {
                chunk_id: chunk_id.to_owned(),
                count,
                next_index: 0,
                byte_length,
                data: Vec::with_capacity(byte_length),
            },
            None => return Err("chunked frame started mid-sequence".to_owned()),
        };
        let mut pending = pending;
        pending.data.extend_from_slice(&decoded);
        pending.next_index += 1;
        if pending.data.len() > pending.byte_length {
            return Err("chunked frame overran its declared length".to_owned());
        }
        if pending.next_index < pending.count {
            self.active = Some(pending);
            return Ok(None);
        }
        if pending.data.len() != pending.byte_length {
            return Err("chunked frame did not match its declared length".to_owned());
        }
        let text = String::from_utf8(pending.data)
            .map_err(|_| "chunked frame was not valid UTF-8".to_owned())?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("chunked frame was not valid JSON: {error}"))
    }
}

/// Pi already computes context occupancy for its own footer. Prefer that
/// native value when session stats are available, and use the active model in
/// `get_state` for the window before the first assistant message arrives.
fn pi_context_usage(state: &Value, stats: Option<&Value>) -> Option<(Option<u64>, Option<u64>)> {
    let context = stats.and_then(|stats| stats.pointer("/data/contextUsage"));
    let tokens = context
        .and_then(|context| context.get("tokens"))
        .and_then(Value::as_u64);
    let window = context
        .and_then(|context| context.get("contextWindow"))
        .and_then(Value::as_u64)
        .or_else(|| {
            state
                .pointer("/data/model/contextWindow")
                .and_then(Value::as_u64)
        })
        .filter(|window| *window > 0);
    (tokens.is_some() || window.is_some()).then_some((tokens, window))
}

/// Pi's providers normally fill `totalTokens`, but Pi itself deliberately
/// falls back to the four component counters when a provider leaves it zero.
/// Keep Waku's meter aligned with that provider-native calculation.
fn pi_message_context_tokens(message: &Value) -> Option<u64> {
    let usage = message.get("usage")?;
    usage
        .get("totalTokens")
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0)
        .or_else(|| {
            let total = ["input", "output", "cacheRead", "cacheWrite"]
                .into_iter()
                .filter_map(|field| usage.get(field).and_then(Value::as_u64))
                .fold(0_u64, u64::saturating_add);
            (total > 0).then_some(total)
        })
}

#[derive(Debug, Eq, PartialEq)]
enum ForkError {
    /// Failure occurred before the live session switched away from its original
    /// session (or during an out-of-process clone).
    Recoverable(String),
    /// The live session already switched to a forked/rolled-back session and
    /// could not be reconciled back to a known cursor; the live driver must
    /// shut down rather than continue serving prompts on the wrong session.
    SessionDesynced(String),
}

#[allow(clippy::too_many_arguments)]
fn fork_pi_session(
    flavor: PiFlavor,
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    binary: &Path,
    cwd: &Path,
    original_cursor: &ProviderResumeCursor,
    turns_to_remove: usize,
    restore_original: bool,
) -> Result<ProviderResumeCursor, ForkError> {
    let name = flavor.display_name();
    let messages = send_request(
        stdin,
        pending,
        next_request_id,
        json!({"type": flavor.branch_messages_command()}),
    )
    .map_err(ForkError::Recoverable)?;
    let messages = messages
        .pointer("/data/messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ForkError::Recoverable(format!("{name} returned an invalid fork-message list"))
        })?;

    // Keeping every turn is a whole-session copy, which Pi does in place and
    // Oh My Pi only does at launch. The out-of-process copy leaves this
    // session untouched, so it never needs restoring afterwards.
    if turns_to_remove == 0 && flavor == PiFlavor::OhMyPi {
        let session_file = flavor
            .session_file_from_cursor(original_cursor)
            .ok_or_else(|| {
                ForkError::Recoverable(format!("{name}'s original session file is unavailable"))
            })?;
        return clone_ohmypi_session(binary, cwd, session_file).map_err(ForkError::Recoverable);
    }

    let request =
        pi_fork_request(flavor, messages, turns_to_remove).map_err(ForkError::Recoverable)?;
    let fork =
        send_request(stdin, pending, next_request_id, request).map_err(ForkError::Recoverable)?;
    if fork.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
        return Err(ForkError::Recoverable(format!(
            "{name} session fork was cancelled"
        )));
    }
    let fork_state = send_request(
        stdin,
        pending,
        next_request_id,
        json!({"type": "get_state"}),
    )
    .map_err(ForkError::SessionDesynced)?;
    let fork_cursor = cursor_from_state(flavor, &fork_state).ok_or_else(|| {
        ForkError::SessionDesynced(format!("{name} did not report the forked session cursor"))
    })?;

    if restore_original {
        let session_file = flavor
            .session_file_from_cursor(original_cursor)
            .ok_or_else(|| {
                ForkError::SessionDesynced(format!("{name}'s original session file is unavailable"))
            })?;
        let switched = send_request(
            stdin,
            pending,
            next_request_id,
            json!({
                "type": "switch_session",
                "sessionPath": session_file
            }),
        )
        .map_err(ForkError::SessionDesynced)?;
        if switched.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
            return Err(ForkError::SessionDesynced(format!(
                "{name} could not return to the source session after forking"
            )));
        }
        let restored_state = send_request(
            stdin,
            pending,
            next_request_id,
            json!({"type": "get_state"}),
        )
        .map_err(ForkError::SessionDesynced)?;
        let restored_cursor = cursor_from_state(flavor, &restored_state).ok_or_else(|| {
            ForkError::SessionDesynced(format!("{name} did not report the restored source session"))
        })?;
        if restored_cursor.native_id() != original_cursor.native_id() {
            return Err(ForkError::SessionDesynced(format!(
                "{name} returned to the wrong source session after forking"
            )));
        }
    }

    Ok(fork_cursor)
}

fn pi_fork_request(
    flavor: PiFlavor,
    messages: &[Value],
    turns_to_remove: usize,
) -> Result<Value, String> {
    let name = flavor.display_name();
    if turns_to_remove > messages.len() {
        return Err(format!(
            "{name} has only {} native turns, but Waku needs to remove {turns_to_remove}",
            messages.len()
        ));
    }
    let retained_turns = messages.len() - turns_to_remove;
    if turns_to_remove == 0 {
        // Only reachable for Pi; Oh My Pi copies out of process instead.
        Ok(json!({"type": "clone"}))
    } else {
        let entry_id = messages
            .get(retained_turns)
            .and_then(|message| message.get("entryId"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{name} returned a fork message without an entry ID"))?;
        Ok(json!({"type": flavor.branch_command(), "entryId": entry_id}))
    }
}

/// Copies a whole Oh My Pi session by launching a throwaway agent with
/// `--fork`, which is the only place it exposes a full-session copy, then
/// reading back the session the copy landed in.
fn clone_ohmypi_session(
    binary: &Path,
    cwd: &Path,
    session_file: &Path,
) -> Result<ProviderResumeCursor, String> {
    let mut command = crate::command_env::command(binary);
    let command = command
        .args(["--mode", "rpc"])
        .arg(PiFlavor::OhMyPi.full_access_arg())
        .arg("--fork")
        .arg(session_file)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let child = crate::command_env::spawn(command)
        .map_err(|error| format!("could not start Oh My Pi to copy the session: {error}"))?;
    let mut scoped = ScopedChildProcess::adopt(child);
    let mut stdin = scoped
        .child
        .stdin
        .take()
        .ok_or_else(|| "Oh My Pi stdin unavailable".to_owned())?;
    let stdout = scoped
        .child
        .stdout
        .take()
        .ok_or_else(|| "Oh My Pi stdout unavailable".to_owned())?;
    let (tx, rx) = bounded(1);
    thread::Builder::new()
        .name("waku-ohmypi-clone".into())
        .spawn(move || {
            let mut chunks = ChunkAssembly::default();
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Ok(Some(value)) = chunks.accept(value) else {
                    continue;
                };
                if value.get("id").and_then(Value::as_str) == Some("waku-clone") {
                    let _ = tx.send(value);
                    break;
                }
            }
        })
        .map_err(|error| format!("could not read the Oh My Pi session copy: {error}"))?;
    write_json_line(
        &mut stdin,
        &json!({"id": "waku-clone", "type": "get_state"}),
    )
    .map_err(|error| format!("could not ask Oh My Pi for the copied session: {error}"))?;
    let state = rx
        .recv_timeout(CLONE_TIMEOUT)
        .map_err(|_| "timed out waiting for Oh My Pi to copy the session".to_owned())?;
    if state.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(state
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Oh My Pi could not copy the session")
            .to_owned());
    }
    cursor_from_state(PiFlavor::OhMyPi, &state)
        .ok_or_else(|| "Oh My Pi did not report the copied session cursor".to_owned())
}

#[derive(Default)]
struct PiStreamState {
    run_started: bool,
    message_saw_text: bool,
    message_saw_reasoning: bool,
    failed: bool,
    tools: HashMap<String, (ActivityKind, String)>,
    turn_active: Arc<AtomicBool>,
}

impl PiStreamState {
    fn reset_turn(&mut self) {
        self.turn_active.store(false, Ordering::Release);
        *self = Self {
            turn_active: self.turn_active.clone(),
            ..Self::default()
        };
    }
}

fn handle_pi_message(
    flavor: PiFlavor,
    value: Value,
    pending: &PendingResponses,
    commands: &Sender<CommandMessage>,
    events: &impl DriverEventSink,
    state: &mut PiStreamState,
) {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "response" {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            return;
        };
        let prompt_response = matches!(pending.lock().get(id), Some(PendingResponse::Prompt));
        if prompt_response {
            let success = value.get("success").and_then(Value::as_bool) == Some(true);
            let local_only =
                value.pointer("/data/agentInvoked").and_then(Value::as_bool) == Some(false);
            if success && !local_only {
                // OMP may acknowledge a prompt before reporting that an
                // extension handled it locally. Retain the id for that second
                // response; normal agent completion retires it below.
                return;
            }
            pending.lock().remove(id);
            if !state.run_started {
                let _ = events.send(DriverEvent::TurnStarted);
            }
            let error = (!success).then(|| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{} RPC command failed", flavor.display_name()))
            });
            if let Some(error) = error.as_ref() {
                let _ = events.send(DriverEvent::Error(error.clone()));
            }
            let _ = events.send(DriverEvent::TurnFinished {
                success,
                summary: error,
            });
            state.reset_turn();
            return;
        }
        let Some(PendingResponse::Request(response)) = pending.lock().remove(id) else {
            return;
        };
        if value.get("success").and_then(Value::as_bool) == Some(true) {
            let _ = response.send(Ok(value));
        } else {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} RPC command failed", flavor.display_name()));
            let _ = response.send(Err(error));
        }
        return;
    }

    if event_type == "available_commands_update" {
        publish_commands(
            crate::slash_command_catalog::parse_oh_my_pi_commands(&value),
            events,
        );
        return;
    }

    if event_type == flavor.session_info_event() {
        let title = value
            .get(flavor.session_info_title_field())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned);
        let _ = events.send(DriverEvent::AutoTitleUpdated(title));
        return;
    }

    // Oh My Pi reuses `agent_end` for intermediate settles, flagging the real
    // one with `isTerminal`. Anything else here would end the turn early while
    // maintenance or async delivery still has work queued.
    if event_type == flavor.settled_event() {
        if value.get("isTerminal").and_then(Value::as_bool) == Some(false) {
            return;
        }
        if state.run_started {
            pending
                .lock()
                .retain(|_, response| matches!(response, PendingResponse::Request(_)));
            let success = !state.failed;
            let _ = events.send(DriverEvent::TurnFinished {
                success,
                summary: (!success).then(|| {
                    tr!(
                        "errors.provider_complete_turn",
                        provider = flavor.display_name()
                    )
                }),
            });
        }
        state.reset_turn();
        return;
    }

    match event_type {
        "command_output" => {
            if let Some(text) = value
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                if !state.run_started {
                    state.run_started = true;
                    state.turn_active.store(true, Ordering::Release);
                    let _ = events.send(DriverEvent::TurnStarted);
                }
                // Each command_output is a complete output block, unlike
                // assistant deltas. Keep successive reports separated.
                let _ = events.send(DriverEvent::TextDelta(format!("{text}\n\n")));
            }
        }
        "agent_start" | "turn_start" => {
            if !state.run_started {
                state.run_started = true;
                state.failed = false;
                state.turn_active.store(true, Ordering::Release);
                let _ = events.send(DriverEvent::TurnStarted);
            }
        }
        "message_start" => {
            if value.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                state.message_saw_text = false;
                state.message_saw_reasoning = false;
            }
        }
        "message_update" => {
            let update = value.get("assistantMessageEvent").unwrap_or(&Value::Null);
            match update.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(delta) = update
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|delta| !delta.is_empty())
                    {
                        state.message_saw_text = true;
                        let _ = events.send(DriverEvent::TextDelta(delta.to_owned()));
                    }
                }
                Some("thinking_delta") => {
                    if let Some(delta) = update
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|delta| !delta.is_empty())
                    {
                        state.message_saw_reasoning = true;
                        let _ = events.send(DriverEvent::ReasoningDelta(delta.to_owned()));
                    }
                }
                Some("error") => {
                    state.failed = true;
                    let _ = events.send(DriverEvent::Error(pi_error_message(flavor, update)));
                }
                _ => {}
            }
        }
        "message_end" => {
            if value.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                if value.pointer("/message/stopReason").and_then(Value::as_str) == Some("error")
                    && !state.failed
                {
                    state.failed = true;
                    let message = value
                        .pointer("/message/errorMessage")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|msg| !msg.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            pi_error_message(flavor, value.get("message").unwrap_or(&value))
                        });
                    let _ = events.send(DriverEvent::Error(message));
                }
                // This is the context the next call starts from, not the
                // cumulative billed total for the whole session.
                if let Some(tokens) = value.get("message").and_then(pi_message_context_tokens) {
                    let _ = events.send(DriverEvent::UsageUpdated {
                        context_tokens: Some(tokens),
                        context_window: None,
                    });
                }
                emit_completed_message_fallback(value.get("message"), events, state);
            }
        }
        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
            let id = value
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let tool_name = value.get("toolName").and_then(Value::as_str);
            let (kind, mut title) = id
                .as_ref()
                .and_then(|id| state.tools.get(id))
                .cloned()
                .unwrap_or_else(|| {
                    tool_name
                        .map(|tool_name| (classify_tool(tool_name), tool_title(tool_name)))
                        .unwrap_or_else(|| (ActivityKind::Tool, tr!("activity.tool")))
                });
            if event_type == "tool_execution_start"
                && let Some(input_title) = activity::input_title(value.get("args"))
            {
                title = input_title;
            }
            if event_type == "tool_execution_start"
                && let Some(id) = id.as_ref()
            {
                state.tools.insert(id.clone(), (kind, title.clone()));
            }
            let arguments = (event_type == "tool_execution_start")
                .then(|| value.get("args"))
                .flatten();
            let output = match event_type {
                "tool_execution_update" => value.get("partialResult"),
                "tool_execution_end" => value.get("result"),
                _ => None,
            };
            let complete = event_type == "tool_execution_end";
            let failed = value.get("isError").and_then(Value::as_bool) == Some(true);
            let item = activity::tool_activity(
                id.clone(),
                kind,
                title,
                arguments,
                output,
                output,
                failed,
                complete,
            )
            .with_tool_name(tool_name);
            let _ = events.send(DriverEvent::RichActivity(item));
            if complete && let Some(id) = id {
                state.tools.remove(&id);
            }
        }
        "auto_retry_end" => {
            if value.get("success").and_then(Value::as_bool) == Some(true) {
                state.failed = false;
            } else {
                state.failed = true;
                let message = value
                    .get("finalError")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!("{} exhausted its automatic retries", flavor.display_name())
                    });
                let _ = events.send(DriverEvent::Error(message));
            }
        }
        "extension_ui_request" => {
            let method = value.get("method").and_then(Value::as_str);
            let id = value.get("id").and_then(Value::as_str);
            if matches!(method, Some("select" | "confirm" | "input" | "editor"))
                && let Some(id) = id
            {
                let _ = commands.send(CommandMessage::CancelExtensionRequest(id.to_owned()));
            }
        }
        "extension_error" => {
            let _ = events.send(DriverEvent::Error(pi_error_message(flavor, &value)));
        }
        _ => {}
    }
}

fn publish_commands(
    commands: Vec<waku_protocol::composer::SlashCommand>,
    events: &impl DriverEventSink,
) {
    let commands = commands
        .into_iter()
        .map(|command| ReportedCommand {
            name: command.name,
            description: command.description,
            argument_hint: command.argument_hint,
        })
        .collect();
    let _ = events.send(DriverEvent::AvailableCommands(commands));
}

fn emit_completed_message_fallback(
    message: Option<&Value>,
    events: &impl DriverEventSink,
    state: &mut PiStreamState,
) {
    let Some(content) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") if !state.message_saw_text => {
                if let Some(text) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    state.message_saw_text = true;
                    let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                }
            }
            Some("thinking") if !state.message_saw_reasoning => {
                if let Some(thinking) = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|thinking| !thinking.is_empty())
                {
                    state.message_saw_reasoning = true;
                    let _ = events.send(DriverEvent::ReasoningDelta(thinking.to_owned()));
                }
            }
            _ => {}
        }
    }
}

fn pi_error_message(flavor: PiFlavor, value: &Value) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.get("errorMessage").and_then(Value::as_str))
        .or_else(|| value.get("reason").and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            tr!(
                "errors.provider_reported_error",
                provider = flavor.display_name()
            )
        })
}

fn classify_tool(name: &str) -> ActivityKind {
    ActivityKind::from_tool_name(name)
}

fn tool_title(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "bash" => tr!("activity.run_command"),
        "edit" => tr!("activity.edit_file"),
        "write" => tr!("activity.write_file"),
        "read" => tr!("activity.read_file"),
        "grep" => tr!("activity.search_files"),
        "find" => tr!("activity.find_files"),
        "ls" => tr!("activity.list_files"),
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::TryRecvError;

    #[test]
    fn live_command_updates_reach_the_composer_and_clear_removed_commands() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for entries in [
            json!([
                {"name": "compact", "source": "builtin", "description": "Compact context"},
                {"name": "skill:verify", "source": "skill", "description": "Verify changes"}
            ]),
            json!([]),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
                json!({"type": "available_commands_update", "commands": entries}),
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        let DriverEvent::AvailableCommands(reported) = event_rx.recv().unwrap() else {
            panic!("missing command update")
        };
        assert_eq!(
            reported
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["compact", "verify"]
        );
        assert_eq!(reported[0].description, "Compact context");
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::AvailableCommands(commands) if commands.is_empty())
        );
        assert!(!state.run_started);
    }

    #[test]
    fn local_slash_command_output_and_completion_do_not_need_an_agent_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        send_prompt(&mut Vec::new(), &pending, &mut 0, "/context").unwrap();
        for frame in [
            json!({"type": "response", "id": "waku-1", "command": "prompt", "success": true}),
            json!({"type": "command_output", "text": "Available commands\n"}),
            json!({"type": "command_output", "text": "/compact"}),
            json!({"type": "response", "id": "waku-1", "command": "prompt", "success": true, "data": {"agentInvoked": false}}),
            json!({"type": "agent_end", "isTerminal": true}),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::TextDelta(text) if text == "Available commands\n\n\n")
        );
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::TextDelta(text) if text == "/compact\n\n")
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(pending.lock().is_empty());
    }

    #[test]
    fn asynchronous_prompt_errors_settle_only_the_current_prompt() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        let mut next = 0;
        let mut wire = Vec::new();
        send_prompt(&mut wire, &pending, &mut next, "/old").unwrap();
        send_prompt(&mut wire, &pending, &mut next, "/new").unwrap();
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "response", "id": "waku-1", "success": false, "error": "stale"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(event_rx.try_recv().is_err());
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "response", "id": "waku-2", "success": false, "error": "command failed"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::Error(error) if error == "command failed")
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: false, .. }
        ));
        assert!(pending.lock().is_empty());
        assert_eq!(String::from_utf8(wire).unwrap().lines().count(), 2);
    }
    fn harness() -> (
        PendingResponses,
        Sender<CommandMessage>,
        crossbeam_channel::Receiver<CommandMessage>,
        PiStreamState,
    ) {
        let (commands, receiver) = unbounded();
        (
            Arc::new(Mutex::new(HashMap::new())),
            commands,
            receiver,
            PiStreamState::default(),
        )
    }

    /// Drives the installed Pi RPC through one real provider turn. Ignored by
    /// default because it needs the CLI, credentials, and network access.
    #[test]
    #[ignore = "requires an installed, authenticated pi"]
    fn pi_context_usage_against_the_real_rpc() {
        let binary = crate::command_env::find_executable("pi").expect("pi is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            PiFlavor::Pi,
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
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
        .expect("the Pi RPC session should start");

        let mut connected = false;
        let mut context_tokens = None;
        let mut context_window = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(30)) {
            match event {
                DriverEvent::Connected { .. } => {
                    connected = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Pi failed to initialize: {error}"),
                _ => {}
            }
        }
        assert!(connected, "Pi never reported its native session");

        driver.prompt("Reply with exactly: OK. Do not use any tools.".into());
        let mut finished = false;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(180)) {
            match event {
                DriverEvent::UsageUpdated {
                    context_tokens: tokens,
                    context_window: window,
                } => {
                    context_tokens = tokens.or(context_tokens);
                    context_window = window.or(context_window);
                }
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "Pi should finish the probe turn");
                    finished = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                _ => {}
            }
        }

        assert!(finished, "Pi never settled the probe turn");
        assert!(context_tokens.is_some_and(|tokens| tokens > 0));
        assert!(context_window.is_some_and(|window| window > 0));
    }

    #[test]
    fn model_and_thinking_changes_reach_the_running_session_but_mode_changes_do_not() {
        let (commands, command_rx) = unbounded();
        let driver = PiDriver {
            flavor: PiFlavor::Pi,
            commands,
            process: Arc::new(PiProcessHandle::detached()),
            computer_use: None,
        };
        let options = |mode| SessionOptions {
            mode,
            model: Some("anthropic/claude-opus-5".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            service_tier: None,
            context_window: None,
        };

        assert!(driver.apply_options(options(RuntimeMode::FullAccess)));
        assert!(matches!(
            command_rx.try_recv(),
            Ok(CommandMessage::Options(_))
        ));

        // Pi has no permission setter and only runs with Full access.
        assert!(!driver.apply_options(options(RuntimeMode::Ask)));
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn pi_computer_use_uses_only_session_scoped_extension_and_skill_arguments() {
        let config = computer_use_runtime::ComputerUseConfig {
            server_path: PathBuf::from("/tmp/Narel Computer Use"),
            repl_path: PathBuf::from("/Applications/Narel.app/Resources/narel_js_repl"),
            skill_path: PathBuf::from("/Applications/Narel.app/Resources/skills/SKILL.md"),
            process_directory: PathBuf::from("/tmp/narel-computer-use/session"),
        };
        let mut command = std::process::Command::new("pi");

        configure_pi_computer_use_command(
            &mut command,
            Some((
                &config,
                Path::new("/Applications/Narel.app/Resources/computer-use/pi-extension.ts"),
            )),
        );

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "--extension",
                "/Applications/Narel.app/Resources/computer-use/pi-extension.ts",
                "--skill",
                "/Applications/Narel.app/Resources/skills/SKILL.md",
            ]
        );
        let environment = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(
            environment.get("WAKU_JS_REPL_SERVER"),
            Some(&Some(
                "/Applications/Narel.app/Resources/narel_js_repl".into()
            ))
        );
        assert_eq!(
            environment.get("WAKU_COMPUTER_USE_PROCESS_DIRECTORY"),
            Some(&Some("/tmp/narel-computer-use/session".into()))
        );
    }

    #[test]
    fn pi_fork_selects_the_first_removed_user_turn_or_clones_the_tip() {
        let messages = [
            json!({"entryId": "turn-1"}),
            json!({"entryId": "turn-2"}),
            json!({"entryId": "turn-3"}),
        ];
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 0).unwrap(),
            json!({"type": "clone"})
        );
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 2).unwrap(),
            json!({"type": "fork", "entryId": "turn-2"})
        );
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 3).unwrap(),
            json!({"type": "fork", "entryId": "turn-1"})
        );
        assert!(pi_fork_request(PiFlavor::Pi, &messages, 4).is_err());
    }

    #[test]
    fn streams_pi_text_reasoning_tools_and_settles_once() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "thinking_delta", "delta": "checking"}
            }),
            json!({
                "type": "tool_execution_start",
                "toolCallId": "tool-1",
                "toolName": "read",
                "args": {"path": "src/main.rs", "title": "Inspect Pi source"}
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": {"content": "..."},
                "isError": false
            }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "text_delta", "delta": "done"}
            }),
            json!({"type": "agent_end", "willRetry": false}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ReasoningDelta(value) if value == "checking"
        ));
        let DriverEvent::RichActivity(started) = event_rx.recv().unwrap() else {
            panic!("expected a rich Pi tool activity");
        };
        assert_eq!(started.title, "Inspect Pi source");
        assert_eq!(started.kind, ActivityKind::FileRead);
        assert_eq!(started.display_target.as_deref(), Some("src/main.rs"));
        assert!(
            started
                .arguments
                .as_deref()
                .is_some_and(|arguments| arguments.contains("src/main.rs"))
        );
        assert!(!started.complete);
        let DriverEvent::RichActivity(completed) = event_rx.recv().unwrap() else {
            panic!("expected a completed rich Pi tool activity");
        };
        assert!(
            completed
                .output
                .as_deref()
                .is_some_and(|output| output.contains("..."))
        );
        assert!(completed.complete);
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "done"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// Drives the installed Oh My Pi RPC through one real provider turn.
    /// Ignored by default because it needs the CLI, credentials, and network.
    #[test]
    #[ignore = "requires an installed, authenticated omp"]
    fn ohmypi_context_usage_against_the_real_rpc() {
        let binary = crate::command_env::find_executable("omp").expect("omp is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            PiFlavor::OhMyPi,
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
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
        .expect("the Oh My Pi RPC session should start");

        let mut cursor = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(60)) {
            match event {
                DriverEvent::Connected { provider_cursor } => {
                    cursor = provider_cursor;
                    break;
                }
                DriverEvent::Error(error) => panic!("Oh My Pi failed to initialize: {error}"),
                _ => {}
            }
        }
        assert!(
            matches!(
                cursor,
                Some(ProviderResumeCursor::OhMyPi {
                    session_file: Some(_),
                    ..
                })
            ),
            "Oh My Pi should report its own cursor with a session file, got {cursor:?}"
        );

        driver.prompt("Reply with exactly: OK. Do not use any tools.".into());
        let mut finished = false;
        let mut context_tokens = None;
        let mut context_window = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(180)) {
            match event {
                DriverEvent::UsageUpdated {
                    context_tokens: tokens,
                    context_window: window,
                } => {
                    context_tokens = tokens.or(context_tokens);
                    context_window = window.or(context_window);
                }
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "Oh My Pi should finish the probe turn");
                    finished = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Oh My Pi reported: {error}"),
                _ => {}
            }
        }

        assert!(finished, "Oh My Pi never settled the probe turn");
        assert!(context_tokens.is_some_and(|tokens| tokens > 0));
        assert!(context_window.is_some_and(|window| window > 0));
    }

    #[test]
    fn ohmypi_branches_where_pi_forks_and_never_asks_it_to_clone() {
        let messages = [
            json!({"entryId": "turn-1"}),
            json!({"entryId": "turn-2"}),
            json!({"entryId": "turn-3"}),
        ];
        assert_eq!(
            pi_fork_request(PiFlavor::OhMyPi, &messages, 2).unwrap(),
            json!({"type": "branch", "entryId": "turn-2"})
        );
        assert!(pi_fork_request(PiFlavor::OhMyPi, &messages, 4).is_err());
    }

    /// Oh My Pi reuses `agent_end` for intermediate settles, so only the
    /// terminal one may end the turn.
    #[test]
    fn ohmypi_settles_on_the_terminal_agent_end_only() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "text_delta", "delta": "done"}
            }),
            json!({"type": "agent_end", "isTerminal": false}),
            json!({"type": "agent_end", "messages": []}),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "done"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// Pi's own settle event carries no meaning for Oh My Pi, and vice versa.
    #[test]
    fn each_flavor_ignores_the_other_settle_and_title_events() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for (flavor, value) in [
            (PiFlavor::OhMyPi, json!({"type": "agent_start"})),
            (PiFlavor::OhMyPi, json!({"type": "agent_settled"})),
            (
                PiFlavor::OhMyPi,
                json!({"type": "session_info_changed", "name": "Pi's spelling"}),
            ),
            (PiFlavor::Pi, json!({"type": "agent_end"})),
            (
                PiFlavor::Pi,
                json!({"type": "session_info_update", "title": "Oh My Pi's spelling"}),
            ),
        ] {
            handle_pi_message(flavor, value, &pending, &commands, &events, &mut state);
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn ohmypi_session_titles_arrive_on_its_own_event() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::OhMyPi,
            json!({"type": "session_info_update", "title": "Named by Oh My Pi"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Named by Oh My Pi"
        ));
    }

    #[test]
    fn chunked_frames_reassemble_and_reject_broken_runs() {
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);

        let payload = json!({"type": "response", "id": "waku-1", "success": true});
        let bytes = serde_json::to_vec(&payload).unwrap();
        let (first, second) = bytes.split_at(bytes.len() / 2);
        let chunk = |index: u64, data: &[u8]| {
            json!({
                "type": "rpc_chunk",
                "chunkId": "rpc-1",
                "index": index,
                "count": 2,
                "byteLength": bytes.len(),
                "data": encode(data),
            })
        };

        let mut assembly = ChunkAssembly::default();
        assert_eq!(assembly.accept(chunk(0, first)).unwrap(), None);
        assert_eq!(assembly.accept(chunk(1, second)).unwrap(), Some(payload));

        // An ordinary frame passes straight through.
        let mut assembly = ChunkAssembly::default();
        let plain = json!({"type": "agent_start"});
        assert_eq!(assembly.accept(plain.clone()).unwrap(), Some(plain.clone()));

        // Anything interleaved into a run invalidates it rather than splicing.
        let mut assembly = ChunkAssembly::default();
        assert_eq!(assembly.accept(chunk(0, first)).unwrap(), None);
        assert!(assembly.accept(plain).is_err());
        assert!(assembly.active.is_none());

        // A run that starts mid-sequence is not a frame Waku can trust.
        let mut assembly = ChunkAssembly::default();
        assert!(assembly.accept(chunk(1, second)).is_err());
    }

    #[test]
    fn session_name_changes_are_forwarded_as_automatic_titles() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();

        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "session_info_changed", "name": "Named by Pi"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Named by Pi"
        ));

        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "session_info_changed", "name": null}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(None)
        ));
    }

    #[test]
    fn tool_only_intermediate_message_does_not_emit_empty_text() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "toolCall", "id": "tool-1", "name": "read"}]
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn completed_message_is_used_when_deltas_were_not_streamed() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "reason"},
                        {"type": "text", "text": "answer"}
                    ]
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ReasoningDelta(value) if value == "reason"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "answer"
        ));
    }

    #[test]
    fn context_usage_uses_pi_components_when_total_is_zero() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [],
                    "usage": {
                        "input": 33,
                        "output": 27,
                        "cacheRead": 5888,
                        "cacheWrite": 4,
                        "totalTokens": 0
                    }
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::UsageUpdated {
                context_tokens: Some(5952),
                context_window: None
            }
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn session_stats_supply_pi_context_tokens_and_window() {
        let state = json!({"data": {"model": {"contextWindow": 200_000}}});
        let stats = json!({
            "data": {
                "contextUsage": {
                    "tokens": 6109,
                    "contextWindow": 1_000_000,
                    "percent": 0.6109
                }
            }
        });

        assert_eq!(
            pi_context_usage(&state, Some(&stats)),
            Some((Some(6109), Some(1_000_000)))
        );
        assert_eq!(pi_context_usage(&state, None), Some((None, Some(200_000))));
    }

    #[test]
    fn recoverable_tool_error_does_not_fail_the_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": {"error": "missing"},
                "isError": true
            }),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        let DriverEvent::RichActivity(completed) = event_rx.recv().unwrap() else {
            panic!("expected a completed rich Pi tool activity");
        };
        assert!(completed.failed);
        assert!(
            completed
                .output
                .as_deref()
                .is_some_and(|output| output.contains("missing"))
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
    }

    #[test]
    fn successful_auto_retry_recovers_the_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "error", "error": "temporary"}
            }),
            json!({"type": "auto_retry_end", "success": true}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::Error(_)));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
    }

    /// Minimal writer that dispatches scripted RPC replies into `pending` as
    /// soon as each JSON-RPC line is flushed, enabling deterministic sync tests
    /// for `apply_live_options` and `fork_pi_session`.
    struct ScriptedRpcWriter {
        pending: PendingResponses,
        replies: std::collections::VecDeque<Result<Value, String>>,
        sent: Vec<Value>,
        buffer: Vec<u8>,
    }

    impl ScriptedRpcWriter {
        fn new(pending: PendingResponses, replies: Vec<Result<Value, String>>) -> Self {
            Self {
                pending,
                replies: replies.into(),
                sent: Vec::new(),
                buffer: Vec::new(),
            }
        }
    }

    impl Write for ScriptedRpcWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buffer.extend_from_slice(buf);
            while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=pos).collect();
                let value: Value = serde_json::from_slice(&line)
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.sent.push(value);
                if let Some(PendingResponse::Request(tx)) = self.pending.lock().remove(&id) {
                    let reply = self
                        .replies
                        .pop_front()
                        .unwrap_or_else(|| Err("no scripted reply".to_owned()));
                    let _ = tx.send(reply);
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn apply_live_options_preserves_model_and_effort_when_rpc_fails() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, event_rx) = unbounded();
        let mut next_id = 0;
        let mut current_model = Some("anthropic/claude-sonnet-4".to_owned());
        let mut current_effort = Some("low".to_owned());

        // Both set_model and set_thinking_level fail.
        let mut writer = ScriptedRpcWriter::new(
            pending.clone(),
            vec![
                Err("model unavailable".to_owned()),
                Err("thinking unsupported".to_owned()),
            ],
        );
        apply_live_options(
            PiFlavor::Pi,
            &mut writer,
            &pending,
            &mut next_id,
            SessionOptions {
                mode: RuntimeMode::FullAccess,
                model: Some("anthropic/claude-opus-5".to_owned()),
                reasoning_effort: Some("high".to_owned()),
                service_tier: None,
                context_window: None,
            },
            &mut current_model,
            &mut current_effort,
            &events,
        );
        assert_eq!(
            current_model.as_deref(),
            Some("anthropic/claude-sonnet-4"),
            "failed set_model must not overwrite current_model"
        );
        assert_eq!(
            current_effort.as_deref(),
            Some("low"),
            "failed set_thinking_level must not overwrite current_effort"
        );
        assert_eq!(writer.sent.len(), 2);
        assert!(matches!(event_rx.try_recv(), Ok(DriverEvent::Error(_))));
        assert!(matches!(event_rx.try_recv(), Ok(DriverEvent::Error(_))));

        // Invalid model slug must also not overwrite current_model.
        apply_live_options(
            PiFlavor::Pi,
            &mut writer,
            &pending,
            &mut next_id,
            SessionOptions {
                mode: RuntimeMode::FullAccess,
                model: Some("invalid-no-slash".to_owned()),
                reasoning_effort: Some("low".to_owned()),
                service_tier: None,
                context_window: None,
            },
            &mut current_model,
            &mut current_effort,
            &events,
        );
        assert_eq!(current_model.as_deref(), Some("anthropic/claude-sonnet-4"));
        assert!(matches!(event_rx.try_recv(), Ok(DriverEvent::Error(_))));

        // Retrying the same target model/effort now succeeds and updates state.
        let mut writer = ScriptedRpcWriter::new(
            pending.clone(),
            vec![
                Ok(json!({"success": true, "data": {"contextWindow": 200_000}})),
                Ok(json!({"success": true})),
            ],
        );
        apply_live_options(
            PiFlavor::Pi,
            &mut writer,
            &pending,
            &mut next_id,
            SessionOptions {
                mode: RuntimeMode::FullAccess,
                model: Some("anthropic/claude-opus-5".to_owned()),
                reasoning_effort: Some("high".to_owned()),
                service_tier: None,
                context_window: None,
            },
            &mut current_model,
            &mut current_effort,
            &events,
        );
        assert_eq!(current_model.as_deref(), Some("anthropic/claude-opus-5"));
        assert_eq!(current_effort.as_deref(), Some("high"));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(DriverEvent::UsageUpdated {
                context_tokens: None,
                context_window: Some(200_000),
            })
        ));
    }

    #[test]
    fn fork_pi_session_flags_desynced_session_when_switching_back_fails() {
        let original_cursor = ProviderResumeCursor::Pi {
            session_id: "orig-session".to_owned(),
            session_file: Some(PathBuf::from("/tmp/orig.jsonl")),
        };

        // Failure before mutating session (cancelled fork) is Recoverable.
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let mut writer = ScriptedRpcWriter::new(
            pending.clone(),
            vec![
                Ok(json!({"success": true, "data": {"messages": [{"entryId": "e1"}]}})),
                Ok(json!({"success": true, "data": {"cancelled": true}})),
            ],
        );
        let err = fork_pi_session(
            PiFlavor::Pi,
            &mut writer,
            &pending,
            &mut 0,
            Path::new("pi"),
            Path::new("/tmp"),
            &original_cursor,
            1,
            true,
        )
        .unwrap_err();
        assert!(matches!(err, ForkError::Recoverable(_)));

        // Failure during switch_session back to original after fork succeeded is SessionDesynced.
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let mut writer = ScriptedRpcWriter::new(
            pending.clone(),
            vec![
                Ok(json!({"success": true, "data": {"messages": [{"entryId": "e1"}]}})),
                Ok(json!({"success": true, "data": {"cancelled": false}})),
                Ok(
                    json!({"success": true, "data": {"sessionId": "fork-1", "sessionFile": "/tmp/fork.jsonl"}}),
                ),
                Err("switch_session failed".to_owned()),
            ],
        );
        let err = fork_pi_session(
            PiFlavor::Pi,
            &mut writer,
            &pending,
            &mut 0,
            Path::new("pi"),
            Path::new("/tmp"),
            &original_cursor,
            1,
            true,
        )
        .unwrap_err();
        assert!(matches!(err, ForkError::SessionDesynced(_)));

        // Restoring to a mismatched session ID is also SessionDesynced.
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let mut writer = ScriptedRpcWriter::new(
            pending.clone(),
            vec![
                Ok(json!({"success": true, "data": {"messages": [{"entryId": "e1"}]}})),
                Ok(json!({"success": true, "data": {"cancelled": false}})),
                Ok(
                    json!({"success": true, "data": {"sessionId": "fork-2", "sessionFile": "/tmp/fork2.jsonl"}}),
                ),
                Ok(json!({"success": true, "data": {"cancelled": false}})),
                Ok(
                    json!({"success": true, "data": {"sessionId": "wrong-id", "sessionFile": "/tmp/wrong.jsonl"}}),
                ),
            ],
        );
        let err = fork_pi_session(
            PiFlavor::Pi,
            &mut writer,
            &pending,
            &mut 0,
            Path::new("pi"),
            Path::new("/tmp"),
            &original_cursor,
            1,
            true,
        )
        .unwrap_err();
        assert!(matches!(err, ForkError::SessionDesynced(_)));
    }

    #[test]
    fn pi_stderr_filter_ignores_benign_diagnostics_and_keeps_real_errors() {
        for benign in [
            "",
            "   ",
            "Loaded extension: error-handler.ts",
            "Loading skill ~/.pi/skills/error-recovery/SKILL.md",
            "warning: ignored error in optional plugin",
            "WARN: transient error will be retried",
            "INFO: completed with 0 errors",
            "DEBUG: error state reset",
            "(node:12345) [DEP0040] DeprecationWarning: error formatting changed",
            "    at formatError (file:///usr/lib/pi/error.js:12:5)",
            r#"{"level":"debug","msg":"checked errors","error":null}"#,
            r#"{"level":30,"msg":"0 errors reported"}"#,
        ] {
            assert_eq!(
                pi_stderr_error_line(benign),
                None,
                "benign stderr line should not trigger DriverEvent::Error: {benign:?}"
            );
        }

        for real_error in [
            "error: Cannot find module 'foo'",
            "\x1b[31mError:\x1b[0m API key missing",
            "TypeError: Cannot read properties of undefined",
            "AssertionError [ERR_ASSERTION]: invalid rpc state",
            "[ERROR] failed to open session file",
            "[omp] error: corrupted config.json",
            "2026-11-21T12:00:00Z ERROR sqlite busy",
            "panic(main thread): Segmentation fault",
            "UnhandledPromiseRejection: ECONNREFUSED",
            r#"{"level":"error","msg":"authentication rejected"}"#,
        ] {
            assert!(
                pi_stderr_error_line(real_error).is_some(),
                "real stderr error should be surfaced: {real_error:?}"
            );
        }
    }

    #[test]
    fn pi_process_handle_guards_pid_reuse_and_scoped_child_cleans_up_on_drop() {
        let handle = PiProcessHandle::new(
            u32::MAX,
            #[cfg(windows)]
            None,
        );
        handle.mark_exited();
        // After mark_exited, terminate must be a no-op (not signaling sentinel PID).
        handle.terminate();
        assert!(handle.is_shutting_down());
        assert_eq!(handle.state.lock().pid, 0);

        #[cfg(windows)]
        let child = {
            let mut cmd = std::process::Command::new("cmd.exe");
            cmd.args(["/d", "/c", "ping -n 30 127.0.0.1"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            crate::command_env::spawn(&mut cmd).unwrap()
        };
        #[cfg(unix)]
        let child = {
            use std::os::unix::process::CommandExt as _;
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.args(["-c", "sleep 30"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .process_group(0);
            crate::command_env::spawn(&mut cmd).unwrap()
        };
        let mut scoped = ScopedChildProcess::adopt(child);
        let mut stdout = scoped.child.stdout.take().unwrap();
        let (done_tx, done_rx) = bounded(1);
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
            let _ = done_tx.send(());
        });
        drop(scoped);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("dropping ScopedChildProcess must terminate the child process tree");
    }

    #[test]
    fn message_end_with_error_stop_reason_fails_empty_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();

        for frame in [
            json!({"type": "agent_start"}),
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "stopReason": "error",
                    "errorMessage": "Quota exceeded",
                    "content": []
                }
            }),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(PiFlavor::Pi, frame, &pending, &commands, &events, &mut state);
        }

        let seen: Vec<_> = event_rx.try_iter().collect();
        assert!(seen.iter().any(|e| matches!(e, DriverEvent::Error(msg) if msg == "Quota exceeded")));
        assert!(seen.iter().any(|e| matches!(
            e,
            DriverEvent::TurnFinished {
                success: false,
                ..
            }
        )));
    }
}
