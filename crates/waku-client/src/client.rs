use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use uuid::Uuid;

use waku_protocol::MAX_WIRE_MESSAGE_BYTES;
use waku_protocol::{
    ClientKind, ClientMessage, Command, PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome,
    ResponsePayload, RpcError, SequencedEvent, ServerMessage,
};

const READ_POLL_INTERVAL: Duration = Duration::from_millis(25);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BUFFERED_EVENTS_PER_RUNTIME: usize = 4096;
const MAX_BUFFERED_OPEN_WORKSPACES: usize = 64;
/// A write that makes no progress for this long means the daemon stopped
/// reading. Without a deadline, a full TCP buffer would block the socket
/// thread — which owns reads as well — and stall the whole client.
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// An otherwise-idle daemon is probed with a WebSocket ping so a silently
/// dead path (sleep/wake, network change) is detected instead of waiting
/// for a request to time out.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
/// Nothing at all from the daemon for this long means it is unreachable.
/// Must exceed `HEARTBEAT_INTERVAL` by enough margin that a busy daemon's
/// pong is never mistaken for silence.
const HEARTBEAT_DEAD_TIMEOUT: Duration = Duration::from_secs(75);

enum Outgoing {
    Message(ClientMessage),
    Shutdown,
    Disconnect,
}

/// An incoming `openWorkspace` push delivered by a daemon to a desktop client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenWorkspaceRequest {
    pub path: Option<String>,
    pub host: Option<String>,
}

/// Outcome returned by `Command::OpenWorkspace`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenWorkspaceOutcome {
    pub delivered: bool,
    pub desktop_pid: Option<u32>,
    pub resolved_path: Option<String>,
}

/// Typed error returned when the daemon accepted the request and replied with
/// an explicit [`RpcError`] (for example, a nonexistent workspace path).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonRpcError(pub String);

impl std::fmt::Display for DaemonRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DaemonRpcError {}

/// Typed error returned by [`request_open_workspace`] when no compatible
/// running daemon could be reached via the given state file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonUnavailableError(pub String);

impl std::fmt::Display for DaemonUnavailableError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DaemonUnavailableError {}

struct ClientInner {
    outgoing: Sender<Outgoing>,
    pending: Mutex<HashMap<Uuid, Sender<Result<ResponsePayload, RpcError>>>>,
    sessions: Mutex<HashMap<(Uuid, Uuid), Sender<SequencedEvent>>>,
    pending_events: Mutex<HashMap<(Uuid, Uuid), VecDeque<SequencedEvent>>>,
    task_state_subscribers: Mutex<Vec<Sender<u64>>>,
    open_workspace_subscribers: Mutex<Vec<Sender<OpenWorkspaceRequest>>>,
    pending_open_workspaces: Mutex<VecDeque<OpenWorkspaceRequest>>,
    last_sequences: Mutex<HashMap<(Uuid, Uuid), LastSequence>>,
    disconnected: AtomicBool,
    /// Why the current or most recent connection ended, for diagnostics.
    disconnect_reason: Mutex<Option<String>>,
    daemon_version: String,
}

#[derive(Clone, Copy)]
struct LastSequence {
    epoch: Uuid,
    sequence: u64,
}

#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<ClientInner>,
}

impl DaemonClient {
    pub fn connect(address: &str, token: String) -> anyhow::Result<Self> {
        Self::connect_with_resume(address, token, Vec::new())
    }

    pub fn connect_with_resume(
        address: &str,
        token: String,
        resume_from: Vec<ReplayCursor>,
    ) -> anyhow::Result<Self> {
        Self::connect_with_identity(address, token, resume_from, ClientKind::Other, None)
    }

