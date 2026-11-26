//! Amp's streaming-JSON session.
//!
//! `--stream-json-input` makes Amp read newline-delimited user messages from
//! stdin and keeps the process alive until both the assistant is done *and*
//! stdin closes, so one process serves the whole conversation.
//!
//! Unlike every other long-lived transport here, Amp exposes no permission
//! request over the stream — its rules live in `amp permissions`, so Waku still
//! decides the posture at launch. Turn completion is not a `result` message
//! either: Amp signals it with `stop_reason: "end_turn"` on the assistant
//! message. Both facts came from probing the real CLI.
//!
//! A plain user message written mid-turn is held by the CLI until the current
//! turn's `end_turn`, then run as a turn of its own. Steering needs the
//! documented top-level `"steer": true` attribute on the message, which marks
//! it for handling at the next interruption point — the running turn absorbs
//! it and one `end_turn` settles everything. Both behaviors were probed
//! against the real CLI; the attribute is in the manual's Streaming JSON
//! section.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::activity;
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{ActivityKind, DriverEvent, ProviderResumeCursor, RuntimeMode};

const MAX_STDERR_LINES: usize = 128;
const MAX_STDERR_LINE_BYTES: usize = 4096;

enum CommandMessage {
    Prompt(String),
    Steer(String),
    Shutdown,
}

struct AmpProcessState {
    pid: u32,
    #[cfg(windows)]
    job: Option<crate::process_tree::KillOnCloseJob>,
}

struct AmpProcessHandle {
    state: Mutex<AmpProcessState>,
    cancelled: AtomicBool,
    turn_active: Arc<Mutex<bool>>,
}

impl AmpProcessHandle {
    fn new(
        pid: u32,
        #[cfg(windows)] job: Option<crate::process_tree::KillOnCloseJob>,
        turn_active: Arc<Mutex<bool>>,
    ) -> Self {
        Self {
            state: Mutex::new(AmpProcessState {
                pid,
                #[cfg(windows)]
                job,
            }),
            cancelled: AtomicBool::new(false),
            turn_active,
        }
    }

    fn terminate(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut state = self.state.lock();
        if state.pid == 0 {
            return;
        }
        let pid = state.pid;
        #[cfg(windows)]
        {
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
            unsafe {
                let _ = libc::kill(pid as i32, libc::SIGINT);
            }
        }
    }

    fn mark_exited(&self) {
        let mut state = self.state.lock();
        state.pid = 0;
        #[cfg(windows)]
        drop(state.job.take());
    }

