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
    Windows,
}

impl RemoteOs {
    fn executable_suffix(self) -> &'static str {
        match self {
            Self::Windows => ".exe",
            Self::Linux | Self::Darwin => "",
        }
    }

    fn daemon_binary_name(self) -> &'static str {
        match self {
            Self::Windows => "waku-daemon.exe",
            Self::Linux | Self::Darwin => DAEMON_NAME,
        }
    }
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
            RemoteOs::Windows => "windows",
        }
    }

    fn arch_name(self) -> &'static str {
        match self.arch {
            RemoteArch::X86_64 => "x86_64",
            RemoteArch::Aarch64 => "aarch64",
        }
    }

    fn executable_suffix(self) -> &'static str {
        self.os.executable_suffix()
    }

    fn daemon_binary_name(self) -> &'static str {
        self.os.daemon_binary_name()
    }

    /// Bundled artifact name, e.g. `waku-daemon-linux-x86_64` or
    /// `waku-daemon-windows-x86_64.exe`.
    pub fn artifact_name(self) -> String {
        format!(
            "{DAEMON_NAME}-{}-{}{}",
            self.os_name(),
            self.arch_name(),
            self.executable_suffix()
        )
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
            (RemoteOs::Windows, RemoteArch::X86_64) => {
                &["x86_64-pc-windows-msvc", "x86_64-pc-windows-gnu"]
            }
            (RemoteOs::Windows, RemoteArch::Aarch64) => {
                &["aarch64-pc-windows-msvc", "aarch64-pc-windows-gnullvm"]
            }
        }
    }

    fn host() -> Option<Self> {
        let os = if cfg!(target_os = "linux") {
            RemoteOs::Linux
        } else if cfg!(target_os = "macos") {
            RemoteOs::Darwin
        } else if cfg!(target_os = "windows") {
            RemoteOs::Windows
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

/// Parse `uname -s<TAB>uname -m` or the Windows PowerShell probe line.
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
        "windows" | "windows_nt" => RemoteOs::Windows,
        other
            if other.starts_with("mingw")
                || other.starts_with("msys")
                || other.starts_with("cygwin") =>
        {
            RemoteOs::Windows
        }
        other => bail!("remote operating system {other:?} is not supported"),
    };
    Ok(RemoteTarget {
        os,
        arch: parse_arch(arch)?,
    })
}

const PROBE_SCRIPT: &str = "printf '%s\\t%s\\n' \"$(uname -s)\" \"$(uname -m)\"";
const WINDOWS_PROBE_SCRIPT: &str = "$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }; [Console]::Out.WriteLine(\"Windows`t$arch\")";

pub fn probe(connection: &Connection) -> anyhow::Result<RemoteTarget> {
    match &connection.kind {
        ConnectionKind::Local => bail!("the local connection needs no probe"),
        ConnectionKind::Wsl { distro } => {
            let command = wsl::command(distro, PROBE_SCRIPT, &[]);
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
            if target.os != RemoteOs::Linux {
                bail!("the WSL distribution did not report Linux");
            }
            Ok(target)
        }
        ConnectionKind::Ssh { .. } => probe_ssh(connection),
    }
}

fn probe_ssh(connection: &Connection) -> anyhow::Result<RemoteTarget> {
    let mut command = ssh::command(connection)?;
    command.arg(ssh::wrap_posix_script(PROBE_SCRIPT));
    let output = output_with_timeout(command, PROBE_TIMEOUT, "the platform probe")?;
    let mut posix_error = None;
    if output.status.success() {
        match parse_target_probe(&output.stdout) {
            Ok(target) => return Ok(target),
            Err(error) => posix_error = Some(error.to_string()),
        }
    }
    // OpenSSH exits 255 on transport, DNS, auth, or host-key failures; do not
    // waste another timeout or prompt cycle retrying PowerShell against a dead link.
    if output.status.code() == Some(255) {
        bail!(
            "{}",
            with_detail(
                &format!("could not reach {}", connection.name),
                &stderr_text(connection, &output.stderr),
            )
        );
    }
    let mut win_command = ssh::command(connection)?;
    win_command.arg(ssh::wrap_powershell_script(WINDOWS_PROBE_SCRIPT));
    let win_output = output_with_timeout(win_command, PROBE_TIMEOUT, "the platform probe")?;
    if win_output.status.success() {
        return parse_target_probe(&win_output.stdout);
    }
    let detail = stderr_text(connection, &win_output.stderr);
    let detail = if let Some(posix_error) = posix_error {
        // The POSIX probe ran and returned an unsupported OS or an
        // unparsable line. Keep that fact alongside the failed Windows
        // fallback instead of replacing it with "powershell.exe not found".
        if detail.is_empty() {
            posix_error
        } else {
            format!("{posix_error}\n{detail}")
        }
    } else if detail.is_empty() {
        stderr_text(connection, &output.stderr)
    } else {
        detail
    };
    bail!(
        "{}",
        with_detail(&format!("could not reach {}", connection.name), &detail)
    );
}