    pub fn connect_with_identity(
        address: &str,
        token: String,
        resume_from: Vec<ReplayCursor>,
        client_kind: ClientKind,
        client_pid: Option<u32>,
    ) -> anyhow::Result<Self> {
        let last_sequences = resume_from
            .iter()
            .map(|cursor| {
                (
                    (cursor.session_id, cursor.runtime_id),
                    LastSequence {
                        epoch: cursor.epoch,
                        sequence: cursor.sequence,
                    },
                )
            })
            .collect();
        let url = daemon_url(address)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_WIRE_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WIRE_MESSAGE_BYTES));
        let (mut socket, _) =
            tungstenite::client::connect_with_config(url.as_str(), Some(config), 3)
                .context("could not connect to Waku daemon")?;
        set_client_timeouts(
            &mut socket,
            Some(Duration::from_secs(5)),
            Some(WRITE_STALL_TIMEOUT),
        )?;
        write_json(
            &mut socket,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token,
                client_id: Uuid::new_v4(),
                resume_from,
                client_kind,
                client_pid,
            },
        )?;
        let hello = read_server_message(&mut socket)?;
        let daemon_version = match hello {
            ServerMessage::Hello {
                protocol_version,
                daemon_version,
            } if protocol_version == PROTOCOL_VERSION => daemon_version,
            ServerMessage::Hello {
                protocol_version, ..
            } => bail!(
                "daemon protocol {protocol_version} does not match desktop protocol {PROTOCOL_VERSION}"
            ),
            ServerMessage::Rejected { message } => bail!("daemon rejected connection: {message}"),
            other => bail!("daemon sent an invalid handshake response: {other:?}"),
        };
        set_client_timeouts(
            &mut socket,
            Some(READ_POLL_INTERVAL),
            Some(WRITE_STALL_TIMEOUT),
        )?;

        let (outgoing, outgoing_rx) = unbounded();
        let inner = Arc::new(ClientInner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            pending_events: Mutex::new(HashMap::new()),
            task_state_subscribers: Mutex::new(Vec::new()),
            open_workspace_subscribers: Mutex::new(Vec::new()),
            pending_open_workspaces: Mutex::new(VecDeque::new()),
            last_sequences: Mutex::new(last_sequences),
            disconnected: AtomicBool::new(false),
            disconnect_reason: Mutex::new(None),
            daemon_version,
        });
        let thread_inner = inner.clone();
        std::thread::Builder::new()
            .name("waku-daemon-client".into())
            .spawn(move || run_client(socket, outgoing_rx, thread_inner))
            .context("could not start Waku daemon client thread")?;
        Ok(Self { inner })
    }

    pub fn subscribe(&self, session_id: Uuid, runtime_id: Uuid) -> Receiver<SequencedEvent> {
        let (events, receiver) = unbounded();
        let key = (session_id, runtime_id);
        let mut sessions = self.inner.sessions.lock();
        // Disconnect cleanup takes the same lock after setting this flag, so
        // a subscription either joins a live client or is removed by cleanup.
        if self.inner.disconnected.load(Ordering::Acquire) {
            return receiver;
        }
        sessions.insert(key, events.clone());
        // Keep the subscription lock while draining the pre-subscription
        // replay queue. The socket thread takes these locks in the same order,
        // so a new live event cannot overtake older replayed events here.
        if let Some(buffered) = self.inner.pending_events.lock().remove(&key) {
            for event in buffered {
                let _ = events.send(event);
            }
        }
        receiver
    }

    /// Whether two handles send through the same WebSocket connection.
    ///
    /// The daemon supervisor publishes replacement clients after a managed
    /// restart. Runtime adapters use this identity check to ignore the
    /// subscription's initial snapshot and wait for an actual replacement.
    pub fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn is_disconnected(&self) -> bool {
        self.inner.disconnected.load(Ordering::Acquire)
    }

    /// Why the current or most recent connection ended, when the socket
    /// thread has already observed it. Cleared only by using a new client.
    pub fn disconnect_reason(&self) -> Option<String> {
        self.inner.disconnect_reason.lock().clone()
    }

    /// The release version the daemon announced in its handshake.
    pub fn daemon_version(&self) -> &str {
        &self.inner.daemon_version
    }

    pub fn unsubscribe(&self, session_id: Uuid, runtime_id: Uuid) {
        self.inner.sessions.lock().remove(&(session_id, runtime_id));
    }

    pub fn subscribe_task_state(&self) -> Receiver<u64> {
        let (events, receiver) = unbounded();
        self.inner.task_state_subscribers.lock().push(events);
        receiver
    }

    /// Subscribe to `openWorkspace` requests pushed by the daemon to desktop
    /// clients. Any requests that arrived after the WebSocket handshake but
    /// before the first subscriber registered are drained immediately in order.
    pub fn subscribe_open_workspace(&self) -> Receiver<OpenWorkspaceRequest> {
        let (events, receiver) = unbounded();
        let mut subscribers = self.inner.open_workspace_subscribers.lock();
        subscribers.push(events.clone());
        let mut pending = self.inner.pending_open_workspaces.lock();
        while let Some(request) = pending.pop_front() {
            let _ = events.send(request);
        }
        receiver
    }

    pub fn open_workspace(
        &self,
        path: Option<String>,
        host: Option<String>,
    ) -> anyhow::Result<OpenWorkspaceOutcome> {
        match self.request(
            Uuid::nil(),
            Uuid::nil(),
            Command::OpenWorkspace { path, host },
        )? {
            ResponsePayload::WorkspaceOpened {
                delivered,
                desktop_pid,
                resolved_path,
            } => Ok(OpenWorkspaceOutcome {
                delivered,
                desktop_pid,
                resolved_path,
            }),
            _ => bail!("Waku daemon returned an invalid openWorkspace response"),
        }
    }

    pub fn request(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<ResponsePayload> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Waku daemon is disconnected");
        }
        let request_id = Uuid::new_v4();
        let (response, response_rx) = bounded(1);
        self.inner.pending.lock().insert(request_id, response);
        let message = ClientMessage::Request(Request {
            request_id,
            session_id,
            runtime_id,
            command,
        });
        if self
            .inner
            .outgoing
            .send(Outgoing::Message(message))
            .is_err()
        {
            self.inner.pending.lock().remove(&request_id);
            bail!("Waku daemon connection is closed");
        }
        match response_rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(error)) => Err(anyhow::Error::new(DaemonRpcError(error.message))),
            Err(error) => {
                self.inner.pending.lock().remove(&request_id);
                Err(anyhow!("timed out waiting for Waku daemon: {error}"))
            }
        }
    }

    pub fn notify(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<()> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Waku daemon is disconnected");
        }
        self.inner
            .outgoing
            .send(Outgoing::Message(ClientMessage::Request(Request {
                // The nil request id is reserved for fire-and-forget controls;
                // the daemon executes them in the runtime mailbox but does
                // not allocate or send a response.
                request_id: Uuid::nil(),
                session_id,
                runtime_id,
                command,
            })))
            .map_err(|_| anyhow!("Waku daemon connection is closed"))
    }

    pub fn last_sequences(&self) -> Vec<ReplayCursor> {
        self.inner
            .last_sequences
            .lock()
            .iter()
            .map(|(&(session_id, runtime_id), cursor)| ReplayCursor {
                session_id,
                runtime_id,
                epoch: cursor.epoch,
                sequence: cursor.sequence,
            })
            .collect()
    }

    pub fn shutdown(&self) {
        let _ = self.inner.outgoing.send(Outgoing::Shutdown);
    }

    /// Close only this transport. Resident terminals remain on the daemon.
    pub fn disconnect(&self) {
        let _ = self.inner.outgoing.send(Outgoing::Disconnect);
    }
}

