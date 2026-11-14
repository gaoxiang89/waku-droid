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
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const WSL_DEPLOY_TIMEOUT: Duration = Duration::from_secs(60);
const SSH_STATUS_TIMEOUT: Duration = Duration::from_secs(30);
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WindowsShell {
    #[default]
    Cmd,
    PowerShell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RemoteTarget {
    pub os: RemoteOs,
    pub arch: RemoteArch,
    pub windows_shell: WindowsShell,
}

impl RemoteTarget {
    pub const fn new(os: RemoteOs, arch: RemoteArch) -> Self {
        Self {
            os,
            arch,
            windows_shell: WindowsShell::Cmd,
        }
    }

    fn matches_platform(self, other: Self) -> bool {
        self.os == other.os && self.arch == other.arch
    }

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
        Some(Self::new(os, arch))
    }
}

fn parse_arch(arch: &str) -> anyhow::Result<RemoteArch> {
    match arch.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Ok(RemoteArch::X86_64),
        "aarch64" | "arm64" => Ok(RemoteArch::Aarch64),
        other => bail!("remote architecture {other:?} is not supported"),
    }
}

/// Parse `uname -s<TAB>uname -m` or the Windows PowerShell probe line
/// (`Windows<TAB>AMD64<TAB>cmd|ps`).
pub fn parse_target_probe(output: &[u8]) -> anyhow::Result<RemoteTarget> {
    let output = String::from_utf8_lossy(output);
    let line = output
        .lines()
        .map(str::trim)
        .rfind(|line| line.contains('\t'))
        .ok_or_else(|| anyhow!("invalid platform probe response {:?}", output.trim()))?;
    let mut parts = line.split('\t');
    let os_raw = parts.next().unwrap_or_default();
    let arch_raw = parts.next().unwrap_or_default();
    let shell_raw = parts.next().unwrap_or_default();
    let os = match os_raw.trim().to_ascii_lowercase().as_str() {
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
    let arch = parse_arch(arch_raw)?;
    let windows_shell = if os == RemoteOs::Windows
        && matches!(
            shell_raw.trim().to_ascii_lowercase().as_str(),
            "ps" | "pwsh" | "powershell"
        ) {
        WindowsShell::PowerShell
    } else {
        WindowsShell::Cmd
    };
    Ok(RemoteTarget {
        os,
        arch,
        windows_shell,
    })
}

const PROBE_SCRIPT: &str = "printf '%s\\t%s\\n' \"$(uname -s)\" \"$(uname -m)\"";
const WINDOWS_PROBE_SCRIPT: &str = "\
$ProgressPreference='SilentlyContinue';\
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }; \
$sh = 'cmd'; \
try { \
  $mo = [wmi]\"Win32_Process.Handle='$PID'\"; \
  if ([Diagnostics.Process]::GetProcessById([int]$mo.ParentProcessId).ProcessName -match '^(pwsh|powershell)$') { $sh = 'ps' } \
} catch {}; \
[Console]::Out.WriteLine(\"Windows`t$arch`t$sh\")";

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
            if RemoteTarget::host().is_some_and(|host| host.matches_platform(target)) {
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
        if RemoteTarget::host().is_some_and(|host| host.matches_platform(target)) {
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

fn wrap_windows_script(shell: WindowsShell, script: &str) -> String {
    match shell {
        WindowsShell::Cmd => ssh::wrap_powershell_script(script),
        WindowsShell::PowerShell => ssh::wrap_powershell_in_process(script),
    }
}

const WINDOWS_OPEN_STDIN_FN: &str = "\
function Stdin{\
  $t=[int].Assembly.GetType('Microsoft.Win32.Win32Native');\
  if(-not $t){$t=[Console].Assembly.GetType('Interop+Kernel32')};\
  $s=if($t){$t.GetMethod('GetStdHandle',[Reflection.BindingFlags]40).Invoke($null,@(-10))}else{(Add-Type '[DllImport(\"kernel32\")]public static extern IntPtr GetStdHandle(int n);' -Name I -Pass)::GetStdHandle(-10)};\
  $f=[Microsoft.Win32.SafeHandles.SafeFileHandle]::new($s,$false);\
  try{[IO.FileStream]::new($f,1,65536,$true)}catch{[IO.FileStream]::new($f,1,65536,$false)}\
};";

/// PowerShell 5.1+ / .NET 4.x installer matching [`install_script`].
/// Reads the exact expected byte count from redirected stdin via a raw
/// `SafeFileHandle`-backed `FileStream` (async first, synchronous fallback)
/// because Windows OpenSSH `sshd` opens child stdio pipes with
/// `FILE_FLAG_OVERLAPPED` (where `[Console]::OpenStandardInput()` hangs) and
/// does not reliably propagate channel EOF to child pipes.
fn windows_install_script(fingerprint: &str, size: u64, force: bool) -> String {
    let dir_rel = windows_remote_directory();
    let daemon_exe = RemoteOs::Windows.daemon_binary_name();
    let frc = if force { 1 } else { 0 };
    format!(
        "Set-StrictMode -Off;$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue';\
         $O=[Console]::Out;$E=[Console]::Error;$m=$null;\
         function _d($p){{if($p){{try{{[IO.File]::Delete($p)}}catch{{}}}}}};\
         function _q([int]$c){{_d $m;$O.Flush();$E.Flush();[Environment]::Exit($c)}};\
         try{{\
           $h=if($env:USERPROFILE){{$env:USERPROFILE}}else{{$HOME}};\
           $d=Join-Path $h '{dir_rel}';$dst=\"$d\\{daemon_exe}\";$exp='{fingerprint}';\
           {WINDOWS_OPEN_STDIN_FN}\
           function _f($p){{\
             $i=[Diagnostics.ProcessStartInfo]::new($p,'--fingerprint');\
             $i.UseShellExecute=$false;$i.RedirectStandardOutput=$true;$i.RedirectStandardError=$true;$i.CreateNoWindow=$true;\
             $x=[Diagnostics.Process]::Start($i);\
             $t=$x.StandardError.ReadToEndAsync();\
             $o=$x.StandardOutput.ReadToEnd().Trim();\
             $e=$t.Result.Trim();$x.WaitForExit();\
             @($o,$e,$x.ExitCode)\
           }};\
           if(-not {frc} -and [IO.File]::Exists($dst)){{\
             try{{if((_f $dst)[0] -eq $exp){{$O.WriteLine('ready');_q 0}}}}catch{{}}\
           }};\
           [void][IO.Directory]::CreateDirectory($d);\
           $g=\"$PID.$([guid]::NewGuid().Guid)\";$m=\"$d\\t.$g.tmp\";\
           $O.WriteLine('upload');$O.Flush();\
           $fs=[IO.File]::Create($m);$in=$null;$rem=[int64]{size};\
           try{{\
             $in=Stdin;$buf=[byte[]]::new(65536);\
             while($rem -gt 0){{\
               $n=$in.Read($buf,0,[Math]::Min(65536,$rem));\
               if($n -le 0){{break}};\
               $fs.Write($buf,0,$n);$rem-=$n\
             }}\
           }}finally{{if($in){{$in.Dispose()}};$fs.Dispose()}};\
           if($rem -gt 0){{throw \"incomplete ($rem B left)\"}};\
           $a='';$er='';\
           try{{\
             $v=_f $m;$a=$v[0];$er=$v[1];\
             if(-not $a -and -not $er -and $v[2] -ne 0){{$er=\"exit $($v[2])\"}}\
           }}catch{{$er=$_.Exception.Message}};\
           if(-not $a -and $er){{\
             $s=($er -replace \"[\\r\\n]+\",' ').Trim();\
             if($s.Length -gt 400){{$s=$s.Substring(0,400)}};\
             $E.WriteLine(\"daemon_failed_to_run`t$s\");_q 43\
           }};\
           if($a -ne $exp){{\
             $w=if($a){{$a}}else{{'no output'}};\
             $E.WriteLine(\"fingerprint_mismatch`texpected $exp, got $w\");_q 43\
           }};\
           $lp=\"$d\\install.lock\";$lk=$null;\
           for($n=0;$n -lt 100;$n++){{try{{$lk=[IO.File]::Open($lp,'OpenOrCreate','ReadWrite','None');break}}catch{{Start-Sleep -m 50}}}};\
           if(-not $lk){{throw 'lock timeout'}};\
           try{{\
             foreach($f in [IO.Directory]::GetFiles($d,'{daemon_exe}.old.*')){{_d $f}};\
             if([IO.File]::Exists($dst)){{$old=\"$dst.old.$g\";[IO.File]::Move($dst,$old);_d $old}};\
             [IO.File]::Move($m,$dst)\
           }}finally{{$lk.Dispose()}};\
           $O.WriteLine('installed');_q 0\
         }}catch{{$E.WriteLine($_.Exception.Message);_q 1}}"
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
            let script = windows_install_script(&artifact.fingerprint, artifact.size, force);
            wrap_windows_script(target.windows_shell, &script)
        }
    };
    let mut command = ssh::upload_command(connection)?;
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

    let install_deadline = std::time::Instant::now() + SSH_INSTALL_TIMEOUT;
    let (upload_tx, upload_rx) = crossbeam_channel::bounded(1);
    let artifact_path = artifact.path.clone();
    std::thread::Builder::new()
        .name("waku-ssh-upload".into())
        .spawn(move || {
            let result = (|| -> std::io::Result<u64> {
                let mut source = std::fs::File::open(&artifact_path)?;
                let copied = std::io::copy(&mut source, &mut stdin)?;
                stdin.flush()?;
                Ok(copied)
            })();
            drop(stdin);
            let _ = upload_tx.send(result);
        })
        .context("could not start the daemon uploader")?;
    let upload_timeout = install_deadline.saturating_duration_since(std::time::Instant::now());
    let uploaded = match upload_rx.recv_timeout(upload_timeout) {
        Ok(result) => result,
        Err(_) => return Err(failure(&mut child, "timed out uploading waku-daemon")),
    };
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
    let install_timeout = install_deadline.saturating_duration_since(std::time::Instant::now());
    let installed = wait_for_status_line(&lines, &["installed"], install_timeout);
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
        "Set-StrictMode -Off;$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue';\
         {WINDOWS_OPEN_STDIN_FN}\
         $h=if($env:USERPROFILE){{$env:USERPROFILE}}else{{$HOME}};\
         $dst=Join-Path (Join-Path $h '{dir_rel}') '{daemon_exe}';\
         $i=New-Object Diagnostics.ProcessStartInfo $dst,'{}';\
         $i.UseShellExecute=$false;$i.RedirectStandardInput=$true;$i.CreateNoWindow=$true;\
         $p=[Diagnostics.Process]::Start($i);\
         $in=$null;\
         try{{\
           $in=Stdin;$out=$p.StandardInput.BaseStream;\
           $buf=New-Object byte[] 65536;\
           $rt=$in.ReadAsync($buf,0,$buf.Length);\
           while(-not $p.HasExited){{\
             if($rt.Wait(100)){{\
               $n=$rt.GetAwaiter().GetResult();\
               if($n -le 0){{break}};\
               $out.Write($buf,0,$n);$out.Flush();\
               $rt=$in.ReadAsync($buf,0,$buf.Length)\
             }}\
           }}\
         }}catch{{}}finally{{if($in){{try{{$in.Dispose()}}catch{{}}}};try{{$p.StandardInput.Close()}}catch{{}}}};\
         $p.WaitForExit();\
         [Console]::Out.Flush();[Console]::Error.Flush();\
         [Environment]::Exit($p.ExitCode)",
        args.join(" ")
    )
}

/// Remote command string passed to `ssh` for starting `waku-daemon bridge`.
pub fn ssh_bridge_command(target: RemoteTarget, replace_incompatible: bool) -> String {
    match target.os {
        RemoteOs::Linux | RemoteOs::Darwin => {
            ssh::wrap_posix_script(&bridge_script(replace_incompatible))
        }
        RemoteOs::Windows => wrap_windows_script(
            target.windows_shell,
            &windows_bridge_script(replace_incompatible),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_parsing_accepts_linux_darwin_and_windows() {
        assert_eq!(
            parse_target_probe(b"Linux\tx86_64\n").unwrap(),
            RemoteTarget::new(RemoteOs::Linux, RemoteArch::X86_64)
        );
        assert_eq!(
            parse_target_probe(b"motd noise\nDarwin\tarm64\n").unwrap(),
            RemoteTarget::new(RemoteOs::Darwin, RemoteArch::Aarch64)
        );
        assert_eq!(
            parse_target_probe(b"Windows\tAMD64\n").unwrap(),
            RemoteTarget::new(RemoteOs::Windows, RemoteArch::X86_64)
        );
        assert_eq!(
            parse_target_probe(b"Windows\tAMD64\tcmd\r\n").unwrap(),
            RemoteTarget::new(RemoteOs::Windows, RemoteArch::X86_64)
        );
        assert_eq!(
            parse_target_probe(b"Windows\tAMD64\tps\r\n").unwrap(),
            RemoteTarget {
                os: RemoteOs::Windows,
                arch: RemoteArch::X86_64,
                windows_shell: WindowsShell::PowerShell,
            }
        );
        assert_eq!(
            parse_target_probe(b"profile banner\r\nWindows\tARM64\r\n").unwrap(),
            RemoteTarget::new(RemoteOs::Windows, RemoteArch::Aarch64)
        );
        assert_eq!(
            parse_target_probe(b"MINGW64_NT-10.0-22631\tx86_64\n").unwrap(),
            RemoteTarget::new(RemoteOs::Windows, RemoteArch::X86_64)
        );
        assert_eq!(
            parse_target_probe(b"MSYS_NT-10.0\tx86_64\n").unwrap(),
            RemoteTarget::new(RemoteOs::Windows, RemoteArch::X86_64)
        );
        assert_eq!(
            parse_target_probe(b"CYGWIN_NT-10.0\tx86_64\n").unwrap(),
            RemoteTarget::new(RemoteOs::Windows, RemoteArch::X86_64)
        );
        assert!(parse_target_probe(b"FreeBSD\tamd64\n").is_err());
        assert!(parse_target_probe(b"Linux\triscv64\n").is_err());
        assert!(parse_target_probe(b"garbage").is_err());
    }

    #[test]
    fn artifact_names_follow_the_bundle_layout() {
        assert_eq!(
            RemoteTarget::new(RemoteOs::Linux, RemoteArch::Aarch64).artifact_name(),
            "waku-daemon-linux-aarch64"
        );
        assert_eq!(
            RemoteTarget::new(RemoteOs::Windows, RemoteArch::X86_64).artifact_name(),
            "waku-daemon-windows-x86_64.exe"
        );
    }

    #[test]
    fn windows_bridge_command_targets_the_installed_daemon() {
        let script = windows_bridge_script(false);
        assert!(script.contains("waku-daemon.exe"));
        assert!(script.contains("bridge"));
        assert!(script.contains("ProcessStartInfo"));
        assert!(!script.contains(waku_protocol::bridge::REPLACE_INCOMPATIBLE_FLAG));

        let replacing = windows_bridge_script(true);
        assert!(replacing.contains(waku_protocol::bridge::REPLACE_INCOMPATIBLE_FLAG));

        let cmd_command = ssh_bridge_command(
            RemoteTarget::new(RemoteOs::Windows, RemoteArch::X86_64),
            false,
        );
        assert!(cmd_command.starts_with("powershell.exe "));
        assert!(cmd_command.contains("-EncodedCommand "));
        assert!(cmd_command.len() < 8191);

        let ps_command = ssh_bridge_command(
            RemoteTarget {
                os: RemoteOs::Windows,
                arch: RemoteArch::X86_64,
                windows_shell: WindowsShell::PowerShell,
            },
            false,
        );
        assert!(ps_command.starts_with("& ([scriptblock]::Create("));
        assert!(ps_command.len() < 8191);
    }

    #[test]
    fn windows_install_command_fits_cmd_length_limit() {
        let wrapped = ssh::wrap_powershell_script(&windows_install_script(
            "0123456789abcdef",
            28_922_880,
            false,
        ));
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
            let script = windows_install_script(fp, bytes.len() as u64, force);
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
        struct KillOnDrop(std::process::Child);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let running = KillOnDrop(
            std::process::Command::new(&dst)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .unwrap(),
        );
        std::thread::sleep(Duration::from_millis(200));
        let (out3, err3, ok3) = run_installer(true, true);
        drop(running);
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

    #[cfg(windows)]
    #[test]
    fn windows_install_script_uploads_over_overlapped_pipe() {
        use std::ffi::c_void;
        use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
        use std::os::windows::process::CommandExt as _;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const PIPE_ACCESS_OUTBOUND: u32 = 0x0000_0002;
        const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
        const PIPE_WAIT: u32 = 0x0000_0000;
        const GENERIC_READ: u32 = 0x8000_0000;
        const OPEN_EXISTING: u32 = 3;
        const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
        const INVALID_HANDLE_VALUE: *mut c_void = (-1_isize) as *mut c_void;

        #[repr(C)]
        struct SecurityAttributes {
            n_length: u32,
            lp_security_descriptor: *mut c_void,
            b_inherit_handle: i32,
        }

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateNamedPipeW(
                lp_name: *const u16,
                dw_open_mode: u32,
                dw_pipe_mode: u32,
                n_max_instances: u32,
                n_out_buffer_size: u32,
                n_in_buffer_size: u32,
                n_default_time_out: u32,
                lp_security_attributes: *const SecurityAttributes,
            ) -> *mut c_void;
            fn CreateFileW(
                lp_file_name: *const u16,
                dw_desired_access: u32,
                dw_share_mode: u32,
                lp_security_attributes: *const SecurityAttributes,
                dw_creation_disposition: u32,
                dw_flags_and_attributes: u32,
                h_template_file: *mut c_void,
            ) -> *mut c_void;
        }

        let root = std::env::temp_dir().join(format!("waku-win-ovl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let src_exe = root.join("sample-daemon.exe");
        let fp = "0123456789abcdef";
        let csharp = format!(
            "using System;\npublic class P {{\n  public static void Main(string[] args) {{\n    if (args.Length > 0 && args[0] == \"--fingerprint\") {{ Console.WriteLine(\"{fp}\"); }}\n  }}\n}}"
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
        assert!(compiled.status.success());
        let bytes = std::fs::read(&src_exe).unwrap();

        let pipe_name: Vec<u16> = format!(r"\\.\pipe\waku-ovl-{}", uuid::Uuid::new_v4())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let server_raw = unsafe {
            CreateNamedPipeW(
                pipe_name.as_ptr(),
                PIPE_ACCESS_OUTBOUND,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                65536,
                65536,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(server_raw, INVALID_HANDLE_VALUE);
        let server_handle = unsafe { OwnedHandle::from_raw_handle(server_raw as _) };
        let sa = SecurityAttributes {
            n_length: std::mem::size_of::<SecurityAttributes>() as u32,
            lp_security_descriptor: std::ptr::null_mut(),
            b_inherit_handle: 1,
        };
        let client_raw = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_READ,
                0,
                &sa,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(client_raw, INVALID_HANDLE_VALUE);
        let client_handle = unsafe { OwnedHandle::from_raw_handle(client_raw as _) };
        let mut writer = std::fs::File::from(server_handle);

        let script = windows_install_script(fp, bytes.len() as u64, false);
        let wrapped = ssh::wrap_powershell_script(&script);
        let encoded = wrapped.split_whitespace().last().unwrap();
        let mut child = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-NoLogo",
                "-OutputFormat",
                "Text",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
                encoded,
            ])
            .env("USERPROFILE", &root)
            .env("HOME", &root)
            .stdin(Stdio::from(client_handle))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();

        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut first = String::new();
        stdout.read_line(&mut first).unwrap();
        assert_eq!(first.trim(), "upload");
        writer.write_all(&bytes).unwrap();
        writer.flush().unwrap();
        // Keep the pipe open briefly after writing, matching sshd not sending
        // an immediate EOF, before closing cleanly.
        let mut rest = String::new();
        stdout.read_to_string(&mut rest).unwrap();
        drop(writer);
        let status = wait_with_deadline(&mut child, Duration::from_secs(15)).expect("timed out");
        let _ = std::fs::remove_dir_all(&root);
        assert!(status.success(), "overlapped upload failed");
        assert_eq!(rest.trim(), "installed");
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