fn stderr_text(connection: &Connection, stderr: &[u8]) -> String {
    match connection.kind {
        ConnectionKind::Wsl { .. } => wsl::decode_wsl_text(stderr).trim().to_owned(),
        _ => ssh::stderr_without_askpass_diagnostics(stderr)
            .trim()
            .to_owned(),
    }
}

/// Rewrite the install script's machine markers into sentences, so a failed
/// connection says why the uploaded daemon could not run — a loader error
/// such as a too-old glibc — instead of reducing it to a fingerprint
/// mismatch.
fn explain_install_stderr(stderr: &str) -> String {
    const RUN_MARKER: &str = "daemon_failed_to_run\t";
    const MISMATCH_MARKER: &str = "fingerprint_mismatch\t";
    stderr
        .split_inclusive('\n')
        .map(|line| {
            if let Some(reason) = line.strip_prefix(RUN_MARKER) {
                format!("the uploaded daemon could not run: {reason}")
            } else if let Some(detail) = line.strip_prefix(MISMATCH_MARKER) {
                format!("the uploaded daemon did not match this build: {detail}")
            } else {
                line.to_owned()
            }
        })
        .collect()
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
                            .join(target.daemon_binary_name()),
                    );
                }
            }
            if RemoteTarget::host() == Some(target) {
                candidates.push(
                    target_directory
                        .join("debug")
                        .join(format!("waku-debug-daemon{}", target.executable_suffix())),
                );
                candidates.push(
                    target_directory
                        .join("debug")
                        .join(target.daemon_binary_name()),
                );
            }
        }
        if RemoteTarget::host() == Some(target) {
            if cfg!(debug_assertions) {
                candidates.push(
                    directory.join(format!("waku-debug-daemon{}", target.executable_suffix())),
                );
            }
            candidates.push(directory.join(target.daemon_binary_name()));
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
         mkdir -p \"$(dirname \"$dst\")\"; chmod 700 \"$(dirname \"$dst\")\"; tmp=\"$dst.tmp.$$\"; err=\"$tmp.err\"; \
         trap 'rm -f \"$tmp\" \"$err\"' EXIT; \
         {fetch}; chmod 700 \"$tmp\"; \
         actual=$(\"$tmp\" --fingerprint 2>\"$err\" || true); \
         if [ -z \"$actual\" ] && [ -s \"$err\" ]; then \
             reason=$(tr '\\n' ' ' < \"$err\" | cut -c1-400); \
             printf 'daemon_failed_to_run\\t%s\\n' \"$reason\" >&2; exit 43; \
         fi; \
         [ \"$actual\" = \"$expected\" ] || {{ printf 'fingerprint_mismatch\\texpected %s, got %s\\n' \"$expected\" \"${{actual:-no output}}\" >&2; exit 43; }}; \
         mv -f \"$tmp\" \"$dst\"; rm -f \"$err\"; trap - EXIT; printf 'installed\\n'"
    )
}

fn windows_remote_directory() -> String {
    REMOTE_DIRECTORY.replace('/', "\\")
}