    fn was_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

pub struct AmpDriver {
    commands: Sender<CommandMessage>,
    process: Arc<AmpProcessHandle>,
}

/// Amp's thread arguments. The prompt never rides here — it goes in on stdin.
pub(super) fn amp_args(
    mode: Option<&str>,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
    thread_id: Option<&str>,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(thread_id) = thread_id {
        args.extend([
            "threads".to_owned(),
            "continue".to_owned(),
            thread_id.to_owned(),
        ]);
    }
    args.extend([
        "--execute".to_owned(),
        // Implies --stream-json, which --stream-json-input requires.
        "--stream-json-thinking".to_owned(),
        "--stream-json-input".to_owned(),
        "--dangerously-allow-all".to_owned(),
    ]);
    if let Some(mode) = mode {
        args.extend(["--mode".to_owned(), mode.to_owned()]);
    }
    if let Some(reasoning_effort) = reasoning_effort {
        args.extend(["--effort".to_owned(), reasoning_effort.to_owned()]);
    }
    if service_tier == Some("fast") {
        args.push("--fast".to_owned());
    }
    args
}

impl AmpDriver {
    pub fn start(options: DriverStartOptions, events: DriverEventSender) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            model,
            reasoning_effort,
            service_tier,
            context_window: _,
            agent_preset: _,
            computer_use_enabled: _,
            provider_cursor,
            config_dir: _,
        } = options;
        if mode != RuntimeMode::FullAccess {
            return Err(anyhow!("Amp currently supports Full access only"));
        }
        let (thread_id, fork_context) = match provider_cursor {
            Some(ProviderResumeCursor::Amp {
                thread_id,
                fork_context,
            }) => ((!thread_id.is_empty()).then_some(thread_id), fork_context),
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume Amp from a {} cursor",
                    cursor.provider().display_name()
                ));
            }
            None => (None, None),
        };

        let title_binary = binary.clone();
        let title_cwd = cwd.clone();
        let reader_initial_thread_id = thread_id.clone();
        let mut command: Command = crate::command_env::command(&binary);
        command.current_dir(&cwd).args(amp_args(
            model.as_deref(),
            reasoning_effort.as_deref(),
            service_tier.as_deref(),
            thread_id.as_deref(),
        ));
        let command = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::command_env::spawn(command)
            .context("failed to start `amp` in streaming-input mode")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Amp stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Amp stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Amp stderr unavailable"))?;
        let child_pid = child.id();
        #[cfg(windows)]
        let job = crate::process_tree::KillOnCloseJob::adopt(child_pid).ok();

        if let Some(thread_id) = thread_id.clone() {
            let _ = events.send(DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::Amp {
                    thread_id,
                    fork_context: fork_context.clone(),
                }),
            });
        }

        let (commands, command_rx) = unbounded();
        let turn_active = Arc::new(Mutex::new(false));
        let process = Arc::new(AmpProcessHandle::new(
            child_pid,
            #[cfg(windows)]
            job,
            turn_active.clone(),
        ));

        let reader_events = events.clone();
        let reader_turn = turn_active.clone();
        let reader_thread = thread::Builder::new()
            .name("waku-amp-reader".into())
            .spawn(move || {
                let mut state = AmpStreamState {
                    thread_id: reader_initial_thread_id,
                    ..Default::default()
                };
                let title_refresh = super::title_refresh::NativeTitleRefresh::default();
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if handle_message(&value, &reader_events, &reader_turn, &mut state)
                        && let Some(thread_id) = state.thread_id.clone()
                    {
                        let binary = title_binary.clone();
                        let cwd = title_cwd.clone();
                        title_refresh.start(
                            "waku-amp-title",
                            vec![Duration::from_millis(500), Duration::from_secs(2)],
                            reader_events.clone(),
                            move || crate::amp_session::thread_title(&binary, &cwd, &thread_id),
                        );
                    }
                }
            })?;

        let writer_events = events.clone();
        let writer_turn = turn_active.clone();
        thread::Builder::new()
            .name("waku-amp-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                // A branch replays its retained history in the first prompt,
                // because Amp has no way to seed a thread otherwise.
                let mut fork_context = fork_context;
                while let Ok(message) = command_rx.recv() {
                    match message {
                        CommandMessage::Prompt(text) => {
                            *writer_turn.lock() = true;
                            let _ = writer_events.send(DriverEvent::TurnStarted);
                            let written = send_prompt_line(&mut stdin, &mut fork_context, text);
                            if let Err(error) = written {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_transport_write",
                                    provider = "Amp",
                                    error = error
                                )));
                                if std::mem::take(&mut *writer_turn.lock()) {
                                    let _ = writer_events.send(DriverEvent::TurnFinished {
                                        success: false,
                                        summary: Some(tr!(
                                            "errors.provider_receive_prompt",
                                            provider = "Amp"
                                        )),
                                    });
                                }
                                break;
                            }
                        }
                        CommandMessage::Steer(text) => {
                            // Without the marker Amp would hold the message
                            // until `end_turn` and run it as a turn of its own;
                            // `"steer": true` has the running turn absorb it at
                            // the next interruption point instead. No
                            // TurnStarted and no turn re-arm: the turn the
                            // message joins is already running.
                            if !*writer_turn.lock() {
                                let _ = writer_events.send(DriverEvent::SteerRejected {
                                    message: text,
                                    reason: tr!("errors.provider_no_active_turn", provider = "Amp"),
                                });
                                continue;
                            }
                            let written = write_line(
                                &mut stdin,
                                &json!({
                                    "type": "user",
                                    "steer": true,
                                    "message": {
                                        "role": "user",
                                        "content": [{"type": "text", "text": text}]
                                    }
                                }),
                            );
                            match written {
                                Ok(()) => {
                                    let _ = writer_events
                                        .send(DriverEvent::steer_accepted(text));
                                }
                                Err(error) => {
                                    let _ = writer_events.send(DriverEvent::SteerRejected {
                                        message: text,
                                        reason: tr!(
                                            "errors.provider_transport_write",
                                            provider = "Amp",
                                            error = error
                                        ),
                                    });
                                    // Stdin is gone, so the running turn cannot
                                    // settle from the CLI side either.
                                    let _ = writer_events.send(DriverEvent::Error(tr!(
                                        "errors.provider_transport_write_short",
                                        provider = "Amp"
                                    )));
                                    if std::mem::take(&mut *writer_turn.lock()) {
                                        let _ = writer_events.send(DriverEvent::TurnFinished {
                                            success: false,
                                            summary: Some(tr!(
                                                "errors.provider_stopped_receiving",
                                                provider = "Amp"
                                            )),
                                        });
                                    }
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
        let stderr_thread = thread::Builder::new()
            .name("waku-amp-stderr".into())
            .spawn(move || {
                let lines = collect_bounded_stderr_lines(stderr);
                if let Some(message) = super::support::provider_stderr_error(lines) {
                    let error = format!("Amp: {message}");
                    *stderr_last_error.lock() = Some(error.clone());
                    let _ = stderr_events.send(DriverEvent::Error(error));
                }
            })?;

        let process_handle = process.clone();
        let process_turn = turn_active;
        thread::Builder::new()
            .name("waku-amp-process".into())
            .spawn(move || {
                let status = child.wait();
                // Clear the tracked PID before dropping `child` so Windows
                // cannot recycle the PID while `terminate()` still holds it.
                process_handle.mark_exited();
                drop(child);
                let _ = reader_thread.join();
                let _ = stderr_thread.join();
                let cancelled = process_handle.was_cancelled();
                let last_stderr = last_visible_stderr.lock().clone();
                finalize_process_exit(status, cancelled, last_stderr, &process_turn, &events);
            })?;

        Ok(Self { commands, process })
    }
}

fn finalize_process_exit(
    status: std::io::Result<std::process::ExitStatus>,
    cancelled: bool,
    last_visible_stderr: Option<String>,
    turn_active: &Mutex<bool>,
    events: &impl DriverEventSink,
) {
    let exit_error = if cancelled {
        None
    } else if let Some(stderr_error) = last_visible_stderr {
        Some(stderr_error)
    } else if let Ok(status) = status
        && !status.success()
    {
        let message = tr!("errors.provider_exited", provider = "Amp", status = status);
        let _ = events.send(DriverEvent::Error(message.clone()));
        Some(message)
    } else {
        None
    };
    if std::mem::take(&mut *turn_active.lock()) {
        let _ = events.send(DriverEvent::TurnFinished {
            success: false,
            summary: exit_error,
        });
    }
    let _ = events.send(DriverEvent::ProcessExited);
}

impl DriverControl for AmpDriver {
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
        // Amp offers no interrupt on the stream, so stopping means ending the
        // process. The thread survives on Amp's side, and the next prompt
        // resumes it with `threads continue` — which is why Amp's runtime is
        // not retained after a cancel.
        let _ = self.commands.send(CommandMessage::Shutdown);
        self.process.terminate();
    }

    fn respond(&self, _request_id: String, _option_id: String) {}

    fn apply_options(&self, _options: SessionOptions) -> bool {
        // Mode, effort and tier are all launch arguments.
        false
    }

    fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        Err(anyhow!(
            "conversation rollback is not supported by this provider transport"
        ))
    }
}

