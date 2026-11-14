//! Desktop end of a WSL or SSH daemon connection.
//!
//! A [`BridgeSession`] launches `waku-daemon bridge` on the host through
//! `wsl.exe` or `ssh`, reads its [`BridgeReady`] line, and exposes the relayed
//! stream on a one-shot loopback listener. [`crate::DaemonClient`] then
//! connects to that listener exactly as it would to a local daemon, so the
//! wire protocol, replay, and reconnect logic stay transport-agnostic.

use std::io::{self, BufRead as _, BufReader, Read, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use parking_lot::Mutex;
use waku_protocol::PROTOCOL_VERSION;
use waku_protocol::bridge::{BridgeBlocked, BridgeReady, IncompatibleDaemon};

use super::connection::{Connection, ConnectionKind};
use super::deploy::{self, DeployOutcome, RemoteTarget};
use super::process_util::{
    ChildJob, StderrTail, adopt_child_into_kill_job, wait_with_deadline, with_detail,
};
use super::{ssh, wsl};

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);

/// Connection stage reported on failure, so the UI can say where it broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeStage {
    Probe,
    Deploy,
    Bridge,
}

impl BridgeStage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Deploy => "deploy",
            Self::Bridge => "bridge",
        }
    }
}

#[derive(Debug)]
pub struct BridgeError {
    pub stage: BridgeStage,
    pub error: anyhow::Error,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} failed: {:#}", self.stage.label(), self.error)
    }
}

impl std::error::Error for BridgeError {}

/// A live `wsl.exe`/`ssh` child relaying one daemon connection. Dropping it
/// kills the child; the remote daemon keeps running.
pub struct BridgeSession {
    target: RemoteTarget,
    address: String,
    token: String,
    remote_pid: u32,
    child: Mutex<Child>,
    _job: Option<ChildJob>,
    stderr: StderrTail,
}

impl BridgeSession {
    /// Prepare the host and start a bridge. Blocking and potentially slow on
    /// first use (probe and upload); run it off the UI thread.
    pub fn open(connection: &Connection) -> Result<Self, BridgeError> {
        Self::open_with(connection, false)
    }

    /// [`Self::open`], optionally ending a running daemon that speaks another
    /// protocol together with its sessions. Pass `true` only after the user
    /// confirmed the [`IncompatibleDaemonError`] this would otherwise return.
    pub fn open_with(
        connection: &Connection,
        replace_incompatible: bool,
    ) -> Result<Self, BridgeError> {
        let stage = |stage| move |error| BridgeError { stage, error };
        let target = deploy::probe(connection).map_err(stage(BridgeStage::Probe))?;
        let outcome =
            deploy::deploy(connection, target, false).map_err(stage(BridgeStage::Deploy))?;
        match Self::spawn_with(connection, target, replace_incompatible) {
            Ok(session) => Ok(session),
            // A reused install that speaks another protocol is the one case a
            // reinstall can fix, e.g. two Waku builds sharing a host.
            Err(error)
                if outcome == DeployOutcome::Reused
                    && error.downcast_ref::<ProtocolMismatch>().is_some() =>
            {
                deploy::deploy(connection, target, true).map_err(stage(BridgeStage::Deploy))?;
                Self::spawn_with(connection, target, replace_incompatible)
                    .map_err(stage(BridgeStage::Bridge))
            }
            Err(error) => Err(stage(BridgeStage::Bridge)(error)),
        }
    }

    /// Start a bridge against the installed daemon on reconnect. Only failures a
    /// reinstall can fix fall back to a full probe and deploy; transport failures
    /// and timeouts return the original error so the monitor backs off.
    pub fn reconnect(connection: &Connection, target: RemoteTarget) -> Result<Self, BridgeError> {
        match Self::spawn_with(connection, target, false) {
            Ok(session) => Ok(session),
            Err(error) if deploy_may_fix(connection, &error) => Self::open_with(connection, false),
            Err(error) => Err(BridgeError {
                stage: BridgeStage::Bridge,
                error,
            }),
        }
    }