/// PowerShell 5.1 / .NET 4.x installer matching [`install_script`].
/// Uses ProcessStartInfo for `--fingerprint` so child stderr never triggers
/// terminating errors under `$ErrorActionPreference = 'Stop'`, and renames any
/// running destination `.exe` aside before moving the verified temporary binary
/// into place.
fn windows_install_script(fingerprint: &str, force: bool) -> String {
    let dir_rel = windows_remote_directory();
    let daemon_exe = RemoteOs::Windows.daemon_binary_name();
    let force_literal = if force { "$true" } else { "$false" };
    format!(
        "$ErrorActionPreference='Stop';\
         $h=if($env:USERPROFILE){{$env:USERPROFILE}}else{{$HOME}};\
         $d=Join-Path $h '{dir_rel}';\
         $dst=Join-Path $d '{daemon_exe}';\
         $exp='{fingerprint}';\
         $frc={force_literal};\
         function Fp([string]$p){{\
           $i=New-Object Diagnostics.ProcessStartInfo $p,'--fingerprint';\
           $i.UseShellExecute=$false;$i.RedirectStandardOutput=$true;$i.RedirectStandardError=$true;$i.CreateNoWindow=$true;\
           $x=[Diagnostics.Process]::Start($i);\
           $o=$x.StandardOutput.ReadToEnd().Trim();\
           $e=$x.StandardError.ReadToEnd().Trim();\
           $x.WaitForExit();\
           return @($o,$e,$x.ExitCode)\
         }};\
         if(-not $frc -and [IO.File]::Exists($dst)){{\
           try{{$r=Fp $dst;if($r[0] -eq $exp){{[Console]::Out.WriteLine('ready');[Console]::Out.Flush();exit 0}}}}catch{{}}\
         }};\
         [IO.Directory]::CreateDirectory($d)|Out-Null;\
         $tag=\"$PID.\"+[guid]::NewGuid().ToString('N');\
         $tmp=Join-Path $d \"waku-daemon.tmp.$tag.exe\";\
         try{{\
           [Console]::Out.WriteLine('upload');[Console]::Out.Flush();\
           $fs=[IO.File]::Create($tmp);\
           try{{[Console]::OpenStandardInput().CopyTo($fs)}}finally{{$fs.Dispose()}};\
           $act='';$err='';\
           try{{\
             $r=Fp $tmp;$act=$r[0];$err=$r[1];\
             if(-not $act -and -not $err -and $r[2] -ne 0){{$err=\"exit code $($r[2])\"}}\
           }}catch{{$err=$_.Exception.Message}};\
           if(-not $act -and $err){{\
             $rs=($err -replace \"[\\r\\n]+\",' ').Trim();\
             if($rs.Length -gt 400){{$rs=$rs.Substring(0,400)}};\
             [Console]::Error.WriteLine(\"daemon_failed_to_run`t$rs\");exit 43\
           }};\
           if($act -ne $exp){{\
             $g=if($act){{$act}}else{{'no output'}};\
             [Console]::Error.WriteLine(\"fingerprint_mismatch`texpected $exp, got $g\");exit 43\
           }};\
           $lp=Join-Path $d 'install.lock';$lk=$null;\
           for($n=0;$n -lt 100;$n++){{try{{$lk=[IO.File]::Open($lp,'OpenOrCreate','ReadWrite','None');break}}catch{{Start-Sleep -Milliseconds 50}}}};\
           if(-not $lk){{throw 'timed out waiting for install lock'}};\
           try{{\
             Get-ChildItem -LiteralPath $d -Filter '{daemon_exe}.old.*' -ErrorAction SilentlyContinue|ForEach-Object{{try{{[IO.File]::Delete($_.FullName)}}catch{{}}}};\
             if([IO.File]::Exists($dst)){{\
               $old=\"$dst.old.$tag\";\
               [IO.File]::Move($dst,$old);\
               try{{[IO.File]::Delete($old)}}catch{{}}\
             }};\
             [IO.File]::Move($tmp,$dst)\
           }}finally{{$lk.Dispose()}};\
           [Console]::Out.WriteLine('installed');[Console]::Out.Flush()\
         }}finally{{if([IO.File]::Exists($tmp)){{try{{[IO.File]::Delete($tmp)}}catch{{}}}}}}"
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
        ConnectionKind::Ssh { .. } => deploy_ssh(connection, target, &artifact, force),
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
                explain_install_stderr(&wsl::decode_wsl_text(&output.stderr)).trim(),
            )
        ),
    }
}