impl Drop for AmpDriver {
    fn drop(&mut self) {
        let _ = self.commands.send(CommandMessage::Shutdown);
        if *self.process.turn_active.lock() || self.process.was_cancelled() {
            self.process.terminate();
        }
    }
}

fn send_prompt_line(
    stdin: &mut impl Write,
    fork_context: &mut Option<String>,
    text: String,
) -> std::io::Result<()> {
    let payload_text = fork_context
        .as_deref()
        .map(|context| crate::amp_session::prompt_with_fork_context(context, &text))
        .unwrap_or(text);
    write_line(
        stdin,
        &json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": payload_text}]
            }
        }),
    )?;
    *fork_context = None;
    Ok(())
}

fn collect_bounded_stderr_lines(reader: impl Read) -> Vec<String> {
    let mut ring = VecDeque::with_capacity(MAX_STDERR_LINES);
    let mut reader = BufReader::new(reader);
    let mut current = Vec::new();
    let push_line = |ring: &mut VecDeque<String>, raw: &mut Vec<u8>| {
        if raw.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(raw);
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            if ring.len() == MAX_STDERR_LINES {
                ring.pop_front();
            }
            ring.push_back(trimmed.to_owned());
        }
        raw.clear();
    };
    loop {
        let available = match reader.fill_buf() {
            Ok([]) | Err(_) => break,
            Ok(chunk) => chunk,
        };
        let mut consumed = 0;
        while consumed < available.len() {
            let rest = &available[consumed..];
            if let Some(newline) = rest.iter().position(|&byte| byte == b'\n') {
                let take = (MAX_STDERR_LINE_BYTES.saturating_sub(current.len())).min(newline);
                current.extend_from_slice(&rest[..take]);
                push_line(&mut ring, &mut current);
                consumed += newline + 1;
            } else {
                let take = (MAX_STDERR_LINE_BYTES.saturating_sub(current.len())).min(rest.len());
                current.extend_from_slice(&rest[..take]);
                consumed += rest.len();
            }
        }
        reader.consume(consumed);
    }
    push_line(&mut ring, &mut current);
    ring.into_iter().collect()
}