    fn spawn_with(
        connection: &Connection,
        target: RemoteTarget,
        replace_incompatible: bool,
    ) -> anyhow::Result<Self> {
        let mut command = match &connection.kind {
            ConnectionKind::Local => bail!("the local connection does not use a bridge"),
            ConnectionKind::Wsl { distro } => {
                let script = deploy::bridge_script(replace_incompatible);
                wsl::command(distro, &script, &[])
            }
            ConnectionKind::Ssh { .. } => {
                let mut command = ssh::command(connection)?;
                command.arg(deploy::ssh_bridge_command(target, replace_incompatible));
                command
            }
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("could not start the bridge to {}", connection.name))?;
        let mut job = adopt_child_into_kill_job(&child);
        let stdin = child.stdin.take().context("bridge stdin is unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("bridge stdout is unavailable")?;
        let stderr = StderrTail::capture(
            child
                .stderr
                .take()
                .context("bridge stderr is unavailable")?,
        );

        let fail = |child: &mut Child, job: &mut Option<ChildJob>, error: anyhow::Error| {
            let _ = child.kill();
            drop(job.take());
            let _ = child.wait();
            let detail = ssh::append_auth_hint(connection, stderr.finish_and_text());
            if detail.is_empty() {
                error
            } else {
                error.context(detail)
            }
        };

        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("waku-bridge-ready".into())
            .spawn(move || {
                let _ = ready_tx.send(read_ready_from_reader(BufReader::new(stdout)));
            })
            .context("could not start the bridge reader")?;
        let (ready, stdout) = match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(ready)) => ready,
            Ok(Err(error)) => {
                let error = if error.downcast_ref::<BridgeExited>().is_some() {
                    anyhow::Error::new(BridgeExited {
                        code: wait_with_deadline(&mut child, Duration::from_secs(2))
                            .and_then(|status| status.code()),
                    })
                } else {
                    error
                };
                return Err(fail(&mut child, &mut job, error));
            }
            Err(_) => {
                return Err(fail(
                    &mut child,
                    &mut job,
                    anyhow!("timed out waiting for the remote daemon"),
                ));
            }
        };
        if ready.protocol_version != PROTOCOL_VERSION {
            return Err(fail(
                &mut child,
                &mut job,
                anyhow::Error::new(ProtocolMismatch {
                    remote: ready.protocol_version,
                }),
            ));
        }
        // Discard non-fatal login/profile stderr emitted before the bridge
        // announced readiness so a later disconnect reports only runtime errors.
        stderr.clear();

        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) => return Err(fail(&mut child, &mut job, error.into())),
        };
        let address = match listener.local_addr() {
            Ok(address) => address.to_string(),
            Err(error) => return Err(fail(&mut child, &mut job, error.into())),
        };
        std::thread::Builder::new()
            .name("waku-bridge-relay".into())
            .spawn(move || relay(listener, stdin, stdout))
            .context("could not start the bridge relay")?;
        Ok(Self {
            target,
            address,
            token: ready.token,
            remote_pid: ready.pid,
            child: Mutex::new(child),
            _job: job,
            stderr,
        })
    }

    pub fn target(&self) -> RemoteTarget {
        self.target
    }

    /// Loopback address the daemon client connects to, once.
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn remote_pid(&self) -> u32 {
        self.remote_pid
    }

    pub fn has_exited(&self) -> bool {
        !matches!(self.child.lock().try_wait(), Ok(None))
    }

    /// Recent bridge stderr, for diagnostics after a disconnect.
    pub fn stderr(&self) -> String {
        self.stderr.text()
    }
}

impl Drop for BridgeSession {
    fn drop(&mut self) {
        let mut child = self.child.lock();
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// The bridge process exited before announcing readiness. `code` is its exit
/// status when known; OpenSSH reports transport, DNS, auth, and host-key
/// failures as 255.
#[derive(Debug)]
pub struct BridgeExited {
    pub code: Option<i32>,
}

impl std::fmt::Display for BridgeExited {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => {
                write!(
                    formatter,
                    "the bridge exited with status {code} before it was ready"
                )
            }
            None => write!(formatter, "the bridge exited before it was ready"),
        }
    }
}

impl std::error::Error for BridgeExited {}

#[derive(Debug)]
pub struct ProtocolMismatch {
    pub remote: u32,
}

impl std::fmt::Display for ProtocolMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the remote daemon speaks protocol {} but this desktop speaks {PROTOCOL_VERSION}",
            self.remote
        )
    }
}

impl std::error::Error for ProtocolMismatch {}

/// The host still runs a daemon that speaks another protocol, with sessions
/// this desktop cannot reach. Nothing was stopped; retrying with
/// [`BridgeSession::open_with`]`(.., true)` ends that daemon and its sessions.
#[derive(Clone, Debug)]
pub struct IncompatibleDaemonError(pub IncompatibleDaemon);

impl std::fmt::Display for IncompatibleDaemonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "running daemon uses protocol {}; stop its sessions before upgrading to protocol {PROTOCOL_VERSION}",
            self.0.protocol_version
        )
    }
}