/// Read a daemon `BridgeState` JSON file (`resident-daemon.json` or
/// `daemon.json`), verify that its daemon PID is alive, connect as
/// [`ClientKind::Cli`], and invoke [`Command::OpenWorkspace`].
///
/// Missing/stale state files, dead PIDs, protocol mismatches, and socket
/// connect failures return a [`DaemonUnavailableError`]; daemon-side RPC
/// rejections (such as a nonexistent workspace path) return a
/// [`DaemonRpcError`].
pub fn request_open_workspace(
    state_file: &Path,
    path: Option<String>,
    host: Option<String>,
) -> anyhow::Result<OpenWorkspaceOutcome> {
    request_open_workspace_as(state_file, path, host, ClientKind::Cli)
}

pub fn request_open_workspace_as(
    state_file: &Path,
    path: Option<String>,
    host: Option<String>,
    client_kind: ClientKind,
) -> anyhow::Result<OpenWorkspaceOutcome> {
    let bytes = std::fs::read(state_file).map_err(|error| {
        anyhow::Error::new(DaemonUnavailableError(format!(
            "could not read daemon state file {}: {error}",
            state_file.display()
        )))
    })?;
    let state: waku_protocol::bridge::BridgeState =
        serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::Error::new(DaemonUnavailableError(format!(
                "invalid daemon state file {}: {error}",
                state_file.display()
            )))
        })?;
    if !crate::pid::is_alive(state.pid) {
        return Err(anyhow::Error::new(DaemonUnavailableError(format!(
            "Waku daemon (pid {}) is not running",
            state.pid
        ))));
    }
    if state.protocol_version != PROTOCOL_VERSION {
        return Err(anyhow::Error::new(DaemonUnavailableError(format!(
            "running Waku daemon uses protocol {}, expected {PROTOCOL_VERSION}",
            state.protocol_version
        ))));
    }
    let address = crate::process::desktop_client_address(&state.address)
        .unwrap_or_else(|_| state.address.clone());
    let client_pid = (client_kind != ClientKind::RemoteCli).then(std::process::id);
    let client = DaemonClient::connect_with_identity(
        &address,
        state.token,
        Vec::new(),
        client_kind,
        client_pid,
    )
    .map_err(|error| anyhow::Error::new(DaemonUnavailableError(format!("{error:#}"))))?;
    let outcome = client.open_workspace(path, host);
    client.disconnect();
    outcome
}