fn write_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[derive(Default)]
struct AmpStreamState {
    tools: HashMap<String, (ActivityKind, String)>,
    thread_id: Option<String>,
}

fn handle_message(
    value: &Value,
    events: &impl DriverEventSink,
    turn_active: &Mutex<bool>,
    state: &mut AmpStreamState,
) -> bool {
    let mut turn_finished = false;
    match value.get("type").and_then(Value::as_str) {
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                state.thread_id = Some(id.to_owned());
                let _ = events.send(DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Amp {
                        thread_id: id.to_owned(),
                        fork_context: None,
                    }),
                });
            }
        }
        Some("assistant") => {
            // Amp shares the Claude wire format; only the main thread's usage
            // describes this session's context.
            if value.get("parent_tool_use_id").is_none_or(Value::is_null)
                && let Some(usage) = value.pointer("/message/usage")
                && let Some(tokens) = super::support::claude_context_tokens(usage)
            {
                let _ = events.send(DriverEvent::UsageUpdated {
                    context_tokens: Some(tokens),
                    context_window: None,
                });
            }
            if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
                for block in content {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty())
                            {
                                let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                            }
                        }
                        Some("thinking") => {
                            if let Some(text) = block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty())
                            {
                                let _ = events.send(DriverEvent::ReasoningDelta(text.to_owned()));
                            }
                        }
                        Some("tool_use") => {
                            let id = block.get("id").and_then(Value::as_str).map(str::to_owned);
                            let wire_title = block
                                .get("name")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .unwrap_or_else(|| tr!("activity.tool"));
                            let kind = super::support::classify_tool(&wire_title);
                            let title =
                                activity::input_title(block.get("input")).unwrap_or(wire_title);
                            if let Some(id) = &id {
                                state.tools.insert(id.clone(), (kind, title.clone()));
                            }
                            let _ =
                                events.send(DriverEvent::RichActivity(activity::tool_activity(
                                    id,
                                    kind,
                                    title,
                                    block.get("input"),
                                    None,
                                    None,
                                    false,
                                    false,
                                )));
                        }
                        // Redacted thinking is provider-private control data.
                        _ => {}
                    }
                }
            }
            // Amp emits no `result` on ordinary completion: the turn settles
            // when the assistant stops for any terminal reason (anything other
            // than `tool_use`).
            if let Some(stop_reason) = value
                .pointer("/message/stop_reason")
                .and_then(Value::as_str)
                && stop_reason != "tool_use"
                && std::mem::take(&mut *turn_active.lock())
            {
                let (success, summary) = match stop_reason {
                    "end_turn" => (true, None),
                    "max_tokens" => (false, Some(tr!("session.agent_ran_out_of_context"))),
                    "refusal" => (false, Some(tr!("session.agent_declined_turn"))),
                    other => (
                        false,
                        Some(tr!("session.agent_stopped_reason", reason = other)),
                    ),
                };
                let _ = events.send(DriverEvent::TurnFinished { success, summary });
                turn_finished = success;
            }
        }
        Some("user") => {
            let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
                return false;
            };
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let (kind, title) = id
                    .as_ref()
                    .and_then(|id| state.tools.remove(id))
                    .unwrap_or((ActivityKind::Tool, "Tool".to_owned()));
                let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
                let _ = events.send(DriverEvent::RichActivity(activity::tool_activity(
                    id,
                    kind,
                    title,
                    None,
                    block.get("content"),
                    block.get("content"),
                    failed,
                    true,
                )));
            }
        }
        Some("result") => {
            if value.get("is_error").and_then(Value::as_bool) == Some(true) {
                let message = value
                    .get("error")
                    .or_else(|| value.get("result"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|msg| !msg.is_empty())
                    .unwrap_or("Amp reported an error");
                let _ = events.send(DriverEvent::Error(message.to_owned()));
                if std::mem::take(&mut *turn_active.lock()) {
                    let _ = events.send(DriverEvent::TurnFinished {
                        success: false,
                        summary: Some(message.to_owned()),
                    });
                }
            } else if std::mem::take(&mut *turn_active.lock()) {
                let _ = events.send(DriverEvent::TurnFinished {
                    success: true,
                    summary: None,
                });
                turn_finished = true;
            }
        }
        Some("system") => {
            if let Some(message) = value.get("error").and_then(Value::as_str) {
                let _ = events.send(DriverEvent::Error(message.to_owned()));
            }
        }
        _ => {}
    }
    turn_finished
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the real CLI through the actual driver, including a second turn
    /// on the same process. Ignored by default: needs the CLI installed,
    /// credentials, and the network.
    #[test]
    #[ignore = "requires an installed, authenticated amp"]
    fn amp_streaming_session_against_the_real_cli() {
        let binary = crate::command_env::find_executable("amp").expect("amp is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = AmpDriver::start(
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
        .expect("the streaming session should start");

        let collect = |driver: &AmpDriver, prompt: &str| -> String {
            driver.prompt(prompt.to_owned());
            let mut text = String::new();
            while let Ok(event) = event_rx.recv_timeout(std::time::Duration::from_secs(180)) {
                match event {
                    DriverEvent::TextDelta(delta) => text.push_str(&delta),
                    DriverEvent::TurnFinished { success, .. } => {
                        assert!(success, "the turn should settle successfully");
                        return text;
                    }
                    DriverEvent::Error(error) => panic!("the CLI reported: {error}"),
                    _ => {}
                }
            }
            panic!("the turn never settled");
        };

        let first = collect(&driver, "Reply with exactly: BANANA. Use no tools.");
        assert!(first.contains("BANANA"), "expected a reply, got {first:?}");

        // Proves one process is serving the conversation and kept its context.
        let second = collect(
            &driver,
            "What word did I just ask you to reply with? Answer with that word only.",
        );
        assert!(
            second.contains("BANANA"),
            "the session should retain context across turns, got {second:?}"
        );
    }

    /// Proves steering through the actual driver: the `"steer": true` message
    /// injected while the shell tool sleeps lands inside the same turn — one
    /// SteerAccepted, one TurnFinished, and a reply that honors both
    /// instructions. Ignored by default: needs the CLI installed, credentials,
    /// and the network.
    #[test]
    #[ignore = "requires an installed, authenticated amp"]
    fn amp_steering_folds_a_mid_turn_message_into_the_running_turn() {
        let binary = crate::command_env::find_executable("amp").expect("amp is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = AmpDriver::start(
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: Some("fast".into()),
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                config_dir: None,
            },
            events,
        )
        .expect("the streaming session should start");

        driver.prompt(
            "Use the Bash tool to run exactly `sleep 6` (nothing else). \
             After the command completes, reply with exactly: FIRST DONE"
                .into(),
        );

        let mut text = String::new();
        let mut steered = false;
        let mut steer_accepted = false;
        let mut turns_finished = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        while std::time::Instant::now() < deadline {
            let Ok(event) = event_rx.recv_timeout(std::time::Duration::from_secs(5)) else {
                // Quiet after the turn settled means no second turn is coming.
                if turns_finished == 1 {
                    break;
                }
                continue;
            };
            match event {
                DriverEvent::RichActivity(item) if !steered && !item.complete => {
                    // The tool is running: the turn is unambiguously live.
                    steered = true;
                    driver.steer(
                        "ADDITIONAL INSTRUCTION: end your very next reply \
                         with the word BANANA."
                            .into(),
                    );
                }
                DriverEvent::SteerAccepted { message, .. } => {
                    assert!(message.contains("BANANA"));
                    steer_accepted = true;
                }
                DriverEvent::SteerRejected { reason, .. } => {
                    panic!("the steer should be accepted, got rejection: {reason}");
                }
                DriverEvent::TextDelta(delta) => text.push_str(&delta),
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "the turn should settle successfully");
                    turns_finished += 1;
                }
                DriverEvent::Error(error) => panic!("the CLI reported: {error}"),
                _ => {}
            }
        }

        assert!(steered, "the probe never saw the tool start");
        assert!(steer_accepted, "the driver should acknowledge the steer");
        assert_eq!(
            turns_finished, 1,
            "a steered message must not settle a second turn"
        );
        assert!(
            text.contains("BANANA"),
            "the steered instruction should shape the same turn's reply, got {text:?}"
        );
    }

    #[test]
    fn cli_args_stream_json_in_and_out_and_resume_the_exact_thread() {
        let args = amp_args(Some("gpt-5"), Some("high"), Some("fast"), Some("T-123"));
        assert_eq!(&args[..3], &["threads", "continue", "T-123"]);
        // --stream-json-thinking implies --stream-json, which the input flag needs.
        assert!(args.contains(&"--stream-json-thinking".to_owned()));
        assert!(args.contains(&"--stream-json-input".to_owned()));
        assert!(args.contains(&"--execute".to_owned()));
        assert!(args.contains(&"--fast".to_owned()));
        // The prompt is never an argument; it goes in on stdin.
        assert!(!args.iter().any(|arg| arg.contains("Reply with")));

        let fresh = amp_args(None, None, None, None);
        assert!(!fresh.contains(&"threads".to_owned()));
        assert!(!fresh.contains(&"--fast".to_owned()));
    }

    #[test]
    fn streams_thinking_text_and_tools_then_settles_on_end_turn() {
        let (events, event_rx) = unbounded();
        let turn = Mutex::new(true);
        let mut state = AmpStreamState::default();
        // Payloads copied from a live `--stream-json-input` session.
        let wire = [
            json!({"type":"system","subtype":"init","session_id":"T-abc","tools":[]}),
            json!({"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"pondering"},
                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"cmd":"ls"}}
            ],"stop_reason":"tool_use"}}),
            json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"a.txt","is_error":false}
            ]}}),
            json!({"type":"assistant","message":{"content":[
                {"type":"text","text":"BANANA."}
            ],"stop_reason":"end_turn"}}),
        ];
        let outcomes = wire
            .into_iter()
            .map(|message| handle_message(&message, &events, &turn, &mut state))
            .collect::<Vec<_>>();
        assert_eq!(outcomes, [false, false, false, true]);

        let mut seen = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            seen.push(event);
        }
        assert!(matches!(
            &seen[0],
            DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::Amp { thread_id, .. })
            } if thread_id == "T-abc"
        ));
        assert!(matches!(&seen[1], DriverEvent::ReasoningDelta(t) if t == "pondering"));
        assert!(matches!(&seen[2], DriverEvent::RichActivity(item)
                if item.kind == ActivityKind::Command && !item.complete));
        assert!(matches!(&seen[3], DriverEvent::RichActivity(item)
                if item.complete && item.output.as_deref() == Some("a.txt")));
        assert!(matches!(&seen[4], DriverEvent::TextDelta(t) if t == "BANANA."));
        assert!(matches!(
            &seen[5],
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert_eq!(seen.len(), 6);
        assert!(!*turn.lock(), "the turn should be settled exactly once");
    }

    #[test]
    fn a_tool_using_assistant_message_does_not_end_the_turn() {
        let (events, event_rx) = unbounded();
        let turn = Mutex::new(true);
        let mut state = AmpStreamState::default();

        handle_message(
            &json!({"type":"assistant","message":{"content":[],"stop_reason":"tool_use"}}),
            &events,
            &turn,
            &mut state,
        );

        assert!(*turn.lock(), "a tool call is mid-turn, not the end of one");
        assert!(
            !std::iter::from_fn(|| event_rx.try_recv().ok())
                .any(|event| matches!(event, DriverEvent::TurnFinished { .. }))
        );
    }

    #[test]
    fn result_error_settles_active_turn_with_failure() {
        let (events, event_rx) = unbounded();
        let turn = Mutex::new(true);
        let mut state = AmpStreamState::default();

        let refresh_title = handle_message(
            &json!({"type":"result","is_error":true,"error":"quota exceeded"}),
            &events,
            &turn,
            &mut state,
        );

        assert!(!refresh_title);
        assert!(!*turn.lock(), "error result must clear turn_active");
        let seen = std::iter::from_fn(|| event_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(matches!(&seen[0], DriverEvent::Error(msg) if msg == "quota exceeded"));
        assert!(matches!(
            &seen[1],
            DriverEvent::TurnFinished {
                success: false,
                summary: Some(summary)
            } if summary == "quota exceeded"
        ));
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn non_end_turn_stop_reasons_settle_turn_without_hanging() {
        for (reason, expected_summary) in [
            ("max_tokens", tr!("session.agent_ran_out_of_context")),
            ("refusal", tr!("session.agent_declined_turn")),
            (
                "stop_sequence",
                tr!("session.agent_stopped_reason", reason = "stop_sequence"),
            ),
        ] {
            let (events, event_rx) = unbounded();
            let turn = Mutex::new(true);
            let mut state = AmpStreamState::default();

            let refresh_title = handle_message(
                &json!({"type":"assistant","message":{"content":[],"stop_reason":reason}}),
                &events,
                &turn,
                &mut state,
            );

            assert!(
                !refresh_title,
                "failed stop reason `{reason}` should not trigger title refresh"
            );
            assert!(
                !*turn.lock(),
                "stop reason `{reason}` must clear turn_active"
            );
            let seen = std::iter::from_fn(|| event_rx.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(seen.len(), 1);
            assert!(
                matches!(
                    &seen[0],
                    DriverEvent::TurnFinished {
                        success: false,
                        summary: Some(summary)
                    } if summary == &expected_summary
                ),
                "unexpected event for `{reason}`: {seen:?}"
            );
        }
    }

    #[test]
    fn send_prompt_line_preserves_fork_context_on_write_failure() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pipe closed",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut fork_context = Some("prior branch history".to_owned());
        let err = send_prompt_line(&mut FailingWriter, &mut fork_context, "hello".to_owned());
        assert!(err.is_err());
        assert_eq!(
            fork_context.as_deref(),
            Some("prior branch history"),
            "fork_context must remain intact when write_line fails"
        );

        let mut buffer = Vec::new();
        send_prompt_line(&mut buffer, &mut fork_context, "hello".to_owned()).unwrap();
        assert!(
            fork_context.is_none(),
            "fork_context should be consumed once write_line succeeds"
        );
        let written = String::from_utf8(buffer).unwrap();
        assert!(written.contains("prior branch history"));
        assert!(written.contains("hello"));
    }

    #[test]
    fn collect_bounded_stderr_lines_caps_count_and_line_size() {
        let mut input = String::new();
        for idx in 0..250 {
            input.push_str(&format!("line-{idx}\n"));
        }
        input.push_str(&"x".repeat(MAX_STDERR_LINE_BYTES * 3));
        input.push('\n');

        let lines = collect_bounded_stderr_lines(input.as_bytes());
        assert_eq!(lines.len(), MAX_STDERR_LINES);
        assert_eq!(lines[0], "line-123");
        assert_eq!(lines[MAX_STDERR_LINES - 2], "line-249");
        assert_eq!(lines[MAX_STDERR_LINES - 1].len(), MAX_STDERR_LINE_BYTES);
    }

    #[test]
    fn cancel_terminates_running_child_process() {
        #[cfg(windows)]
        let mut child = Command::new("cmd.exe")
            .args(["/d", "/c", "ping -n 30 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn windows child");
        #[cfg(not(windows))]
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unix child");

        let pid = child.id();
        #[cfg(windows)]
        let job = crate::process_tree::KillOnCloseJob::adopt(pid).ok();
        let turn_active = Arc::new(Mutex::new(true));
        let handle = Arc::new(AmpProcessHandle::new(
            pid,
            #[cfg(windows)]
            job,
            turn_active,
        ));
        let (commands, command_rx) = unbounded();
        let driver = AmpDriver {
            commands,
            process: handle.clone(),
        };

        driver.cancel();
        assert!(handle.was_cancelled());
        assert!(matches!(
            command_rx.try_recv(),
            Ok(CommandMessage::Shutdown)
        ));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let exited = loop {
            if let Ok(Some(_)) = child.try_wait() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            thread::sleep(Duration::from_millis(25));
        };
        handle.mark_exited();
        assert!(
            exited,
            "AmpDriver::cancel should terminate the child promptly"
        );
        // Calling terminate after mark_exited must be a safe no-op.
        handle.terminate();
    }

    fn test_exit_status(code: u32) -> std::process::ExitStatus {
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt as _;
            std::process::ExitStatus::from_raw(code)
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            std::process::ExitStatus::from_raw((code as i32) << 8)
        }
    }

    #[test]
    fn finalize_process_exit_preserves_error_summary_and_clears_cancel_summary() {
        // Unexpected mid-turn exit with stderr diagnostic keeps that summary.
        let (events, event_rx) = unbounded();
        let turn = Mutex::new(true);
        finalize_process_exit(
            Ok(test_exit_status(1)),
            false,
            Some("Amp: unauthorized".to_owned()),
            &turn,
            &events,
        );
        assert!(!*turn.lock());
        let seen = std::iter::from_fn(|| event_rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(seen.len(), 2);
        assert!(matches!(
            &seen[0],
            DriverEvent::TurnFinished {
                success: false,
                summary: Some(summary)
            } if summary == "Amp: unauthorized"
        ));
        assert!(matches!(&seen[1], DriverEvent::ProcessExited));

        // Unexpected mid-turn non-zero exit without stderr emits Error and reuses it as summary.
        let (events, event_rx) = unbounded();
        let turn = Mutex::new(true);
        let status = test_exit_status(2);
        let expected = tr!("errors.provider_exited", provider = "Amp", status = status);
        finalize_process_exit(Ok(status), false, None, &turn, &events);
        assert!(!*turn.lock());
        let seen = std::iter::from_fn(|| event_rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(seen.len(), 3);
        assert!(matches!(&seen[0], DriverEvent::Error(msg) if msg == &expected));
        assert!(matches!(
            &seen[1],
            DriverEvent::TurnFinished {
                success: false,
                summary: Some(summary)
            } if summary == &expected
        ));
        assert!(matches!(&seen[2], DriverEvent::ProcessExited));

        // Cancelled mid-turn exit suppresses Error and emits TurnFinished with summary: None.
        let (events, event_rx) = unbounded();
        let turn = Mutex::new(true);
        finalize_process_exit(
            Ok(test_exit_status(1)),
            true,
            Some("Amp: interrupted".to_owned()),
            &turn,
            &events,
        );
        assert!(!*turn.lock());
        let seen = std::iter::from_fn(|| event_rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(seen.len(), 2);
        assert!(matches!(
            &seen[0],
            DriverEvent::TurnFinished {
                success: false,
                summary: None
            }
        ));
        assert!(matches!(&seen[1], DriverEvent::ProcessExited));
    }
}
