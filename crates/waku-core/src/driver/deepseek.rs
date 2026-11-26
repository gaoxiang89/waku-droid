//! Native DeepSeek Harness driver.
//!
//! `dsh web` is the Harness client API: unary session operations travel over
//! typed HTTP envelopes while ordered session events, projections, approval
//! requests, questions, and background jobs arrive on its downlink streams.
//! Keeping that protocol intact gives Waku native resume/fork semantics and
//! avoids reverse-engineering the human CLI output.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::thread;

use anyhow::{Context as _, anyhow, bail};
use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};
use uuid::Uuid;

use super::activity;
use crate::deepseek_pool::PooledDeepSeekServer;
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityKind, BackgroundWorkEvent, BackgroundWorkItem, BackgroundWorkKind,
    BackgroundWorkStatus, DriverEvent, PermissionOption, ProviderResumeCursor, ReportedCommand,
    RuntimeMode, UserInputAnswer, UserInputOption, UserInputQuestion,
};

trait DeepSeekRpcClient {
    fn rpc(&self, method: &str, payload: Value) -> anyhow::Result<Value>;
    fn respond(&self, rpc_id: &str, value: Value) -> anyhow::Result<()>;
    fn reject_response(&self, rpc_id: &str, message: &str) -> anyhow::Result<()>;
}

impl DeepSeekRpcClient for PooledDeepSeekServer {
    fn rpc(&self, method: &str, payload: Value) -> anyhow::Result<Value> {
        (&**self).rpc(method, payload)
    }

    fn respond(&self, rpc_id: &str, value: Value) -> anyhow::Result<()> {
        (&**self).respond(rpc_id, value)
    }

    fn reject_response(&self, rpc_id: &str, message: &str) -> anyhow::Result<()> {
        (&**self).reject_response(rpc_id, message)
    }
}

/// DeepSeek Harness runs every non-FullAccess mode under
/// `/permission workspace-write`, which already permits in-workspace edits
/// natively and has no AI reviewer for `Auto`. Any `approval/requested` that
/// still reaches Waku outside `FullAccess` is an action beyond
/// `workspace-write` and must be decided by the user.
fn auto_approves_approval(mode: RuntimeMode) -> bool {
    mode == RuntimeMode::FullAccess
}

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
    ApplyOptions(SessionOptions),
    Shutdown,
}

enum PendingInteraction {
    Approval {
        rpc_id: String,
        approval_id: String,
    },
    Question {
        rpc_id: String,
        questions: Vec<DeepSeekPendingQuestion>,
    },
}

struct DeepSeekPendingQuestion {
    id: String,
    option_labels: HashSet<String>,
}

#[derive(Clone)]
struct ToolCallState {
    name: String,
    title: String,
    kind: ActivityKind,
    arguments: Value,
}

struct StreamState {
    last_seq: i64,
    turn_active: bool,
    command_names: HashSet<String>,
    completed_turn_seqs: Arc<Mutex<Vec<u64>>>,
    streamed_steps: HashSet<(u64, u64)>,
    tools: HashMap<String, ToolCallState>,
    pending: HashMap<String, PendingInteraction>,
}

struct HistorySnapshot {
    events: Vec<Value>,
    projection_values: Option<Value>,
    last_seq: i64,
    turn_active: bool,
    completed_turn_seqs: Vec<u64>,
}

pub struct DeepSeekDriver {
    // As with the other resident transports, Drop releases this lease before
    // waking the worker so final process teardown cannot block the UI thread.
    server: Option<PooledDeepSeekServer>,
    session_id: String,
    cwd: std::path::PathBuf,
    agent_preset: Option<String>,
    commands: Sender<CommandMessage>,
    completed_turn_seqs: Arc<Mutex<Vec<u64>>>,
}

impl DeepSeekDriver {
    pub fn start(options: DriverStartOptions, events: DriverEventSender) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            model,
            reasoning_effort,
            service_tier,
            context_window: _,
            agent_preset,
            computer_use_enabled: _,
            provider_cursor,
            config_dir: _,
        } = options;
        let (requested_session_id, resuming) = match provider_cursor {
            Some(ProviderResumeCursor::DeepSeek { session_id }) if !session_id.is_empty() => {
                (session_id, true)
            }
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume DeepSeek Harness from a {} cursor",
                    cursor.provider().display_name()
                ));
            }
            _ => (Uuid::new_v4().to_string(), false),
        };

        let server = crate::deepseek_pool::acquire(&binary)?;
        // Subscribe first. A create immediately publishes host and mux state,
        // and buffering that state closes the create/history race.
        let event_rx = server.subscribe(&requested_session_id);
        let created = server
            .rpc(
                "session.create",
                session_create_payload(
                    &cwd,
                    &requested_session_id,
                    if resuming {
                        None
                    } else {
                        agent_preset.as_deref()
                    },
                ),
            )
            .context("could not open a DeepSeek Harness session")?;
        let session_id = created
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("DeepSeek Harness returned no session ID"))?;
        if session_id != requested_session_id {
            bail!("DeepSeek Harness returned a different session ID than requested");
        }
        let selected_agent_preset = created
            .get("agentPreset")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(agent_preset);

        let baseline = fetch_history(&server, &session_id)
            .context("could not read DeepSeek Harness session history")?;
        normalize_legacy_read_only_mode(&server, &session_id, baseline.projection_values.as_ref())
            .context("could not normalize the DeepSeek Harness session")?;
        let available_commands = fetch_commands(&server, &session_id)
            .context("could not read DeepSeek Harness commands")?;
        let command_names = available_commands
            .iter()
            .map(|command| command.name.clone())
            .collect::<HashSet<_>>();
        let initial_options = SessionOptions {
            mode,
            model,
            reasoning_effort,
            service_tier,
            context_window: None,
        };
        apply_session_options(
            &server,
            &session_id,
            &initial_options,
            baseline.projection_values.as_ref(),
        )
        .context("could not configure the DeepSeek Harness session")?;
        // Selecting a model or changing a native command-backed option appends
        // durable state. Take the history cut after those operations so their
        // already-buffered stream frames are deduplicated by sequence.
        let history = fetch_history(&server, &session_id)
            .context("could not refresh DeepSeek Harness session history")?;
        let _ = events.send(DriverEvent::Connected {
            provider_cursor: Some(ProviderResumeCursor::DeepSeek {
                session_id: session_id.clone(),
            }),
        });
        let _ = events.send(DriverEvent::AgentPresetSelected(
            selected_agent_preset.clone(),
        ));
        let _ = events.send(DriverEvent::AvailableCommands(available_commands));
        if let Some(values) = history.projection_values.as_ref() {
            emit_projection_values(values, &events);
        }
        if history.turn_active {
            let _ = events.send(DriverEvent::TurnStarted);
        }

        let completed_turn_seqs = Arc::new(Mutex::new(history.completed_turn_seqs));
        let mode = Arc::new(Mutex::new(mode));
        let (commands, command_rx) = unbounded();
        let worker_server = server.clone();
        let worker_session_id = session_id.clone();
        let worker_events = events;
        let worker_mode = Arc::clone(&mode);
        let worker_completed_turn_seqs = Arc::clone(&completed_turn_seqs);
        thread::Builder::new()
            .name("waku-deepseek-driver".into())
            .spawn(move || {
                let mut state = StreamState {
                    last_seq: history.last_seq,
                    turn_active: history.turn_active,
                    command_names,
                    completed_turn_seqs: worker_completed_turn_seqs,
                    streamed_steps: HashSet::new(),
                    tools: HashMap::new(),
                    pending: HashMap::new(),
                };
                loop {
                    crossbeam_channel::select! {
                        recv(command_rx) -> message => {
                            let Ok(message) = message else { return; };
                            if !handle_command(
                                message,
                                &worker_server,
                                &worker_session_id,
                                &worker_events,
                                &worker_mode,
                                &mut state,
                            ) {
                                return;
                            }
                        }
                        recv(event_rx) -> envelope => {
                            let Ok(envelope) = envelope else { return; };
                            if !handle_envelope(
                                &envelope,
                                &worker_server,
                                &worker_session_id,
                                &worker_events,
                                &worker_mode,
                                &mut state,
                            ) {
                                return;
                            }
                        }
                    }
                }
            })?;

        Ok(Self {
            server: Some(server),
            session_id,
            cwd,
            agent_preset: selected_agent_preset,
            commands,
            completed_turn_seqs,
        })
    }
}