/// Result of asking a running daemon at `address` to shut down cleanly.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ShutdownProbeOutcome {
    /// Authenticated with the daemon and sent `ClientMessage::Shutdown`.
    ShuttingDown,
    /// The daemon rejected our credentials or refused shutdown.
    Rejected(String),
    /// Could not reach a daemon at `address` or handshake failed.
    Unreachable(String),
}

/// Connect to a daemon at `address`, negotiate its protocol version if it
/// announces a different one, authenticate with `token`, and send
/// `ClientMessage::Shutdown`.
///
/// Only a daemon holding the matching `token` accepts `Shutdown`, so probing an
/// address never affects an unrelated service or another user's daemon.
pub(crate) fn shutdown_daemon_at(address: &str, token: &str) -> ShutdownProbeOutcome {
    match shutdown_with_protocol(address, PROTOCOL_VERSION, token) {
        Ok(ShutdownStep::Outcome(outcome)) => outcome,
        Ok(ShutdownStep::ProtocolMismatch(expected)) if expected != PROTOCOL_VERSION => {
            match shutdown_with_protocol(address, expected, token) {
                Ok(ShutdownStep::Outcome(outcome)) => outcome,
                Ok(ShutdownStep::ProtocolMismatch(other)) => ShutdownProbeOutcome::Rejected(
                    format!("daemon expected protocol {other} after reporting {expected}"),
                ),
                Err(error) => ShutdownProbeOutcome::Unreachable(format!("{error:#}")),
            }
        }
        Ok(ShutdownStep::ProtocolMismatch(expected)) => {
            ShutdownProbeOutcome::Rejected(format!("daemon rejected matching protocol {expected}"))
        }
        Err(error) => ShutdownProbeOutcome::Unreachable(format!("{error:#}")),
    }
}

enum ShutdownStep {
    Outcome(ShutdownProbeOutcome),
    ProtocolMismatch(u32),
}

