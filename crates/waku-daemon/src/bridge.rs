//! `waku-daemon bridge`: the stdio entry point for WSL and SSH hosts.
//!
//! The desktop launches this through `wsl.exe --exec` or `ssh`, so its stdout
//! is the transport and must carry nothing but the [`BridgeReady`] line and
//! the relayed bytes. Diagnostics go to stderr, which the desktop reports.

use std::ffi::{OsStr, OsString};
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
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_WRITE, WRITE_DAC};
            options.access_mode(FILE_GENERIC_WRITE | WRITE_DAC);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        platform::restrict_file_permissions(&file)?;
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
    describe_tree(
        state.pid,
        state.protocol_version,
        &platform::process_table(),
    )
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

fn children_of(pid: u32, table: &[ProcessEntry]) -> impl DoubleEndedIterator<Item = &ProcessEntry> {
    table.iter().filter(move |entry| entry.parent == pid)
}

fn descendant_pids(pid: u32, table: &[ProcessEntry]) -> Vec<u32> {
    let mut descendants = Vec::new();
    let mut stack: Vec<u32> = children_of(pid, table)
        .rev()
        .map(|entry| entry.pid)
        .collect();
    let mut seen = std::collections::HashSet::from([pid]);
    while let Some(child) = stack.pop() {
        if !seen.insert(child) {
            continue;
        }
        if !table.iter().any(|entry| entry.pid == child) {
            continue;
        }
        descendants.push(child);
        stack.extend(children_of(child, table).rev().map(|entry| entry.pid));
    }
    descendants
}