impl DriverControl for DeepSeekDriver {
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

    fn respond(&self, request_id: String, option_id: String) {
        let _ = self.commands.send(CommandMessage::Respond {
            request_id,
            option_id,
        });
    }

    fn respond_user_input(&self, request_id: String, answers: Vec<UserInputAnswer>) {
        let _ = self.commands.send(CommandMessage::RespondUserInput {
            request_id,
            answers,
        });
    }

    fn apply_options(&self, options: SessionOptions) -> bool {
        self.commands
            .send(CommandMessage::ApplyOptions(options))
            .is_ok()
    }

    fn rollback(&self, turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        if turns == 0 {
            return Ok(None);
        }
        self.fork(turns).map(Some)
    }

    fn fork(&self, turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
        let server = self
            .server
            .as_ref()
            .ok_or_else(|| anyhow!("DeepSeek Harness driver is shutting down"))?;
        fork_session_with_client(
            server,
            &self.session_id,
            &self.cwd,
            self.agent_preset.as_deref(),
            &self.completed_turn_seqs,
            turns_to_remove,
        )
    }
}

fn fork_session_with_client(
    server: &impl DeepSeekRpcClient,
    session_id: &str,
    cwd: &Path,
    agent_preset: Option<&str>,
    completed_turn_seqs: &Mutex<Vec<u64>>,
    turns_to_remove: usize,
) -> anyhow::Result<ProviderResumeCursor> {
    let fork_at_seq = {
        let turns = completed_turn_seqs.lock();
        let retained = turns.len().checked_sub(turns_to_remove).ok_or_else(|| {
            anyhow!(
                "cannot remove {turns_to_remove} turns from a {}-turn DeepSeek Harness session",
                turns.len()
            )
        })?;
        retained.checked_sub(1).map(|index| turns[index])
    };
    let forked_session_id = match fork_at_seq {
        None => {
            let new_session_id = Uuid::new_v4().to_string();
            let created = server.rpc(
                "session.create",
                session_create_payload(cwd, &new_session_id, agent_preset),
            )?;
            created
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("DeepSeek Harness returned no fork session ID"))?
        }
        Some(at_seq) => {
            let forked = server.rpc(
                "session.fork",
                json!({
                    "sessionId": session_id,
                    "atSeq": at_seq,
                }),
            )?;
            forked
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("DeepSeek Harness returned no fork session ID"))?
        }
    };
    Ok(ProviderResumeCursor::DeepSeek {
        session_id: forked_session_id,
    })
}

impl Drop for DeepSeekDriver {
    fn drop(&mut self) {
        drop(self.server.take());
        let _ = self.commands.send(CommandMessage::Shutdown);
    }
}

fn session_create_payload(
    cwd: &std::path::Path,
    session_id: &str,
    agent_preset: Option<&str>,
) -> Value {
    let mut payload = json!({
        "cwd": cwd.to_string_lossy(),
        "sessionId": session_id,
    });
    if let Some(agent_preset) = agent_preset {
        payload["agentPreset"] = Value::String(agent_preset.to_owned());
    }
    payload
}

fn handle_command(
    message: CommandMessage,
    server: &impl DeepSeekRpcClient,
    session_id: &str,
    events: &impl DriverEventSink,
    mode: &Mutex<RuntimeMode>,
    state: &mut StreamState,
) -> bool {
    match message {
        CommandMessage::Prompt(text) => {
            if !state.turn_active {
                state.turn_active = true;
                let _ = events.send(DriverEvent::TurnStarted);
            }
            if harness_command_name(&text).is_some_and(|name| state.command_names.contains(name)) {
                match execute_harness_command(server, session_id, &text) {
                    Ok(execution) => {
                        let summary = execution.text.or_else(|| {
                            execution
                                .success
                                .then(|| "DeepSeek Harness command completed".to_owned())
                        });
                        state.turn_active = false;
                        let _ = events.send(DriverEvent::TurnFinished {
                            success: execution.success,
                            summary,
                        });
                    }
                    Err(error) => {
                        state.turn_active = false;
                        let _ = events.send(DriverEvent::TurnFinished {
                            success: false,
                            summary: Some(format!(
                                "DeepSeek Harness rejected the command: {error}"
                            )),
                        });
                    }
                }
                return true;
            }
            match prompt(server, session_id, &text, "queue") {
                Ok(_) => {}
                Err(error) => {
                    state.turn_active = false;
                    let _ = events.send(DriverEvent::Error(format!(
                        "DeepSeek Harness rejected the prompt: {error}"
                    )));
                    let _ = events.send(DriverEvent::TurnFinished {
                        success: false,
                        summary: Some("DeepSeek Harness could not start the turn".into()),
                    });
                }
            }
        }
        CommandMessage::Steer(text) => match prompt(server, session_id, &text, "steer") {
            Ok(_) => {
                let _ = events.send(DriverEvent::SteerAccepted { message: text });
            }
            Err(error) => {
                let _ = events.send(DriverEvent::SteerRejected {
                    message: text,
                    reason: error.to_string(),
                });
            }
        },
        CommandMessage::Cancel => {
            if let Err(error) = server.rpc("session.cancel", json!({"sessionId": session_id})) {
                let _ = events.send(DriverEvent::Error(format!(
                    "DeepSeek Harness could not cancel the turn: {error}"
                )));
            }
        }
        CommandMessage::Respond {
            request_id,
            option_id,
        } => {
            let Some(pending) = state.pending.remove(&request_id) else {
                return true;
            };
            let response = match pending {
                PendingInteraction::Approval {
                    rpc_id,
                    approval_id,
                } => server.respond(
                    &rpc_id,
                    json!({
                        "sessionId": session_id,
                        "approvalId": approval_id,
                        "outcome": if option_id == "allow" { "allowed-once" } else { "rejected" },
                    }),
                ),
                PendingInteraction::Question { rpc_id, .. } => server.reject_response(
                    &rpc_id,
                    "a structured question requires a structured answer",
                ),
            };
            if let Err(error) = response {
                let _ = events.send(DriverEvent::Error(format!(
                    "DeepSeek Harness rejected the interaction response: {error}"
                )));
            }
        }
        CommandMessage::RespondUserInput {
            request_id,
            answers,
        } => {
            let Some(PendingInteraction::Question { rpc_id, questions }) =
                state.pending.remove(&request_id)
            else {
                return true;
            };
            let answers = deepseek_question_answers(&questions, &answers);
            if let Err(error) = server.respond(
                &rpc_id,
                json!({
                    "sessionId": session_id,
                    "answer": {"answers": answers}
                }),
            ) {
                let _ = events.send(DriverEvent::Error(format!(
                    "DeepSeek Harness rejected the question response: {error}"
                )));
            }
        }
        CommandMessage::ApplyOptions(options) => {
            let projections = fetch_history(server, session_id)
                .ok()
                .and_then(|history| history.projection_values);
            match apply_session_options(server, session_id, &options, projections.as_ref()) {
                Ok(()) => *mode.lock() = options.mode,
                Err(error) => {
                    let _ = events.send(DriverEvent::Error(format!(
                        "DeepSeek Harness could not apply the session options: {error}"
                    )));
                }
            }
        }
        CommandMessage::Shutdown => return false,
    }
    true
}