enum StatusWaitError {
    Timeout,
    Disconnected(Option<String>),
}

fn wait_for_status_line(
    lines: &crossbeam_channel::Receiver<String>,
    expected: &[&str],
    timeout: Duration,
) -> Result<String, StatusWaitError> {
    let deadline = std::time::Instant::now() + timeout;
    let mut unexpected_line = None;
    loop {
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return Err(StatusWaitError::Timeout);
        };
        match lines.recv_timeout(remaining) {
            Ok(line) => {
                let trimmed = line.trim();
                if expected.contains(&trimmed) {
                    return Ok(trimmed.to_owned());
                }
                if !trimmed.is_empty() {
                    unexpected_line = Some(trimmed.to_owned());
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                return Err(StatusWaitError::Timeout);
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return Err(StatusWaitError::Disconnected(unexpected_line));
            }
        }
    }
}

fn deploy_ssh(
    connection: &Connection,
    target: RemoteTarget,
    artifact: &Artifact,
    force: bool,
) -> anyhow::Result<DeployOutcome> {
    let remote_command = match target.os {
        RemoteOs::Linux | RemoteOs::Darwin => {
            let script = install_script(&artifact.fingerprint, force, InstallSource::Stdin);
            ssh::wrap_posix_script(&script)
        }
        RemoteOs::Windows => {
            let script = windows_install_script(&artifact.fingerprint, force);
            ssh::wrap_powershell_script(&script)
        }
    };
    let mut command = ssh::command(connection)?;
    command
        .arg(remote_command)
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
        anyhow!(
            "{}",
            with_detail(message, &explain_install_stderr(&stderr.text()))
        )
    };

    let status_line = match wait_for_status_line(&lines, &["ready", "upload"], SSH_STATUS_TIMEOUT) {
        Ok(line) => line,
        Err(StatusWaitError::Timeout) => {
            return Err(failure(&mut child, "timed out preparing the remote daemon"));
        }
        Err(StatusWaitError::Disconnected(Some(unexpected))) => {
            drop(stdin);
            let _ = wait_with_deadline(&mut child, Duration::from_secs(5));
            let message = format!("unexpected remote install status {unexpected:?}");
            return Err(anyhow!(
                "{}",
                with_detail(&message, &explain_install_stderr(&stderr.text()))
            ));
        }
        Err(StatusWaitError::Disconnected(None)) => {
            drop(stdin);
            let _ = wait_with_deadline(&mut child, Duration::from_secs(5));
            return Err(anyhow!(
                "{}",
                with_detail(
                    &format!("could not prepare the daemon on {}", connection.name),
                    &explain_install_stderr(&stderr.text())
                )
            ));
        }
    };
    match status_line.as_str() {
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
    let installed = wait_for_status_line(&lines, &["installed"], SSH_INSTALL_TIMEOUT);
    if let Err(StatusWaitError::Timeout) = installed {
        return Err(failure(&mut child, "timed out installing waku-daemon"));
    }
    let status = wait_with_deadline(&mut child, Duration::from_secs(10));
    if installed.is_ok() && status.is_some_and(|status| status.success()) {
        return Ok(DeployOutcome::Installed);
    }
    Err(anyhow!(
        "{}",
        with_detail(
            "could not install waku-daemon",
            &explain_install_stderr(&stderr.text())
        )
    ))
}

/// Remote script that starts or reuses the host's daemon and relays it.
/// `replace_incompatible` lets it end a daemon speaking another protocol.
pub fn bridge_script(replace_incompatible: bool) -> String {
    let mut script = format!(
        "exec \"$HOME/{REMOTE_DIRECTORY}/{DAEMON_NAME}\" {}",
        waku_protocol::bridge::BRIDGE_SUBCOMMAND
    );
    if replace_incompatible {
        script.push(' ');
        script.push_str(waku_protocol::bridge::REPLACE_INCOMPATIBLE_FLAG);
    }
    script
}

