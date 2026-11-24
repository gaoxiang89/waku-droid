//! Shared CLI argument parsing, path resolution, and desktop delivery for
//! both `narel` (GUI binary) and `narel_cli` (Windows console helper).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, anyhow, bail};

/// A workspace-open request originating from CLI arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupOpenRequest {
    pub path: Option<String>,
    pub host: Option<String>,
}

/// Parsed top-level CLI intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliAction {
    Help,
    Version,
    Open(StartupOpenRequest),
}

/// Outcome of attempting to hand an open request to an already-running desktop.
#[derive(Debug)]
pub enum DeliveryResult {
    Delivered(waku_client::OpenWorkspaceOutcome),
    NotRunning,
}

pub fn help_text() -> &'static str {
    "Usage: narel [OPTIONS] [PATH]\n\n\
     Open a workspace directory in Narel or focus the running Narel window.\n\n\
     Options:\n  \
       --host <SPEC>    Open on a remote host (ssh:<alias>, wsl:<distro>, or saved id)\n  \
       --remote <SPEC>  Alias for --host\n  \
       -h, --help       Print help\n  \
       -V, --version    Print version"
}

pub fn version_text() -> String {
    format!(
        "narel {} (community fork of egoist/waku, GPL-3.0)",
        env!("CARGO_PKG_VERSION")
    )
}

/// Parse CLI arguments shared by `waku` and `waku_cli`.
///
/// When `ignore_launch_services_psn` is true (the GUI entrypoint), macOS
/// Launch Services process-serial-number arguments (`-psn_*`) are ignored.
pub fn parse_args(
    args: impl IntoIterator<Item = String>,
    ignore_launch_services_psn: bool,
) -> anyhow::Result<CliAction> {
    let mut host: Option<String> = None;
    let mut path: Option<String> = None;
    let mut literal_only = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if !literal_only {
            if ignore_launch_services_psn && arg.starts_with("-psn_") {
                continue;
            }
            match arg.as_str() {
                "-h" | "--help" => return Ok(CliAction::Help),
                "-V" | "--version" => return Ok(CliAction::Version),
                "--" => {
                    literal_only = true;
                    continue;
                }
                "--host" | "--remote" => {
                    let value = args
                        .next()
                        .map(|v| v.trim().to_owned())
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| anyhow!("{arg} requires a host spec"))?;
                    host = Some(value);
                    continue;
                }
                _ if arg.starts_with("--host=") || arg.starts_with("--remote=") => {
                    let (flag, value) = arg.split_once('=').expect("checked '=' prefix");
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        bail!("{flag} requires a host spec");
                    }
                    host = Some(trimmed.to_owned());
                    continue;
                }
                _ if arg.starts_with('-') && arg != "-" => {
                    bail!("unknown option {arg:?}");
                }
                _ => {}
            }
        }
        if path.is_some() {
            bail!("expected at most one path argument");
        }
        path = Some(arg);
    }
    Ok(CliAction::Open(StartupOpenRequest { path, host }))
}

/// Resolve a CLI path argument against `cwd` for local requests, while leaving
/// remote paths untouched so the remote daemon interprets them on its host.
pub fn resolve_cli_path(raw: &str, host: Option<&str>, cwd: Option<&Path>) -> String {
    if host.is_some() {
        return raw.to_owned();
    }
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

/// Normalize host spec and resolve local relative paths against `cwd`.
pub fn prepare_request(mut request: StartupOpenRequest, cwd: Option<&Path>) -> StartupOpenRequest {
    request.host = request
        .host
        .map(|spec| spec.trim().to_owned())
        .filter(|spec| !spec.is_empty() && spec != waku_client::remote::LOCAL_CONNECTION_ID);
    request.path = request
        .path
        .map(|raw| resolve_cli_path(&raw, request.host.as_deref(), cwd));
    request
}

/// Validate and canonicalize a local workspace path before cold-launching the
/// desktop GUI:
/// - expands a leading `~`, `~/`, or `~\` to the local home directory;
/// - checks that the path exists;
/// - maps a regular file to its parent directory;
/// - canonicalizes and strips any Windows `\\?\` / `\\?\UNC\` verbatim prefix.
pub fn validate_local_workspace_path(raw: &str) -> anyhow::Result<String> {
    if raw.trim().is_empty() {
        bail!("workspace path must not be empty");
    }
    let expanded = expand_leading_tilde(raw)?;
    let metadata = fs::metadata(&expanded).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow!("workspace path does not exist: {}", expanded.display())
        } else {
            anyhow!(
                "could not access workspace path {}: {error}",
                expanded.display()
            )
        }
    })?;
    let target = if metadata.is_dir() {
        expanded
    } else if metadata.is_file() {
        expanded
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        bail!(
            "workspace path is not a directory or file: {}",
            expanded.display()
        );
    };
    let canonical = fs::canonicalize(&target)
        .with_context(|| format!("could not resolve workspace path: {}", target.display()))?;
    Ok(strip_windows_verbatim(canonical)
        .to_string_lossy()
        .into_owned())
}