fn deepseek_question_answers(
    questions: &[DeepSeekPendingQuestion],
    answers: &[UserInputAnswer],
) -> Vec<Value> {
    questions
        .iter()
        .map(|question| {
            let submitted = answers
                .iter()
                .find(|answer| answer.question_id == question.id)
                .map(|answer| answer.answers.as_slice())
                .unwrap_or_default();
            let selected = submitted
                .iter()
                .filter(|answer| question.option_labels.contains(answer.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let custom = submitted
                .iter()
                .filter(|answer| !question.option_labels.contains(answer.as_str()))
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let mut answer = json!({"id": question.id, "selected": selected});
            if !custom.is_empty() {
                answer["custom"] = Value::String(custom);
            }
            answer
        })
        .collect()
}

fn handle_envelope(
    envelope: &Value,
    server: &impl DeepSeekRpcClient,
    session_id: &str,
    events: &impl DriverEventSink,
    mode: &Mutex<RuntimeMode>,
    state: &mut StreamState,
) -> bool {
    let payload = envelope.get("payload").unwrap_or(envelope);
    match payload.get("type").and_then(Value::as_str) {
        Some("session/event") => {
            if let Some(event) = payload.get("event") {
                handle_session_event(event, payload.get("view"), events, state);
            }
        }
        Some("session/subscribed") => {
            let remote_last_seq = payload.get("lastSeq").and_then(Value::as_i64).unwrap_or(-1);
            if remote_last_seq > state.last_seq {
                match fetch_history(server, session_id) {
                    Ok(history) => {
                        for entry in history.events {
                            let event = entry.get("event").unwrap_or(&entry);
                            let view = entry.get("view");
                            handle_session_event(event, view, events, state);
                        }
                        if let Some(values) = history.projection_values.as_ref() {
                            emit_projection_values(values, events);
                        }
                    }
                    Err(error) => {
                        let _ = events.send(DriverEvent::Error(format!(
                            "DeepSeek Harness could not recover its event stream: {error}"
                        )));
                    }
                }
            }
        }
        Some("approval/requested") => {
            handle_approval_request(envelope, payload, server, session_id, events, mode, state);
        }
        Some("approval/resolved") => {
            if let Some(approval_id) = payload.get("approvalId").and_then(Value::as_str) {
                state.pending.retain(|_, pending| {
                    !matches!(pending, PendingInteraction::Approval { approval_id: pending_id, .. } if pending_id == approval_id)
                });
            }
        }
        Some("question/requested") => {
            handle_question_request(envelope, payload, server, session_id, events, state);
        }
        Some("question/resolved") => {
            if let Some(question_rpc_id) = payload.get("questionRpcId").and_then(Value::as_str) {
                state.pending.remove(question_rpc_id);
            }
        }
        Some("session/jobs") => emit_jobs(payload, events),
        Some("session/projection") => {
            if let (Some(key), Some(value)) = (
                payload.get("key").and_then(Value::as_str),
                payload.get("value"),
            ) {
                emit_projection(key, value, events);
            }
        }
        Some("host/remote-event")
            if payload.get("event").and_then(Value::as_str) == Some("commands/change") =>
        {
            match fetch_commands(server, session_id) {
                Ok(commands) => {
                    state.command_names = commands
                        .iter()
                        .map(|command| command.name.clone())
                        .collect();
                    let _ = events.send(DriverEvent::AvailableCommands(commands));
                }
                Err(error) => {
                    let _ = events.send(DriverEvent::Error(format!(
                        "DeepSeek Harness could not refresh its commands: {error}"
                    )));
                }
            }
        }
        Some("host/agent-error") => {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("DeepSeek Harness agent failed");
            let _ = events.send(DriverEvent::Error(message.to_owned()));
        }
        Some("stream/error") => {
            let message = payload
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("DeepSeek Harness event stream failed");
            let _ = events.send(DriverEvent::Error(message.to_owned()));
        }
        Some("waku/process-exited") => {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("DeepSeek Harness exited");
            state.pending.clear();
            let _ = events.send(DriverEvent::Error(message.to_owned()));
            if state.turn_active {
                state.turn_active = false;
                let _ = events.send(DriverEvent::TurnFinished {
                    success: false,
                    summary: Some(message.to_owned()),
                });
            }
            let _ = events.send(DriverEvent::ProcessExited);
            return false;
        }
        _ => {}
    }
    true
}

fn handle_session_event(
    event: &Value,
    view: Option<&Value>,
    events: &impl DriverEventSink,
    state: &mut StreamState,
) {
    let Some(seq) = event.get("seq").and_then(Value::as_u64) else {
        return;
    };
    if i64::try_from(seq).is_ok_and(|seq| seq <= state.last_seq) {
        return;
    }
    state.last_seq = i64::try_from(seq).unwrap_or(i64::MAX);
    let data = event.get("data").unwrap_or(&Value::Null);
    match event.get("type").and_then(Value::as_str) {
        Some("turn/start") => {
            if !state.turn_active {
                state.turn_active = true;
                let _ = events.send(DriverEvent::TurnStarted);
            }
        }
        Some("turn/end") => {
            let mut turns = state.completed_turn_seqs.lock();
            if turns.last().copied() != Some(seq) {
                turns.push(seq);
            }
            drop(turns);
            let reason = data.pointer("/reason/kind").and_then(Value::as_str);
            let success = matches!(reason, Some("completed" | "max-tokens"));
            let summary = turn_end_summary(data, reason);
            if !success && let Some(summary) = summary.as_ref() {
                let _ = events.send(DriverEvent::Error(summary.clone()));
            }
            if state.turn_active {
                state.turn_active = false;
                let _ = events.send(DriverEvent::TurnFinished { success, summary });
            }
        }
        Some("assistant/chunk") => {
            let chunk = data.get("chunk").unwrap_or(&Value::Null);
            let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(0);
            let step = data.get("step").and_then(Value::as_u64).unwrap_or(0);
            match chunk.get("type").and_then(Value::as_str) {
                Some("text-delta") => {
                    state.streamed_steps.insert((turn, step));
                    if let Some(text) = chunk.get("text").and_then(Value::as_str) {
                        let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                    }
                }
                Some("reasoning-delta") => {
                    state.streamed_steps.insert((turn, step));
                    if let Some(text) = chunk.get("text").and_then(Value::as_str) {
                        let _ = events.send(DriverEvent::ReasoningDelta(text.to_owned()));
                    }
                }
                Some("usage") => emit_usage(chunk.get("usage"), events),
                _ => {}
            }
        }
        Some("assistant/message") => {
            let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(0);
            let step = data.get("step").and_then(Value::as_u64).unwrap_or(0);
            if !state.streamed_steps.remove(&(turn, step)) {
                for block in data
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let text = block.get("text").and_then(Value::as_str);
                    match (block.get("type").and_then(Value::as_str), text) {
                        (Some("text"), Some(text)) => {
                            let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                        }
                        (Some("reasoning"), Some(text)) => {
                            let _ = events.send(DriverEvent::ReasoningDelta(text.to_owned()));
                        }
                        _ => {}
                    }
                }
            }
            emit_usage(data.get("usage"), events);
        }
        Some("tool/call") => handle_tool_call(data, view, events, state),
        Some("tool/result") => handle_tool_result(data, view, events, state),
        Some("todo/write") => {
            let todos = data.get("todos").cloned().unwrap_or_else(|| json!([]));
            let item = activity::tool_activity(
                Some(format!("deepseek-todo-{seq}")),
                ActivityKind::Plan,
                "Plan updated".into(),
                None,
                Some(&todos),
                None,
                false,
                true,
            );
            let _ = events.send(DriverEvent::RichActivity(item));
        }
        Some("request/context") => {
            let window = data
                .get("contextWindow")
                .and_then(Value::as_u64)
                .filter(|window| *window > 0);
            if window.is_some() {
                let _ = events.send(DriverEvent::UsageUpdated {
                    context_tokens: None,
                    context_window: window,
                });
            }
        }
        Some("session/title") => {
            let title = data
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_owned);
            let _ = events.send(DriverEvent::AutoTitleUpdated(title));
        }
        _ => {}
    }
}