fn shutdown_with_protocol(
    address: &str,
    protocol_version: u32,
    token: &str,
) -> anyhow::Result<ShutdownStep> {
    let url = daemon_url(address)?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WIRE_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_WIRE_MESSAGE_BYTES));
    let (mut socket, _) = tungstenite::client::connect_with_config(url.as_str(), Some(config), 3)
        .context("could not connect to Waku daemon")?;
    set_client_timeouts(
        &mut socket,
        Some(Duration::from_secs(2)),
        Some(WRITE_STALL_TIMEOUT),
    )?;
    write_json(
        &mut socket,
        &ClientMessage::Hello {
            protocol_version,
            token: token.to_owned(),
            client_id: Uuid::new_v4(),
            resume_from: Vec::new(),
            client_kind: ClientKind::Other,
            client_pid: None,
        },
    )?;
    match read_server_message(&mut socket)? {
        ServerMessage::Hello { .. } => {}
        ServerMessage::Rejected { message } => {
            if let Some(expected) = parse_expected_protocol_version(&message) {
                return Ok(ShutdownStep::ProtocolMismatch(expected));
            }
            return Ok(ShutdownStep::Outcome(ShutdownProbeOutcome::Rejected(
                message,
            )));
        }
        other => bail!("daemon sent an invalid handshake response: {other:?}"),
    }
    write_json(&mut socket, &ClientMessage::Shutdown)?;
    let _ = socket.flush();
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => match serde_json::from_str::<ServerMessage>(text.as_ref()) {
                Ok(ServerMessage::ShuttingDown) => {
                    return Ok(ShutdownStep::Outcome(ShutdownProbeOutcome::ShuttingDown));
                }
                Ok(ServerMessage::Rejected { message }) => {
                    return Ok(ShutdownStep::Outcome(ShutdownProbeOutcome::Rejected(
                        message,
                    )));
                }
                _ => {}
            },
            Ok(Message::Ping(_)) => {
                let _ = socket.flush();
            }
            Ok(Message::Close(_))
            | Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(ShutdownStep::Outcome(ShutdownProbeOutcome::ShuttingDown));
            }
            Err(_) => {
                return Ok(ShutdownStep::Outcome(ShutdownProbeOutcome::ShuttingDown));
            }
            _ => {}
        }
    }
}

