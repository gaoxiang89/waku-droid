//! `waku-daemon bridge`: the stdio entry point for WSL and SSH hosts.
//!
//! The desktop launches this through `wsl.exe --exec` or `ssh`, so its stdout
//! is the transport and must carry nothing but the [`BridgeReady`] line and
//! the relayed bytes. Diagnostics go to stderr, which the desktop reports.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use waku_protocol::PROTOCOL_VERSION;
use waku_protocol::bridge::{BridgeReady, BridgeState, Fingerprint};

pub const STATE_FILE: &str = "daemon.json";
const LOCK_FILE: &str = "daemon.lock";
const LOG_FILE: &str = "daemon.log";
/// How long a bridge-managed daemon stays up with no client and no runtime.
pub const IDLE_EXIT_SECONDS: u64 = 30 * 60;
const START_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

pub fn file_fingerprint(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut fingerprint = Fingerprint::default();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(fingerprint.finish());
        }
        fingerprint.update(&buffer[..read]);
    }
}

pub fn own_fingerprint() -> anyhow::Result<String> {
    let executable = std::env::current_exe().context("could not locate waku-daemon")?;
    file_fingerprint(&executable)
        .with_context(|| format!("could not fingerprint {}", executable.display()))
}

/// Persist the listener record a bridge reads. The token makes this a
/// credential, so it is created owner-only and swapped in atomically.
pub fn write_state(path: &Path, state: &BridgeState) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        file.write_all(serde_json::to_string(state)?.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)
        .with_context(|| format!("could not install {}", path.display()))?;
    Ok(())
}

pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let mut state_dir = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| anyhow!("--state-dir requires a path"))?,
                ));
            }
            unknown => bail!("unknown bridge argument {unknown:?}"),
        }
    }
    let executable = std::env::current_exe().context("could not locate waku-daemon")?;
    let state_dir = match state_dir {
        Some(directory) => directory,
        None => executable
            .parent()
            .ok_or_else(|| anyhow!("waku-daemon has no parent directory"))?
            .to_owned(),
    };
    fs::create_dir_all(&state_dir)
        .with_context(|| format!("could not create {}", state_dir.display()))?;
    let fingerprint = file_fingerprint(&executable)
        .with_context(|| format!("could not fingerprint {}", executable.display()))?;

    let (state, stream) = {
        let _lock = platform::lock(&state_dir.join(LOCK_FILE))?;
        match reusable_daemon(&state_dir, &fingerprint)? {
            Some(reused) => reused,
            None => {
                let state = start_daemon(&executable, &state_dir, &fingerprint)?;
                let stream = TcpStream::connect(&state.address)
                    .with_context(|| format!("could not reach waku-daemon at {}", state.address))?;
                (state, stream)
            }
        }
    };
    let ready = BridgeReady {
        token: state.token,
        protocol_version: state.protocol_version,
        fingerprint: state.fingerprint,
        pid: state.pid,
    };
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{}", serde_json::to_string(&ready)?)?;
    stdout.flush()?;
    drop(stdout);
    relay(stream)
}

/// The running daemon from this build, already connected. The probe socket
/// becomes the relayed one, so the daemon never sees an abandoned handshake.
fn reusable_daemon(
    state_dir: &Path,
    fingerprint: &str,
) -> anyhow::Result<Option<(BridgeState, TcpStream)>> {
    let Some(state) = reusable_daemon_checked(state_dir, fingerprint)? else {
        return Ok(None);
    };
    let address = state.address.parse()?;
    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .context("resident daemon is unreachable; leaving its sessions intact")?;
    Ok(Some((state, stream)))
}

fn reusable_daemon_checked(
    state_dir: &Path,
    fingerprint: &str,
) -> anyhow::Result<Option<BridgeState>> {
    let Some(state) = read_live_state(state_dir) else {
        return Ok(None);
    };
    if state.protocol_version != PROTOCOL_VERSION {
        bail!(
            "running daemon uses protocol {}; stop its sessions before upgrading to protocol {}",
            state.protocol_version,
            PROTOCOL_VERSION
        );
    }
    if state.fingerprint != fingerprint {
        let client = waku_client::DaemonClient::connect(&state.address, state.token.clone())?;
        let result = client.request(
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            waku_protocol::Command::ListTerminals,
        );
        client.disconnect();
        match result? {
            waku_protocol::ResponsePayload::Terminals { terminals }
                if terminals.iter().any(|t| !t.exited) =>
            {
                return Ok(Some(state));
            }
            waku_protocol::ResponsePayload::Terminals { .. } => {}
            _ => bail!("could not verify terminal sessions before daemon upgrade"),
        }
        platform::terminate(state.pid);
        return Ok(None);
    }
    Ok(Some(state))
}