fn handle_tool_call(
    data: &Value,
    view: Option<&Value>,
    events: &impl DriverEventSink,
    state: &mut StreamState,
) {
    let Some(call_id) = data
        .get("callId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let name = data
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_owned();
    let arguments = data
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|arguments| serde_json::from_str(arguments).ok())
        .unwrap_or_else(|| data.get("arguments").cloned().unwrap_or(Value::Null));
    let presented = view.and_then(|view| view.get("view"));
    let title = presented
        .and_then(|view| view.get("title"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| activity::input_title(Some(&arguments)))
        .unwrap_or_else(|| name.clone());
    let kind = presented
        .and_then(presented_activity_kind)
        .unwrap_or_else(|| super::support::classify_tool(&name));
    state.tools.insert(
        call_id.clone(),
        ToolCallState {
            name,
            title: title.clone(),
            kind,
            arguments: arguments.clone(),
        },
    );
    let item = activity::tool_activity(
        Some(call_id),
        kind,
        title,
        Some(&arguments),
        None,
        presented,
        false,
        false,
    )
    .with_tool_name(data.get("name").and_then(Value::as_str));
    let _ = events.send(DriverEvent::RichActivity(item));
}

fn handle_tool_result(
    data: &Value,
    view: Option<&Value>,
    events: &impl DriverEventSink,
    state: &mut StreamState,
) {
    let call_id = data
        .pointer("/message/source/callId")
        .or_else(|| data.pointer("/message/content/0/toolCallId"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let stored = call_id
        .as_ref()
        .and_then(|call_id| state.tools.remove(call_id));
    let presented = view.and_then(|view| view.get("view"));
    let name = stored
        .as_ref()
        .map(|tool| tool.name.as_str())
        .unwrap_or("tool");
    let kind = stored
        .as_ref()
        .map(|tool| tool.kind)
        .or_else(|| presented.and_then(presented_activity_kind))
        .unwrap_or_else(|| super::support::classify_tool(name));
    let title = presented
        .and_then(|view| view.get("title"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| stored.as_ref().map(|tool| tool.title.clone()))
        .unwrap_or_else(|| name.to_owned());
    let arguments = stored.as_ref().map(|tool| &tool.arguments);
    let output = data.pointer("/message/content");
    let failed = data.get("error").is_some_and(|error| !error.is_null())
        || data
            .pointer("/message/content/0/isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let item = activity::tool_activity(
        call_id, kind, title, arguments, output, presented, failed, true,
    )
    .with_tool_name(stored.as_ref().map(|tool| tool.name.as_str()));
    let _ = events.send(DriverEvent::RichActivity(item));
}

fn presented_activity_kind(view: &Value) -> Option<ActivityKind> {
    match view.get("card").and_then(Value::as_str) {
        Some("terminal") => Some(ActivityKind::Command),
        Some("diff") => Some(ActivityKind::FileChange),
        Some("generic") => match view.get("kind").and_then(Value::as_str) {
            Some("read") => Some(ActivityKind::FileRead),
            Some("edit" | "delete" | "move") => Some(ActivityKind::FileChange),
            Some("search") => Some(ActivityKind::FileSearch),
            Some("execute") => Some(ActivityKind::Command),
            Some("fetch") => Some(ActivityKind::Search),
            _ => Some(ActivityKind::Tool),
        },
        Some("search" | "search-matches" | "search-paths") => Some(ActivityKind::FileSearch),
        Some("read") => Some(ActivityKind::FileRead),
        Some("web" | "web-search" | "web-fetch") => Some(ActivityKind::Search),
        _ => None,
    }
}

const MAX_APPROVAL_SUMMARY_CHARS: usize = 400;

fn truncate_summary(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_APPROVAL_SUMMARY_CHARS {
        return trimmed.to_owned();
    }
    let mut short = trimmed
        .chars()
        .take(MAX_APPROVAL_SUMMARY_CHARS)
        .collect::<String>();
    short.push('…');
    short
}

fn parsed_approval_arguments(payload: &Value, state: &StreamState) -> Option<Value> {
    if let Some(raw) = payload
        .get("arguments")
        .or_else(|| payload.get("args"))
        .or_else(|| payload.get("input"))
        .filter(|value| !value.is_null())
    {
        return Some(
            raw.as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .unwrap_or_else(|| raw.clone()),
        );
    }
    payload
        .get("callId")
        .or_else(|| payload.get("toolCallId"))
        .and_then(Value::as_str)
        .and_then(|call_id| state.tools.get(call_id))
        .map(|tool| tool.arguments.clone())
        .filter(|value| !value.is_null())
}

fn approval_arguments_summary(payload: &Value, state: &StreamState) -> Option<String> {
    for key in ["command", "cmd", "filePath", "file_path", "path"] {
        if let Some(text) = payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return Some(truncate_summary(text));
        }
    }
    let arguments = parsed_approval_arguments(payload, state)?;
    if let Some(text) = arguments.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(truncate_summary(text));
    }
    if let Some(title) = activity::input_title(Some(&arguments)) {
        return Some(truncate_summary(&title));
    }
    for key in [
        "command",
        "cmd",
        "file_path",
        "filePath",
        "path",
        "url",
        "query",
        "pattern",
        "description",
    ] {
        if let Some(text) = arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return Some(truncate_summary(text));
        }
    }
    let is_empty_collection = arguments.as_object().is_some_and(|map| map.is_empty())
        || arguments.as_array().is_some_and(|items| items.is_empty());
    if is_empty_collection {
        return None;
    }
    serde_json::to_string(&arguments)
        .ok()
        .filter(|serialized| !serialized.is_empty())
        .map(|serialized| truncate_summary(&serialized))
}

fn approval_detail(payload: &Value, state: &StreamState) -> String {
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    let summary = approval_arguments_summary(payload, state);
    match (reason, summary) {
        (Some(reason), Some(summary)) if !reason.contains(&summary) => {
            format!("{reason}\n{summary}")
        }
        (Some(reason), _) => reason.to_owned(),
        (None, Some(summary)) => summary,
        (None, None) => "DeepSeek Harness is asking for permission to continue".to_owned(),
    }
}

fn handle_approval_request(
    envelope: &Value,
    payload: &Value,
    server: &impl DeepSeekRpcClient,
    session_id: &str,
    events: &impl DriverEventSink,
    mode: &Mutex<RuntimeMode>,
    state: &mut StreamState,
) {
    let Some(rpc_id) = envelope.get("rpcId").and_then(Value::as_str) else {
        return;
    };
    let Some(approval_id) = payload.get("approvalId").and_then(Value::as_str) else {
        return;
    };
    if auto_approves_approval(*mode.lock()) {
        if let Err(error) = server.respond(
            rpc_id,
            json!({
                "sessionId": session_id,
                "approvalId": approval_id,
                "outcome": "allowed-once",
            }),
        ) {
            let _ = events.send(DriverEvent::Error(format!(
                "DeepSeek Harness rejected an automatic approval: {error}"
            )));
        }
        return;
    }

    state.pending.insert(
        rpc_id.to_owned(),
        PendingInteraction::Approval {
            rpc_id: rpc_id.to_owned(),
            approval_id: approval_id.to_owned(),
        },
    );
    let tool_name = payload
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let detail = approval_detail(payload, state);
    let _ = events.send(DriverEvent::Permission {
        request_id: rpc_id.to_owned(),
        title: format!("Allow {tool_name}?"),
        detail,
        options: vec![
            PermissionOption {
                id: "allow".into(),
                label: tr!("permission.allow_once"),
                allow: true,
            },
            PermissionOption {
                id: "reject".into(),
                label: tr!("common.deny"),
                allow: false,
            },
        ],
    });
}

fn handle_question_request(
    envelope: &Value,
    payload: &Value,
    server: &impl DeepSeekRpcClient,
    session_id: &str,
    events: &impl DriverEventSink,
    state: &mut StreamState,
) {
    let Some(rpc_id) = envelope.get("rpcId").and_then(Value::as_str) else {
        return;
    };
    let Some(questions) = payload.get("questions").and_then(Value::as_array) else {
        return;
    };
    let mut pending_questions = Vec::new();
    let visible_questions = questions
        .iter()
        .enumerate()
        .filter_map(|(index, question)| {
            let text = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            if text.is_empty() {
                return None;
            }
            let id = question
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("question-{index}"));
            let options = question
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
                            .or_else(|| option.get("detail"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|description| !description.is_empty())
                            .map(str::to_owned),
                    })
                })
                .collect::<Vec<_>>();
            pending_questions.push(DeepSeekPendingQuestion {
                id: id.clone(),
                option_labels: options.iter().map(|option| option.label.clone()).collect(),
            });
            let detail = question
                .get("detail")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|detail| !detail.is_empty());
            Some(UserInputQuestion {
                id,
                header: question
                    .get("header")
                    .and_then(Value::as_str)
                    .filter(|header| !header.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Question {}", index + 1)),
                question: detail
                    .map(|detail| format!("{text}\n\n{detail}"))
                    .unwrap_or_else(|| text.to_owned()),
                options,
                multi_select: question
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                required: true,
                accepts_custom: true,
            })
        })
        .collect::<Vec<_>>();
    if visible_questions.is_empty() {
        let _ = server.reject_response(rpc_id, "the question request contained no questions");
        return;
    }
    state.pending.insert(
        rpc_id.to_owned(),
        PendingInteraction::Question {
            rpc_id: rpc_id.to_owned(),
            questions: pending_questions,
        },
    );
    let _ = events.send(DriverEvent::UserInputRequested {
        request_id: rpc_id.to_owned(),
        questions: visible_questions,
    });

    // Keep the argument intentionally used: every answer includes the exact
    // session the Host used to scope this question.
    let _ = session_id;
}

