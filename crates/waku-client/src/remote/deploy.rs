//! Content-addressed installation of `waku-daemon` on WSL and SSH hosts.
//!
//! An installed daemon is reused only when `waku-daemon --fingerprint`
//! matches the binary this desktop carries. Otherwise the binary is copied
//! (WSL) or streamed over ssh's stdin (SSH) into a temporary file, verified,
//! and atomically moved into place, so concurrent installers and interrupted
//! uploads can never leave a half-written daemon behind.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, anyhow, bail};
use parking_lot::Mutex;
use waku_protocol::bridge::Fingerprint;

use super::connection::{Connection, ConnectionKind};
use super::process_util::{StderrTail, output_with_timeout, wait_with_deadline, with_detail};
use super::{ssh, wsl};

/// Install directory below the remote `$HOME`. Development builds keep their
/// own so they never replace a production daemon a released app relies on.
pub const REMOTE_DIRECTORY: &str = if cfg!(debug_assertions) {
    ".cache/waku-dev/server"
} else {
    ".cache/waku/server"
};
const DAEMON_NAME: &str = "waku-daemon";
const REMOTE_DEV_DIRECTORY: &str = "remote-dev";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const WSL_DEPLOY_TIMEOUT: Duration = Duration::from_secs(60);
const SSH_STATUS_TIMEOUT: Duration = Duration::from_secs(15);
const SSH_INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RemoteOs {
    Linux,
    Darwin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RemoteArch {
    X86_64,
    Aarch64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RemoteTarget {
    pub os: RemoteOs,
    pub arch: RemoteArch,
}

impl RemoteTarget {
    fn os_name(self) -> &'static str {
        match self.os {
            RemoteOs::Linux => "linux",
            RemoteOs::Darwin => "darwin",
        }
    }

    fn arch_name(self) -> &'static str {
        match self.arch {
            RemoteArch::X86_64 => "x86_64",
            RemoteArch::Aarch64 => "aarch64",
        }
    }

    /// Bundled artifact name, e.g. `waku-daemon-linux-x86_64`.
    pub fn artifact_name(self) -> String {
        format!("{DAEMON_NAME}-{}-{}", self.os_name(), self.arch_name())
    }

    fn rust_triples(self) -> &'static [&'static str] {
        match (self.os, self.arch) {
            (RemoteOs::Linux, RemoteArch::X86_64) => {
                &["x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"]
            }
            (RemoteOs::Linux, RemoteArch::Aarch64) => {
                &["aarch64-unknown-linux-musl", "aarch64-unknown-linux-gnu"]
            }
            (RemoteOs::Darwin, RemoteArch::X86_64) => &["x86_64-apple-darwin"],
            (RemoteOs::Darwin, RemoteArch::Aarch64) => &["aarch64-apple-darwin"],
        }
    }

    fn host() -> Option<Self> {
        let os = if cfg!(target_os = "linux") {
            RemoteOs::Linux
        } else if cfg!(target_os = "macos") {
            RemoteOs::Darwin
        } else {
            return None;
        };
        let arch = parse_arch(std::env::consts::ARCH).ok()?;
        Some(Self { os, arch })
    }
}

fn parse_arch(arch: &str) -> anyhow::Result<RemoteArch> {
    match arch.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Ok(RemoteArch::X86_64),
        "aarch64" | "arm64" => Ok(RemoteArch::Aarch64),
        other => bail!("remote architecture {other:?} is not supported"),
    }
}

/// Parse `uname -s<TAB>uname -m`.
pub fn parse_target_probe(output: &[u8]) -> anyhow::Result<RemoteTarget> {
    let output = String::from_utf8_lossy(output);
    let line = output
        .lines()
        .map(str::trim)
        .rfind(|line| line.contains('\t'))
        .ok_or_else(|| anyhow!("invalid platform probe response {:?}", output.trim()))?;
    let (os, arch) = line.split_once('\t').unwrap_or_default();
    let os = match os.trim().to_ascii_lowercase().as_str() {
        "linux" => RemoteOs::Linux,
        "darwin" => RemoteOs::Darwin,
        other => bail!("remote operating system {other:?} is not supported"),
    };
    Ok(RemoteTarget {
        os,
        arch: parse_arch(arch)?,
    })
}

