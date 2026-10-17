//! Connect to a WSL or SSH daemon from the command line.
//!
//! ```sh
//! cargo run -p waku-client --example remote_connect -- wsl:Ubuntu-24.04
//! cargo run -p waku-client --example remote_connect -- ssh:devbox
//! ```
//!
//! It installs or reuses the daemon, prints its settings and task counts,
//! lists the remote home directory, and with `--drop` kills the bridge once to
//! confirm the supervisor reconnects to the same daemon.

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use uuid::Uuid;
use waku_client::remote::{Connection, ConnectionKind, ConnectionsFile};
use waku_client::{
    Command, DaemonSupervisor, ResponsePayload, WorkspaceOperation, WorkspaceResult,
};

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let spec = arguments
        .next()
        .context("usage: remote_connect <wsl:DISTRO|ssh:HOST|saved-id> [--drop]")?;
    let drop_once = arguments.any(|argument| argument == "--drop");
    let saved = ConnectionsFile::load().unwrap_or_default().connections;
    let connection = Connection::from_spec(&spec, &saved).context("unknown connection")?;
    if connection.is_local() {
        bail!("pick a WSL or SSH connection");
    }

    let started = Instant::now();
    let daemon = DaemonSupervisor::connect_remote(connection)?;
    println!("connected in {:?}", started.elapsed());
    report(&daemon)?;

    if drop_once {
        // Kill the remote bridge process, as a dropped network would. The
        // daemon itself must survive and accept the reconnect.
        println!("killing the remote bridge…");
        kill_remote_bridge(&daemon.connection().context("no connection")?)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !daemon.is_disconnected() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        println!("disconnected: {}", daemon.is_disconnected());
        let deadline = Instant::now() + Duration::from_secs(60);
        while daemon.is_disconnected() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
        }
        if daemon.is_disconnected() {
            bail!("the supervisor did not reconnect");
        }
        println!("reconnected");
        report(&daemon)?;
    }
    Ok(())
}

fn report(daemon: &DaemonSupervisor) -> anyhow::Result<()> {
    let client = daemon.client();
    let settings = daemon.settings();
    println!(
        "settings: {} top-level keys",
        serde_json::to_value(&settings)?
            .as_object()
            .map_or(0, |map| map.len())
    );
    match client.request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)? {
        ResponsePayload::TaskState {
            projects, sessions, ..
        } => println!(
            "tasks: {} projects, {} sessions",
            projects.len(),
            sessions.len()
        ),
        other => bail!("unexpected task state response: {other:?}"),
    }
    let workspace = waku_client::WorkspaceClient::new(client);
    match workspace.request(WorkspaceOperation::BrowseDirectory { path: None })? {
        WorkspaceResult::Directory { path, entries, .. } => {
            println!("home: {} ({} entries)", path.display(), entries.len());
        }
        _ => bail!("unexpected browse response"),
    }
    Ok(())
}

fn kill_remote_bridge(connection: &Connection) -> anyhow::Result<()> {
    let script = "pkill -f '[w]aku-daemon bridge' || true";
    let mut command = match &connection.kind {
        ConnectionKind::Wsl { distro } => waku_client::remote::wsl::command(distro, script, &[]),
        ConnectionKind::Ssh { .. } => {
            let mut command = waku_client::remote::ssh::command(connection)?;
            command.arg(waku_client::remote::ssh::wrap_posix_script(script));
            command
        }
        ConnectionKind::Local => bail!("local has no bridge"),
    };
    command.status()?;
    Ok(())
}