fn emit_jobs(payload: &Value, events: &impl DriverEventSink) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let items = payload
        .get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|job| {
            let id = job.get("id").and_then(Value::as_str)?;
            let provider_kind = job.get("kind").and_then(Value::as_str).unwrap_or("process");
            let kind = if provider_kind.contains("subagent") {
                BackgroundWorkKind::Subagent
            } else if provider_kind.contains("monitor") {
                BackgroundWorkKind::Monitor
            } else {
                BackgroundWorkKind::Process
            };
            let status = match job.get("status").and_then(Value::as_str) {
                Some("stopping") => BackgroundWorkStatus::Stopping,
                Some("completed") => BackgroundWorkStatus::Completed,
                Some("killed") => BackgroundWorkStatus::Stopped,
                Some("failed") => BackgroundWorkStatus::Failed,
                _ => BackgroundWorkStatus::Running,
            };
            let label = job
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or(provider_kind);
            let mut item = BackgroundWorkItem::new(kind, id, label, status);
            item.detail = job.get("detail").and_then(Value::as_str).map(str::to_owned);
            item.started_at_ms = job.get("startedAt").and_then(Value::as_u64).unwrap_or(now);
            let finished_at = job.get("finishedAt").and_then(Value::as_u64);
            item.updated_at_ms = finished_at.unwrap_or(now);
            item.duration_ms =
                finished_at.map(|finished| finished.saturating_sub(item.started_at_ms));
            item.command = matches!(provider_kind, "bash" | "pwsh").then(|| label.to_owned());
            item.background = true;
            // rc.6 exposes no general job-stop RPC. Do not pretend a UI stop
            // button can control work the Host has not made controllable.
            item.can_stop = false;
            Some(item)
        })
        .collect();
    let _ = events.send(DriverEvent::BackgroundWork(
        BackgroundWorkEvent::ReconcileLive { items },
    ));
}

fn emit_projection_values(values: &Value, events: &impl DriverEventSink) {
    let Some(values) = values.as_object() else {
        return;
    };
    for (key, value) in values {
        emit_projection(key, value, events);
    }
}

fn emit_projection(key: &str, value: &Value, events: &impl DriverEventSink) {
    match key {
        "title" => {
            let title = value
                .as_str()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_owned);
            let _ = events.send(DriverEvent::AutoTitleUpdated(title));
        }
        "contextPressure" => {
            let tokens = value
                .get("projectedTokens")
                .or_else(|| value.get("pressureTokens"))
                .and_then(Value::as_u64);
            let window = value.get("contextWindow").and_then(Value::as_u64);
            if tokens.is_some() || window.is_some() {
                let _ = events.send(DriverEvent::UsageUpdated {
                    context_tokens: tokens,
                    context_window: window,
                });
            }
        }
        _ => {}
    }
}

fn emit_usage(usage: Option<&Value>, events: &impl DriverEventSink) {
    let Some(usage) = usage else {
        return;
    };
    let total = [
        "inputTokens",
        "outputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
    ]
    .into_iter()
    .filter_map(|field| usage.get(field).and_then(Value::as_u64))
    .fold(0_u64, u64::saturating_add);
    if total > 0 {
        let _ = events.send(DriverEvent::UsageUpdated {
            context_tokens: Some(total),
            context_window: None,
        });
    }
}

fn turn_end_summary(data: &Value, reason: Option<&str>) -> Option<String> {
    match reason {
        Some("completed") => None,
        Some("max-tokens") => Some("DeepSeek Harness reached the model output limit".into()),
        Some("aborted") => Some("DeepSeek Harness turn was cancelled".into()),
        Some("blocked") => Some("DeepSeek Harness blocked the turn".into()),
        Some("error") => Some(
            data.pointer("/reason/error/message")
                .and_then(Value::as_str)
                .unwrap_or("DeepSeek Harness turn failed")
                .to_owned(),
        ),
        Some("interrupted") => Some("DeepSeek Harness recovered an interrupted turn".into()),
        Some(other) => Some(format!("DeepSeek Harness ended the turn: {other}")),
        None => Some("DeepSeek Harness ended the turn without a reason".into()),
    }
}

fn prompt(
    server: &impl DeepSeekRpcClient,
    session_id: &str,
    text: &str,
    mode: &str,
) -> anyhow::Result<Value> {
    server.rpc(
        "session.prompt",
        json!({
            "sessionId": session_id,
            "mode": mode,
            "content": [{"type": "text", "text": text}],
        }),
    )
}

