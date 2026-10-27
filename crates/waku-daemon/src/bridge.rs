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
use waku_protocol::bridge::{
    BridgeBlocked, BridgeReady, BridgeState, DaemonProcess, Fingerprint, IncompatibleDaemon,
    REPLACE_INCOMPATIBLE_FLAG,
};

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
    let mut replace_incompatible = false;
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
            REPLACE_INCOMPATIBLE_FLAG => replace_incompatible = true,
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
        let reused = match reusable_daemon(&state_dir, &fingerprint, replace_incompatible) {
            Ok(reused) => reused,
            Err(error) => {
                // The desktop cannot speak to that daemon, so describe it here
                // and let the user decide whether to end its sessions.
                if let Some(Blocked(daemon)) = error.downcast_ref::<Blocked>() {
                    let line = serde_json::to_string(&BridgeBlocked {
                        incompatible_daemon: daemon.clone(),
                    })?;
                    let mut stdout = io::stdout().lock();
                    writeln!(stdout, "{line}")?;
                    stdout.flush()?;
                }
                return Err(error);
            }
        };
        match reused {
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
    replace_incompatible: bool,
) -> anyhow::Result<Option<(BridgeState, TcpStream)>> {
    let Some(state) = reusable_daemon_checked(state_dir, fingerprint, replace_incompatible)? else {
        return Ok(None);
    };
    let address = state.address.parse()?;
    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .context("resident daemon is unreachable; leaving its sessions intact")?;
    Ok(Some((state, stream)))
}

/// A running daemon that speaks another protocol, left intact.
#[derive(Debug)]
struct Blocked(IncompatibleDaemon);

impl std::fmt::Display for Blocked {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "running daemon uses protocol {}; stop its sessions before upgrading to protocol {}",
            self.0.protocol_version, PROTOCOL_VERSION
        )
    }
}

impl std::error::Error for Blocked {}