fn expand_leading_tilde(raw: &str) -> anyhow::Result<PathBuf> {
    if raw == "~" {
        return dirs::home_dir().ok_or_else(|| anyhow!("home directory is unavailable"));
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory is unavailable"))?;
        let trimmed = rest.trim_start_matches(['/', '\\']);
        return Ok(if trimmed.is_empty() {
            home
        } else {
            home.join(trimmed)
        });
    }
    #[cfg(windows)]
    {
        let bytes = raw.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return Ok(PathBuf::from(format!("{raw}\\")));
        }
    }
    Ok(PathBuf::from(raw))
}

fn strip_windows_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

/// Grant foreground-window focus permission on Windows so the running or
/// freshly launched desktop window can call `SetForegroundWindow`.
#[cfg(target_os = "windows")]
pub fn allow_set_foreground_window(pid: Option<u32>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};

    let target = pid.unwrap_or(ASFW_ANY);
    unsafe {
        let _ = AllowSetForegroundWindow(target);
    }
}

#[cfg(not(target_os = "windows"))]
pub fn allow_set_foreground_window(_pid: Option<u32>) {}

/// Attempt to deliver `request` to a running Waku desktop via its daemon.
///
/// - Returns `Ok(DeliveryResult::Delivered(_))` when a live desktop received it.
/// - Returns `Ok(DeliveryResult::NotRunning)` when no daemon or no desktop is
///   connected (cold-launch fallback).
/// - Returns `Err(_)` when the daemon explicitly rejected the request (e.g.
///   nonexistent workspace path) so the caller prints the error and exits 1
///   without cold-launching the GUI.
pub fn try_deliver_to_running_desktop(
    request: &StartupOpenRequest,
) -> anyhow::Result<DeliveryResult> {
    // AllowSetForegroundWindow must run *before* sending the RPC so the
    // desktop's immediate SetForegroundWindow upon daemon broadcast is not
    // blocked by Windows' foreground lock.
    allow_set_foreground_window(None);

    let env_address = std::env::var(waku_client::DAEMON_ADDRESS_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let env_token = std::env::var(waku_client::DAEMON_TOKEN_ENV)
        .ok()
        .filter(|value| !value.is_empty());

    let outcome = match (env_address, env_token) {
        (Some(address), Some(token)) => {
            let client = match waku_client::DaemonClient::connect_with_identity(
                address.trim(),
                token,
                Vec::new(),
                waku_client::ClientKind::Cli,
                None,
            ) {
                Ok(client) => client,
                Err(_) => return Ok(DeliveryResult::NotRunning),
            };
            let result = client.open_workspace(request.path.clone(), request.host.clone());
            client.disconnect();
            result
        }
        _ => waku_client::request_open_workspace(
            &waku_client::resident_daemon_state_path(),
            request.path.clone(),
            request.host.clone(),
        ),
    };

    match outcome {
        Ok(outcome) if outcome.delivered => {
            if outcome.desktop_pid.is_some() {
                allow_set_foreground_window(outcome.desktop_pid);
            }
            Ok(DeliveryResult::Delivered(outcome))
        }
        Ok(_) => Ok(DeliveryResult::NotRunning),
        Err(error) => {
            if error.downcast_ref::<waku_client::DaemonRpcError>().is_some() {
                Err(error)
            } else {
                // DaemonUnavailableError or a transport drop while the daemon
                // is exiting falls back to cold-launching the desktop.
                Ok(DeliveryResult::NotRunning)
            }
        }
    }
}

/// Build the argv list to forward from `waku_cli` when cold-launching the GUI.
pub fn forwarded_gui_args(request: &StartupOpenRequest) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(host) = &request.host {
        args.push("--host".to_owned());
        args.push(host.clone());
    }
    if let Some(path) = &request.path {
        args.push("--".to_owned());
        args.push(path.clone());
    }
    args
}