struct HarnessCommandExecution {
    success: bool,
    text: Option<String>,
}

fn harness_command_name(line: &str) -> Option<&str> {
    line.strip_prefix('/')?
        .split_once(char::is_whitespace)
        .map(|(name, _)| name)
        .or_else(|| line.strip_prefix('/'))
        .filter(|name| !name.is_empty())
}

fn fetch_commands(
    server: &impl DeepSeekRpcClient,
    session_id: &str,
) -> anyhow::Result<Vec<ReportedCommand>> {
    let commands = server.rpc("commands/list", json!({"args": {"agentId": session_id}}))?;
    Ok(commands
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|command| {
            Some(ReportedCommand {
                name: command.get("name")?.as_str()?.to_owned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                argument_hint: None,
            })
        })
        .collect())
}

fn execute_harness_command(
    server: &impl DeepSeekRpcClient,
    session_id: &str,
    line: &str,
) -> anyhow::Result<HarnessCommandExecution> {
    let execution = server
        .rpc(
            "commands/execute",
            json!({"args": {"agentId": session_id, "line": line, "images": []}}),
        )
        .or_else(|error| {
            // Harness 0.1.1 made the image list a required command argument. Its
            // older strict descriptor rejects that field, so retry only that
            // compatibility failure with the legacy payload.
            let message = error.to_string();
            if message.contains("args fields do not match the descriptor")
                && message.contains("images")
            {
                server.rpc(
                    "commands/execute",
                    json!({"args": {"agentId": session_id, "line": line}}),
                )
            } else {
                Err(error)
            }
        })?;
    if execution.is_null() {
        bail!("unknown or malformed command: {line}");
    }
    let result = execution
        .get("result")
        .ok_or_else(|| anyhow!("DeepSeek Harness returned no command result"))?;
    let kind = result
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("DeepSeek Harness returned an invalid command result"))?;
    let text = result
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned);
    match kind {
        "success" => Ok(HarnessCommandExecution {
            success: true,
            text,
        }),
        "error" => Ok(HarnessCommandExecution {
            success: false,
            text: Some(text.unwrap_or_else(|| "DeepSeek Harness command failed".to_owned())),
        }),
        other => bail!("DeepSeek Harness returned unknown command result {other}"),
    }
}

fn apply_session_options(
    server: &impl DeepSeekRpcClient,
    session_id: &str,
    options: &SessionOptions,
    projections: Option<&Value>,
) -> anyhow::Result<()> {
    if options.model.is_some() || options.reasoning_effort.is_some() {
        let (provider, model) = match options.model.as_deref() {
            Some(model) => match model.split_once('/') {
                Some((provider, model)) => (provider.to_owned(), model.to_owned()),
                None => {
                    let catalog = server.rpc("session.models", json!({"sessionId": session_id}))?;
                    let provider = catalog
                        .pointer("/current/provider")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| {
                            anyhow!("DeepSeek Harness returned no current model provider")
                        })?
                        .to_owned();
                    (provider, model.to_owned())
                }
            },
            None => {
                let catalog = server.rpc("session.models", json!({"sessionId": session_id}))?;
                let provider = catalog
                    .pointer("/current/provider")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow!("DeepSeek Harness returned no current model provider"))?
                    .to_owned();
                let model = catalog
                    .pointer("/current/model")
                    .or_else(|| catalog.pointer("/current/id"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow!("DeepSeek Harness returned no current model"))?
                    .to_owned();
                (provider, model)
            }
        };
        let mut selection = json!({
            "sessionId": session_id,
            "provider": provider,
            "model": model,
        });
        if let Some(reasoning_effort) = options.reasoning_effort.as_deref() {
            selection["reasoningEffort"] = json!(reasoning_effort);
        }
        server.rpc("session.selectModel", selection)?;
    }

    let permission = if options.mode == RuntimeMode::FullAccess {
        "danger-full-access"
    } else {
        "workspace-write"
    };
    let current_permission = projections
        .and_then(|values| values.pointer("/permissions/currentValue"))
        .and_then(Value::as_str);
    if current_permission != Some(permission) {
        let execution =
            execute_harness_command(server, session_id, &format!("/permission {permission}"))?;
        if !execution.success {
            bail!(
                "permission command failed: {}",
                execution.text.unwrap_or_else(|| "unknown error".to_owned())
            );
        }
    }
    Ok(())
}

/// Older sessions can retain Harness's command-backed read-only state. Clear
/// it once while attaching; ordinary option changes no longer know about or
/// manipulate that state.
fn normalize_legacy_read_only_mode(
    server: &impl DeepSeekRpcClient,
    session_id: &str,
    projections: Option<&Value>,
) -> anyhow::Result<()> {
    if !legacy_read_only_mode_active(projections) {
        return Ok(());
    }
    let execution = execute_harness_command(server, session_id, "/plan off")?;
    if execution.success {
        Ok(())
    } else {
        bail!(
            "read-only cleanup command failed: {}",
            execution.text.unwrap_or_else(|| "unknown error".to_owned())
        )
    }
}

