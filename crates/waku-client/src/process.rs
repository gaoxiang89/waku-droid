use std::collections::HashSet;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::SystemTime;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::DaemonClient;
use crate::remote::{BridgeError, BridgeSession, BridgeStage, Connection};
use waku_protocol::{
    APP_EXECUTABLE_ENV, Command, DAEMON_TOKEN_ENV, DaemonReady, DaemonSettings, PROTOCOL_VERSION,
    ReplayCursor, ResponsePayload,
};
const START_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const REBUILD_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Upper bound between attempts to reach an unreachable remote host.
const MAX_REMOTE_BACKOFF: Duration = Duration::from_secs(30);
pub const DEFAULT_EXPOSED_DAEMON_PORT: u16 = 34_123;

/// Desktop-owned launch configuration for the daemon it supervises.
///
/// Provider settings belong to the daemon and live in `settings.json`; this
/// is an app preference because it controls how the desktop launches its own
/// child process. The bearer token is intentionally stable across daemon-only
/// rebuilds and desktop relaunches so a configured web client keeps working.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct DaemonExposureSettings {
    pub enabled: bool,
    pub port: u16,
    pub allowed_origins: Vec<String>,
    pub token: String,
}

impl Default for DaemonExposureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_EXPOSED_DAEMON_PORT,
            allowed_origins: vec!["http://localhost:3001".into()],
            token: Self::new_token(),
        }
    }
}

impl DaemonExposureSettings {
    pub fn new_token() -> String {
        Uuid::new_v4().simple().to_string()
    }

    pub fn ensure_token(&mut self) -> bool {
        if !self.token.trim().is_empty() {
            return false;
        }
        self.token = Self::new_token();
        true
    }

    pub fn allowed_origins_text(&self) -> String {
        self.allowed_origins.join(", ")
    }

    pub fn with_allowed_origins_text(mut self, text: &str) -> anyhow::Result<Self> {
        self.allowed_origins = parse_allowed_origins(text)?;
        Ok(self)
    }

    pub fn validate(mut self) -> anyhow::Result<Self> {
        if self.port == 0 {
            bail!("daemon port must be between 1 and 65535");
        }
        if self.token.trim().is_empty() {
            bail!("daemon authentication token is empty");
        }
        self.allowed_origins = parse_allowed_origins(&self.allowed_origins_text())?;
        Ok(self)
    }

    fn bind_address(&self) -> String {
        if self.enabled {
            format!("0.0.0.0:{}", self.port)
        } else {
            "127.0.0.1:0".into()
        }
    }
}

/// Parse the comma-separated exact browser origins edited by the desktop.
/// Browser Origin headers contain only an HTTP(S) origin, never a path.
pub fn parse_allowed_origins(text: &str) -> anyhow::Result<Vec<String>> {
    let mut origins = Vec::new();
    let mut seen = HashSet::new();
    for candidate in text
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let url = url::Url::parse(candidate)
            .with_context(|| format!("invalid browser origin {candidate:?}"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            bail!(
                "browser origin {candidate:?} must be an exact http:// or https:// origin without a path"
            );
        }
        let origin = url.origin().ascii_serialization();
        if origin == "null" {
            bail!("browser origin {candidate:?} is not a network origin");
        }
        if seen.insert(origin.clone()) {
            origins.push(origin);
        }
    }
    Ok(origins)
}

pub struct DaemonProcess {
    client: Mutex<DaemonClient>,
    /// Loopback address and bearer token for re-dialing this daemon after a
    /// dropped WebSocket; the port is chosen by the daemon at spawn time.
    address: String,
    token: String,
    child: Option<Child>,
    state_file: PathBuf,
    pid: u32,
}

impl DaemonProcess {
    pub fn spawn(executable: &Path) -> anyhow::Result<Self> {
        Self::spawn_configured(executable, DaemonExposureSettings::default())
    }