/// Locate the Narel GUI executable relative to the running `narel_cli` binary.
pub fn resolve_gui_executable() -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe().context("could not locate narel CLI executable")?;
    if let Some(found) = resolve_gui_executable_from(&current) {
        return Ok(found);
    }
    if let Ok(canonical) = fs::canonicalize(&current)
        && canonical != current
        && let Some(found) = resolve_gui_executable_from(&canonical)
    {
        return Ok(found);
    }
    bail!(
        "could not locate the Narel GUI executable near {}",
        current.display()
    )
}

/// Pure helper resolving the GUI executable from a given CLI executable path.
///
/// Checks the packaged `<app>\bin\narel.exe` -> `<app>\narel.exe` layout first,
/// then falls back to the sibling Cargo layout (`target/<profile>/narel_cli` ->
/// `target/<profile>/narel`), rejecting any candidate that resolves to `cli_exe`
/// itself so a misconfigured directory can never self-exec loop.
pub fn resolve_gui_executable_from(cli_exe: &Path) -> Option<PathBuf> {
    let exe_dir = cli_exe.parent()?;
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["narel.exe"]
    } else if cfg!(target_os = "macos") {
        &["Narel", "narel"]
    } else {
        &["narel"]
    };

    let search_dirs: Vec<&Path> = exe_dir
        .parent()
        .into_iter()
        .chain(std::iter::once(exe_dir))
        .collect();

    for dir in search_dirs {
        for name in candidates {
            let candidate = dir.join(name);
            if candidate.is_file() && !is_same_executable(&candidate, cli_exe) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_same_executable(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Spawn the Waku GUI binary detached from the calling terminal.
pub fn spawn_gui_detached(gui_exe: &Path, args: &[String]) -> anyhow::Result<()> {
    let mut command = Command::new(gui_exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = command
        .spawn()
        .with_context(|| format!("could not launch Narel GUI at {}", gui_exe.display()))?;
    allow_set_foreground_window(Some(child.id()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_basic_open_help_version_and_separator_args() {
        assert_eq!(
            parse_args(strings(&[]), false).unwrap(),
            CliAction::Open(StartupOpenRequest {
                path: None,
                host: None,
            })
        );
        assert_eq!(
            parse_args(strings(&["."]), false).unwrap(),
            CliAction::Open(StartupOpenRequest {
                path: Some(".".into()),
                host: None,
            })
        );
        assert_eq!(parse_args(strings(&["-h"]), false).unwrap(), CliAction::Help);
        assert_eq!(
            parse_args(strings(&["--help"]), false).unwrap(),
            CliAction::Help
        );
        assert_eq!(
            parse_args(strings(&["-V"]), false).unwrap(),
            CliAction::Version
        );
        assert_eq!(
            parse_args(strings(&["--version"]), false).unwrap(),
            CliAction::Version
        );
        assert_eq!(
            parse_args(strings(&["--", "-weird-folder"]), false).unwrap(),
            CliAction::Open(StartupOpenRequest {
                path: Some("-weird-folder".into()),
                host: None,
            })
        );
    }

    #[test]
    fn parses_host_and_remote_forms_and_rejects_invalid_args() {
        assert_eq!(
            parse_args(strings(&["--host", "ssh:devbox", "~/repo"]), false).unwrap(),
            CliAction::Open(StartupOpenRequest {
                path: Some("~/repo".into()),
                host: Some("ssh:devbox".into()),
            })
        );
        assert_eq!(
            parse_args(strings(&["--remote", "wsl:Ubuntu"]), false).unwrap(),
            CliAction::Open(StartupOpenRequest {
                path: None,
                host: Some("wsl:Ubuntu".into()),
            })
        );
        assert_eq!(
            parse_args(strings(&["--host=ssh:prod", "/srv/app"]), false).unwrap(),
            CliAction::Open(StartupOpenRequest {
                path: Some("/srv/app".into()),
                host: Some("ssh:prod".into()),
            })
        );
        assert_eq!(
            parse_args(strings(&["--remote=wsl:Debian", "--", "-dir"]), false).unwrap(),
            CliAction::Open(StartupOpenRequest {
                path: Some("-dir".into()),
                host: Some("wsl:Debian".into()),
            })
        );

        assert!(parse_args(strings(&["--host"]), false).is_err());
        assert!(parse_args(strings(&["--host="]), false).is_err());
        assert!(parse_args(strings(&["--remote", "   "]), false).is_err());
        assert!(parse_args(strings(&["--unknown"]), false).is_err());
        assert!(parse_args(strings(&["a", "b"]), false).is_err());
    }

    #[test]
    fn ignores_macos_launch_services_psn_flag_only_when_enabled() {
        assert_eq!(
            parse_args(strings(&["-psn_0_123456"]), true).unwrap(),
            CliAction::Open(StartupOpenRequest {
                path: None,
                host: None,
            })
        );
        assert_eq!(
            parse_args(strings(&["-psn_0_9876", "/tmp/work"]), true).unwrap(),
            CliAction::Open(StartupOpenRequest {
                path: Some("/tmp/work".into()),
                host: None,
            })
        );
        assert!(parse_args(strings(&["-psn_0_123456"]), false).is_err());
    }

    #[test]
    fn prepares_local_paths_against_cwd_and_preserves_remote_paths() {
        let cwd = Path::new(if cfg!(windows) { r"C:\work" } else { "/work" });

        let local_dot = prepare_request(
            StartupOpenRequest {
                path: Some(".".into()),
                host: None,
            },
            Some(cwd),
        );
        assert_eq!(local_dot.path.as_deref(), Some(cwd.to_string_lossy().as_ref()));
        assert_eq!(local_dot.host, None);

        let local_rel = prepare_request(
            StartupOpenRequest {
                path: Some("sub/repo".into()),
                host: None,
            },
            Some(cwd),
        );
        assert_eq!(
            local_rel.path.as_deref(),
            Some(cwd.join("sub/repo").to_string_lossy().as_ref())
        );

        let local_tilde = prepare_request(
            StartupOpenRequest {
                path: Some("~/repo".into()),
                host: None,
            },
            Some(cwd),
        );
        assert_eq!(local_tilde.path.as_deref(), Some("~/repo"));

        // Explicit `--host local` normalizes to local and resolves against cwd.
        let explicit_local = prepare_request(
            StartupOpenRequest {
                path: Some(".".into()),
                host: Some("local".into()),
            },
            Some(cwd),
        );
        assert_eq!(explicit_local.host, None);
        assert_eq!(
            explicit_local.path.as_deref(),
            Some(cwd.to_string_lossy().as_ref())
        );

        // Remote paths must NEVER be resolved against local cwd.
        let remote = prepare_request(
            StartupOpenRequest {
                path: Some("~/remote-proj".into()),
                host: Some("ssh:devbox".into()),
            },
            Some(cwd),
        );
        assert_eq!(remote.host.as_deref(), Some("ssh:devbox"));
        assert_eq!(remote.path.as_deref(), Some("~/remote-proj"));

        let remote_rel = prepare_request(
            StartupOpenRequest {
                path: Some("relative/on/remote".into()),
                host: Some("wsl:Ubuntu".into()),
            },
            Some(cwd),
        );
        assert_eq!(remote_rel.path.as_deref(), Some("relative/on/remote"));
    }

    #[test]
    fn validates_local_directories_files_and_rejects_missing_paths() {
        let root = std::env::temp_dir().join(format!("waku-cli-validate-{}", uuid::Uuid::new_v4()));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("README.md");
        fs::write(&file, "hi").unwrap();

        let expected = strip_windows_verbatim(fs::canonicalize(&nested).unwrap())
            .to_string_lossy()
            .into_owned();

        let from_dir = validate_local_workspace_path(&nested.to_string_lossy()).unwrap();
        assert_eq!(from_dir, expected);
        assert!(!from_dir.starts_with(r"\\?\"));

        let from_file = validate_local_workspace_path(&file.to_string_lossy()).unwrap();
        assert_eq!(from_file, expected);

        let missing = root.join("does-not-exist");
        let err = validate_local_workspace_path(&missing.to_string_lossy()).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "err={err}");

        assert!(validate_local_workspace_path("   ").is_err());

        fs::remove_dir_all(&root).unwrap();
    }

    /// Default macOS volumes are case-insensitive, so `Waku` and `waku` name
    /// the same file there. Compare the directory and filename instead of the
    /// exact path string, which the resolver may return with either case.
    fn assert_resolves_to(cli: &Path, expected: &Path) {
        let resolved = resolve_gui_executable_from(cli).expect("gui executable resolves");
        assert!(resolved.is_file());
        assert_eq!(resolved.parent(), expected.parent());
        assert!(resolved
            .file_name()
            .unwrap()
            .eq_ignore_ascii_case(expected.file_name().unwrap()));
    }

    #[test]
    fn resolves_gui_executable_in_packaged_and_cargo_layouts() {
        let gui_name = if cfg!(target_os = "windows") {
            "narel.exe"
        } else {
            "narel"
        };
        let cli_cargo_name = if cfg!(target_os = "windows") {
            "narel_cli.exe"
        } else {
            "narel_cli"
        };

        // 1. Packaged layout: `<app>/bin/narel.exe` -> `<app>/narel.exe`.
        let packaged =
            std::env::temp_dir().join(format!("narel-cli-packaged-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(packaged.join("bin")).unwrap();
        let packaged_gui = packaged.join(gui_name);
        let packaged_cli = packaged.join("bin").join(gui_name);
        fs::write(&packaged_gui, b"gui").unwrap();
        fs::write(&packaged_cli, b"cli").unwrap();
        assert_resolves_to(&packaged_cli, &packaged_gui);

        // Self-exec guard: if `<app>/narel.exe` is missing, `<app>/bin/narel.exe`
        // must NOT resolve to itself.
        fs::remove_file(&packaged_gui).unwrap();
        assert_eq!(resolve_gui_executable_from(&packaged_cli), None);
        fs::remove_dir_all(&packaged).unwrap();

        // 2. Cargo layout: `target/debug/narel_cli.exe` -> `target/debug/narel.exe`.
        let cargo_dir =
            std::env::temp_dir().join(format!("narel-cli-cargo-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&cargo_dir).unwrap();
        let cargo_gui = cargo_dir.join(gui_name);
        let cargo_cli = cargo_dir.join(cli_cargo_name);
        fs::write(&cargo_gui, b"gui").unwrap();
        fs::write(&cargo_cli, b"cli").unwrap();
        assert_resolves_to(&cargo_cli, &cargo_gui);
        fs::remove_dir_all(&cargo_dir).unwrap();
    }

    #[test]
    fn builds_forwarded_gui_args_safely() {
        assert_eq!(
            forwarded_gui_args(&StartupOpenRequest {
                path: None,
                host: None,
            }),
            Vec::<String>::new()
        );
        assert_eq!(
            forwarded_gui_args(&StartupOpenRequest {
                path: Some("-dash-dir".into()),
                host: None,
            }),
            strings(&["--", "-dash-dir"])
        );
        assert_eq!(
            forwarded_gui_args(&StartupOpenRequest {
                path: Some("~/repo".into()),
                host: Some("ssh:devbox".into()),
            }),
            strings(&["--host", "ssh:devbox", "--", "~/repo"])
        );
    }
}