fn reusable_daemon_checked(
    state_dir: &Path,
    fingerprint: &str,
    replace_incompatible: bool,
) -> anyhow::Result<Option<BridgeState>> {
    let Some(state) = read_live_state(state_dir) else {
        return Ok(None);
    };
    if state.protocol_version != PROTOCOL_VERSION {
        if !replace_incompatible {
            return Err(Blocked(describe_daemon(&state)).into());
        }
        end_daemon_and_sessions(state.pid);
        let _ = fs::remove_file(state_dir.join(STATE_FILE));
        return Ok(None);
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

/// One row of the host's process table.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessEntry {
    pid: u32,
    parent: u32,
    command: String,
}

/// The daemon's descendants, depth-first, from the OS rather than the wire:
/// the bridge cannot speak the old daemon's protocol.
fn describe_daemon(state: &BridgeState) -> IncompatibleDaemon {
    describe_tree(state.pid, state.protocol_version, &platform::process_table())
}

fn describe_tree(pid: u32, protocol_version: u32, table: &[ProcessEntry]) -> IncompatibleDaemon {
    let mut processes = Vec::new();
    let mut omitted = 0_u32;
    let mut stack: Vec<(u32, u32)> = children_of(pid, table)
        .rev()
        .map(|entry| (entry.pid, 0))
        .collect();
    let mut seen = std::collections::HashSet::from([pid]);
    while let Some((child, depth)) = stack.pop() {
        if !seen.insert(child) {
            continue;
        }
        let Some(entry) = table.iter().find(|entry| entry.pid == child) else {
            continue;
        };
        if processes.len() < IncompatibleDaemon::MAX_PROCESSES {
            processes.push(DaemonProcess {
                pid: entry.pid,
                depth,
                command: entry.command.clone(),
            });
        } else {
            omitted += 1;
        }
        stack.extend(
            children_of(child, table)
                .rev()
                .map(|entry| (entry.pid, depth + 1)),
        );
    }
    IncompatibleDaemon {
        pid,
        protocol_version,
        processes,
        omitted,
    }
}

fn children_of(
    pid: u32,
    table: &[ProcessEntry],
) -> impl DoubleEndedIterator<Item = &ProcessEntry> {
    table.iter().filter(move |entry| entry.parent == pid)
}

/// End an incompatible daemon and the sessions it owns. Its direct children
/// are terminal shells and agent processes; closing the daemon's PTY masters
/// already hangs up their foreground jobs, but agents run on pipes and a
/// shell ignores SIGTERM, so each child is hung up and terminated as well.
/// Processes a user deliberately detached from those sessions are left alone.
fn end_daemon_and_sessions(pid: u32) {
    let children: Vec<u32> = children_of(pid, &platform::process_table())
        .map(|entry| entry.pid)
        .collect();
    platform::terminate(pid);
    platform::end_children(&children);
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

/// Longest command line kept per process; the desktop only shows a summary.
const MAX_COMMAND_CHARS: usize = 160;

fn shorten_command(command: &str) -> String {
    let command = command.trim();
    match command.char_indices().nth(MAX_COMMAND_CHARS) {
        Some((end, _)) => format!("{}…", &command[..end]),
        None => command.to_owned(),
    }
}

/// `(comm, ppid)` from `/proc/<pid>/stat`. `comm` may itself contain spaces
/// and parentheses, so the fields after it are found from the last `)`.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_proc_stat(stat: &str) -> Option<(String, u32)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_owned();
    let mut fields = stat.get(close + 1..)?.split_whitespace();
    let _state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    Some((name, parent))
}

#[cfg_attr(not(unix), allow(dead_code))]
fn join_cmdline(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    shorten_command(
        &text
            .split('\0')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Rows of `ps -Ao pid=,ppid=,command=`.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_ps(output: &str) -> Vec<ProcessEntry> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            let (parent, command) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            Some(ProcessEntry {
                pid: pid.parse().ok()?,
                parent: parent.parse().ok()?,
                command: shorten_command(command),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, parent: u32, command: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent,
            command: command.into(),
        }
    }

    #[test]
    fn describes_descendants_depth_first() {
        let table = [
            entry(1, 0, "init"),
            entry(10, 1, "waku-daemon"),
            entry(11, 10, "bash"),
            entry(12, 11, "vim notes"),
            entry(13, 10, "claude"),
            entry(20, 1, "unrelated"),
        ];
        let daemon = describe_tree(10, 7, &table);
        let rows: Vec<_> = daemon
            .processes
            .iter()
            .map(|process| (process.pid, process.depth))
            .collect();
        assert_eq!(rows, [(11, 0), (12, 1), (13, 0)]);
        assert_eq!(daemon.omitted, 0);
        assert_eq!(daemon.protocol_version, 7);
    }

    #[test]
    fn caps_described_processes() {
        let mut table = vec![entry(10, 1, "waku-daemon")];
        let extra = IncompatibleDaemon::MAX_PROCESSES as u32 + 3;
        table.extend((0..extra).map(|index| entry(100 + index, 10, "bash")));
        let daemon = describe_tree(10, 7, &table);
        assert_eq!(daemon.processes.len(), IncompatibleDaemon::MAX_PROCESSES);
        assert_eq!(daemon.omitted, 3);
    }

    #[test]
    fn parses_proc_stat_with_awkward_names() {
        assert_eq!(
            parse_proc_stat("42 (tmux: server (1)) S 7 42 42 0 -1"),
            Some(("tmux: server (1)".into(), 7))
        );
        assert_eq!(parse_proc_stat("garbage"), None);
    }

    #[test]
    fn joins_cmdline_arguments() {
        assert_eq!(join_cmdline(b"bash\0-l\0"), "bash -l");
        assert_eq!(join_cmdline(b""), "");
    }

    #[test]
    fn parses_ps_rows() {
        let rows = parse_ps("  10     1 /usr/bin/waku-daemon --bind x\n 11 10 -zsh\nbad row\n");
        assert_eq!(
            rows,
            [
                entry(10, 1, "/usr/bin/waku-daemon --bind x"),
                entry(11, 10, "-zsh")
            ]
        );
    }

    #[test]
    fn shortens_long_commands() {
        let long = "x".repeat(MAX_COMMAND_CHARS + 10);
        assert_eq!(shorten_command(&long).chars().count(), MAX_COMMAND_CHARS + 1);
    }
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

    /// Hang up and terminate each process, then kill whatever outlives a
    /// grace period. A shell exits on SIGHUP; most other programs on SIGTERM.
    pub fn end_children(pids: &[u32]) {
        for &pid in pids {
            unsafe {
                libc::kill(pid as i32, libc::SIGHUP);
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && pids.iter().any(|&pid| process_is_alive(pid)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        for &pid in pids {
            if process_is_alive(pid) {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
    }

    /// Every visible process with its parent and command line: `/proc` on
    /// Linux, `ps` elsewhere. Empty when neither is readable.
    pub fn process_table() -> Vec<super::ProcessEntry> {
        if let Ok(entries) = fs::read_dir("/proc") {
            let table: Vec<_> = entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
                .filter_map(proc_entry)
                .collect();
            if !table.is_empty() {
                return table;
            }
        }
        let Ok(output) = std::process::Command::new("ps")
            .args(["-Ao", "pid=,ppid=,command="])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            return Vec::new();
        };
        super::parse_ps(&String::from_utf8_lossy(&output.stdout))
    }

    fn proc_entry(pid: u32) -> Option<super::ProcessEntry> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (name, parent) = super::parse_proc_stat(&stat)?;
        let command = fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .map(|bytes| super::join_cmdline(&bytes))
            .filter(|command| !command.is_empty())
            .unwrap_or_else(|| format!("[{name}]"));
        Some(super::ProcessEntry {
            pid,
            parent,
            command,
        })
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

    pub fn end_children(_pids: &[u32]) {}

    pub fn process_table() -> Vec<super::ProcessEntry> {
        Vec::new()
    }

    pub fn detach(_command: &mut std::process::Command) {}
}