fn parse_expected_protocol_version(message: &str) -> Option<u32> {
    let tail = message.split("expected ").nth(1)?;
    let digits: String = tail.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn daemon_url(address: &str) -> anyhow::Result<String> {
    let normalized = if address.starts_with("ws://") || address.starts_with("wss://") {
        address.to_owned()
    } else {
        format!("ws://{address}")
    };
    let mut url = url::Url::parse(&normalized).context("Waku daemon address is invalid")?;
    url.set_path("/v1");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

fn run_client(
    mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
    outgoing: Receiver<Outgoing>,
    inner: Arc<ClientInner>,
) {
    let mut last_inbound = Instant::now();
    let mut last_ping_sent = Instant::now();
    // Set by every exit path below before the connection is torn down.
    let close_reason: String;
    'connection: loop {
        // Probe a silent daemon so a dead path is discovered while idle
        // instead of by a request timing out minutes later.
        let now = Instant::now();
        if now.duration_since(last_inbound) >= HEARTBEAT_INTERVAL
            && now.duration_since(last_ping_sent) >= HEARTBEAT_INTERVAL
        {
            if socket.send(Message::Ping(Vec::new().into())).is_err() {
                close_reason = "the daemon stopped accepting writes".into();
                break 'connection;
            }
            last_ping_sent = now;
        }
        while let Ok(message) = outgoing.try_recv() {
            match message {
                Outgoing::Message(message) => {
                    if let Err(error) = write_json(&mut socket, &message) {
                        close_reason = format!("a write to the daemon failed: {error}");
                        break 'connection;
                    }
                }
                Outgoing::Disconnect => {
                    let _ = socket.close(None);
                    close_reason = "the client detached".into();
                    break 'connection;
                }
                Outgoing::Shutdown => {
                    let _ = write_json(&mut socket, &ClientMessage::Shutdown);
                    let _ = socket.flush();
                    close_reason = "the desktop shut the daemon connection down".into();
                    break 'connection;
                }
            }
        }

        match socket.read() {
            Ok(message) => {
                last_inbound = Instant::now();
                match message {
                    Message::Text(text) => {
                        let Ok(message) = serde_json::from_str::<ServerMessage>(text.as_ref())
                        else {
                            continue;
                        };
                        match message {
                            ServerMessage::Response {
                                request_id,
                                outcome,
                            } => {
                                if let Some(pending) = inner.pending.lock().remove(&request_id) {
                                    let result = match outcome {
                                        ResponseOutcome::Ok { payload } => Ok(payload),
                                        ResponseOutcome::Error { error } => Err(error),
                                    };
                                    let _ = pending.send(result);
                                }
                            }
                            ServerMessage::Event(event) => {
                                let should_deliver = {
                                    let mut sequences = inner.last_sequences.lock();
                                    let previous = sequences
                                        .entry((event.session_id, event.runtime_id))
                                        .or_insert(LastSequence {
                                            epoch: event.epoch,
                                            sequence: 0,
                                        });
                                    if previous.epoch == event.epoch
                                        && event.sequence <= previous.sequence
                                    {
                                        false
                                    } else {
                                        previous.epoch = event.epoch;
                                        previous.sequence = event.sequence;
                                        true
                                    }
                                };
                                if should_deliver {
                                    let key = (event.session_id, event.runtime_id);
                                    let sessions = inner.sessions.lock();
                                    if let Some(events) = sessions.get(&key) {
                                        let _ = events.send(event);
                                    } else if event.event.kind != "terminalOutput" {
                                        let mut pending = inner.pending_events.lock();
                                        let buffered = pending.entry(key).or_default();
                                        buffered.push_back(event);
                                        while buffered.len() > MAX_BUFFERED_EVENTS_PER_RUNTIME {
                                            buffered.pop_front();
                                        }
                                    }
                                }
                            }
                            ServerMessage::TaskStateChanged { revision } => {
                                inner
                                    .task_state_subscribers
                                    .lock()
                                    .retain(|subscriber| subscriber.send(revision).is_ok());
                            }
                            ServerMessage::OpenWorkspace { path, host } => {
                                let request = OpenWorkspaceRequest { path, host };
                                let mut subscribers = inner.open_workspace_subscribers.lock();
                                subscribers
                                    .retain(|subscriber| subscriber.send(request.clone()).is_ok());
                                if subscribers.is_empty() {
                                    let mut pending = inner.pending_open_workspaces.lock();
                                    pending.push_back(request);
                                    while pending.len() > MAX_BUFFERED_OPEN_WORKSPACES {
                                        pending.pop_front();
                                    }
                                }
                            }
                            ServerMessage::ShuttingDown => {
                                close_reason = "the daemon is shutting down".into();
                                break;
                            }
                            ServerMessage::Hello { .. } | ServerMessage::Rejected { .. } => {}
                        }
                    }
                    Message::Close(_) => {
                        close_reason = "the daemon closed the connection".into();
                        break;
                    }
                    Message::Ping(_) => {
                        let _ = socket.flush();
                    }
                    _ => {}
                }
            }
            Err(tungstenite::Error::Io(error)) if retryable_io(&error) => {
                if is_windows_io_pending(&error) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                close_reason = "the daemon connection was closed".into();
                break;
            }
            Err(error) => {
                close_reason = format!("the daemon connection failed: {error}");
                break;
            }
        }
        if last_inbound.elapsed() >= HEARTBEAT_DEAD_TIMEOUT {
            close_reason = format!("the daemon was silent for over {HEARTBEAT_DEAD_TIMEOUT:?}");
            break 'connection;
        }
    }

    eprintln!("Waku daemon connection lost: {close_reason}");
    inner.disconnect_reason.lock().replace(close_reason);
    inner.disconnected.store(true, Ordering::Release);
    let pending = std::mem::take(&mut *inner.pending.lock());
    for (_, response) in pending {
        let _ = response.send(Err(RpcError {
            message: "Waku daemon disconnected".into(),
        }));
    }
    // Closing the desktop transport is not evidence that a daemon-owned
    // provider exited. Drop the subscription senders so runtime adapters can
    // hand off to a replacement client and ask the daemon whether the same
    // runtime still exists. Real provider exits arrive through the replayable
    // `processExited` event emitted by the daemon.
    drop(std::mem::take(&mut *inner.sessions.lock()));
    inner.task_state_subscribers.lock().clear();
    inner.open_workspace_subscribers.lock().clear();
}

fn set_client_timeouts(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    read: Option<Duration>,
    write: Option<Duration>,
) -> io::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => {
            stream.set_read_timeout(read)?;
            stream.set_write_timeout(write)
        }
        MaybeTlsStream::Rustls(stream) => {
            stream.sock.set_read_timeout(read)?;
            stream.sock.set_write_timeout(write)
        }
        #[allow(unreachable_patterns)]
        _ => Ok(()),
    }
}