impl std::error::Error for IncompatibleDaemonError {}

impl IncompatibleDaemonError {
    /// Find this error anywhere in a connection failure's chain.
    pub fn find(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| {
            cause.downcast_ref::<Self>().or_else(|| {
                cause
                    .downcast_ref::<BridgeError>()
                    .and_then(|bridge| bridge.error.downcast_ref::<Self>())
            })
        })
    }
}

fn deploy_may_fix(connection: &Connection, error: &anyhow::Error) -> bool {
    if error.downcast_ref::<ProtocolMismatch>().is_some() {
        return true;
    }
    error
        .downcast_ref::<BridgeExited>()
        .and_then(|exited| exited.code)
        .is_some_and(|code| !(code == 255 && matches!(connection.kind, ConnectionKind::Ssh { .. })))
}

fn read_ready_from_reader<R: Read>(
    mut reader: BufReader<R>,
) -> anyhow::Result<(BridgeReady, BufReader<R>)> {
    const MAX_LEADING_LINES: usize = 64;
    let mut line = String::new();
    let mut last_invalid = None;
    for _ in 0..MAX_LEADING_LINES {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(match last_invalid {
                    Some(invalid) => anyhow!("the bridge sent an invalid ready line {invalid:?}"),
                    None => BridgeExited { code: None }.into(),
                });
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match parse_ready_line(trimmed) {
                    Ok(ready) => return Ok((ready, reader)),
                    Err(error) if error.downcast_ref::<IncompatibleDaemonError>().is_some() => {
                        return Err(error);
                    }
                    Err(_) => {
                        last_invalid = Some(trimmed.to_owned());
                    }
                }
            }
            Err(error) => return Err(anyhow!("could not read the bridge: {error}")),
        }
    }
    Err(match last_invalid {
        Some(invalid) => anyhow!("the bridge sent an invalid ready line {invalid:?}"),
        None => BridgeExited { code: None }.into(),
    })
}

fn parse_ready_line(line: &str) -> anyhow::Result<BridgeReady> {
    if let Ok(ready) = serde_json::from_str::<BridgeReady>(line) {
        return Ok(ready);
    }
    if let Ok(blocked) = serde_json::from_str::<BridgeBlocked>(line) {
        return Err(IncompatibleDaemonError(blocked.incompatible_daemon).into());
    }
    Err(anyhow!("the bridge sent an invalid ready line {line:?}"))
}

/// Accept the daemon client's single connection and splice it to the bridge's
/// stdio. Whichever side closes first tears the other down, so a dead bridge
/// surfaces as an ordinary client disconnect and triggers a reconnect.
fn relay(listener: TcpListener, mut stdin: ChildStdin, mut stdout: impl Read + Send + 'static) {
    let Some(socket) = accept_one(&listener) else {
        return;
    };
    drop(listener);
    let _ = socket.set_nodelay(true);
    let Ok(mut inbound) = socket.try_clone() else {
        return;
    };
    let outbound = std::thread::Builder::new()
        .name("waku-bridge-outbound".into())
        .spawn(move || {
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                match inbound.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if stdin.write_all(&buffer[..read]).is_err() || stdin.flush().is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            // Closing stdin tells the bridge, and through it the daemon
            // socket, that this client is gone.
            drop(stdin);
            let _ = inbound.shutdown(Shutdown::Both);
        });
    let mut socket = socket;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if socket.write_all(&buffer[..read]).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = socket.shutdown(Shutdown::Both);
    if let Ok(outbound) = outbound {
        let _ = outbound.join();
    }
}

