use std::io::Write as _;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use waku_daemon::bridge;
use waku_protocol::bridge::{BRIDGE_SUBCOMMAND, BridgeState, FINGERPRINT_FLAG};
use waku_protocol::{DAEMON_TOKEN_ENV, DaemonReady, PROTOCOL_VERSION};

const OPEN_SUBCOMMAND: &str = "open";
const NO_DESKTOP_CONNECTED_MESSAGE: &str =
    "No Narel desktop app is currently connected to this host.";

fn main() -> anyhow::Result<()> {
    let mut all_arguments = std::env::args();
    let argv0 = all_arguments.next();
    let mut raw_arguments = all_arguments.peekable();
    if invoked_as_narel_cli(argv0.as_deref()) {
        return run_open_cli(raw_arguments);
    }
    match raw_arguments.peek().map(String::as_str) {
        Some(FINGERPRINT_FLAG) => {
            println!("{}", bridge::own_fingerprint()?);
            return Ok(());
        }
        Some(BRIDGE_SUBCOMMAND) => {
            raw_arguments.next();
            return bridge::run(raw_arguments);
        }
        Some(OPEN_SUBCOMMAND) => {
            raw_arguments.next();
            return run_open_cli(raw_arguments);
        }
        _ => {}
    }
    let arguments = Arguments::parse(raw_arguments)?;
    // Finish migration before advertising readiness or opening settings/SQLite.
    waku_protocol::identity::import_legacy_user_data()
        .context("could not import Waku files; originals are unchanged")?;
    waku_core::persistence::import_legacy_database()
        .context("could not import Waku history; originals are unchanged")?;
    let token =
        std::env::var(DAEMON_TOKEN_ENV).context("Waku daemon authentication token is missing")?;
    // The bearer capability belongs only to this server process. Remove it
    // before any provider or workspace subprocess can inherit the daemon's
    // environment.
    unsafe { std::env::remove_var(DAEMON_TOKEN_ENV) };
    let listener = TcpListener::bind(&arguments.bind)
        .with_context(|| format!("could not bind Waku daemon to {}", arguments.bind))?;
    let address = listener.local_addr()?;
    ensure_bind_allowed(address, arguments.allow_non_loopback)?;
    let ready = DaemonReady {
        address: address.to_string(),
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
    };
    if let Some(state_file) = &arguments.state_file {
        bridge::write_state(
            state_file,
            &BridgeState {
                address: ready.address.clone(),
                token: token.clone(),
                pid: ready.pid,
                protocol_version: PROTOCOL_VERSION,
                fingerprint: bridge::own_fingerprint()?,
            },
        )?;
    }
    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;

    let shutdown = Arc::new(AtomicBool::new(false));
    if let Some(parent_pid) = arguments.parent_pid {
        let monitor_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("waku-daemon-parent".into())
            .spawn(move || {
                while !monitor_shutdown.load(Ordering::Acquire) {
                    if !waku_client::pid::is_alive(parent_pid) {
                        monitor_shutdown.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })?;
    }

    let task_path = waku_core::persistence::StateStore::default_path();
    let settings = waku_core::DaemonSettingsStore::open_with_legacy(
        waku_core::DaemonSettings::default_path(),
        [task_path.with_file_name("settings.json")],
    )
    .context("could not load daemon settings")?;
    let task_store = waku_core::persistence::StateStore::daemon(task_path);
    let backend = waku_core::daemon::WakuBackend::new(settings, task_store)?
        .with_cli_bin_dir(arguments.cli_bin_dir.clone());
    waku_core::serve(
        listener,
        token,
        Arc::new(backend),
        shutdown,
        waku_core::ServerOptions {
            allowed_origins: arguments.allowed_origins.into_iter().collect(),
            allow_shutdown: arguments.parent_pid.is_some() || arguments.allow_shutdown,
            idle_exit: arguments.idle_exit,
            cli_bin_dir: arguments.cli_bin_dir,
        },
    )
}

fn invoked_as_narel_cli(argv0: Option<&str>) -> bool {
    let Some(argv0) = argv0 else {
        return false;
    };
    let name = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    stem == "narel"
}

#[derive(Debug, Eq, PartialEq)]
enum OpenAction {
    Help,
    Version,
    Open {
        state_dir: Option<PathBuf>,
        path: Option<String>,
    },
}

fn parse_open_arguments(
    arguments: impl IntoIterator<Item = String>,
) -> anyhow::Result<OpenAction> {
    let mut state_dir = None;
    let mut path = None;
    let mut literal_only = false;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if !literal_only {
            match argument.as_str() {
                "-h" | "--help" => return Ok(OpenAction::Help),
                "-V" | "--version" => return Ok(OpenAction::Version),
                "--" => {
                    literal_only = true;
                    continue;
                }
                "--state-dir" => {
                    let dir = arguments
                        .next()
                        .ok_or_else(|| anyhow!("--state-dir requires a path"))?;
                    state_dir = Some(PathBuf::from(dir));
                    continue;
                }
                _ if argument.starts_with("--state-dir=") => {
                    let dir = &argument["--state-dir=".len()..];
                    if dir.is_empty() {
                        bail!("--state-dir requires a path");
                    }
                    state_dir = Some(PathBuf::from(dir));
                    continue;
                }
                _ if argument.starts_with('-') && argument != "-" => {
                    bail!("unknown option {argument:?}");
                }
                _ => {}
            }
        }
        if path.is_some() {
            bail!("expected at most one path argument");
        }
        path = Some(argument);
    }
    Ok(OpenAction::Open { state_dir, path })
}

fn resolve_open_cli_path(raw: &str, cwd: Option<&Path>) -> String {
    if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") || raw.trim().is_empty() {
        return raw.to_owned();
    }
    #[cfg(windows)]
    {
        let bytes = raw.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return format!("{raw}\\");
        }
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return raw.to_owned();
    }
    let Some(cwd) = cwd else {
        return raw.to_owned();
    };
    if raw == "." {
        return cwd.to_string_lossy().into_owned();
    }
    cwd.join(path).to_string_lossy().into_owned()
}