fn retryable_io(error: &io::Error) -> bool {
    retryable_error(error)
}

fn retryable_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<io::Error>() {
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        ) {
            return true;
        }
        #[cfg(windows)]
        if error.raw_os_error() == Some(997) {
            return true;
        }
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::EAGAIN)
            || error.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            return true;
        }
    }
    error.source().is_some_and(retryable_error)
}

fn is_windows_io_pending(error: &(dyn std::error::Error + 'static)) -> bool {
    #[cfg(windows)]
    {
        if let Some(io_error) = error.downcast_ref::<io::Error>()
            && io_error.raw_os_error() == Some(997)
        {
            return true;
        }
        error.source().is_some_and(is_windows_io_pending)
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

fn write_json<S: io::Read + io::Write, T: serde::Serialize>(
    socket: &mut WebSocket<S>,
    value: &T,
) -> anyhow::Result<()> {
    let payload = serde_json::to_string(value)?;
    socket.send(Message::Text(payload.into()))?;
    Ok(())
}

fn read_server_message(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> anyhow::Result<ServerMessage> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_ref())?),
            Message::Ping(_) => socket.flush()?,
            Message::Close(_) => bail!("Waku daemon closed during handshake"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_endpoint_accepts_addresses_and_secure_urls() {
        assert_eq!(
            daemon_url("127.0.0.1:4312").unwrap(),
            "ws://127.0.0.1:4312/v1"
        );
        assert_eq!(
            daemon_url("wss://waku.example.test/old?ignored=1").unwrap(),
            "wss://waku.example.test/v1"
        );
    }

    #[test]
    fn parses_expected_protocol_from_rejection_message() {
        assert_eq!(
            parse_expected_protocol_version("protocol 12 is unsupported; expected 11"),
            Some(11)
        );
        assert_eq!(
            parse_expected_protocol_version("authentication failed"),
            None
        );
    }

    #[test]
    fn subscribe_after_disconnect_returns_a_disconnected_channel() {
        let (outgoing, _outgoing_rx) = unbounded();
        let client = DaemonClient {
            inner: Arc::new(ClientInner {
                outgoing,
                pending: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                pending_events: Mutex::new(HashMap::new()),
                task_state_subscribers: Mutex::new(Vec::new()),
                open_workspace_subscribers: Mutex::new(Vec::new()),
                pending_open_workspaces: Mutex::new(VecDeque::new()),
                last_sequences: Mutex::new(HashMap::new()),
                disconnected: AtomicBool::new(true),
                disconnect_reason: Mutex::new(None),
                daemon_version: "test".into(),
            }),
        };

        let events = client.subscribe(Uuid::new_v4(), Uuid::new_v4());

        assert!(matches!(
            events.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ));
        assert!(client.inner.sessions.lock().is_empty());
    }

    #[test]
    fn shutdown_daemon_at_negotiates_older_protocol_and_rejects_wrong_token() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let older_protocol = PROTOCOL_VERSION.saturating_sub(1).max(1);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let Ok(stream) = stream else { break };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut ws = tungstenite::accept(stream).unwrap();
                let Message::Text(text) = ws.read().unwrap() else {
                    panic!("expected text hello");
                };
                let ClientMessage::Hello {
                    protocol_version,
                    token,
                    ..
                } = serde_json::from_str(text.as_ref()).unwrap()
                else {
                    panic!("expected hello");
                };
                if protocol_version != older_protocol {
                    let _ = write_json(
                        &mut ws,
                        &ServerMessage::Rejected {
                            message: format!(
                                "protocol {protocol_version} is unsupported; expected {older_protocol}"
                            ),
                        },
                    );
                    continue;
                }
                if token != "right-token" {
                    let _ = write_json(
                        &mut ws,
                        &ServerMessage::Rejected {
                            message: "authentication failed".into(),
                        },
                    );
                    continue;
                }
                write_json(
                    &mut ws,
                    &ServerMessage::Hello {
                        protocol_version: older_protocol,
                        daemon_version: "0.1.49".into(),
                    },
                )
                .unwrap();
                if let Ok(Message::Text(text)) = ws.read()
                    && let Ok(ClientMessage::Shutdown) = serde_json::from_str(text.as_ref())
                {
                    let _ = write_json(&mut ws, &ServerMessage::ShuttingDown);
                    let _ = shutdown_tx.send(());
                    break;
                }
            }
        });

        let wrong = shutdown_daemon_at(&addr, "wrong-token");
        assert_eq!(
            wrong,
            ShutdownProbeOutcome::Rejected("authentication failed".into())
        );

        let ok = shutdown_daemon_at(&addr, "right-token");
        assert_eq!(ok, ShutdownProbeOutcome::ShuttingDown);
        shutdown_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("old-protocol daemon should receive shutdown");
        server.join().unwrap();
    }

    #[test]
    fn connect_with_identity_and_open_workspace_buffering_work() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (identity_tx, identity_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut ws = tungstenite::accept(stream).unwrap();
            let Message::Text(text) = ws.read().unwrap() else {
                panic!("expected text hello");
            };
            let ClientMessage::Hello {
                client_kind,
                client_pid,
                ..
            } = serde_json::from_str(text.as_ref()).unwrap()
            else {
                panic!("expected hello");
            };
            identity_tx.send((client_kind, client_pid)).unwrap();
            write_json(
                &mut ws,
                &ServerMessage::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    daemon_version: "test".into(),
                },
            )
            .unwrap();
            // Push immediately after Hello, before the caller subscribes.
            write_json(
                &mut ws,
                &ServerMessage::OpenWorkspace {
                    path: Some("/early".into()),
                    host: None,
                },
            )
            .unwrap();
            while let Ok(msg) = ws.read() {
                if matches!(msg, Message::Close(_)) {
                    break;
                }
            }
        });

        let client = DaemonClient::connect_with_identity(
            &addr,
            "tok".into(),
            Vec::new(),
            ClientKind::Desktop,
            Some(7777),
        )
        .unwrap();
        assert_eq!(
            identity_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            (ClientKind::Desktop, Some(7777))
        );
        // Give the client reader thread a moment to receive the early push.
        std::thread::sleep(Duration::from_millis(50));
        let rx = client.subscribe_open_workspace();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            OpenWorkspaceRequest {
                path: Some("/early".into()),
                host: None,
            }
        );
        client.disconnect();
        server.join().unwrap();
    }

    #[test]
    fn request_open_workspace_reports_unavailable_for_missing_or_dead_state() {
        let dir = std::env::temp_dir().join(format!("waku-cli-open-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.json");
        let err = request_open_workspace(&missing, None, None).unwrap_err();
        assert!(err.downcast_ref::<DaemonUnavailableError>().is_some());

        let dead_state = dir.join("dead.json");
        std::fs::write(
            &dead_state,
            serde_json::to_string(&waku_protocol::bridge::BridgeState {
                address: "127.0.0.1:1".into(),
                token: "tok".into(),
                pid: 0,
                protocol_version: PROTOCOL_VERSION,
                fingerprint: "fp".into(),
                allowed_origins: None,
            })
            .unwrap(),
        )
        .unwrap();
        let err = request_open_workspace(&dead_state, None, None).unwrap_err();
        assert!(err.downcast_ref::<DaemonUnavailableError>().is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_io_pending_is_retryable() {
        let error = io::Error::from_raw_os_error(997);
        assert!(retryable_io(&error));
        assert!(is_windows_io_pending(&error));

        let non_retryable = io::Error::from_raw_os_error(10054);
        assert!(!retryable_io(&non_retryable));
        assert!(!is_windows_io_pending(&non_retryable));
    }
}