const PROBE_SCRIPT: &str = "printf '%s\\t%s\\n' \"$(uname -s)\" \"$(uname -m)\"";

pub fn probe(connection: &Connection) -> anyhow::Result<RemoteTarget> {
    let command = match &connection.kind {
        ConnectionKind::Local => bail!("the local connection needs no probe"),
        ConnectionKind::Wsl { distro } => wsl::command(distro, PROBE_SCRIPT, &[]),
        ConnectionKind::Ssh { .. } => {
            let mut command = ssh::command(connection)?;
            command.arg(ssh::wrap_posix_script(PROBE_SCRIPT));
            command
        }
    };
    let output = output_with_timeout(command, PROBE_TIMEOUT, "the platform probe")?;
    if !output.status.success() {
        bail!(
            "{}",
            with_detail(
                &format!("could not reach {}", connection.name),
                &stderr_text(connection, &output.stderr),
            )
        );
    }
    let target = parse_target_probe(&output.stdout)?;
    if matches!(connection.kind, ConnectionKind::Wsl { .. }) && target.os != RemoteOs::Linux {
        bail!("the WSL distribution did not report Linux");
    }
    Ok(target)
}

fn stderr_text(connection: &Connection, stderr: &[u8]) -> String {
    match connection.kind {
        ConnectionKind::Wsl { .. } => wsl::decode_wsl_text(stderr).trim().to_owned(),
        _ => ssh::stderr_without_askpass_diagnostics(stderr)
            .trim()
            .to_owned(),
    }
}

/// A daemon build for one remote target.
#[derive(Clone, Debug)]
pub struct Artifact {
    pub path: PathBuf,
    pub fingerprint: String,
    pub size: u64,
}

/// Fingerprints by path, size, and modification time, so an unchanged binary
/// is hashed once per process while a rebuilt one is hashed again.
type FingerprintCache = Mutex<HashMap<(PathBuf, u64, Option<SystemTime>), String>>;

/// Find the daemon binary for `target`, in order: an explicit override, the
/// package's `remote/` resources, `target/remote-dev` and Cargo's target
/// directories in development, and finally this host's own daemon when the
/// platforms match.
pub fn artifact(target: RemoteTarget) -> anyhow::Result<Artifact> {
    let path = artifact_path(target)?;
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("could not inspect {}", path.display()))?;
    let key = (path.clone(), metadata.len(), metadata.modified().ok());
    static CACHE: OnceLock<FingerprintCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(fingerprint) = cache.lock().get(&key).cloned() {
        return Ok(Artifact {
            path,
            fingerprint,
            size: metadata.len(),
        });
    }
    let fingerprint = file_fingerprint(&path)?;
    cache.lock().insert(key, fingerprint.clone());
    Ok(Artifact {
        path,
        fingerprint,
        size: metadata.len(),
    })
}

fn file_fingerprint(path: &Path) -> anyhow::Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
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

fn artifact_path(target: RemoteTarget) -> anyhow::Result<PathBuf> {
    let name = target.artifact_name();
    let override_variable = format!(
        "WAKU_REMOTE_DAEMON_{}_{}",
        target.os_name().to_ascii_uppercase(),
        target.arch_name().to_ascii_uppercase()
    );
    if let Some(path) = std::env::var_os(&override_variable).filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "{override_variable} points to a missing file: {}",
            path.display()
        );
    }
    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join("remote").join(&name));
        candidates.push(directory.join("../Resources/remote").join(&name));
        // The Linux archive keeps executables in bin/ and data in share/waku.
        candidates.push(directory.join("../share/waku/remote").join(&name));
        if cfg!(debug_assertions)
            && let Some(target_directory) = directory
                .ancestors()
                .find(|candidate| candidate.file_name().is_some_and(|name| name == "target"))
        {
            // Where `scripts/dev.ts` keeps a daemon built for a host this
            // desktop cannot compile for itself, such as WSL from Windows.
            candidates.push(target_directory.join(REMOTE_DEV_DIRECTORY).join(&name));
            for triple in target.rust_triples() {
                for profile in ["debug", "release"] {
                    candidates.push(
                        target_directory
                            .join(triple)
                            .join(profile)
                            .join(DAEMON_NAME),
                    );
                }
            }
        }
        if RemoteTarget::host() == Some(target) {
            candidates.push(directory.join(DAEMON_NAME));
        }
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            anyhow!(
                "this Waku build carries no daemon for {} {}; set {override_variable} to a {name} binary",
                target.os_name(),
                target.arch_name()
            )
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeployOutcome {
    /// The installed daemon already matched.
    Reused,
    Installed,
}