fn accept_one(listener: &TcpListener) -> Option<TcpStream> {
    listener.set_nonblocking(true).ok()?;
    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((socket, peer)) if peer.ip().is_loopback() => {
                socket.set_nonblocking(false).ok()?;
                return Some(socket);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// Probe, deploy, and handshake once without keeping the connection, for a
/// settings page "Test" button. Returns the remote daemon's pid.
pub fn test_connection(connection: &Connection) -> Result<u32, BridgeError> {
    let session = BridgeSession::open(connection)?;
    match crate::DaemonClient::connect(session.address(), session.token().to_owned()) {
        Ok(client) => {
            drop(client);
            Ok(session.remote_pid())
        }
        Err(error) => {
            let stderr = session.stderr.clone();
            drop(session);
            let detail = ssh::append_auth_hint(connection, stderr.finish_and_text());
            Err(BridgeError {
                stage: BridgeStage::Bridge,
                error: anyhow!("{}", with_detail(&format!("{error:#}"), &detail)),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_line_surfaces_as_incompatible_daemon() {
        let line = r#"{"incompatibleDaemon":{"pid":9,"protocolVersion":7,"processes":[{"pid":10,"depth":0,"command":"bash"}]}}"#;
        let error = parse_ready_line(line).unwrap_err();
        let wrapped: anyhow::Error = BridgeError {
            stage: BridgeStage::Bridge,
            error: error.context("stderr detail"),
        }
        .into();
        let found = IncompatibleDaemonError::find(&wrapped).expect("incompatible daemon");
        assert_eq!(found.0.pid, 9);
        assert_eq!(found.0.processes[0].command, "bash");
        assert_eq!(found.0.omitted, 0);
    }

    #[test]
    fn ready_line_still_parses() {
        let line = r#"{"token":"t","protocolVersion":8,"fingerprint":"f","pid":3}"#;
        assert_eq!(parse_ready_line(line).unwrap().pid, 3);
        assert!(parse_ready_line("nonsense").is_err());
    }

    #[test]
    fn ready_reader_skips_profile_noise_and_keeps_trailing_bytes() {
        let mut payload = b"Welcome banner\r\n\r\n{\"token\":\"t\",\"protocolVersion\":8,\"fingerprint\":\"f\",\"pid\":3}\n".to_vec();
        let binary_tail = [0x81_u8, 0x02, 0x00, 0xFF];
        payload.extend_from_slice(&binary_tail);
        let (ready, mut reader) =
            read_ready_from_reader(BufReader::new(payload.as_slice())).unwrap();
        assert_eq!(ready.pid, 3);
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, binary_tail);
    }

    #[test]
    fn ready_reader_surfaces_incompatible_daemon_and_invalid_eof() {
        let blocked = b"profile banner\n{\"incompatibleDaemon\":{\"pid\":9,\"protocolVersion\":7,\"processes\":[]}}\n";
        let err = read_ready_from_reader(BufReader::new(blocked.as_slice()))
            .err()
            .expect("incompatible daemon error");
        assert!(err.downcast_ref::<IncompatibleDaemonError>().is_some());

        let invalid = b"nonsense\n";
        let err = read_ready_from_reader(BufReader::new(invalid.as_slice()))
            .err()
            .expect("invalid ready line");
        assert!(err.to_string().contains("invalid ready line \"nonsense\""));
    }

    #[test]
    fn empty_ready_stream_reports_bridge_exit() {
        let err = read_ready_from_reader(BufReader::new([].as_slice()))
            .err()
            .expect("empty stream error");
        assert_eq!(err.downcast_ref::<BridgeExited>().unwrap().code, None);
        assert_eq!(err.to_string(), "the bridge exited before it was ready");
    }

    fn ssh_connection() -> Connection {
        Connection {
            id: "ssh:test".to_owned(),
            name: "test".to_owned(),
            kind: ConnectionKind::Ssh {
                target: crate::remote::connection::SshTarget::ConfigHost {
                    host: "example".to_owned(),
                    auth_method: Default::default(),
                },
            },
        }
    }

    fn wsl_connection() -> Connection {
        Connection {
            id: "wsl:ubuntu".to_owned(),
            name: "ubuntu".to_owned(),
            kind: ConnectionKind::Wsl {
                distro: "Ubuntu".to_owned(),
            },
        }
    }

    #[test]
    fn deploy_may_fix_only_for_failures_a_reinstall_can_fix() {
        let ssh = ssh_connection();
        let wsl = wsl_connection();
        let mismatch = || {
            anyhow::Error::new(ProtocolMismatch {
                remote: PROTOCOL_VERSION + 1,
            })
        };
        assert!(deploy_may_fix(&ssh, &mismatch()));
        assert!(deploy_may_fix(&wsl, &mismatch()));

        let exited = |code| anyhow::Error::new(BridgeExited { code });
        assert!(deploy_may_fix(&ssh, &exited(Some(127))));
        assert!(deploy_may_fix(&ssh, &exited(Some(1))));
        assert!(!deploy_may_fix(&ssh, &exited(Some(255))));
        assert!(deploy_may_fix(&wsl, &exited(Some(255))));
        assert!(!deploy_may_fix(&ssh, &exited(None)));
        assert!(!deploy_may_fix(&wsl, &exited(None)));

        assert!(!deploy_may_fix(
            &ssh,
            &anyhow!("timed out waiting for the remote daemon")
        ));
        assert!(deploy_may_fix(
            &ssh,
            &anyhow::Error::new(BridgeExited { code: Some(127) }).context("stderr detail")
        ));
    }
}