/// End an incompatible daemon and the sessions it owns. On Unix, direct
/// children are terminal shells and agent processes; closing the daemon's PTY
/// masters hangs up foreground jobs while detached processes reparented to
/// PID 1 are left alone. On Windows, pipe-based agent wrappers (`cmd.exe` /
/// `pwsh.exe` -> `node.exe`) receive no PTY hangup cascade, so all live
/// descendants rooted at `pid` are terminated.
fn end_daemon_and_sessions(pid: u32) {
    let table = platform::process_table();
    let targets: Vec<u32> = if cfg!(windows) {
        descendant_pids(pid, &table)
    } else {
        children_of(pid, &table).map(|entry| entry.pid).collect()
    };
    platform::terminate(pid);
    platform::end_children(&targets);
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
    let mut diagnostic_log = log.try_clone().ok();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let idle_secs = IDLE_EXIT_SECONDS.to_string();
    let args = [
        OsStr::new("--bind"),
        OsStr::new("127.0.0.1:0"),
        OsStr::new("--state-file"),
        state_path.as_os_str(),
        OsStr::new("--idle-exit-secs"),
        OsStr::new(&idle_secs),
    ];
    let mut owned_envs: Vec<(&OsStr, OsString)> = vec![(
        OsStr::new(waku_protocol::DAEMON_TOKEN_ENV),
        OsString::from(&token),
    )];
    // A debug build would otherwise use the checkout path it was compiled
    // in, which on WSL is often the Windows host's own data via /mnt/c.
    if cfg!(debug_assertions)
        && std::env::var_os(waku_core::persistence::DATA_DIRECTORY_ENV).is_none()
        && let Some(data) = dirs::data_local_dir()
    {
        owned_envs.push((
            OsStr::new(waku_core::persistence::DATA_DIRECTORY_ENV),
            data.join(waku_protocol::identity::DATA_DIRECTORY_NAME)
                .into_os_string(),
        ));
    }
    let envs: Vec<(&OsStr, &OsStr)> = owned_envs
        .iter()
        .map(|(key, value)| (*key, value.as_os_str()))
        .collect();
    let home = dirs::home_dir();
    let (mut child, broke_away) =
        platform::spawn_detached(executable, &args, &envs, home.as_deref(), log)
            .with_context(|| format!("could not launch {}", executable.display()))?;
    if !broke_away && let Some(ref mut log) = diagnostic_log {
        let _ = log.write_all(
            b"waku-daemon bridge: CREATE_BREAKAWAY_FROM_JOB was denied by the host job object; the daemon will exit when the SSH session closes\n",
        );
    }
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
            child.kill();
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
        assert_eq!(descendant_pids(10, &table), vec![11, 12, 13]);
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
        assert_eq!(
            shorten_command(&long).chars().count(),
            MAX_COMMAND_CHARS + 1
        );
    }

    #[test]
    fn platform_lock_process_check_and_table_work() {
        let dir = std::env::temp_dir().join(format!("waku-bridge-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let lock = platform::lock(&dir.join(LOCK_FILE)).unwrap();
        drop(lock);
        let _ = fs::remove_dir_all(&dir);

        let self_pid = std::process::id();
        assert!(platform::process_is_alive(self_pid));

        let table = platform::process_table();
        assert!(table.iter().any(|entry| entry.pid == self_pid));

        // 259 is STILL_ACTIVE; the wait must still report the process as dead.
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "exit 259"])
            .spawn()
            .unwrap();
        #[cfg(unix)]
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let exited_pid = child.id();
        let _ = child.wait();
        assert!(!platform::process_is_alive(exited_pid));
    }

    #[test]
    fn write_state_creates_owner_only_credentials() {
        let dir = std::env::temp_dir().join(format!("waku-state-acl-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join(STATE_FILE);
        let state = BridgeState {
            address: "127.0.0.1:4242".into(),
            token: "secret-token".into(),
            pid: std::process::id(),
            protocol_version: PROTOCOL_VERSION,
            fingerprint: "abcd".into(),
        };
        write_state(&state_path, &state).unwrap();
        let loaded: BridgeState =
            serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(loaded, state);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&state_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
            use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
            use windows_sys::Win32::Security::{
                ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
                GetAclInformation, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR,
                SE_DACL_PROTECTED,
            };

            let file = fs::File::open(&state_path).unwrap();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let err = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle() as HANDLE,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut sd,
                )
            };
            assert_eq!(err, 0, "GetSecurityInfo failed with {err}");
            assert!(!dacl.is_null());
            let mut control = 0_u16;
            let mut revision = 0_u32;
            assert_ne!(
                unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) },
                0
            );
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            let mut acl_size = ACL_SIZE_INFORMATION::default();
            assert_ne!(
                unsafe {
                    GetAclInformation(
                        dacl,
                        (&mut acl_size as *mut ACL_SIZE_INFORMATION).cast(),
                        std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                        AclSizeInformation,
                    )
                },
                0
            );
            assert_eq!(acl_size.AceCount, 3);
            unsafe { LocalFree(sd) };
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_line_quoting_preserves_spaces_quotes_and_backslashes() {
        let wide = platform::build_command_line(
            Path::new(r"C:\Program Files\Waku\waku-daemon.exe"),
            &[
                OsStr::new("--arg"),
                OsStr::new(""),
                OsStr::new(r"C:\Users\BrainCo\"),
                OsStr::new(r#"a"b\"c"#),
                OsStr::new(r"C:\Program Files\"),
            ],
        );
        assert_eq!(wide.last().copied(), Some(0));
        let rendered = String::from_utf16(&wide[..wide.len() - 1]).unwrap();
        assert_eq!(
            rendered,
            r#""C:\Program Files\Waku\waku-daemon.exe" --arg "" C:\Users\BrainCo\ "a\"b\\\"c" "C:\Program Files\\""#
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_spawn_detached_isolates_unrelated_inheritable_handles() {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};

        let dir = std::env::temp_dir().join(format!("waku-detached-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join(LOG_FILE);
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();

        let (mut reader, writer) = std::io::pipe().unwrap();
        assert_ne!(
            unsafe {
                SetHandleInformation(
                    writer.as_raw_handle() as HANDLE,
                    HANDLE_FLAG_INHERIT,
                    HANDLE_FLAG_INHERIT,
                )
            },
            0
        );

        let comspec = std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe"));
        let args = [
            OsStr::new("/d"),
            OsStr::new("/c"),
            OsStr::new(
                "echo %WAKU_TEST_MARKER% 1>&2 & \"%SystemRoot%\\System32\\ping.exe\" -n 6 127.0.0.1 >NUL",
            ),
        ];
        let envs = [(OsStr::new("WAKU_TEST_MARKER"), OsStr::new("isolated_ok"))];
        let (mut child, _) =
            platform::spawn_detached(Path::new(&comspec), &args, &envs, Some(&dir), log).unwrap();

        // Only the child's configured NUL/log handles should be inherited.
        drop(writer);
        let mut buffer = [0_u8; 16];
        let read = reader.read(&mut buffer).unwrap();
        assert_eq!(read, 0, "unrelated inheritable pipe was inherited by child");
        assert!(
            child.try_wait().unwrap().is_none(),
            "child should still be running after parent closed unrelated pipe"
        );

        let log_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < log_deadline {
            if fs::read(&log_path)
                .map(|bytes| String::from_utf8_lossy(&bytes).contains("isolated_ok"))
                .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        child.kill();
        assert!(!platform::process_is_alive(child.id()));
        let logged = String::from_utf8_lossy(&fs::read(&log_path).unwrap()).into_owned();
        assert!(logged.contains("isolated_ok"), "unexpected log: {logged:?}");

        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use anyhow::Context as _;

    pub struct Lock(#[allow(dead_code)] fs::File);

    pub struct DetachedChild(std::process::Child);

    impl DetachedChild {
        pub fn id(&self) -> u32 {
            self.0.id()
        }

        pub fn try_wait(&mut self) -> std::io::Result<Option<String>> {
            self.0
                .try_wait()
                .map(|status| status.map(|status| status.to_string()))
        }

        pub fn kill(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

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

    pub fn restrict_file_permissions(file: &fs::File) -> anyhow::Result<()> {
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("could not set owner-only permissions on credential file");
        }
        Ok(())
    }

    /// A new session detaches the daemon from the SSH or WSL session, so a
    /// hangup on disconnect does not take running agents down with it.
    pub fn spawn_detached(
        executable: &Path,
        args: &[&OsStr],
        envs: &[(&OsStr, &OsStr)],
        current_dir: Option<&Path>,
        stderr_log: fs::File,
    ) -> std::io::Result<(DetachedChild, bool)> {
        let mut command = std::process::Command::new(executable);
        command
            .args(args)
            .envs(envs.iter().copied())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(stderr_log);
        if let Some(directory) = current_dir {
            command.current_dir(directory);
        }
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        command.spawn().map(|child| (DetachedChild(child), true))
    }
}

#[cfg(windows)]
mod platform {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, bail};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, FILETIME, HANDLE, HANDLE_FLAG_INHERIT,
        INVALID_HANDLE_VALUE, LocalFree, STILL_ACTIVE, SetHandleInformation, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess,
        OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject,
    };

    pub struct Lock(#[allow(dead_code)] fs::File);

    pub struct DetachedChild {
        handle: OwnedHandle,
        pid: u32,
    }

    impl DetachedChild {
        pub fn id(&self) -> u32 {
            self.pid
        }

        pub fn try_wait(&mut self) -> io::Result<Option<String>> {
            let wait = unsafe { WaitForSingleObject(self.handle.0, 0) };
            if wait == WAIT_TIMEOUT {
                return Ok(None);
            }
            if wait == WAIT_OBJECT_0 {
                let mut exit_code = 0_u32;
                if unsafe { GetExitCodeProcess(self.handle.0, &mut exit_code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(Some(format!("exit code {exit_code}")));
            }
            Err(io::Error::last_os_error())
        }

        pub fn kill(&mut self) {
            unsafe {
                TerminateProcess(self.handle.0, 1);
                WaitForSingleObject(self.handle.0, 3_000);
            }
        }
    }

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    struct OwnedLocalMem(*mut core::ffi::c_void);
    impl Drop for OwnedLocalMem {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }

    struct ProcThreadAttrGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for ProcThreadAttrGuard {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }

    pub fn restrict_file_permissions(file: &fs::File) -> anyhow::Result<()> {
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("could not open process token for credential ACL");
        }
        let token = OwnedHandle(token);

        let mut needed = 0_u32;
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(std::io::Error::last_os_error())
                .context("could not size process token user");
        }
        let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("could not query process token user");
        }
        let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };

        let mut sid_wide: *mut u16 = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_wide) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("could not format owner SID for credential ACL");
        }
        let _sid_guard = OwnedLocalMem(sid_wide.cast());
        let mut len = 0;
        while unsafe { *sid_wide.add(len) } != 0 {
            len += 1;
        }
        let sid_string =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_wide, len) });

        let sddl: Vec<u16> = format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{sid_string})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("could not build owner-only credential security descriptor");
        }
        let _sd_guard = OwnedLocalMem(sd);

        let mut dacl_present = 0;
        let mut dacl_defaulted = 0;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        if unsafe {
            GetSecurityDescriptorDacl(sd, &mut dacl_present, &mut dacl, &mut dacl_defaulted)
        } == 0
            || dacl_present == 0
            || dacl.is_null()
        {
            bail!("generated credential security descriptor contained no DACL");
        }

        let err = unsafe {
            SetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(err as i32))
                .context("could not apply owner-only ACL to credential file");
        }
        Ok(())
    }

    pub fn lock(path: &Path) -> anyhow::Result<Lock> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("could not open {}", path.display()))?;
        // std::fs::File handles are non-inheritable and LockFileEx is dropped
        // by the kernel if the bridge exits unexpectedly.
        file.lock()
            .with_context(|| format!("could not lock {}", path.display()))?;
        Ok(Lock(file))
    }

    pub fn process_is_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            );
            if handle.is_null() {
                return std::io::Error::last_os_error().raw_os_error()
                    == Some(ERROR_ACCESS_DENIED as i32);
            }
            let mut exit_code = 0_u32;
            let read = GetExitCodeProcess(handle, &mut exit_code);
            // STILL_ACTIVE is also a legal exit code (259). The wait is
            // signaled only after the process exits, so it disambiguates a
            // live process from one that happened to return that value.
            let wait = WaitForSingleObject(handle, 0);
            CloseHandle(handle);
            read != 0 && exit_code == STILL_ACTIVE as u32 && wait == WAIT_TIMEOUT
        }
    }

    pub fn terminate(pid: u32) {
        if pid == 0 {
            return;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return;
            }
            TerminateProcess(handle, 1);
            WaitForSingleObject(handle, 3_000);
            CloseHandle(handle);
        }
    }

    pub fn end_children(pids: &[u32]) {
        let handles: Vec<HANDLE> = pids
            .iter()
            .copied()
            .filter(|&pid| pid != 0)
            .filter_map(|pid| unsafe {
                let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid);
                (!handle.is_null()).then_some(handle)
            })
            .collect();
        for &handle in &handles {
            unsafe {
                TerminateProcess(handle, 1);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        for &handle in &handles {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(u32::MAX as u128) as u32;
            unsafe {
                WaitForSingleObject(handle, remaining);
                CloseHandle(handle);
            }
        }
    }

    pub fn process_table() -> Vec<super::ProcessEntry> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut table = Vec::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while has_entry {
            let len = entry
                .szExeFile
                .iter()
                .position(|&ch| ch == 0)
                .unwrap_or(entry.szExeFile.len());
            let command =
                super::shorten_command(&String::from_utf16_lossy(&entry.szExeFile[..len]));
            table.push(super::ProcessEntry {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                command,
            });
            has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        unsafe { CloseHandle(snapshot) };
        // Windows never clears `th32ParentProcessID` when a parent exits, so
        // drop stale parent links whose PID no longer exists or was recycled
        // by a newer process created after the child.
        let live_pids: std::collections::HashSet<u32> =
            table.iter().map(|entry| entry.pid).collect();
        let mut creation_cache = std::collections::HashMap::new();
        for row in &mut table {
            if row.parent == 0 || row.parent == row.pid || !live_pids.contains(&row.parent) {
                row.parent = 0;
                continue;
            }
            let parent_start = *creation_cache
                .entry(row.parent)
                .or_insert_with(|| process_creation_time(row.parent));
            let child_start = *creation_cache
                .entry(row.pid)
                .or_insert_with(|| process_creation_time(row.pid));
            if let (Some(parent_time), Some(child_time)) = (parent_start, child_start)
                && child_time < parent_time
            {
                row.parent = 0;
            }
        }
        table
    }

    fn process_creation_time(pid: u32) -> Option<u64> {
        if pid == 0 {
            return None;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            let ok = GetProcessTimes(
                handle,
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            );
            CloseHandle(handle);
            (ok != 0).then_some(
                ((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64),
            )
        }
    }

    fn append_quoted_arg(out: &mut Vec<u16>, arg: &OsStr) {
        let wide: Vec<u16> = arg.encode_wide().collect();
        let needs_quotes = wide.is_empty()
            || wide.iter().any(|&ch| {
                ch == b' ' as u16
                    || ch == b'\t' as u16
                    || ch == b'\n' as u16
                    || ch == 0x0b
                    || ch == b'"' as u16
            });
        if !needs_quotes {
            out.extend_from_slice(&wide);
            return;
        }
        out.push(b'"' as u16);
        let mut backslashes = 0_usize;
        for &ch in &wide {
            if ch == b'\\' as u16 {
                backslashes += 1;
            } else if ch == b'"' as u16 {
                out.resize(out.len() + backslashes * 2 + 1, b'\\' as u16);
                backslashes = 0;
                out.push(b'"' as u16);
            } else {
                out.resize(out.len() + backslashes, b'\\' as u16);
                backslashes = 0;
                out.push(ch);
            }
        }
        out.resize(out.len() + backslashes * 2, b'\\' as u16);
        out.push(b'"' as u16);
    }

    pub(super) fn build_command_line(executable: &Path, args: &[&OsStr]) -> Vec<u16> {
        let mut cmd = Vec::new();
        append_quoted_arg(&mut cmd, executable.as_os_str());
        for &arg in args {
            cmd.push(b' ' as u16);
            append_quoted_arg(&mut cmd, arg);
        }
        cmd.push(0);
        cmd
    }

    fn fold_env_key_upper(key: &OsStr) -> Vec<u16> {
        let mut folded = Vec::new();
        for decoded in char::decode_utf16(key.encode_wide()) {
            match decoded {
                Ok(ch) => {
                    let mut buf = [0_u16; 2];
                    for upper in ch.to_uppercase() {
                        folded.extend_from_slice(upper.encode_utf16(&mut buf));
                    }
                }
                Err(unpaired) => folded.push(unpaired.unpaired_surrogate()),
            }
        }
        folded
    }

    fn build_environment_block(overrides: &[(&OsStr, &OsStr)]) -> io::Result<Vec<u16>> {
        let mut entries: BTreeMap<Vec<u16>, (Vec<u16>, Vec<u16>)> = BTreeMap::new();
        for (key, value) in std::env::vars_os() {
            let orig_key: Vec<u16> = key.encode_wide().collect();
            let val_wide: Vec<u16> = value.encode_wide().collect();
            if orig_key.is_empty() || orig_key.contains(&0) || val_wide.contains(&0) {
                continue;
            }
            entries.insert(fold_env_key_upper(&key), (orig_key, val_wide));
        }
        for &(key, value) in overrides {
            let orig_key: Vec<u16> = key.encode_wide().collect();
            let val_wide: Vec<u16> = value.encode_wide().collect();
            if orig_key.is_empty()
                || orig_key.contains(&0)
                || orig_key.contains(&(b'=' as u16))
                || val_wide.contains(&0)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid Windows environment override",
                ));
            }
            entries.insert(fold_env_key_upper(key), (orig_key, val_wide));
        }
        if entries.is_empty() {
            return Ok(vec![0, 0]);
        }
        let mut block = Vec::new();
        for (_, (key, value)) in entries {
            block.extend_from_slice(&key);
            block.push(b'=' as u16);
            block.extend_from_slice(&value);
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }

    fn encode_wide_nul_checked(value: &OsStr) -> io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = value.encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interior NUL in Windows process path or directory",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    fn enable_inheritance(handle: HANDLE) -> io::Result<()> {
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Launch the resident daemon isolated from extraneous OpenSSH/PowerShell
    /// handles via `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, and break it out of
    /// the SSH session's job object when allowed.
    pub fn spawn_detached(
        executable: &Path,
        args: &[&OsStr],
        envs: &[(&OsStr, &OsStr)],
        current_dir: Option<&Path>,
        stderr_log: fs::File,
    ) -> io::Result<(DetachedChild, bool)> {
        let nul = fs::OpenOptions::new().read(true).write(true).open("NUL")?;
        let nul_handle = nul.as_raw_handle() as HANDLE;
        let log_handle = stderr_log.as_raw_handle() as HANDLE;
        enable_inheritance(nul_handle)?;
        if log_handle != nul_handle {
            enable_inheritance(log_handle)?;
        }

        let handles = if log_handle == nul_handle {
            vec![nul_handle]
        } else {
            vec![nul_handle, log_handle]
        };

        let mut attr_size = 0_usize;
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
        }
        if attr_size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attr_buf = vec![0_usize; attr_size.div_ceil(std::mem::size_of::<usize>())];
        let attr_list: LPPROC_THREAD_ATTRIBUTE_LIST = attr_buf.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _attr_guard = ProcThreadAttrGuard(attr_list);
        if unsafe {
            UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                handles.len() * std::mem::size_of::<HANDLE>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        let mut cmd_wide = build_command_line(executable, args);
        if cmd_wide[..cmd_wide.len() - 1].contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interior NUL in Windows process argument",
            ));
        }
        let app_wide = encode_wide_nul_checked(executable.as_os_str())?;
        let env_wide = build_environment_block(envs)?;
        let cwd_wide = current_dir
            .map(|directory| encode_wide_nul_checked(directory.as_os_str()))
            .transpose()?;
        let cwd_ptr = cwd_wide
            .as_ref()
            .map_or(std::ptr::null(), |wide| wide.as_ptr());

        let mut startup_info = STARTUPINFOEXW::default();
        startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup_info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup_info.StartupInfo.hStdInput = nul_handle;
        startup_info.StartupInfo.hStdOutput = nul_handle;
        startup_info.StartupInfo.hStdError = log_handle;
        startup_info.lpAttributeList = attr_list;

        let base_flags = CREATE_NO_WINDOW
            | CREATE_NEW_PROCESS_GROUP
            | CREATE_UNICODE_ENVIRONMENT
            | EXTENDED_STARTUPINFO_PRESENT;
        let mut process_info = PROCESS_INFORMATION::default();
        let mut broke_away = true;
        let mut created = unsafe {
            CreateProcessW(
                app_wide.as_ptr(),
                cmd_wide.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                base_flags | CREATE_BREAKAWAY_FROM_JOB,
                env_wide.as_ptr().cast(),
                cwd_ptr,
                (&startup_info as *const STARTUPINFOEXW).cast::<STARTUPINFOW>(),
                &mut process_info,
            )
        };
        if created == 0
            && io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
        {
            broke_away = false;
            let mut cmd_retry = build_command_line(executable, args);
            created = unsafe {
                CreateProcessW(
                    app_wide.as_ptr(),
                    cmd_retry.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                    base_flags,
                    env_wide.as_ptr().cast(),
                    cwd_ptr,
                    (&startup_info as *const STARTUPINFOEXW).cast::<STARTUPINFOW>(),
                    &mut process_info,
                )
            };
        }
        if created == 0 {
            return Err(io::Error::last_os_error());
        }
        if !process_info.hThread.is_null() && process_info.hThread != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(process_info.hThread) };
        }
        Ok((
            DetachedChild {
                handle: OwnedHandle(process_info.hProcess),
                pid: process_info.dwProcessId,
            },
            broke_away,
        ))
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;

    pub struct Lock;

    pub struct DetachedChild(std::process::Child);

    impl DetachedChild {
        pub fn id(&self) -> u32 {
            self.0.id()
        }

        pub fn try_wait(&mut self) -> std::io::Result<Option<String>> {
            self.0
                .try_wait()
                .map(|status| status.map(|status| status.to_string()))
        }

        pub fn kill(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    pub fn lock(_path: &Path) -> anyhow::Result<Lock> {
        anyhow::bail!("waku-daemon bridge runs only on Linux, macOS, and Windows hosts")
    }

    pub fn restrict_file_permissions(_file: &fs::File) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn process_is_alive(_pid: u32) -> bool {
        false
    }

    pub fn terminate(_pid: u32) {}

    pub fn end_children(_pids: &[u32]) {}

    pub fn process_table() -> Vec<super::ProcessEntry> {
        Vec::new()
    }

    pub fn spawn_detached(
        executable: &Path,
        args: &[&OsStr],
        envs: &[(&OsStr, &OsStr)],
        current_dir: Option<&Path>,
        stderr_log: fs::File,
    ) -> std::io::Result<(DetachedChild, bool)> {
        let mut command = std::process::Command::new(executable);
        command
            .args(args)
            .envs(envs.iter().copied())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(stderr_log);
        if let Some(directory) = current_dir {
            command.current_dir(directory);
        }
        command.spawn().map(|child| (DetachedChild(child), true))
    }
}