fn legacy_read_only_mode_active(projections: Option<&Value>) -> bool {
    let active = projections
        .and_then(|values| values.pointer("/plan/active"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pending = projections
        .and_then(|values| values.pointer("/plan/pending"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    active || pending
}

fn fetch_history(
    server: &impl DeepSeekRpcClient,
    session_id: &str,
) -> anyhow::Result<HistorySnapshot> {
    let mut entries = Vec::new();
    let mut projection_values = None;
    let mut before_seq = None;
    let mut seen = HashSet::new();
    loop {
        let mut payload = json!({"sessionId": session_id, "maxMessages": 200});
        if let Some(before_seq) = before_seq {
            payload["beforeSeq"] = json!(before_seq);
        }
        let page = server.rpc("session.history", payload)?;
        if projection_values.is_none() {
            projection_values = page.pointer("/projections/values").cloned();
        }
        let page_entries = page
            .get("events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let oldest = page_entries
            .iter()
            .filter_map(|entry| {
                entry
                    .pointer("/event/seq")
                    .or_else(|| entry.get("seq"))
                    .and_then(Value::as_u64)
            })
            .min();
        entries.extend(page_entries);
        if !page
            .get("hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            break;
        }
        let Some(oldest) = oldest.filter(|oldest| *oldest > 0) else {
            break;
        };
        if !seen.insert(oldest) || before_seq.is_some_and(|previous| oldest >= previous) {
            break;
        }
        before_seq = Some(oldest);
    }
    entries.sort_by_key(|entry| {
        entry
            .pointer("/event/seq")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX)
    });
    entries.dedup_by_key(|entry| {
        entry
            .pointer("/event/seq")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX)
    });

    let mut last_seq = -1_i64;
    let mut last_turn_start = None;
    let mut last_turn_end = None;
    let mut completed_turn_seqs = Vec::new();
    for entry in &entries {
        let event = entry.get("event").unwrap_or(entry);
        let Some(seq) = event.get("seq").and_then(Value::as_u64) else {
            continue;
        };
        last_seq = last_seq.max(i64::try_from(seq).unwrap_or(i64::MAX));
        match event.get("type").and_then(Value::as_str) {
            Some("turn/start") => last_turn_start = Some(seq),
            Some("turn/end") => {
                last_turn_end = Some(seq);
                completed_turn_seqs.push(seq);
            }
            _ => {}
        }
    }
    Ok(HistorySnapshot {
        events: entries,
        projection_values,
        last_seq,
        turn_active: last_turn_start
            .is_some_and(|start| last_turn_end.is_none_or(|end| start > end)),
        completed_turn_seqs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_read_only_projection_is_normalized_on_attach() {
        assert!(legacy_read_only_mode_active(Some(&json!({
            "plan": {"active": true, "pending": false}
        }))));
        assert!(legacy_read_only_mode_active(Some(&json!({
            "plan": {"active": false, "pending": true}
        }))));
        assert!(!legacy_read_only_mode_active(Some(&json!({
            "plan": {"active": false, "pending": false}
        }))));
    }

    #[test]
    fn question_answers_separate_provider_options_from_custom_text() {
        let questions = vec![DeepSeekPendingQuestion {
            id: "files".into(),
            option_labels: ["Source".to_owned(), "Tests".to_owned()]
                .into_iter()
                .collect(),
        }];
        let answers = deepseek_question_answers(
            &questions,
            &[UserInputAnswer {
                question_id: "files".into(),
                answers: vec!["Source".into(), "Also update docs".into()],
            }],
        );

        assert_eq!(answers[0]["selected"], json!(["Source"]));
        assert_eq!(answers[0]["custom"], "Also update docs");
    }

    fn harness() -> (
        Sender<DriverEvent>,
        crossbeam_channel::Receiver<DriverEvent>,
        StreamState,
    ) {
        let (events, event_rx) = unbounded();
        let state = StreamState {
            last_seq: -1,
            turn_active: false,
            command_names: HashSet::new(),
            completed_turn_seqs: Arc::new(Mutex::new(Vec::new())),
            streamed_steps: HashSet::new(),
            tools: HashMap::new(),
            pending: HashMap::new(),
        };
        (events, event_rx, state)
    }

    #[test]
    fn recognizes_only_leading_harness_commands() {
        assert_eq!(harness_command_name("/plan"), Some("plan"));
        assert_eq!(
            harness_command_name("/permission workspace-write"),
            Some("permission")
        );
        assert_eq!(harness_command_name("explain /plan"), None);
        assert_eq!(harness_command_name("/"), None);
    }

    #[test]
    fn fresh_session_creation_carries_the_agent_preset() {
        assert_eq!(
            session_create_payload(
                std::path::Path::new("/tmp/project"),
                "session-1",
                Some("code")
            ),
            json!({
                "cwd": "/tmp/project",
                "sessionId": "session-1",
                "agentPreset": "code"
            })
        );
        assert!(
            session_create_payload(std::path::Path::new("/tmp/project"), "session-2", None)
                .get("agentPreset")
                .is_none()
        );
    }

    #[test]
    fn native_turn_and_text_events_settle_once() {
        let (events, event_rx, mut state) = harness();
        handle_session_event(
            &json!({"type":"turn/start","seq":0,"time":1,"data":{"turn":0}}),
            None,
            &events,
            &mut state,
        );
        handle_session_event(
            &json!({
                "type":"assistant/chunk","seq":1,"time":2,
                "data":{"turn":0,"step":0,"chunk":{"type":"text-delta","index":0,"text":"OK"}}
            }),
            None,
            &events,
            &mut state,
        );
        handle_session_event(
            &json!({
                "type":"assistant/message","seq":2,"time":3,
                "data":{"turn":0,"step":0,"message":{"content":[{"type":"text","text":"OK"}]}}
            }),
            None,
            &events,
            &mut state,
        );
        handle_session_event(
            &json!({"type":"turn/end","seq":3,"time":4,"data":{"turn":0,"reason":{"kind":"completed"}}}),
            None,
            &events,
            &mut state,
        );

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TextDelta(text) if text == "OK"));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(
            event_rx.try_recv().is_err(),
            "assembled message must not duplicate streamed text"
        );
        assert_eq!(&*state.completed_turn_seqs.lock(), &[3]);
    }

    #[test]
    fn tool_views_preserve_native_title_and_kind() {
        let (events, event_rx, mut state) = harness();
        handle_session_event(
            &json!({
                "type":"tool/call","seq":1,"time":1,
                "data":{"turn":0,"step":0,"callId":"call-1","name":"bash","arguments":"{\"command\":\"pwd\"}"}
            }),
            Some(&json!({"for":"call","view":{"card":"terminal","title":"pwd","cwd":"/tmp"}})),
            &events,
            &mut state,
        );
        handle_session_event(
            &json!({
                "type":"tool/result","seq":2,"time":2,
                "data":{"turn":0,"step":0,"message":{"source":{"kind":"tool","callId":"call-1"},"content":[{"type":"tool-result","toolCallId":"call-1","content":[{"type":"text","text":"/tmp"}]}]}}
            }),
            Some(&json!({"for":"result","view":{"card":"terminal","output":"/tmp","exitCode":0}})),
            &events,
            &mut state,
        );

        let DriverEvent::RichActivity(started) = event_rx.recv().unwrap() else {
            panic!("expected the pending tool activity");
        };
        assert_eq!(started.kind, ActivityKind::Command);
        assert_eq!(started.title, "pwd");
        assert!(!started.complete);
        let DriverEvent::RichActivity(completed) = event_rx.recv().unwrap() else {
            panic!("expected the completed tool activity");
        };
        assert_eq!(completed.source_id.as_deref(), Some("call-1"));
        assert!(completed.complete);
    }

    #[test]
    fn projections_feed_title_and_native_context_pressure() {
        let (events, event_rx, _) = harness();
        emit_projection_values(
            &json!({
                "title":"Harness title",
                "contextPressure":{"pressureTokens":100,"projectedTokens":120,"contextWindow":8192}
            }),
            &events,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Harness title"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::UsageUpdated {
                context_tokens: Some(120),
                context_window: Some(8192)
            }
        ));
    }

    struct MockDeepSeekClient<F> {
        rpc_handler: F,
        rpc_calls: Mutex<Vec<(String, Value)>>,
        responses: Mutex<Vec<(String, Value)>>,
        rejections: Mutex<Vec<(String, String)>>,
    }

    impl<F> MockDeepSeekClient<F>
    where
        F: Fn(&str, &Value) -> anyhow::Result<Value>,
    {
        fn new(rpc_handler: F) -> Self {
            Self {
                rpc_handler,
                rpc_calls: Mutex::new(Vec::new()),
                responses: Mutex::new(Vec::new()),
                rejections: Mutex::new(Vec::new()),
            }
        }
    }

    impl<F> DeepSeekRpcClient for MockDeepSeekClient<F>
    where
        F: Fn(&str, &Value) -> anyhow::Result<Value>,
    {
        fn rpc(&self, method: &str, payload: Value) -> anyhow::Result<Value> {
            self.rpc_calls
                .lock()
                .push((method.to_owned(), payload.clone()));
            (self.rpc_handler)(method, &payload)
        }

        fn respond(&self, rpc_id: &str, value: Value) -> anyhow::Result<()> {
            self.responses.lock().push((rpc_id.to_owned(), value));
            Ok(())
        }

        fn reject_response(&self, rpc_id: &str, message: &str) -> anyhow::Result<()> {
            self.rejections
                .lock()
                .push((rpc_id.to_owned(), message.to_owned()));
            Ok(())
        }
    }

    #[test]
    fn only_full_access_auto_approves_deepseek_requests() {
        let envelope = json!({
            "rpcId": "rpc-1",
            "payload": {
                "type": "approval/requested",
                "sessionId": "session-1",
                "approvalId": "appr-1",
                "toolName": "bash",
                "reason": "Run shell command",
                "arguments": "{\"command\":\"rm -rf /tmp/test\"}"
            }
        });
        let payload = envelope.get("payload").unwrap();

        for mode in [
            RuntimeMode::Ask,
            RuntimeMode::AutoAcceptEdits,
            RuntimeMode::Auto,
        ] {
            let (events, event_rx, mut state) = harness();
            let client = MockDeepSeekClient::new(|_, _| Ok(Value::Null));
            let mode_lock = Mutex::new(mode);
            handle_approval_request(
                &envelope,
                payload,
                &client,
                "session-1",
                &events,
                &mode_lock,
                &mut state,
            );
            assert!(
                client.responses.lock().is_empty(),
                "{mode:?} must not auto-approve escalated workspace-write approvals"
            );
            let DriverEvent::Permission {
                request_id,
                title,
                detail,
                ..
            } = event_rx.try_recv().unwrap()
            else {
                panic!("{mode:?} must surface a Permission prompt");
            };
            assert_eq!(request_id, "rpc-1");
            assert_eq!(title, "Allow bash?");
            assert_eq!(detail, "Run shell command\nrm -rf /tmp/test");
            assert!(state.pending.contains_key("rpc-1"));
        }

        let (events, event_rx, mut state) = harness();
        let client = MockDeepSeekClient::new(|_, _| Ok(Value::Null));
        let full_access = Mutex::new(RuntimeMode::FullAccess);
        handle_approval_request(
            &envelope,
            payload,
            &client,
            "session-1",
            &events,
            &full_access,
            &mut state,
        );
        assert!(event_rx.try_recv().is_err());
        assert!(state.pending.is_empty());
        let responses = client.responses.lock();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].0, "rpc-1");
        assert_eq!(responses[0].1["outcome"], "allowed-once");
    }

    #[test]
    fn approval_detail_surfaces_command_or_tool_arguments_without_duplication() {
        let (_, _, mut state) = harness();
        state.tools.insert(
            "call-9".into(),
            ToolCallState {
                name: "bash".into(),
                title: "git push --force".into(),
                kind: ActivityKind::Command,
                arguments: json!({"command": "git push --force"}),
            },
        );

        // Falls back to stored tool-call arguments via `callId`.
        assert_eq!(
            approval_detail(
                &json!({"toolName": "bash", "callId": "call-9", "reason": "Requires approval"}),
                &state,
            ),
            "Requires approval\ngit push --force"
        );
        // Avoids duplicating the command when `reason` already contains it.
        assert_eq!(
            approval_detail(
                &json!({
                    "toolName": "bash",
                    "command": "git status",
                    "reason": "Execute git status outside sandbox"
                }),
                &state,
            ),
            "Execute git status outside sandbox"
        );
        // Surfaces compact JSON when no reason or known key is present.
        let serialized = approval_detail(
            &json!({"toolName": "custom", "arguments": {"target": "prod", "force": true}}),
            &state,
        );
        assert_eq!(
            serde_json::from_str::<Value>(&serialized).unwrap(),
            json!({"target": "prod", "force": true})
        );
    }

    #[test]
    fn fork_releases_completed_turn_seqs_lock_before_rpc() {
        let completed_turn_seqs = Arc::new(Mutex::new(vec![10, 25]));
        let lock_for_rpc = Arc::clone(&completed_turn_seqs);
        let client = MockDeepSeekClient::new(move |method, payload| {
            assert!(
                lock_for_rpc.try_lock().is_some(),
                "completed_turn_seqs lock must not be held across {method} RPC"
            );
            assert_eq!(method, "session.fork");
            assert_eq!(payload["atSeq"], 10);
            Ok(json!({"sessionId": "forked-1"}))
        });

        let cursor = fork_session_with_client(
            &client,
            "session-1",
            Path::new("/tmp/project"),
            Some("code"),
            &completed_turn_seqs,
            1,
        )
        .unwrap();
        assert_eq!(
            cursor,
            ProviderResumeCursor::DeepSeek {
                session_id: "forked-1".into()
            }
        );
    }

    #[test]
    fn fetch_history_stops_on_repeated_pagination_cursors() {
        let calls = Mutex::new(0_usize);
        let client = MockDeepSeekClient::new(|method, payload| {
            assert_eq!(method, "session.history");
            let mut count = calls.lock();
            *count += 1;
            assert!(*count <= 3, "pagination failed to break out of cycle");
            if payload.get("beforeSeq").is_none() {
                Ok(json!({
                    "hasMore": true,
                    "events": [{"event": {"type": "turn/end", "seq": 10}}]
                }))
            } else {
                // Server keeps returning the same oldest seq with hasMore=true.
                Ok(json!({
                    "hasMore": true,
                    "events": [{"event": {"type": "turn/start", "seq": 10}}]
                }))
            }
        });

        let snapshot = fetch_history(&client, "session-1").unwrap();
        assert_eq!(*calls.lock(), 2);
        assert_eq!(snapshot.last_seq, 10);
    }

    #[test]
    fn apply_session_options_applies_reasoning_effort_when_model_is_none() {
        let client = MockDeepSeekClient::new(|method, _payload| match method {
            "session.models" => Ok(json!({
                "current": {"provider": "deepseek", "model": "deepseek-reasoner"}
            })),
            "session.selectModel" => Ok(json!({})),
            other => bail!("unexpected RPC {other}"),
        });
        let options = SessionOptions {
            mode: RuntimeMode::AutoAcceptEdits,
            model: None,
            reasoning_effort: Some("high".into()),
            service_tier: None,
            context_window: None,
        };
        let projections = json!({"permissions": {"currentValue": "workspace-write"}});

        apply_session_options(&client, "session-1", &options, Some(&projections)).unwrap();

        let calls = client.rpc_calls.lock();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "session.models");
        assert_eq!(calls[1].0, "session.selectModel");
        assert_eq!(
            calls[1].1,
            json!({
                "sessionId": "session-1",
                "provider": "deepseek",
                "model": "deepseek-reasoner",
                "reasoningEffort": "high",
            })
        );
    }

    #[test]
    fn process_exit_mid_turn_emits_failed_turn_finished_and_clears_pending() {
        let (events, event_rx, mut state) = harness();
        state.turn_active = true;
        state.pending.insert(
            "rpc-1".into(),
            PendingInteraction::Approval {
                rpc_id: "rpc-1".into(),
                approval_id: "appr-1".into(),
            },
        );
        let client = MockDeepSeekClient::new(|_, _| Ok(Value::Null));
        let mode = Mutex::new(RuntimeMode::Ask);
        let envelope = json!({
            "payload": {
                "type": "waku/process-exited",
                "message": "DeepSeek Harness exited (signal 9)"
            }
        });

        assert!(!handle_envelope(
            &envelope,
            &client,
            "session-1",
            &events,
            &mode,
            &mut state,
        ));
        assert!(!state.turn_active);
        assert!(state.pending.is_empty());

        let seen: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert!(matches!(
            &seen[0],
            DriverEvent::Error(message) if message == "DeepSeek Harness exited (signal 9)"
        ));
        assert!(matches!(
            &seen[1],
            DriverEvent::TurnFinished {
                success: false,
                summary: Some(message),
            } if message == "DeepSeek Harness exited (signal 9)"
        ));
        assert!(matches!(&seen[2], DriverEvent::ProcessExited));
        assert_eq!(seen.len(), 3);

        // When no turn is active, ProcessExited is emitted without TurnFinished.
        let (events, event_rx, mut idle_state) = harness();
        assert!(!handle_envelope(
            &envelope,
            &client,
            "session-1",
            &events,
            &mode,
            &mut idle_state,
        ));
        let idle_seen: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert_eq!(idle_seen.len(), 2);
        assert!(matches!(&idle_seen[0], DriverEvent::Error(_)));
        assert!(matches!(&idle_seen[1], DriverEvent::ProcessExited));
    }
}