/// Shell script shared by both installers. `$1` is the WSL-visible source
/// for WSL; SSH streams the binary on stdin after printing `upload`.
fn install_script(fingerprint: &str, force: bool, source: InstallSource) -> String {
    let fetch = match source {
        InstallSource::WslPath => {
            "src=$(wslpath -u \"$1\"); [ -f \"$src\" ] || { printf 'source_missing\\t%s\\n' \"$src\" >&2; exit 44; }; cp -- \"$src\" \"$tmp\""
        }
        InstallSource::Stdin => "printf 'upload\\n'; cat > \"$tmp\"",
    };
    format!(
        "set -e; dst=\"$HOME/{REMOTE_DIRECTORY}/{DAEMON_NAME}\"; expected=\"{fingerprint}\"; force={force}; \
         actual=$(\"$dst\" --fingerprint 2>/dev/null || true); \
         if [ \"$force\" = \"false\" ] && [ \"$actual\" = \"$expected\" ]; then printf 'ready\\n'; exit 0; fi; \
         mkdir -p \"$(dirname \"$dst\")\"; chmod 700 \"$(dirname \"$dst\")\"; tmp=\"$dst.tmp.$$\"; trap 'rm -f \"$tmp\"' EXIT; \
         {fetch}; chmod 700 \"$tmp\"; \
         actual=$(\"$tmp\" --fingerprint 2>/dev/null || true); \
         [ \"$actual\" = \"$expected\" ] || {{ printf 'fingerprint_mismatch\\n' >&2; exit 43; }}; \
         mv -f \"$tmp\" \"$dst\"; trap - EXIT; printf 'installed\\n'"
    )
}

#[derive(Clone, Copy)]
enum InstallSource {
    WslPath,
    Stdin,
}

pub fn deploy(
    connection: &Connection,
    target: RemoteTarget,
    force: bool,
) -> anyhow::Result<DeployOutcome> {
    let artifact = artifact(target)?;
    match &connection.kind {
        ConnectionKind::Local => bail!("the local connection needs no deployment"),
        ConnectionKind::Wsl { distro } => deploy_wsl(distro, &artifact, force),
        ConnectionKind::Ssh { .. } => deploy_ssh(connection, &artifact, force),
    }
}

fn deploy_wsl(distro: &str, artifact: &Artifact, force: bool) -> anyhow::Result<DeployOutcome> {
    let source = wsl::normalize_windows_path(&artifact.path.to_string_lossy());
    let script = install_script(&artifact.fingerprint, force, InstallSource::WslPath);
    let command = wsl::command(distro, &script, &["waku-deploy", &source]);
    let output = output_with_timeout(command, WSL_DEPLOY_TIMEOUT, "the WSL daemon install")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    match (output.status.success(), stdout.trim()) {
        (true, "ready") => Ok(DeployOutcome::Reused),
        (true, "installed") => Ok(DeployOutcome::Installed),
        _ => bail!(
            "{}",
            with_detail(
                &format!("could not install waku-daemon in WSL {distro}"),
                wsl::decode_wsl_text(&output.stderr).trim(),
            )
        ),
    }
}