fn windows_bridge_script(replace_incompatible: bool) -> String {
    let dir_rel = windows_remote_directory();
    let daemon_exe = RemoteOs::Windows.daemon_binary_name();
    let mut args = vec![waku_protocol::bridge::BRIDGE_SUBCOMMAND];
    if replace_incompatible {
        args.push(waku_protocol::bridge::REPLACE_INCOMPATIBLE_FLAG);
    }
    format!(
        "$h=if($env:USERPROFILE){{$env:USERPROFILE}}else{{$HOME}};\
         $dst=Join-Path (Join-Path $h '{dir_rel}') '{daemon_exe}';\
         & $dst {};exit $LASTEXITCODE",
        args.join(" ")
    )
}

/// Remote command string passed to `ssh` for starting `waku-daemon bridge`.
pub fn ssh_bridge_command(target: RemoteTarget, replace_incompatible: bool) -> String {
    match target.os {
        RemoteOs::Linux | RemoteOs::Darwin => {
            ssh::wrap_posix_script(&bridge_script(replace_incompatible))
        }
        RemoteOs::Windows => {
            ssh::wrap_powershell_script(&windows_bridge_script(replace_incompatible))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_parsing_accepts_linux_darwin_and_windows() {
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
        assert_eq!(
            parse_target_probe(b"Windows\tAMD64\n").unwrap(),
            RemoteTarget {
                os: RemoteOs::Windows,
                arch: RemoteArch::X86_64
            }
        );
        assert_eq!(
            parse_target_probe(b"profile banner\r\nWindows\tARM64\r\n").unwrap(),
            RemoteTarget {
                os: RemoteOs::Windows,
                arch: RemoteArch::Aarch64
            }
        );
        assert_eq!(
            parse_target_probe(b"MINGW64_NT-10.0-22631\tx86_64\n").unwrap(),
            RemoteTarget {
                os: RemoteOs::Windows,
                arch: RemoteArch::X86_64
            }
        );
        assert_eq!(
            parse_target_probe(b"MSYS_NT-10.0\tx86_64\n").unwrap(),
            RemoteTarget {
                os: RemoteOs::Windows,
                arch: RemoteArch::X86_64
            }
        );
        assert_eq!(
            parse_target_probe(b"CYGWIN_NT-10.0\tx86_64\n").unwrap(),
            RemoteTarget {
                os: RemoteOs::Windows,
                arch: RemoteArch::X86_64
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
        assert_eq!(
            RemoteTarget {
                os: RemoteOs::Windows,
                arch: RemoteArch::X86_64
            }
            .artifact_name(),
            "waku-daemon-windows-x86_64.exe"
        );
    }

    #[test]
    fn windows_bridge_command_targets_the_installed_daemon() {
        let script = windows_bridge_script(false);
        assert!(script.contains("waku-daemon.exe"));
        assert!(script.contains("bridge"));
        assert!(!script.contains(waku_protocol::bridge::REPLACE_INCOMPATIBLE_FLAG));

        let replacing = windows_bridge_script(true);
        assert!(replacing.contains(waku_protocol::bridge::REPLACE_INCOMPATIBLE_FLAG));

        let command = ssh_bridge_command(
            RemoteTarget {
                os: RemoteOs::Windows,
                arch: RemoteArch::X86_64,
            },
            false,
        );
        assert!(command.starts_with("powershell.exe "));
        assert!(command.contains("-EncodedCommand "));
        assert!(command.len() < 8191);
    }

    #[test]
    fn windows_install_command_fits_cmd_length_limit() {
        let wrapped =
            ssh::wrap_powershell_script(&windows_install_script("0123456789abcdef", false));
        assert!(
            wrapped.len() < 7500,
            "wrapped powershell command length {} exceeded 7500",
            wrapped.len()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_script_uploads_reuses_and_replaces_running_exe() {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let root = std::env::temp_dir().join(format!("waku-win-deploy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let src_exe = root.join("sample-daemon.exe");
        let fp = "0123456789abcdef";
        let csharp = format!(
            "using System;\nusing System.Threading;\npublic class P {{\n  public static void Main(string[] args) {{\n    if (args.Length > 0 && args[0] == \"--fingerprint\") {{ Console.WriteLine(\"{fp}\"); return; }}\n    Thread.Sleep(60000);\n  }}\n}}"
        );
        let compile_cmd = format!(
            "Add-Type -TypeDefinition '{}' -OutputAssembly '{}' -OutputType ConsoleApplication",
            csharp.replace('\'', "''"),
            src_exe.to_string_lossy().replace('\'', "''"),
        );
        let compiled = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-NoLogo",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &compile_cmd,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "Add-Type failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let bytes = std::fs::read(&src_exe).unwrap();

        let run_installer = |force: bool, send_bytes: bool| -> (String, String, bool) {
            let script = windows_install_script(fp, force);
            let wrapped = ssh::wrap_powershell_script(&script);
            let encoded = wrapped.split_whitespace().last().unwrap();
            let mut child = std::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-NoLogo",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-EncodedCommand",
                    encoded,
                ])
                .env("USERPROFILE", &root)
                .env("HOME", &root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let mut first = String::new();
            stdout.read_line(&mut first).unwrap();
            if send_bytes {
                assert_eq!(first.trim(), "upload");
                stdin.write_all(&bytes).unwrap();
                stdin.flush().unwrap();
            }
            drop(stdin);
            let mut rest = String::new();
            stdout.read_to_string(&mut rest).unwrap();
            let status = child.wait().unwrap();
            let mut err = String::new();
            if let Some(mut se) = child.stderr {
                let _ = se.read_to_string(&mut err);
            }
            (
                format!("{first}{rest}").trim().to_owned(),
                err,
                status.success(),
            )
        };

        // 1. Fresh upload -> upload + installed
        let (out1, err1, ok1) = run_installer(false, true);
        assert!(ok1, "fresh install failed: out={out1:?} err={err1:?}");
        assert_eq!(
            out1.lines().map(str::trim).collect::<Vec<_>>(),
            ["upload", "installed"]
        );

        let dst = root
            .join(windows_remote_directory())
            .join(RemoteOs::Windows.daemon_binary_name());
        assert!(dst.is_file());

        // 2. Reuse existing -> ready
        let (out2, err2, ok2) = run_installer(false, false);
        assert!(ok2, "reuse failed: out={out2:?} err={err2:?}");
        assert_eq!(out2.trim(), "ready");

        // 3. Force replace while destination .exe is actively running -> upload + installed
        let mut running = std::process::Command::new(&dst)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let (out3, err3, ok3) = run_installer(true, true);
        let _ = running.kill();
        let _ = running.wait();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            ok3,
            "forced replace while running failed: out={out3:?} err={err3:?}"
        );
        assert_eq!(
            out3.lines().map(str::trim).collect::<Vec<_>>(),
            ["upload", "installed"]
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

    #[test]
    fn install_scripts_report_why_a_daemon_cannot_run() {
        let script = install_script("0123456789abcdef", false, InstallSource::Stdin);
        // A binary that cannot execute — an old glibc host, say — must surface
        // the loader's message rather than being reported as a mismatch.
        let cannot_run = script.find("daemon_failed_to_run").unwrap();
        let mismatch = script.find("fingerprint_mismatch").unwrap();
        assert!(cannot_run < mismatch);
        assert!(script.contains("2>\"$err\""));
        assert!(script.contains("trap 'rm -f \"$tmp\" \"$err\"' EXIT"));
    }

    #[test]
    fn install_markers_are_explained_in_errors() {
        assert_eq!(
            explain_install_stderr(
                "daemon_failed_to_run\t/tmp/x: /lib/libc.so.6: version `GLIBC_2.38' not found\n"
            ),
            "the uploaded daemon could not run: /tmp/x: /lib/libc.so.6: version `GLIBC_2.38' not found\n"
        );
        assert_eq!(
            explain_install_stderr("fingerprint_mismatch\texpected abc, got no output\n"),
            "the uploaded daemon did not match this build: expected abc, got no output\n"
        );
        assert_eq!(
            explain_install_stderr("Permission denied\n"),
            "Permission denied\n"
        );
    }
}