fn default_open_state_dir() -> anyhow::Result<PathBuf> {
    let executable = std::env::current_exe().context("could not locate narel-daemon")?;
    let canonical = std::fs::canonicalize(&executable).unwrap_or(executable);
    canonical
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("narel-daemon has no parent directory"))
}

fn run_open_cli(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let action = match parse_open_arguments(arguments) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    match action {
        OpenAction::Help => {
            println!(
                "Usage: narel [PATH]\n\n\
                 Open a directory or focus the connected Narel desktop window.\n\n\
                 Options:\n  \
                   --state-dir <DIR>  Override the daemon state directory\n  \
                   -h, --help         Print help\n  \
                   -V, --version      Print version"
            );
            Ok(())
        }
        OpenAction::Version => {
            println!(
                "narel {} (community fork of egoist/waku, GPL-3.0)",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        OpenAction::Open { state_dir, path } => {
            let state_dir = match state_dir {
                Some(dir) => dir,
                None => default_open_state_dir()?,
            };
            let state_file = state_dir.join(bridge::STATE_FILE);
            let cwd = std::env::current_dir().ok();
            let resolved_path = path.map(|raw| resolve_open_cli_path(&raw, cwd.as_deref()));
            match waku_client::request_open_workspace(&state_file, resolved_path, None) {
                Ok(outcome) if outcome.delivered => Ok(()),
                Ok(_) => {
                    eprintln!("{NO_DESKTOP_CONNECTED_MESSAGE}");
                    std::process::exit(1);
                }
                Err(error) => {
                    if error
                        .downcast_ref::<waku_client::DaemonUnavailableError>()
                        .is_some()
                    {
                        eprintln!("{NO_DESKTOP_CONNECTED_MESSAGE}");
                    } else {
                        eprintln!("{error}");
                    }
                    std::process::exit(1);
                }
            }
        }
    }
}

fn ensure_bind_allowed(address: SocketAddr, allow_non_loopback: bool) -> anyhow::Result<()> {
    if address.ip().is_loopback() || allow_non_loopback {
        return Ok(());
    }
    bail!(
        "refusing non-loopback daemon bind {address}; pass --allow-non-loopback only after configuring authentication and exact browser origins"
    )
}

struct Arguments {
    allow_shutdown: bool,
    bind: String,
    parent_pid: Option<u32>,
    allowed_origins: Vec<String>,
    allow_non_loopback: bool,
    state_file: Option<std::path::PathBuf>,
    idle_exit: Option<Duration>,
    cli_bin_dir: Option<std::path::PathBuf>,
}

impl Arguments {
    fn parse(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut bind = "127.0.0.1:0".to_owned();
        let mut allow_shutdown = false;
        let mut parent_pid = None;
        let mut allowed_origins = Vec::new();
        let mut allow_non_loopback = false;
        let mut state_file = None;
        let mut idle_exit = None;
        let mut cli_bin_dir = None;
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--allow-shutdown" => allow_shutdown = true,
                "--bind" => {
                    bind = arguments
                        .next()
                        .ok_or_else(|| anyhow!("--bind requires an address"))?;
                }
                "--parent-pid" => {
                    parent_pid = Some(
                        arguments
                            .next()
                            .ok_or_else(|| anyhow!("--parent-pid requires a process id"))?
                            .parse()
                            .context("--parent-pid is not a valid process id")?,
                    );
                }
                "--allow-origin" => {
                    let origin = arguments
                        .next()
                        .filter(|origin| !origin.trim().is_empty())
                        .ok_or_else(|| anyhow!("--allow-origin requires an origin"))?;
                    allowed_origins.push(origin);
                }
                "--allow-non-loopback" => {
                    allow_non_loopback = true;
                }
                "--state-file" => {
                    state_file = Some(
                        arguments
                            .next()
                            .ok_or_else(|| anyhow!("--state-file requires a path"))?
                            .into(),
                    );
                }
                "--idle-exit-secs" => {
                    let seconds: u64 = arguments
                        .next()
                        .ok_or_else(|| anyhow!("--idle-exit-secs requires a number"))?
                        .parse()
                        .context("--idle-exit-secs is not a valid number")?;
                    idle_exit = Some(Duration::from_secs(seconds));
                }
                "--cli-bin-dir" => {
                    cli_bin_dir = Some(
                        arguments
                            .next()
                            .ok_or_else(|| anyhow!("--cli-bin-dir requires a path"))?
                            .into(),
                    );
                }
                "--help" | "-h" => {
                    println!(
                        "usage: {0} [--bind ADDRESS] [--allow-non-loopback] [--parent-pid PID] [--allow-origin ORIGIN]... [--state-file PATH] [--idle-exit-secs N] [--cli-bin-dir DIR]\n       {0} bridge [--state-dir DIR]\n       {0} open [--state-dir DIR] [PATH]\n       {0} --fingerprint",
                        env!("CARGO_BIN_NAME")
                    );
                    std::process::exit(0);
                }
                unknown => bail!("unknown argument {unknown:?}"),
            }
        }
        Ok(Self {
            allow_shutdown,
            bind,
            parent_pid,
            allowed_origins,
            allow_non_loopback,
            state_file,
            idle_exit,
            cli_bin_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_loopback_listener_requires_an_explicit_flag() {
        assert!(ensure_bind_allowed("127.0.0.1:3000".parse().unwrap(), false).is_ok());
        assert!(ensure_bind_allowed("[::1]:3000".parse().unwrap(), false).is_ok());
        assert!(ensure_bind_allowed("0.0.0.0:3000".parse().unwrap(), false).is_err());
        assert!(ensure_bind_allowed("[::]:3000".parse().unwrap(), false).is_err());
        assert!(ensure_bind_allowed("0.0.0.0:3000".parse().unwrap(), true).is_ok());
    }

    #[test]
    fn parses_repeated_browser_origin_allowlist_entries() {
        let arguments = Arguments::parse([
            "--allow-origin".into(),
            "https://app.waku.test".into(),
            "--allow-origin".into(),
            "http://localhost:3000".into(),
        ])
        .unwrap();

        assert_eq!(
            arguments.allowed_origins,
            ["https://app.waku.test", "http://localhost:3000"]
        );
        assert!(!arguments.allow_non_loopback);
    }

    #[test]
    fn parses_explicit_non_loopback_opt_in() {
        let arguments = Arguments::parse(["--allow-non-loopback".into()]).unwrap();
        assert!(arguments.allow_non_loopback);
    }

    #[test]
    fn parses_cli_bin_dir_argument() {
        let arguments =
            Arguments::parse(["--cli-bin-dir".into(), "/tmp/waku/bin".into()]).unwrap();
        assert_eq!(arguments.cli_bin_dir, Some(PathBuf::from("/tmp/waku/bin")));
    }

    #[test]
    fn detects_narel_multicall_argv0() {
        assert!(invoked_as_narel_cli(Some("narel")));
        assert!(invoked_as_narel_cli(Some("NAREL.EXE")));
        assert!(invoked_as_narel_cli(Some("/home/me/.cache/narel/server/bin/narel")));
        assert!(invoked_as_narel_cli(Some(r"C:\Users\me\.cache\narel\server\bin\narel.exe")));
        assert!(!invoked_as_narel_cli(Some("waku")));
        assert!(!invoked_as_narel_cli(Some("narel-daemon")));
        assert!(!invoked_as_narel_cli(Some("narel-daemon.exe")));
        assert!(!invoked_as_narel_cli(Some("narel-debug-daemon")));
        assert!(!invoked_as_narel_cli(None));
    }

    #[test]
    fn parses_open_subcommand_arguments_and_resolves_relative_paths() {
        assert_eq!(
            parse_open_arguments(["--help".into()]).unwrap(),
            OpenAction::Help
        );
        assert_eq!(
            parse_open_arguments(["-h".into()]).unwrap(),
            OpenAction::Help
        );
        assert_eq!(
            parse_open_arguments(["--version".into()]).unwrap(),
            OpenAction::Version
        );
        assert_eq!(
            parse_open_arguments(["-V".into()]).unwrap(),
            OpenAction::Version
        );
        assert_eq!(
            parse_open_arguments([
                "--state-dir".into(),
                "/tmp/state".into(),
                ".".into(),
            ])
            .unwrap(),
            OpenAction::Open {
                state_dir: Some(PathBuf::from("/tmp/state")),
                path: Some(".".into()),
            }
        );
        assert_eq!(
            parse_open_arguments(["--state-dir=/tmp/state".into()]).unwrap(),
            OpenAction::Open {
                state_dir: Some(PathBuf::from("/tmp/state")),
                path: None,
            }
        );
        assert_eq!(
            parse_open_arguments(["--".into(), "-weird-dir".into()]).unwrap(),
            OpenAction::Open {
                state_dir: None,
                path: Some("-weird-dir".into()),
            }
        );
        assert!(parse_open_arguments(["--bogus".into()]).is_err());
        assert!(parse_open_arguments(["a".into(), "b".into()]).is_err());

        let cwd = Path::new(if cfg!(windows) { r"C:\work" } else { "/work" });
        assert_eq!(
            resolve_open_cli_path(".", Some(cwd)),
            cwd.to_string_lossy()
        );
        assert_eq!(
            resolve_open_cli_path("sub/dir", Some(cwd)),
            cwd.join("sub/dir").to_string_lossy()
        );
        assert_eq!(resolve_open_cli_path("~", Some(cwd)), "~");
        assert_eq!(resolve_open_cli_path("~/proj", Some(cwd)), "~/proj");
    }
}