fn deploy_ssh(
    connection: &Connection,
    artifact: &Artifact,
    force: bool,
) -> anyhow::Result<DeployOutcome> {
    let script = install_script(&artifact.fingerprint, force, InstallSource::Stdin);
    let mut command = ssh::command(connection)?;
    command
        .arg(ssh::wrap_posix_script(&script))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("could not run ssh")?;
    let mut stdin = child.stdin.take().context("ssh stdin is unavailable")?;
    let stdout = child.stdout.take().context("ssh stdout is unavailable")?;
    let stderr = StderrTail::capture(child.stderr.take().context("ssh stderr is unavailable")?);
    let (lines_tx, lines) = crossbeam_channel::unbounded();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let failure = |child: &mut std::process::Child, message: &str| {
        let _ = child.kill();
        let _ = child.wait();
        anyhow!("{}", with_detail(message, &stderr.text()))
    };

    let status_line = match lines.recv_timeout(SSH_STATUS_TIMEOUT) {
        Ok(line) => line,
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            return Err(failure(&mut child, "timed out preparing the remote daemon"));
        }
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
            drop(stdin);
            let _ = wait_with_deadline(&mut child, Duration::from_secs(5));
            return Err(anyhow!(
                "{}",
                with_detail(
                    &format!("could not prepare the daemon on {}", connection.name),
                    &stderr.text()
                )
            ));
        }
    };
    match status_line.trim() {
        "ready" => {
            drop(stdin);
            let status = wait_with_deadline(&mut child, Duration::from_secs(10));
            if status.is_some_and(|status| status.success()) {
                return Ok(DeployOutcome::Reused);
            }
            return Err(failure(&mut child, "could not reuse the remote daemon"));
        }
        "upload" => {}
        other => {
            let message = format!("unexpected remote install status {other:?}");
            return Err(failure(&mut child, &message));
        }
    }

    let uploaded = (|| -> std::io::Result<u64> {
        let mut source = std::fs::File::open(&artifact.path)?;
        let copied = std::io::copy(&mut source, &mut stdin)?;
        stdin.flush()?;
        Ok(copied)
    })();
    drop(stdin);
    match uploaded {
        Ok(copied) if copied == artifact.size => {}
        Ok(copied) => {
            let message = format!(
                "daemon upload was truncated: wrote {copied} of {} bytes",
                artifact.size
            );
            return Err(failure(&mut child, &message));
        }
        Err(error) => {
            let message = format!("could not upload waku-daemon: {error}");
            return Err(failure(&mut child, &message));
        }
    }
    let installed = lines.recv_timeout(SSH_INSTALL_TIMEOUT).unwrap_or_default();
    let status = wait_with_deadline(&mut child, Duration::from_secs(10));
    if installed.trim() == "installed" && status.is_some_and(|status| status.success()) {
        return Ok(DeployOutcome::Installed);
    }
    Err(anyhow!(
        "{}",
        with_detail("could not install waku-daemon", &stderr.text())
    ))
}

/// Remote script that starts or reuses the host's daemon and relays it.
pub fn bridge_script() -> String {
    format!(
        "exec \"$HOME/{REMOTE_DIRECTORY}/{DAEMON_NAME}\" {}",
        waku_protocol::bridge::BRIDGE_SUBCOMMAND
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_parsing_accepts_linux_and_darwin() {
        assert_eq!(
            parse_target_probe(b"Linux\tx86_64\n").unwrap(),
            RemoteTarget {
                os: RemoteOs::Linux,
                arch: RemoteArch::X86_64
            }
        );
        assert_eq!(
            parse_target_probe(b"motd noise\nDarwin\tarm64\n").unwrap(),
            RemoteTarget {
                os: RemoteOs::Darwin,
                arch: RemoteArch::Aarch64
            }
        );
        assert!(parse_target_probe(b"FreeBSD\tamd64\n").is_err());
        assert!(parse_target_probe(b"Linux\triscv64\n").is_err());
        assert!(parse_target_probe(b"garbage").is_err());
    }

    #[test]
    fn artifact_names_follow_the_bundle_layout() {
        assert_eq!(
            RemoteTarget {
                os: RemoteOs::Linux,
                arch: RemoteArch::Aarch64
            }
            .artifact_name(),
            "waku-daemon-linux-aarch64"
        );
    }

    #[test]
    fn install_scripts_verify_before_replacing() {
        for source in [InstallSource::WslPath, InstallSource::Stdin] {
            let script = install_script("0123456789abcdef", false, source);
            let verify = script.find("fingerprint_mismatch").unwrap();
            let replace = script.find("mv -f").unwrap();
            assert!(verify < replace);
            assert!(script.contains("expected=\"0123456789abcdef\""));
        }
        assert!(install_script("x", false, InstallSource::Stdin).contains("printf 'upload\\n'"));
    }
}
