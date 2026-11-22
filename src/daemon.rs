//! Desktop ownership of the Waku daemon process.

use std::path::PathBuf;

use anyhow::{Context as _, anyhow, bail};

pub fn start_process() -> anyhow::Result<waku_client::DaemonSupervisor> {
    let address = std::env::var(waku_client::DAEMON_ADDRESS_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let token = std::env::var(waku_client::DAEMON_TOKEN_ENV)
        .ok()
        .filter(|value| !value.is_empty());
    match (address, token) {
        (Some(address), Some(token)) => {
            return waku_client::DaemonSupervisor::connect(address.trim(), token);
        }
        (Some(_), None) => bail!(
            "{} is set but {} is missing",
            waku_client::DAEMON_ADDRESS_ENV,
            waku_client::DAEMON_TOKEN_ENV
        ),
        (None, Some(_)) => bail!(
            "{} is set but {} is missing",
            waku_client::DAEMON_TOKEN_ENV,
            waku_client::DAEMON_ADDRESS_ENV
        ),
        (None, None) => {}
    }
    let app_settings = waku_client::persistence::load_or_create_app_settings()
        .context("could not load desktop daemon settings")?;
    waku_client::DaemonSupervisor::spawn_configured(
        &daemon_executable_path()?,
        cfg!(debug_assertions),
        app_settings.daemon_exposure,
    )
}

/// Resolve a host spec (`ssh:<alias>`, `wsl:<distro>`, `local`, or a saved
/// connection id) against `saved`, preferring saved SSH connections whose
/// config alias or display host matches before synthesizing a new Agent-auth
/// entry via [`waku_client::remote::Connection::from_spec`].
pub(crate) fn resolve_connection_spec(
    spec: &str,
    saved: &[waku_client::remote::Connection],
) -> Option<waku_client::remote::Connection> {
    use waku_client::remote::{Connection, ConnectionKind, SshTarget};

    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if let Some(connection) = saved.iter().find(|connection| connection.id == spec) {
        return Some(connection.clone());
    }
    if let Some(alias) = spec.strip_prefix("ssh:").filter(|value| !value.is_empty()) {
        if let Some(connection) = saved.iter().find(|connection| {
            matches!(
                &connection.kind,
                ConnectionKind::Ssh {
                    target: SshTarget::ConfigHost { host, .. },
                } if host == alias
            )
        }) {
            return Some(connection.clone());
        }
        if let Some(connection) = saved.iter().find(|connection| {
            matches!(
                &connection.kind,
                ConnectionKind::Ssh { target } if target.display_host() == alias
            )
        }) {
            return Some(connection.clone());
        }
    }
    Connection::from_spec(spec, saved)
}

/// The WSL or SSH connection to open at launch: `WAKU_CONNECTION` (a saved
/// id, `wsl:<distro>`, or `ssh:<config host>`) wins over the saved choice.
pub(crate) fn startup_connection_from(
    saved: &waku_client::remote::ConnectionsFile,
) -> Option<waku_client::remote::Connection> {
    let connection = match std::env::var("WAKU_CONNECTION") {
        Ok(spec) if !spec.trim().is_empty() => {
            let connection = resolve_connection_spec(&spec, &saved.connections);
            if connection.is_none() {
                eprintln!("WAKU_CONNECTION={spec:?} names no connection");
            }
            connection
        }
        _ => saved.active_connection().cloned(),
    };
    connection.filter(|connection| !connection.is_local())
}

/// Resolve the local host name once during app construction. Settings can
/// then show a useful LAN URL without touching the OS from a render frame.
pub fn local_hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buffer = [0_u8; 256];
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result == 0 {
            let length = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            let hostname = String::from_utf8_lossy(&buffer[..length]).trim().to_owned();
            if !hostname.is_empty() {
                return Some(hostname);
            }
        }
    }
    // `COMPUTERNAME` is the Windows equivalent and is always set; `HOSTNAME`
    // covers the shells that export it.
    ["COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|hostname| hostname.trim().to_owned())
        .find(|hostname| !hostname.is_empty())
}

fn daemon_executable_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("WAKU_DAEMON_PATH").filter(|path| !path.is_empty()) {
        return Ok(path.into());
    }
    let executable = format!("waku-daemon{}", std::env::consts::EXE_SUFFIX);
    let current = std::env::current_exe().context("could not locate the Waku executable")?;

    // Development keeps the daemon beside Cargo's debug artifacts rather than
    // inside Waku Debug.app. The supervisor watches this file and swaps only
    // the daemon when the development watcher relinks it.
    #[cfg(debug_assertions)]
    if let Some(debug_directory) = current
        .ancestors()
        .find(|candidate| candidate.file_name().is_some_and(|name| name == "debug"))
    {
        // Match scripts/dev.ts: its rebuild target is the feature-gated
        // debug daemon. A stale regular daemon may also live beside it.
        for name in [
            format!("waku-debug-daemon{}", std::env::consts::EXE_SUFFIX),
            executable.clone(),
        ] {
            let external = debug_directory.join(name);
            if external.is_file() {
                return Ok(external);
            }
        }
    }

    let sibling = current
        .parent()
        .map(|directory| directory.join(&executable))
        .ok_or_else(|| anyhow!("Waku executable has no parent directory"))?;
    if sibling.is_file() {
        return Ok(sibling);
    }
    #[cfg(debug_assertions)]
    bail!(
        "Waku daemon was not found in Cargo's debug directory or next to the app executable: {}",
        sibling.display(),
    );
    #[cfg(not(debug_assertions))]
    bail!(
        "Waku daemon is missing next to the app executable: {}",
        sibling.display(),
    )
}