fn read_live_state(state_dir: &Path) -> Option<BridgeState> {
    let text = fs::read_to_string(state_dir.join(STATE_FILE)).ok()?;
    let state = serde_json::from_str::<BridgeState>(&text).ok()?;
    if !platform::process_is_alive(state.pid) {
        return None;
    }
    Some(state)
}

fn start_daemon(
    executable: &Path,
    state_dir: &Path,
    fingerprint: &str,
) -> anyhow::Result<BridgeState> {
    let state_path = state_dir.join(STATE_FILE);
    let _ = fs::remove_file(&state_path);
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(LOG_FILE))
        .context("could not open the waku-daemon log")?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let mut command = std::process::Command::new(executable);
    command
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--state-file")
        .arg(&state_path)
        .arg("--idle-exit-secs")
        .arg(IDLE_EXIT_SECONDS.to_string())
        .env(waku_protocol::DAEMON_TOKEN_ENV, &token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    if let Some(home) = dirs::home_dir() {
        command.current_dir(home);
    }
    // A debug build would otherwise use the checkout path it was compiled
    // in, which on WSL is often the Windows host's own data via /mnt/c.
    if cfg!(debug_assertions)
        && std::env::var_os(waku_core::persistence::DATA_DIRECTORY_ENV).is_none()
        && let Some(data) = dirs::data_local_dir()
    {
        command.env(
            waku_core::persistence::DATA_DIRECTORY_ENV,
            data.join(waku_protocol::identity::DATA_DIRECTORY_NAME),
        );
    }
    platform::detach(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not launch {}", executable.display()))?;
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(text) = fs::read_to_string(&state_path)
            && let Ok(state) = serde_json::from_str::<BridgeState>(&text)
            && state.pid == child.id()
        {
            if state.fingerprint != fingerprint {
                bail!("waku-daemon reported an unexpected fingerprint");
            }
            return Ok(state);
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "waku-daemon exited during startup ({status}); see {}",
                state_dir.join(LOG_FILE).display()
            );
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!("timed out waiting for waku-daemon to start");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Copy bytes both ways until either side closes. Closing stdin (the desktop
/// went away) shuts the socket down so the daemon sees a disconnect promptly.
fn relay(stream: TcpStream) -> anyhow::Result<()> {
    let mut upstream = stream.try_clone()?;
    let inbound = std::thread::Builder::new()
        .name("waku-bridge-inbound".into())
        .spawn(move || {
            let _ = io::copy(&mut io::stdin().lock(), &mut upstream);
            let _ = upstream.shutdown(std::net::Shutdown::Both);
        })?;
    let mut downstream = stream;
    let mut stdout = io::stdout().lock();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match downstream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if stdout.write_all(&buffer[..read]).is_err() || stdout.flush().is_err() {
            break;
        }
    }
    let _ = downstream.shutdown(std::net::Shutdown::Both);
    drop(stdout);
    // The inbound thread blocks on stdin, which only the desktop can close.
    // Exiting the process ends it; joining would wait on the peer.
    drop(inbound);
    Ok(())
}

#[cfg(unix)]
mod platform {
    use std::fs;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use anyhow::Context as _;

    pub struct Lock(#[allow(dead_code)] fs::File);

    pub fn lock(path: &Path) -> anyhow::Result<Lock> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("could not open {}", path.display()))?;
        // The kernel drops the lock when this process exits, so a crashed
        // bridge can never wedge later ones.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("could not lock {}", path.display()));
        }
        Ok(Lock(file))
    }

    pub fn process_is_alive(pid: u32) -> bool {
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    pub fn terminate(pid: u32) {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !process_is_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }

    /// A new session detaches the daemon from the SSH or WSL session, so a
    /// hangup on disconnect does not take running agents down with it.
    pub fn detach(command: &mut std::process::Command) {
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
}

#[cfg(not(unix))]
mod platform {
    use std::path::Path;

    pub struct Lock;

    pub fn lock(_path: &Path) -> anyhow::Result<Lock> {
        anyhow::bail!("waku-daemon bridge runs only on Linux and macOS hosts")
    }

    pub fn process_is_alive(_pid: u32) -> bool {
        false
    }

    pub fn terminate(_pid: u32) {}

    pub fn detach(_command: &mut std::process::Command) {}
}