    fn spawn_configured(
        executable: &Path,
        settings: DaemonExposureSettings,
    ) -> anyhow::Result<Self> {
        let settings = settings.validate()?;
        let token = settings.token.clone();
        let state_file =
            crate::persistence::StateStore::default_path().with_file_name("resident-daemon.json");
        if let Some(parent) = state_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let launch_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_file.with_extension("lock"))?;
        launch_lock.lock().context("lock resident daemon startup")?;
        if let Ok(bytes) = std::fs::read(&state_file)
            && let Ok(record) = serde_json::from_slice::<waku_protocol::bridge::BridgeState>(&bytes)
            && resident_process_alive(record.pid)
            && (record.protocol_version != PROTOCOL_VERSION || record.token != token)
        {
            bail!(
                "a resident daemon is still running with different protocol or credentials; end its sessions before replacing it"
            );
        }
        if let Ok(bytes) = std::fs::read(&state_file)
            && let Ok(record) = serde_json::from_slice::<waku_protocol::bridge::BridgeState>(&bytes)
            && record.protocol_version == PROTOCOL_VERSION
            && record.token == token
        {
            if resident_daemon_tainted(record.pid) {
                if let Ok(address) = desktop_client_address(&record.address)
                    && let Ok(client) = DaemonClient::connect(&address, token.clone())
                {
                    client.shutdown();
                    let _ = std::fs::remove_file(&state_file);
                    if !wait_for_resident_exit(record.pid, Duration::from_secs(5)) {
                        bail!(
                            "a sandboxed resident daemon is still exiting; try again once it releases its port"
                        );
                    }
                }
            } else if let Ok(address) = desktop_client_address(&record.address)
                && let Ok(client) = DaemonClient::connect(&address, token.clone())
            {
                return Ok(Self {
                    client: Mutex::new(client),
                    address,
                    token,
                    child: None,
                    state_file,
                    pid: record.pid,
                });
            }
        }
        if let Ok(bytes) = std::fs::read(&state_file)
            && let Ok(record) = serde_json::from_slice::<waku_protocol::bridge::BridgeState>(&bytes)
            && resident_process_alive(record.pid)
        {
            bail!("resident daemon is running but unreachable; leaving its sessions intact");
        }
        if let Some(parent) = state_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let app_executable = std::env::current_exe().context("could not locate Waku executable")?;
        let mut command = ProcessCommand::new(executable);
        // The desktop is a GUI-subsystem binary on Windows, so a console
        // child would get a console window of its own. `stderr` still reaches
        // the app's inherited handle.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;

            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
            .arg("--bind")
            .arg(settings.bind_address())
            .arg("--state-file")
            .arg(&state_file)
            .arg("--allow-shutdown")
            .arg("--idle-exit-secs")
            .arg("1800");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        if settings.enabled {
            command.arg("--allow-non-loopback");
        }
        for origin in &settings.allowed_origins {
            command.arg("--allow-origin").arg(origin);
        }
        let mut child = command
            .env(DAEMON_TOKEN_ENV, &token)
            .env(APP_EXECUTABLE_ENV, app_executable)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(state_file.with_extension("log"))?,
            ))
            .spawn()
            .with_context(|| format!("could not launch {}", executable.display()))?;
        let stdout = child
            .stdout
            .take()
            .context("Waku daemon did not expose its readiness stream")?;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("waku-daemon-ready".into())
            .spawn(move || {
                let mut line = String::new();
                let result = BufReader::new(stdout)
                    .read_line(&mut line)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| {
                        if bytes == 0 {
                            bail!("Waku daemon exited before becoming ready")
                        }
                        serde_json::from_str::<DaemonReady>(&line).map_err(anyhow::Error::from)
                    });
                let _ = ready_tx.send(result);
            })
            .context("could not start Waku daemon readiness reader")?;
        let ready = match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(ready)) => ready,
            Ok(Err(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("timed out waiting for Waku daemon: {error}");
            }
        };
        if ready.protocol_version != PROTOCOL_VERSION {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "daemon protocol {} does not match desktop protocol {}",
                ready.protocol_version,
                PROTOCOL_VERSION
            );
        }
        let client_address = match desktop_client_address(&ready.address) {
            Ok(address) => address,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let client = match DaemonClient::connect(&client_address, token.clone()) {
            Ok(client) => client,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            client: Mutex::new(client),
            address: client_address,
            token,
            child: Some(child),
            state_file,
            pid: ready.pid,
        })
    }

    pub fn client(&self) -> DaemonClient {
        self.client.lock().clone()
    }

    /// Swap in a re-established connection to the same daemon process after
    /// the previous WebSocket dropped. The caller has verified the target
    /// still owns this process; the old client is already disconnected.
    fn replace_client(&self, client: DaemonClient) {
        *self.client.lock() = client;
    }

    fn reconnect_address(&self) -> &str {
        &self.address
    }

    fn reconnect_token(&self) -> &str {
        &self.token
    }

    fn has_exited(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => !matches!(child.try_wait(), Ok(None)),
            None => !resident_process_alive(self.pid),
        }
    }

    fn stop(&mut self) {
        self.client.lock().shutdown();
        let _ = std::fs::remove_file(&self.state_file);
        let Some(child) = self.child.as_mut() else {
            let _ = wait_for_resident_exit(self.pid, SHUTDOWN_TIMEOUT);
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        // The resident daemon owns terminal processes. A client exiting is
        // not a request to terminate them; explicit replacement calls stop.
        self.client.lock().disconnect();
    }
}

fn desktop_client_address(address: &str) -> anyhow::Result<String> {
    let address = address
        .parse::<std::net::SocketAddr>()
        .with_context(|| format!("Waku daemon returned an invalid address {address:?}"))?;
    let ip = if address.ip().is_unspecified() {
        if address.is_ipv4() {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
    } else {
        address.ip()
    };
    Ok(std::net::SocketAddr::new(ip, address.port()).to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExecutableStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl ExecutableStamp {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("could not inspect {}", path.display()))?;
        Ok(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

struct SupervisorInner {
    executable: Option<PathBuf>,
    connection: Mutex<Option<Connection>>,
    target: Mutex<DaemonTarget>,
    exposure: Mutex<Option<DaemonExposureSettings>>,
    restart: Mutex<()>,
    settings: Mutex<DaemonSettings>,
    persisted_settings: Mutex<Option<DaemonSettings>>,
    settings_updates: Sender<DaemonSettings>,
    client_updates: Mutex<Vec<Sender<DaemonClient>>>,
    running: AtomicBool,
    /// Mirrors the remote client's disconnected flag, refreshed by the
    /// monitor, so readers never take the target lock.
    transport_down: AtomicBool,
}

enum DaemonTarget {
    Local(DaemonProcess),
    Restarting(DaemonClient),
    Remote {
        client: DaemonClient,
        endpoint: RemoteEndpoint,
    },
}

/// How a daemon the desktop does not own is reached again after a drop.
#[derive(Clone)]
enum RemoteEndpoint {
    /// A fixed WebSocket address, e.g. from `WAKU_DAEMON_ADDRESS`.
    Direct { address: String, token: String },
    /// A WSL distribution or SSH host reached through `waku-daemon bridge`.
    /// The session is replaced on reconnect; the remote daemon is not.
    Bridged {
        connection: Connection,
        #[allow(dead_code)] // Held so the bridge child lives with the client.
        session: Arc<BridgeSession>,
    },
}

impl RemoteEndpoint {
    fn reconnect(&self, resume_from: Vec<ReplayCursor>) -> anyhow::Result<(DaemonClient, Self)> {
        match self {
            Self::Direct { address, token } => {
                let client =
                    DaemonClient::connect_with_resume(address, token.clone(), resume_from)?;
                Ok((
                    client,
                    Self::Direct {
                        address: address.clone(),
                        token: token.clone(),
                    },
                ))
            }
            Self::Bridged {
                connection,
                session,
            } => {
                let session = Arc::new(match BridgeSession::spawn(connection, session.target()) {
                    Ok(session) => session,
                    Err(_) => BridgeSession::open_with(connection, false)?,
                });
                let client = DaemonClient::connect_with_resume(
                    session.address(),
                    session.token().to_owned(),
                    resume_from,
                )?;
                Ok((
                    client,
                    Self::Bridged {
                        connection: connection.clone(),
                        session,
                    },
                ))
            }
        }
    }
}

impl DaemonTarget {
    fn client(&self) -> DaemonClient {
        match self {
            Self::Local(process) => process.client(),
            Self::Restarting(client) => client.clone(),
            Self::Remote { client, .. } => client.clone(),
        }
    }

    fn take_local(&mut self) -> Option<DaemonProcess> {
        match self {
            Self::Local(process) => {
                let disconnected = process.client();
                match std::mem::replace(self, Self::Restarting(disconnected)) {
                    Self::Local(process) => Some(process),
                    _ => unreachable!("local daemon target changed while locked"),
                }
            }
            Self::Restarting(_) | Self::Remote { .. } => None,
        }
    }
}

/// A disconnected transport the monitor should re-establish this tick.
#[derive(Clone)]
enum DisconnectedTarget {
    Remote {
        client: DaemonClient,
        endpoint: RemoteEndpoint,
        resume_from: Vec<ReplayCursor>,
    },
    Local {
        client: DaemonClient,
    },
}

/// Owns the current daemon and, in development, swaps it after a successful
/// rebuild without requiring the desktop process to relaunch.
pub struct DaemonSupervisor {
    inner: Arc<SupervisorInner>,
    // Track external supervisor handles independently of `Arc::strong_count(&inner)`
    // because background monitor/settings threads temporarily upgrade `Weak<SupervisorInner>`.
    handles: Arc<AtomicUsize>,
}

impl DaemonSupervisor {
    pub fn spawn(executable: &Path, watch_for_rebuilds: bool) -> anyhow::Result<Self> {
        Self::spawn_configured(
            executable,
            watch_for_rebuilds,
            DaemonExposureSettings::default(),
        )
    }

    pub fn spawn_configured(
        executable: &Path,
        watch_for_rebuilds: bool,
        exposure: DaemonExposureSettings,
    ) -> anyhow::Result<Self> {
        let exposure = exposure.validate()?;
        let process = DaemonProcess::spawn_configured(executable, exposure.clone())?;
        let settings = read_settings(&process.client())?;
        let initial_stamp = ExecutableStamp::read(executable)?;
        let supervisor = Self::from_target(
            DaemonTarget::Local(process),
            Some(executable.to_owned()),
            Some(exposure),
            settings,
        )?;
        let weak_inner = Arc::downgrade(&supervisor.inner);
        std::thread::Builder::new()
            .name("waku-daemon-supervisor".into())
            .spawn(move || monitor_daemon(weak_inner, Some(initial_stamp), watch_for_rebuilds))
            .context("could not start Waku daemon supervisor")?;
        Ok(supervisor)
    }

    /// Connect to a daemon managed on another host (or by an external local
    /// service manager). Dropping the desktop never shuts this daemon down.
    pub fn connect(address: &str, token: String) -> anyhow::Result<Self> {
        let client = DaemonClient::connect(address, token.clone())?;
        let settings = read_settings(&client)?;
        Self::remote(
            client,
            RemoteEndpoint::Direct {
                address: address.to_owned(),
                token,
            },
            None,
            settings,
        )
    }

    /// Connect to the daemon on a WSL distribution or SSH host, installing or
    /// updating it first when needed. Blocking: the first connection to a host
    /// may upload the daemon. The remote daemon outlives this desktop.
    pub fn connect_remote(connection: Connection) -> Result<Self, BridgeError> {
        Self::connect_remote_with(connection, false)
    }

    /// [`Self::connect_remote`], optionally ending a running daemon that
    /// speaks another protocol and its sessions. Only a user confirmation of
    /// [`crate::remote::IncompatibleDaemonError`] may pass `true`.
    pub fn connect_remote_with(
        connection: Connection,
        replace_incompatible: bool,
    ) -> Result<Self, BridgeError> {
        let session = Arc::new(BridgeSession::open_with(&connection, replace_incompatible)?);
        let bridge_error = |error: anyhow::Error| BridgeError {
            stage: BridgeStage::Bridge,
            error: match session.stderr() {
                detail if detail.is_empty() => error,
                detail => error.context(detail),
            },
        };
        let client = DaemonClient::connect(session.address(), session.token().to_owned())
            .map_err(bridge_error)?;
        let settings = read_settings(&client).map_err(bridge_error)?;
        Self::remote(
            client,
            RemoteEndpoint::Bridged {
                connection: connection.clone(),
                session: session.clone(),
            },
            Some(connection),
            settings,
        )
        .map_err(bridge_error)
    }

    fn remote(
        client: DaemonClient,
        endpoint: RemoteEndpoint,
        connection: Option<Connection>,
        settings: DaemonSettings,
    ) -> anyhow::Result<Self> {
        let supervisor = Self::from_target(
            DaemonTarget::Remote { client, endpoint },
            None,
            None,
            settings,
        )?;
        *supervisor.inner.connection.lock() = connection;
        let weak_inner = Arc::downgrade(&supervisor.inner);
        std::thread::Builder::new()
            .name("waku-remote-daemon-supervisor".into())
            .spawn(move || monitor_daemon(weak_inner, None, false))
            .context("could not start remote Waku daemon supervisor")?;
        Ok(supervisor)
    }

    /// The WSL or SSH connection this supervisor is bound to, if any.
    pub fn connection(&self) -> Option<Connection> {
        self.inner.connection.lock().clone()
    }

    /// Whether the transport to a remote daemon is down while the supervisor
    /// retries. Lock-free, so a frame may read it.
    pub fn is_disconnected(&self) -> bool {
        self.inner.transport_down.load(Ordering::Acquire)
    }

    fn from_target(
        target: DaemonTarget,
        executable: Option<PathBuf>,
        exposure: Option<DaemonExposureSettings>,
        settings: DaemonSettings,
    ) -> anyhow::Result<Self> {
        let (settings_updates, settings_update_rx) = unbounded();
        let inner = Arc::new(SupervisorInner {
            executable,
            connection: Mutex::new(None),
            target: Mutex::new(target),
            exposure: Mutex::new(exposure),
            restart: Mutex::new(()),
            settings: Mutex::new(settings),
            // The desktop sends one normalized snapshot after it has migrated
            // the legacy combined settings document into app.json.
            persisted_settings: Mutex::new(None),
            settings_updates,
            client_updates: Mutex::new(Vec::new()),
            running: AtomicBool::new(true),
            transport_down: AtomicBool::new(false),
        });
        let weak_inner = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("waku-daemon-settings".into())
            .spawn(move || persist_settings(weak_inner, settings_update_rx))
            .context("could not start Waku daemon settings writer")?;
        Ok(Self {
            inner,
            handles: Arc::new(AtomicUsize::new(1)),
        })
    }

    pub fn client(&self) -> DaemonClient {
        self.inner.target.lock().client()
    }

    /// Subscribe to the active daemon connection. The current client is sent
    /// immediately, followed by each replacement after a managed restart.
    pub fn subscribe_clients(&self) -> Receiver<DaemonClient> {
        let (updates, receiver) = unbounded();
        // Holding the target lock through registration makes the initial send
        // atomic with respect to replacement: a subscriber sees either the old
        // client followed by the new one, or the new client directly.
        let target = self.inner.target.lock();
        self.inner.client_updates.lock().push(updates.clone());
        let _ = updates.send(target.client());
        receiver
    }

    pub fn is_remote(&self) -> bool {
        self.inner.executable.is_none()
    }

    pub fn settings(&self) -> DaemonSettings {
        self.inner.settings.lock().clone()
    }

    /// Restart only the desktop-managed daemon with a new listener policy.
    /// The caller should run this off the UI thread.
    pub fn reconfigure(&self, exposure: DaemonExposureSettings) -> anyhow::Result<()> {
        let exposure = exposure.validate()?;
        let executable = self
            .inner
            .executable
            .as_ref()
            .context("the connected daemon is managed outside Waku Desktop")?
            .clone();
        let _restart = self.inner.restart.lock();
        let previous = self
            .inner
            .exposure
            .lock()
            .clone()
            .context("managed daemon launch settings are unavailable")?;
        match replace_local_daemon(&self.inner, &executable, &exposure) {
            Ok(()) => {
                *self.inner.exposure.lock() = Some(exposure);
                queue_settings_refresh(&self.inner);
                Ok(())
            }
            Err(error) => {
                let restore = replace_local_daemon(&self.inner, &executable, &previous);
                if restore.is_ok() {
                    queue_settings_refresh(&self.inner);
                    Err(error)
                } else {
                    Err(error.context(format!(
                        "the previous daemon configuration also failed to restart: {:#}",
                        restore.unwrap_err()
                    )))
                }
            }
        }
    }

    /// Queue a daemon settings update without blocking the desktop UI thread.
    pub fn update_settings(&self, settings: DaemonSettings) -> anyhow::Result<()> {
        *self.inner.settings.lock() = settings.clone();
        if self.inner.persisted_settings.lock().as_ref() == Some(&settings) {
            return Ok(());
        }
        self.inner
            .settings_updates
            .send(settings)
            .map_err(|_| anyhow::anyhow!("Waku daemon settings writer is closed"))
    }

    /// Stops monitoring and shuts down the desktop-managed local daemon, if any.
    pub fn stop_local(&self) {
        self.inner.running.store(false, Ordering::Release);
        if self.inner.executable.is_none()
            && matches!(&*self.inner.target.lock(), DaemonTarget::Remote { .. })
        {
            return;
        }
        let _restart = self.inner.restart.lock();
        let previous = self.inner.target.lock().take_local();
        if let Some(mut process) = previous {
            process.stop();
        }
    }
}

impl Clone for DaemonSupervisor {
    fn clone(&self) -> Self {
        self.handles.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
            handles: Arc::clone(&self.handles),
        }
    }
}

impl Drop for DaemonSupervisor {
    fn drop(&mut self) {
        if self.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.running.store(false, Ordering::Release);
        }
    }
}

fn monitor_daemon(
    weak_inner: std::sync::Weak<SupervisorInner>,
    mut active_stamp: Option<ExecutableStamp>,
    watch_for_rebuilds: bool,
) {
    let mut remote_backoff = REBUILD_POLL_INTERVAL;
    let mut next_remote_attempt = Instant::now();
    let mut local_backoff = REBUILD_POLL_INTERVAL;
    let mut next_local_attempt = Instant::now();
    let mut next_upgrade_attempt = Instant::now();
    loop {
        std::thread::sleep(REBUILD_POLL_INTERVAL);
        let Some(inner) = weak_inner.upgrade() else {
            return;
        };
        if !inner.running.load(Ordering::Acquire) {
            return;
        }
        // Transport liveness is mirrored for every target kind — a local
        // connection drop is as visible to the UI as a remote one — and a
        // disconnected transport becomes this tick's reconnection work.
        let reconnect = {
            let target = inner.target.lock();
            let down = target.client().is_disconnected();
            inner.transport_down.store(down, Ordering::Release);
            match &*target {
                DaemonTarget::Remote { client, endpoint } if down => {
                    Some(DisconnectedTarget::Remote {
                        client: client.clone(),
                        endpoint: endpoint.clone(),
                        resume_from: client.last_sequences(),
                    })
                }
                DaemonTarget::Local(_) if down => Some(DisconnectedTarget::Local {
                    client: target.client(),
                }),
                _ => None,
            }
        };
        // A healthy tick clears the local re-dial state, so one outage's
        // backoff and hang patience never bleed into the next one (the
        // crash path below can replace the daemon without passing through
        // the re-dial branch that resets them).
        if reconnect.is_none() {
            local_backoff = REBUILD_POLL_INTERVAL;
            next_local_attempt = Instant::now();
        }
        match reconnect {
            Some(DisconnectedTarget::Remote {
                client: disconnected,
                endpoint,
                resume_from,
            }) => {
                if Instant::now() >= next_remote_attempt {
                    let _restart = inner.restart.lock();
                    if !inner.running.load(Ordering::Acquire) {
                        return;
                    }
                    let still_current = matches!(
                        &*inner.target.lock(),
                        DaemonTarget::Remote { client, .. }
                            if client.same_connection(&disconnected) && client.is_disconnected()
                    );
                    if still_current {
                        // The target lock is released here: a bridged reconnect
                        // starts `ssh`, and UI-side `client()` calls must not
                        // wait behind it.
                        match endpoint.reconnect(resume_from) {
                            Ok((replacement, endpoint)) => {
                                if !inner.running.load(Ordering::Acquire) {
                                    replacement.disconnect();
                                    return;
                                }
                                remote_backoff = REBUILD_POLL_INTERVAL;
                                *inner.target.lock() = DaemonTarget::Remote {
                                    client: replacement.clone(),
                                    endpoint,
                                };
                                inner.transport_down.store(false, Ordering::Release);
                                inner.client_updates.lock().retain(|subscriber| {
                                    subscriber.send(replacement.clone()).is_ok()
                                });
                            }
                            Err(error) => {
                                if !inner.running.load(Ordering::Acquire) {
                                    return;
                                }
                                eprintln!("could not reconnect to the Waku daemon: {error:#}");
                                remote_backoff = (remote_backoff * 2).min(MAX_REMOTE_BACKOFF);
                                next_remote_attempt = Instant::now() + remote_backoff;
                            }
                        }
                    }
                }
                continue;
            }
            Some(DisconnectedTarget::Local {
                client: disconnected,
            }) if Instant::now() >= next_local_attempt => {
                // A dropped WebSocket says nothing about the daemon process.
                // Re-dial the same process so live provider runtimes survive;
                // the crashed-process path below stays reachable because a
                // backoff-gated tick matches no arm above and falls through.
                let _restart = inner.restart.lock();
                if !inner.running.load(Ordering::Acquire) {
                    return;
                }
                let current = {
                    let target = inner.target.lock();
                    match &*target {
                        DaemonTarget::Local(process)
                            if process.client().same_connection(&disconnected) =>
                        {
                            Some((
                                process.reconnect_address().to_owned(),
                                process.reconnect_token().to_owned(),
                                disconnected.last_sequences(),
                            ))
                        }
                        _ => None,
                    }
                };
                match current {
                    None => {
                        // Another path already replaced the target.
                        local_backoff = REBUILD_POLL_INTERVAL;
                        next_local_attempt = Instant::now();
                    }
                    Some((address, token, resume_from)) => {
                        {
                            // The target lock is released here: dialing a
                            // wedged daemon can block for seconds, and
                            // UI-side `client()` calls must not wait.
                            match DaemonClient::connect_with_resume(&address, token, resume_from) {
                                Ok(replacement) => {
                                    if !inner.running.load(Ordering::Acquire) {
                                        replacement.disconnect();
                                        return;
                                    }
                                    local_backoff = REBUILD_POLL_INTERVAL;
                                    next_local_attempt = Instant::now();
                                    let mut target = inner.target.lock();
                                    let replaced = match &mut *target {
                                        DaemonTarget::Local(process)
                                            if process.client().same_connection(&disconnected) =>
                                        {
                                            process.replace_client(replacement.clone());
                                            true
                                        }
                                        _ => false,
                                    };
                                    drop(target);
                                    if replaced {
                                        inner.transport_down.store(false, Ordering::Release);
                                        inner.client_updates.lock().retain(|subscriber| {
                                            subscriber.send(replacement.clone()).is_ok()
                                        });
                                    }
                                }
                                Err(error) => {
                                    if !inner.running.load(Ordering::Acquire) {
                                        return;
                                    }
                                    eprintln!("could not reconnect to the Waku daemon: {error:#}");
                                    local_backoff = (local_backoff * 2).min(MAX_REMOTE_BACKOFF);
                                    next_local_attempt = Instant::now() + local_backoff;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let process_exited = match &mut *inner.target.lock() {
            DaemonTarget::Local(process) => process.has_exited(),
            DaemonTarget::Restarting(_) => true,
            DaemonTarget::Remote { .. } => continue,
        };
        let Some(executable) = inner.executable.as_ref() else {
            return;
        };
        let observed_stamp = ExecutableStamp::read(executable).ok();
        let executable_changed = watch_for_rebuilds
            && observed_stamp.is_some_and(|observed| Some(observed) != active_stamp);
        if !process_exited && !executable_changed {
            continue;
        }
        if !process_exited && Instant::now() < next_upgrade_attempt {
            continue;
        }
        let _restart = inner.restart.lock();
        if !inner.running.load(Ordering::Acquire) {
            return;
        }
        let Some(exposure) = inner.exposure.lock().clone() else {
            return;
        };
        match replace_local_daemon(&inner, executable, &exposure) {
            Ok(()) => {}
            Err(error) => {
                eprintln!("could not restart rebuilt Waku daemon: {error:#}");
                next_upgrade_attempt = Instant::now() + MAX_REMOTE_BACKOFF;
                continue;
            }
        }
        queue_settings_refresh(&inner);
        if let Some(observed_stamp) = observed_stamp {
            active_stamp = Some(observed_stamp);
        }
        drop(_restart);
        drop(inner);
    }
}

fn replace_local_daemon(
    inner: &SupervisorInner,
    executable: &Path,
    exposure: &DaemonExposureSettings,
) -> anyhow::Result<()> {
    let client = inner.target.lock().client();
    if client.is_disconnected() {
        let mut target = inner.target.lock();
        let running = match &mut *target {
            DaemonTarget::Local(process) => !process.has_exited(),
            _ => false,
        };
        if running {
            bail!("resident daemon is unreachable; leaving its sessions intact");
        }
    }
    if !client.is_disconnected() {
        if let ResponsePayload::Terminals { terminals } =
            client.request(Uuid::nil(), Uuid::nil(), Command::ListTerminals)?
        {
            if terminals.iter().any(|terminal| !terminal.exited) {
                bail!("daemon restart deferred while terminal sessions are running");
            }
        }
    }
    let previous = {
        let mut target = inner.target.lock();
        if matches!(&*target, DaemonTarget::Remote { .. }) {
            bail!("the connected daemon is managed outside Waku Desktop");
        }
        target.take_local()
    };
    // Dropping can wait briefly for graceful shutdown, but the target lock is
    // already released so UI actions never block behind process teardown.
    if let Some(mut previous) = previous {
        previous.stop();
    }
    let replacement = DaemonProcess::spawn_configured(executable, exposure.clone())?;
    let client = replacement.client();
    *inner.target.lock() = DaemonTarget::Local(replacement);
    inner
        .client_updates
        .lock()
        .retain(|subscriber| subscriber.send(client.clone()).is_ok());
    Ok(())
}

#[cfg(unix)]
fn resident_process_alive(pid: u32) -> bool {
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}
#[cfg(windows)]
fn resident_process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    };
    if pid == 0 {
        return false;
    }
    unsafe {
        let synch_handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if !synch_handle.is_null() {
            // `STILL_ACTIVE` (259) is also a legal process exit code; `WaitForSingleObject`
            // is signaled (`WAIT_OBJECT_0`) only once the process actually terminates.
            let wait = WaitForSingleObject(synch_handle, 0);
            let mut code = 0_u32;
            let result = GetExitCodeProcess(synch_handle, &mut code);
            CloseHandle(synch_handle);
            if wait == WAIT_OBJECT_0 {
                return false;
            }
            if wait == WAIT_TIMEOUT {
                return result == 0 || code == STILL_ACTIVE as u32;
            }
            return result == 0 || code == STILL_ACTIVE as u32;
        }
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0_u32;
        let result = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        result == 0 || code == STILL_ACTIVE as u32
    }
}

fn wait_for_resident_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while resident_process_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    true
}

/// A Windows daemon originally spawned inside an agent sandbox inherits
/// `ProcessRedirectionTrustPolicy` (`EnforceRedirectionTrust`), which blocks
/// every provider CLI it spawns from traversing user-created NTFS symlinks.
/// When a normal desktop process sees such a resident daemon, replace it
/// instead of inheriting the sandbox restriction across sessions.
#[cfg(not(windows))]
fn resident_daemon_tainted(_pid: u32) -> bool {
    false
}

#[cfg(windows)]
fn resident_daemon_tainted(pid: u32) -> bool {
    !current_process_enforces_redirection_trust() && process_enforces_redirection_trust(pid)
}

#[cfg(windows)]
fn current_process_enforces_redirection_trust() -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe { handle_enforces_redirection_trust(GetCurrentProcess()) }
}

#[cfg(windows)]
fn process_enforces_redirection_trust(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let enforced = handle_enforces_redirection_trust(handle);
        CloseHandle(handle);
        enforced
    }
}

#[cfg(windows)]
unsafe fn handle_enforces_redirection_trust(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> bool {
    use windows_sys::Win32::System::Threading::{
        GetProcessMitigationPolicy, ProcessRedirectionTrustPolicy,
    };
    const ENFORCE_REDIRECTION_TRUST_MASK: u32 = 0x1;
    let mut flags: u32 = 0;
    let ok = unsafe {
        GetProcessMitigationPolicy(
            handle,
            ProcessRedirectionTrustPolicy,
            (&raw mut flags).cast(),
            std::mem::size_of::<u32>(),
        )
    };
    ok != 0 && (flags & ENFORCE_REDIRECTION_TRUST_MASK) != 0
}

fn queue_settings_refresh(inner: &SupervisorInner) {
    let settings = inner.settings.lock().clone();
    *inner.persisted_settings.lock() = None;
    let _ = inner.settings_updates.send(settings);
}

fn read_settings(client: &DaemonClient) -> anyhow::Result<DaemonSettings> {
    match client.request(Uuid::nil(), Uuid::nil(), Command::GetSettings)? {
        ResponsePayload::Settings { settings } => Ok(settings),
        _ => bail!("Waku daemon returned an invalid settings response"),
    }
}

fn persist_settings(
    weak_inner: std::sync::Weak<SupervisorInner>,
    updates: Receiver<DaemonSettings>,
) {
    while let Ok(mut settings) = updates.recv() {
        while let Ok(newer) = updates.try_recv() {
            settings = newer;
        }
        loop {
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            if !inner.running.load(Ordering::Acquire) {
                return;
            }
            let desired = inner.settings.lock().clone();
            if desired != settings {
                settings = desired;
            }
            let client = inner.target.lock().client();
            let result = client.request(
                Uuid::nil(),
                Uuid::nil(),
                Command::UpdateSettings {
                    settings: settings.clone(),
                },
            );
            match result {
                Ok(ResponsePayload::Ack) => {
                    *inner.persisted_settings.lock() = Some(settings);
                    break;
                }
                Ok(_) => {
                    eprintln!("Waku daemon returned an invalid settings update response");
                }
                Err(error) => {
                    eprintln!("could not persist Waku daemon settings: {error:#}");
                }
            }
            drop(inner);
            std::thread::sleep(REBUILD_POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_origins_are_exact_and_deduplicated() {
        assert_eq!(
            parse_allowed_origins(
                "https://app.waku.test, http://localhost:3001, https://app.waku.test"
            )
            .unwrap(),
            ["https://app.waku.test", "http://localhost:3001"]
        );
        assert!(parse_allowed_origins("https://app.waku.test/path").is_err());
        assert!(parse_allowed_origins("ws://app.waku.test").is_err());
    }

    #[test]
    fn desktop_uses_loopback_to_reach_an_unspecified_listener() {
        assert_eq!(
            desktop_client_address("0.0.0.0:34123").unwrap(),
            "127.0.0.1:34123"
        );
        assert_eq!(desktop_client_address("[::]:34123").unwrap(), "[::1]:34123");
    }

    #[cfg(windows)]
    #[test]
    fn redirection_trust_probe_matches_current_process_and_never_taints_self() {
        let pid = std::process::id();
        assert_eq!(
            process_enforces_redirection_trust(pid),
            current_process_enforces_redirection_trust()
        );
        assert!(!resident_daemon_tainted(pid));
    }

    #[cfg(windows)]
    #[test]
    fn resident_process_alive_distinguishes_live_and_exited_259_processes() {
        assert!(!resident_process_alive(0));
        assert!(resident_process_alive(std::process::id()));

        let mut child = ProcessCommand::new("cmd.exe")
            .args(["/d", "/c", "exit 259"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "exit-259 child did not finish");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(259));
        // `child` is still in scope holding the Windows process handle, so
        // `OpenProcess` still observes exit code 259 (`STILL_ACTIVE`).
        assert!(
            !resident_process_alive(pid),
            "exited process with exit code 259 must not be reported alive"
        );
        drop(child);
        assert!(!resident_process_alive(pid));
    }

    #[test]
    fn supervisor_handle_drop_stops_monitoring_and_stop_local_shuts_down_local_daemon() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for stream in listener.incoming().take(2) {
                let Ok(stream) = stream else { break };
                let shutdown_tx = shutdown_tx.clone();
                handlers.push(std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut ws = tungstenite::accept(stream).unwrap();
                    let hello = match ws.read().unwrap() {
                        tungstenite::Message::Text(text) => {
                            serde_json::from_str::<waku_protocol::ClientMessage>(text.as_ref())
                                .unwrap()
                        }
                        other => panic!("unexpected handshake frame: {other:?}"),
                    };
                    assert!(matches!(hello, waku_protocol::ClientMessage::Hello { .. }));
                    ws.send(tungstenite::Message::Text(
                        serde_json::to_string(&waku_protocol::ServerMessage::Hello {
                            protocol_version: PROTOCOL_VERSION,
                            daemon_version: "test".into(),
                        })
                        .unwrap()
                        .into(),
                    ))
                    .unwrap();
                    while let Ok(msg) = ws.read() {
                        if let tungstenite::Message::Text(text) = msg {
                            if let Ok(waku_protocol::ClientMessage::Shutdown) =
                                serde_json::from_str(text.as_ref())
                            {
                                let _ = shutdown_tx.send(());
                                break;
                            }
                        } else if matches!(msg, tungstenite::Message::Close(_)) {
                            break;
                        }
                    }
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });

        let client1 = DaemonClient::connect(&addr, "tok".into()).unwrap();
        let supervisor = DaemonSupervisor::from_target(
            DaemonTarget::Restarting(client1.clone()),
            None,
            None,
            DaemonSettings::default(),
        )
        .unwrap();
        // Simulate `monitor_daemon` holding an upgraded `Arc<SupervisorInner>`.
        let active_monitor_arc = Arc::clone(&supervisor.inner);
        let cloned = supervisor.clone();
        assert!(active_monitor_arc.running.load(Ordering::Acquire));
        drop(supervisor);
        assert!(active_monitor_arc.running.load(Ordering::Acquire));
        drop(cloned);
        assert!(!active_monitor_arc.running.load(Ordering::Acquire));
        client1.disconnect();

        let client2 = DaemonClient::connect(&addr, "tok".into()).unwrap();
        let child = ProcessCommand::new(std::env::current_exe().unwrap())
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let state_file =
            std::env::temp_dir().join(format!("waku-supervisor-test-{}.json", Uuid::new_v4()));
        let process = DaemonProcess {
            client: Mutex::new(client2),
            address: addr,
            token: "tok".into(),
            child: Some(child),
            state_file,
            pid: std::process::id(),
        };
        let local_supervisor = DaemonSupervisor::from_target(
            DaemonTarget::Local(process),
            None,
            None,
            DaemonSettings::default(),
        )
        .unwrap();
        local_supervisor.stop_local();
        assert!(!local_supervisor.inner.running.load(Ordering::Acquire));
        assert!(matches!(
            &*local_supervisor.inner.target.lock(),
            DaemonTarget::Restarting(_)
        ));
        shutdown_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("stop_local should send ClientMessage::Shutdown");
        server.join().unwrap();
    }
}
