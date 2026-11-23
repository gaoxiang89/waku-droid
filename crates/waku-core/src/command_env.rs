//! Process environment capture and provider-safe command spawning.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{OnceLock, RwLock};

use std::fs::{self, OpenOptions};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::ffi::CStr;
#[cfg(unix)]
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const LOGIN_SHELL_ENV_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const INTERACTIVE_SHELL_ENV_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(unix)]
const SHELL_ENV_COMMAND: &str = "/usr/bin/env -0 > \"$WAKU_SHELL_ENV_CAPTURE_FILE\"";

type ShellEnvironment = Vec<(OsString, OsString)>;

static LOGIN_SHELL_ENVIRONMENT: OnceLock<RwLock<Option<ShellEnvironment>>> = OnceLock::new();
static SHELL_ENV_CAPTURE_ID: AtomicU64 = AtomicU64::new(0);

/// Build a command with the environment a terminal-launched Waku normally
/// inherits. Apps opened through LaunchServices do not receive variables
/// exported by the user's shell, including the PATH needed by script-based
/// CLIs whose shebang uses `/usr/bin/env` (for example, an npm-installed Codex
/// launcher needs `node`). Callers can add provider-specific overrides after
/// this.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    let search_path = child_search_path(Path::new(program));
    let mut command = plain_command(program);
    command.envs(shell_environment());
    if let Some(search_path) = search_path {
        command.env("PATH", search_path);
    }
    command
}

/// The `PATH` a provider CLI runs with: every directory Waku itself searched,
/// plus the one the binary was found in.
///
/// Detection resolves CLIs from more directories than the desktop process
/// inherits — a Bun or npm global prefix that the GUI `PATH` predates, for
/// example — so a CLI found in one of them has to *run* with them too.
/// Launcher-based installs depend on it and fail silently without it: Bun's
/// Windows `pi.EXE` is a shim that launches `bun.exe` from its own directory,
/// and both an npm `.cmd` shim and a `/usr/bin/env node` shebang need `node`
/// on the child's `PATH`. Detection still succeeds in that state — it only
/// looks for the file — so the provider shows up as installed while every
/// probe it runs comes back empty.
///
/// Windows needs this most: the login-shell probe there is best-effort — no
/// PowerShell may be present, and a profile can refuse to load — so a
/// GUI-launched Waku can still be running with only the `PATH` it inherited.
fn child_search_path(program: &Path) -> Option<OsString> {
    let mut directories = executable_search_paths();
    // Last, not first: an install outside the known prefixes still finds its
    // runtime, while the user's own `PATH` order decides everything else.
    directories.extend(
        program
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf),
    );
    let mut seen = HashSet::new();
    directories.retain(|directory| seen.insert(directory.clone()));
    std::env::join_paths(directories).ok()
}

/// A command that never flashes a console window.
///
/// Waku's Windows build is a GUI-subsystem binary with no console of its own,
/// so `CreateProcess` allocates one for every console child — `git`, a
/// provider CLI, the daemon — and flashes it on screen. `CREATE_NO_WINDOW`
/// keeps the child's console hidden while its pipes still work.
pub fn plain_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    detach_console(&mut command);
    command
}

fn detach_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// Spawn `command` with `SIGCHLD` unblocked in the child. On macOS, libdispatch
/// worker threads (which back GPUI's background executor) block `SIGCHLD`, and
/// a process spawned from such a thread inherits the blocked mask. That breaks
/// provider-side async process reapers. The caller's mask is restored as soon
/// as the child has been created.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    detach_console(command);
    with_sigchld_unblocked(|| command.spawn())
}

/// Spawn `command` through [`spawn`] and collect its output.
///
/// `Command::spawn` inherits standard streams by default, unlike
/// `Command::output`. Own all three streams here so callers keep the latter's
/// behavior while the signal mask is changed only for the spawn itself.
pub fn output(command: &mut Command) -> io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(command)?.wait_with_output()
}

/// Spawn `command` through [`spawn`] and collect its output, terminating and
/// reaping the child (and its process group / Windows job) if it does not exit
/// within `timeout`.
///
/// Stdout and stderr are drained concurrently on reader threads so a verbose
/// child cannot deadlock on full pipe buffers while waiting for exit.
pub(crate) fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);

    let mut child = spawn(command)?;
    let mut job = ChildJobGuard::adopt(&child);

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_reader = match std::thread::Builder::new()
        .name("waku-cmd-stdout".into())
        .spawn(move || read_pipe_to_end(stdout_pipe))
    {
        Ok(handle) => handle,
        Err(error) => {
            terminate_child_with_job(&mut child, &mut job);
            return Err(error);
        }
    };
    let stderr_reader = match std::thread::Builder::new()
        .name("waku-cmd-stderr".into())
        .spawn(move || read_pipe_to_end(stderr_pipe))
    {
        Ok(handle) => handle,
        Err(error) => {
            terminate_child_with_job(&mut child, &mut job);
            let _ = stdout_reader.join();
            return Err(error);
        }
    };

    let status = wait_for_child_with_job(&mut child, timeout, &mut job)?;
    // Tear down any stray descendants in the Windows job after the direct
    // child has exited so pipe readers cannot hang indefinitely.
    job.terminate();
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_pipe_to_end<R: io::Read>(pipe: Option<R>) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = io::Read::read_to_end(&mut pipe, &mut buffer);
    }
    buffer
}

struct ChildJobGuard {
    #[cfg(windows)]
    job: Option<crate::process_tree::KillOnCloseJob>,
}

impl ChildJobGuard {
    fn adopt(child: &Child) -> Self {
        #[cfg(windows)]
        {
            Self {
                job: crate::process_tree::KillOnCloseJob::adopt(child.id()).ok(),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Self {}
        }
    }

    fn terminate(&mut self) {
        #[cfg(windows)]
        {
            let _ = self.job.take();
        }
    }
}

/// Normalize a Waku-owned provider thread before a dependency spawns the child
/// internally. The ACP SDK owns its `async_process::Command`, so its dedicated
/// connection thread uses this once at startup instead of [`spawn`].
pub(crate) fn unblock_sigchld_for_current_thread() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let sigchld = sigchld_set()?;
        pthread_result(unsafe {
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &sigchld, std::ptr::null_mut())
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(())
    }
}

fn with_sigchld_unblocked<T>(operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    #[cfg(target_os = "macos")]
    let _restore = SignalMaskRestore::unblock_sigchld()?;
    operation()
}

#[cfg(target_os = "macos")]
fn sigchld_set() -> io::Result<libc::sigset_t> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    if unsafe { libc::sigemptyset(set.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut set = unsafe { set.assume_init() };
    if unsafe { libc::sigaddset(&mut set, libc::SIGCHLD) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(set)
}

#[cfg(target_os = "macos")]
fn pthread_result(status: libc::c_int) -> io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        // pthread APIs return the error number directly instead of setting
        // errno, so `last_os_error` would report unrelated thread state.
        Err(io::Error::from_raw_os_error(status))
    }
}

#[cfg(target_os = "macos")]
struct SignalMaskRestore(libc::sigset_t);

#[cfg(target_os = "macos")]
impl SignalMaskRestore {
    fn unblock_sigchld() -> io::Result<Self> {
        let sigchld = sigchld_set()?;
        let mut previous = MaybeUninit::<libc::sigset_t>::uninit();
        pthread_result(unsafe {
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &sigchld, previous.as_mut_ptr())
        })?;
        Ok(Self(unsafe { previous.assume_init() }))
    }
}

#[cfg(target_os = "macos")]
impl Drop for SignalMaskRestore {
    fn drop(&mut self) {
        let _ = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()) };
    }
}

pub fn find_executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return resolve_executable_file(candidate);
    }
    executable_search_paths()
        .into_iter()
        .find_map(|directory| resolve_executable_file(&directory.join(name)))
}

/// On Windows, try the path with each `PATHEXT` suffix first, then accept
/// `candidate` as it stands. Elsewhere the candidate is used directly.
///
/// Nothing is executable by name alone on Windows: an npm-installed provider
/// CLI lands as `claude.cmd` beside `claude.ps1`, and Bun and Cargo install
/// `.exe`. Trying `PATHEXT` in its configured order picks the same file the
/// shell would, and `std::process::Command` runs a `.cmd`/`.bat` through
/// `cmd.exe` for us.
///
/// A global npm install also writes an extensionless POSIX shim next to those
/// two, and `CreateProcess` cannot run it. Accepting the bare name before the
/// suffixes would hand back that shim and every launch of the provider would
/// fail, so the suffixed names have to win.
fn resolve_executable_file(candidate: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(stem) = candidate.file_name() {
        let stem = stem.to_owned();
        for extension in executable_extensions() {
            let mut name = stem.clone();
            name.push(&extension);
            let suffixed = candidate.with_file_name(name);
            if suffixed.is_file() {
                return Some(suffixed);
            }
        }
    }
    if candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    None
}

#[cfg(windows)]
fn executable_extensions() -> Vec<OsString> {
    const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

    let configured = std::env::var("PATHEXT").unwrap_or_default();
    let configured = if configured.trim().is_empty() {
        DEFAULT_PATHEXT
    } else {
        configured.as_str()
    };
    configured
        .split(';')
        .map(str::trim)
        .filter(|extension| extension.starts_with('.'))
        .map(OsString::from)
        .collect()
}

/// Resolve a user-supplied binary override: `~` expands to the home
/// directory, a path must point at an existing file, and a bare name searches
/// the same directories as [`find_executable`].
pub fn resolve_binary_override(spec: &str) -> Option<PathBuf> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if let Some(rest) = spec.strip_prefix("~/").or_else(|| spec.strip_prefix("~\\")) {
        let candidate = dirs::home_dir()?.join(rest);
        return candidate.is_file().then_some(candidate);
    }
    find_executable(spec)
}

/// Resolve an account-scoped configuration directory on the daemon host.
///
/// Expands `~` against the daemon host's home directory without requiring the
/// directory to exist yet — provider CLIs initialize missing directories on
/// first sign-in or launch.
pub fn resolve_config_dir(spec: &str) -> Option<PathBuf> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if spec == "~" {
        return dirs::home_dir();
    }
    if let Some(rest) = spec.strip_prefix("~/").or_else(|| spec.strip_prefix("~\\")) {
        return dirs::home_dir().map(|home| home.join(rest));
    }
    Some(PathBuf::from(spec))
}

const ANTIGRAVITY_ENV_ALLOWLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "AGY_ADC_AUTH",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_QUOTA_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
];
const MAX_ANTIGRAVITY_ENV_FILE_BYTES: u64 = 16 * 1024;

/// Derive process environment overrides for an Antigravity account root.
///
/// Inspects `<config_dir>/application_default_credentials.json` (enabling
/// Vertex ADC via `AGY_ADC_AUTH=true` + `GOOGLE_APPLICATION_CREDENTIALS`) and
/// allowlisted keys in `<config_dir>/.env` (`GEMINI_API_KEY`, ADC, and GCP
/// project/location overrides), with `.env` taking precedence over auto-detected
/// ADC keys.
pub fn antigravity_account_env(config_dir: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let adc_path = config_dir.join("application_default_credentials.json");
    if adc_path.is_file() {
        env.push(("AGY_ADC_AUTH".into(), "true".into()));
        env.push((
            "GOOGLE_APPLICATION_CREDENTIALS".into(),
            adc_path.to_string_lossy().into_owned(),
        ));
    }
    let env_file = config_dir.join(".env");
    if let Ok(meta) = std::fs::metadata(&env_file)
        && meta.is_file()
    {
        let limit = meta.len().min(MAX_ANTIGRAVITY_ENV_FILE_BYTES) as usize;
        if let Ok(file) = std::fs::File::open(&env_file) {
            use std::io::Read as _;
            let mut buf = Vec::with_capacity(limit);
            if file.take(MAX_ANTIGRAVITY_ENV_FILE_BYTES).read_to_end(&mut buf).is_ok() {
                let text = String::from_utf8_lossy(&buf);
                for raw_line in text.lines() {
                    let line = raw_line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
                    let Some((raw_key, raw_val)) = line.split_once('=') else {
                        continue;
                    };
                    let key = raw_key.trim();
                    if !ANTIGRAVITY_ENV_ALLOWLIST.contains(&key) {
                        continue;
                    }
                    let val = raw_val.trim();
                    let unquoted = if val.len() >= 2
                        && ((val.starts_with('"') && val.ends_with('"'))
                            || (val.starts_with('\'') && val.ends_with('\'')))
                    {
                        &val[1..val.len() - 1]
                    } else {
                        val
                    };
                    if let Some(existing) = env.iter_mut().find(|(k, _)| k == key) {
                        existing.1 = unquoted.to_owned();
                    } else {
                        env.push((key.to_owned(), unquoted.to_owned()));
                    }
                }
            }
        }
    }
    env
}

/// Returns `<config_dir>/settings.json` when an Antigravity account profile
/// carries its own CLI settings file.
pub fn antigravity_settings_arg(config_dir: Option<&Path>) -> Option<PathBuf> {
    let dir = config_dir?;
    let settings = dir.join("settings.json");
    settings.is_file().then_some(settings)
}

/// Inject the provider-specific configuration root environment variable when a
/// custom account directory is active.
///
/// When `config_dir` is `None` (the default account), no variable is set:
/// setting `CLAUDE_CONFIG_DIR` even to `~/.claude` makes Claude Code hash the
/// path for its macOS Keychain lookup and miss the default login item.
pub fn apply_provider_config_dir(
    command: &mut Command,
    provider: crate::model::ProviderKind,
    config_dir: Option<&Path>,
) {
    let Some(dir) = config_dir else {
        return;
    };
    match provider {
        crate::model::ProviderKind::Antigravity => {
            for (key, value) in antigravity_account_env(dir) {
                command.env(key, value);
            }
        }
        crate::model::ProviderKind::Claude => {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
        crate::model::ProviderKind::Codex => {
            command.env("CODEX_HOME", dir);
        }
        _ => {}
    }
}

pub fn executable_search_path() -> Option<std::ffi::OsString> {
    std::env::join_paths(executable_search_paths()).ok()
}

/// Resolve the user's interactive login-shell environment and cache it for
/// provider discovery and every later child process. This starts a shell and
/// must therefore only be called from a background thread.
#[cfg(unix)]
pub fn refresh_from_default_shell() -> bool {
    let Some(environment) = resolve_default_shell_environment(LOGIN_SHELL_ENV_TIMEOUT) else {
        return false;
    };
    *login_shell_environment()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(environment);
    true
}

/// Windows has no login shell, but it still needs this probe. A GUI-launched
/// Waku inherits explorer's `PATH`, which predates later installs, and the
/// package managers users actually add to it — fnm, Volta, nvm — extend
/// `PATH` only in the PowerShell profile, which never reaches the machine or
/// user environment block. Probe PowerShell with the profile loaded, capture
/// the fresh user and machine registry `PATH` values in the same run, and
/// cache the merge for provider discovery and every later child. This starts
/// PowerShell — which runs the user's profile — and must therefore only be
/// called from a background thread.
#[cfg(windows)]
pub fn refresh_from_default_shell() -> bool {
    let Some(environment) = resolve_windows_profile_environment(LOGIN_SHELL_ENV_TIMEOUT) else {
        return false;
    };
    *login_shell_environment()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(environment);
    true
}

/// Targets with neither probe keep the inherited environment.
#[cfg(not(any(unix, windows)))]
pub fn refresh_from_default_shell() -> bool {
    true
}

/// A hanging or exiting profile loses the whole probe run, so the
/// no-profile retry gets its own short leash.
#[cfg(windows)]
const WINDOWS_NO_PROFILE_ENV_TIMEOUT: Duration = Duration::from_secs(2);

/// The PowerShell script the probe runs. Each captured variable is written to
/// the capture file as NUL-separated `name=value` entries — the same format
/// as `env -0` on Unix — so profile noise on stdout cannot corrupt the
/// result. `[Environment]::GetEnvironmentVariable` reads the child's
/// *process* environment, which includes whatever the profile did to
/// `$env:PATH`; the two-argument form reads the fresh registry blocks, which
/// the inherited `PATH` may be older than. Use .NET methods directly so the
/// probe does not need to auto-load a PowerShell module for `New-Object`.
#[cfg(windows)]
const WINDOWS_ENV_CAPTURE_COMMAND: &str = "\
$ErrorActionPreference = 'Continue'
$entries = [System.Collections.Generic.List[string]]::new()
$path = [Environment]::GetEnvironmentVariable('PATH')
if ($path) { $entries.Add('PATH=' + $path) }
foreach ($entry in [Environment]::GetEnvironmentVariables().GetEnumerator()) {
  $name = [string]$entry.Key
  $value = [string]$entry.Value
  if ($name -and $value -and $name -inotlike 'PATH' -and $name -inotlike 'WAKU_*') {
    $entries.Add($name + '=' + $value)
  }
}
foreach ($target in @('User', 'Machine')) {
  $value = [Environment]::GetEnvironmentVariable('PATH', $target)
  if ($value) { $entries.Add('WAKU_' + $target.ToUpper() + '_PATH=' + [Environment]::ExpandEnvironmentVariables($value)) }
}
[IO.File]::WriteAllText($env:WAKU_SHELL_ENV_CAPTURE_FILE, [string]::Join([string][char]0, $entries))
";

/// PowerShell 7 first, then the in-box Windows PowerShell. `cmd.exe` is not a
/// candidate: it has no profile to load and no registry API.
#[cfg(windows)]
fn windows_powershell_candidates() -> Vec<PathBuf> {
    ["pwsh.exe", "powershell.exe"]
        .into_iter()
        .filter_map(find_executable)
        .collect()
}

#[cfg(windows)]
fn resolve_windows_profile_environment(timeout: Duration) -> Option<ShellEnvironment> {
    let started_at = Instant::now();
    // Load each shell's profile first — fnm/Volta/nvm extend PATH there and
    // nowhere else. The registry PATH is captured by the same run, so a
    // profile that merely lacks PATH still succeeds via the registry values.
    for shell in windows_powershell_candidates() {
        let remaining = timeout.checked_sub(started_at.elapsed())?;
        if remaining.is_zero() {
            return None;
        }
        if let Some(environment) = capture_windows_environment(&shell, true, remaining) {
            if let Some(environment) = merge_windows_environment(environment) {
                return Some(environment);
            }
        }
    }
    // A hanging or exiting profile loses the whole run. Retry without it on a
    // short leash; the registry PATH alone is still worth the spawn.
    for shell in windows_powershell_candidates() {
        if let Some(environment) =
            capture_windows_environment(&shell, false, WINDOWS_NO_PROFILE_ENV_TIMEOUT)
        {
            if let Some(environment) = merge_windows_environment(environment) {
                return Some(environment);
            }
        }
    }
    None
}

#[cfg(windows)]
fn capture_windows_environment(
    shell: &Path,
    load_profile: bool,
    timeout: Duration,
) -> Option<ShellEnvironment> {
    let capture = ShellEnvironmentCapture::create()?;
    let mut command = Command::new(shell);
    command.arg("-NoLogo").arg("-NonInteractive");
    if !load_profile {
        command.arg("-NoProfile");
    }
    command
        .arg("-Command")
        .arg(WINDOWS_ENV_CAPTURE_COMMAND)
        .env("WAKU_SHELL_ENV_CAPTURE_FILE", capture.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = spawn(&mut command).ok()?;
    if !wait_for_child(&mut child, timeout).ok()?.success() {
        return None;
    }
    parse_shell_environment(&fs::read(capture.path()).ok()?)
}

/// Combine the profile variables with the fresh registry `PATH` values into
/// the cached environment. The profile `PATH` comes first — it is the user's
/// own order — then the user registry `PATH`, then the machine one; the
/// inherited `PATH` is appended afterwards by [`search_paths_from`], and a
/// child `PATH` built by [`child_search_path`] keeps that order. fnm's
/// variables ride along so its shims can resolve their Node installation.
/// `PATH` matching is case-insensitive on Windows, so deduplicate that way.
#[cfg(windows)]
fn merge_windows_environment(mut environment: ShellEnvironment) -> Option<ShellEnvironment> {
    let mut directories = Vec::new();
    for name in ["PATH", "WAKU_USER_PATH", "WAKU_MACHINE_PATH"] {
        if let Some(value) = take_environment_variable(&mut environment, name) {
            directories.extend(std::env::split_paths(&value));
        }
    }
    let mut seen = HashSet::new();
    directories
        .retain(|directory| seen.insert(directory.as_os_str().to_string_lossy().to_lowercase()));
    if directories.is_empty() {
        return None;
    }
    let path = std::env::join_paths(directories).ok()?;
    environment.insert(0, (OsString::from("PATH"), path));
    Some(environment)
}

#[cfg(windows)]
fn take_environment_variable(environment: &mut ShellEnvironment, name: &str) -> Option<OsString> {
    let position = environment
        .iter()
        .position(|(candidate, _)| candidate.to_string_lossy().eq_ignore_ascii_case(name))?;
    Some(environment.remove(position).1)
}

fn executable_search_paths() -> Vec<PathBuf> {
    search_paths_from(
        cached_login_shell_variable(OsStr::new("PATH")).as_deref(),
        std::env::var_os("PATH").as_deref(),
        dirs::home_dir().as_deref(),
    )
}

fn login_shell_environment() -> &'static RwLock<Option<ShellEnvironment>> {
    LOGIN_SHELL_ENVIRONMENT.get_or_init(|| RwLock::new(None))
}

pub(crate) fn shell_environment() -> ShellEnvironment {
    login_shell_environment()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .unwrap_or_default()
}

/// Look up an environment variable, preferring the current process environment
/// (so tests and explicit launch overrides win) and falling back to the cached
/// login-shell environment when it has already been populated.
///
/// Never spawns a shell: if [`refresh_from_default_shell`] has not run yet,
/// only `std::env::var_os` is consulted.
pub fn env_var_os(key: &str) -> Option<OsString> {
    if let Some(value) = std::env::var_os(key) {
        return Some(value);
    }
    cached_login_shell_variable(OsStr::new(key))
}

fn env_var_name_matches(candidate: &OsStr, target: &OsStr) -> bool {
    #[cfg(windows)]
    {
        candidate
            .as_encoded_bytes()
            .eq_ignore_ascii_case(target.as_encoded_bytes())
    }
    #[cfg(not(windows))]
    {
        candidate == target
    }
}

fn lookup_env_var_os(
    key: &OsStr,
    process_value: Option<OsString>,
    shell_env: Option<&[(OsString, OsString)]>,
) -> Option<OsString> {
    process_value.or_else(|| {
        shell_env?
            .iter()
            .find(|(candidate, _)| env_var_name_matches(candidate, key))
            .map(|(_, value)| value.clone())
    })
}

fn cached_login_shell_variable(name: &OsStr) -> Option<OsString> {
    let lock = LOGIN_SHELL_ENVIRONMENT.get()?;
    let guard = lock.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    lookup_env_var_os(name, None, guard.as_deref())
}

fn search_paths_from(
    shell_path: Option<&OsStr>,
    inherited_path: Option<&OsStr>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    for path in [shell_path, inherited_path].into_iter().flatten() {
        directories.extend(std::env::split_paths(path));
    }
    if let Some(home) = home {
        directories.extend(user_tool_directories(home));
    }
    directories.extend(system_tool_directories());

    let mut seen = HashSet::new();
    directories.retain(|directory| seen.insert(directory.clone()));
    directories
}

/// Where per-user package managers put the provider CLIs, in case the
/// inherited `PATH` predates the install.
#[cfg(not(windows))]
fn user_tool_directories(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".local/bin"),
        home.join(".bun/bin"),
        home.join(".cargo/bin"),
        home.join(".local/share/mise/shims"),
        home.join(".volta/bin"),
    ]
}

#[cfg(windows)]
fn user_tool_directories(home: &Path) -> Vec<PathBuf> {
    let mut directories = vec![
        // npm's global prefix, where a `claude.cmd` shim lands.
        home.join("AppData/Roaming/npm"),
        home.join(".bun/bin"),
        home.join(".cargo/bin"),
        home.join("scoop/shims"),
        home.join("AppData/Local/Microsoft/WindowsApps"),
        home.join(".local/bin"),
    ];
    // Antigravity (`agy`), Volta, pnpm, and the user-scoped Node installer
    // default to LocalAppData (the same list T3 Code probes); it can be
    // redirected away from home.
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let local_app_data = PathBuf::from(local_app_data);
        directories.push(local_app_data.join("agy/bin"));
        directories.push(local_app_data.join("Volta/bin"));
        directories.push(local_app_data.join("pnpm"));
        directories.push(local_app_data.join("Programs/nodejs"));
    } else {
        directories.push(home.join("AppData/Local/agy/bin"));
    }
    directories
}

#[cfg(not(windows))]
fn system_tool_directories() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ]
}

#[cfg(windows)]
fn system_tool_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        directories.push(PathBuf::from(program_files).join("nodejs"));
    }
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        let system32 = PathBuf::from(system_root).join("System32");
        directories.push(system32.join("WindowsPowerShell/v1.0"));
        directories.push(system32);
    }
    directories
}

#[cfg(unix)]
fn resolve_default_shell_environment(timeout: Duration) -> Option<ShellEnvironment> {
    let started_at = Instant::now();
    for shell in default_shell_candidates() {
        for shell_args in [["-i", "-l", "-c"].as_slice(), ["-l", "-c"].as_slice()] {
            let remaining = timeout.checked_sub(started_at.elapsed())?;
            if remaining.is_zero() {
                return None;
            }
            // Leave part of the total budget for a non-interactive login-shell
            // fallback when an interactive rc file blocks or exits early.
            let attempt_timeout = if shell_args.first() == Some(&"-i") {
                remaining.min(INTERACTIVE_SHELL_ENV_TIMEOUT)
            } else {
                remaining
            };
            if let Some(environment) =
                capture_shell_environment(&shell, shell_args, attempt_timeout)
            {
                return Some(environment);
            }
        }
    }
    None
}

fn default_shell_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    // `SHELL` is a POSIX convention. On Windows it is set only by ported
    // toolchains such as Git Bash, and usually to an MSYS path that Win32
    // cannot open, so the native shells are resolved instead.
    #[cfg(unix)]
    {
        if let Some(shell) = std::env::var_os("SHELL").filter(|shell| !shell.is_empty()) {
            candidates.push(PathBuf::from(shell));
        }
        if let Some(shell) = account_default_shell() {
            candidates.push(shell);
        }
    }
    #[cfg(target_os = "macos")]
    candidates.push(PathBuf::from("/bin/zsh"));
    #[cfg(target_os = "linux")]
    candidates.extend([PathBuf::from("/bin/bash"), PathBuf::from("/bin/sh")]);
    #[cfg(windows)]
    candidates.extend(windows_shell_candidates());

    let mut seen = HashSet::new();
    candidates.retain(|shell| seen.insert(shell.clone()));
    candidates
}

/// PowerShell 7 first, then the in-box Windows PowerShell, then whatever
/// `COMSPEC` names — the same order a Windows Terminal profile list uses.
#[cfg(windows)]
fn windows_shell_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for shell in ["pwsh.exe", "powershell.exe"] {
        candidates.extend(find_executable(shell));
    }
    if let Some(comspec) = std::env::var_os("COMSPEC").filter(|comspec| !comspec.is_empty()) {
        candidates.push(PathBuf::from(comspec));
    }
    candidates
}

/// Pick the user's configured login shell for an interactive terminal, with a
/// platform shell as a final fallback when desktop launchers omit `SHELL`.
pub fn default_terminal_shell() -> PathBuf {
    let mut candidates = Vec::new();
    // The shell the user chose outranks whatever `SHELL` was inherited from;
    // see the note in waku-client's `unix_terminal_shell_candidates`. The
    // environment probe below keeps its own order, since it wants the shell
    // whose rc files produced this process's `PATH`.
    #[cfg(unix)]
    candidates.extend(account_default_shell());
    candidates.extend(default_shell_candidates());

    candidates
        .into_iter()
        .find(|shell| shell.is_file())
        .unwrap_or_else(default_terminal_shell_fallback)
}

#[cfg(not(windows))]
fn default_terminal_shell_fallback() -> PathBuf {
    PathBuf::from("/bin/sh")
}

#[cfg(windows)]
fn default_terminal_shell_fallback() -> PathBuf {
    PathBuf::from("cmd.exe")
}

/// The arguments that open `shell` the way the user's own terminal would.
///
/// A POSIX shell needs `-l` so the login files that set `PATH` are read.
/// Windows applies the environment before the process starts, so there is no
/// login mode to ask for — PowerShell's `-Login` exists on Unix hosts only,
/// and passing it here is an error. Suppressing its banner is the one thing
/// worth saying.
pub fn default_terminal_shell_args(shell: &Path) -> Vec<String> {
    #[cfg(not(windows))]
    {
        let _ = shell;
        vec!["-l".to_owned()]
    }
    #[cfg(windows)]
    {
        let is_powershell = shell
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| {
                stem.eq_ignore_ascii_case("pwsh") || stem.eq_ignore_ascii_case("powershell")
            });
        if is_powershell {
            vec!["-NoLogo".to_owned()]
        } else {
            Vec::new()
        }
    }
}

#[cfg(unix)]
fn account_default_shell() -> Option<PathBuf> {
    let suggested_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer_size = if suggested_size > 0 {
        suggested_size as usize
    } else {
        16 * 1024
    };
    loop {
        let mut passwd = MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_size];
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer_size < 1024 * 1024 {
            buffer_size *= 2;
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let shell = unsafe { (*result).pw_shell };
        if shell.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(shell) }.to_bytes();
        return (!bytes.is_empty()).then(|| PathBuf::from(OsString::from_vec(bytes.to_vec())));
    }
}

#[cfg(unix)]
fn capture_shell_environment(
    shell: &Path,
    shell_args: &[&str],
    timeout: Duration,
) -> Option<ShellEnvironment> {
    let capture = ShellEnvironmentCapture::create()?;
    let mut command = Command::new(shell);
    command
        .args(shell_args)
        .arg(SHELL_ENV_COMMAND)
        .env("WAKU_SHELL_ENV_CAPTURE_FILE", capture.path())
        // Match shell-env's safeguards for common interactive zsh setups so
        // an update prompt or tmux auto-start cannot consume the probe budget.
        .env("DISABLE_AUTO_UPDATE", "true")
        .env("ZSH_TMUX_AUTOSTARTED", "true")
        .env("ZSH_TMUX_AUTOSTART", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);

    let mut child = spawn(&mut command).ok()?;
    if !wait_for_child(&mut child, timeout).ok()?.success() {
        return None;
    }
    parse_shell_environment(&fs::read(capture.path()).ok()?)
}

fn parse_shell_environment(bytes: &[u8]) -> Option<ShellEnvironment> {
    let environment = bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let separator = entry.iter().position(|byte| *byte == b'=')?;
            if separator == 0 {
                return None;
            }
            let name = os_string_from_bytes(&entry[..separator])?;
            if is_shell_capture_variable(&name) {
                return None;
            }
            let value = os_string_from_bytes(&entry[separator + 1..])?;
            Some((name, value))
        })
        .collect::<Vec<_>>();
    (!environment.is_empty()).then_some(environment)
}

fn is_waku_env_variable(name: &OsStr) -> bool {
    name.as_encoded_bytes()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"WAKU_"))
}

fn is_shell_capture_variable(name: &OsStr) -> bool {
    if is_waku_env_variable(name) {
        // The Windows PowerShell capture script emits these two internal
        // sentinels for `merge_windows_environment` to consume and remove.
        #[cfg(windows)]
        if name
            .as_encoded_bytes()
            .eq_ignore_ascii_case(b"WAKU_USER_PATH")
            || name
                .as_encoded_bytes()
                .eq_ignore_ascii_case(b"WAKU_MACHINE_PATH")
        {
            return false;
        }
        return true;
    }
    [
        "DISABLE_AUTO_UPDATE",
        "ZSH_TMUX_AUTOSTARTED",
        "ZSH_TMUX_AUTOSTART",
    ]
    .into_iter()
    .any(|candidate| name == OsStr::new(candidate))
}

fn os_string_from_bytes(bytes: &[u8]) -> Option<OsString> {
    #[cfg(unix)]
    {
        Some(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(windows)]
    {
        // The Windows probe writes UTF-8; PowerShell encodes .NET strings
        // exactly, so lossy decoding loses nothing.
        Some(String::from_utf8_lossy(bytes).into_owned().into())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = bytes;
        None
    }
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let mut job = ChildJobGuard::adopt(child);
    wait_for_child_with_job(child, timeout, &mut job)
}

fn wait_for_child_with_job(
    child: &mut Child,
    timeout: Duration,
    job: &mut ChildJobGuard,
) -> io::Result<ExitStatus> {
    let started_at = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started_at.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                terminate_child_with_job(child, job);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("child process did not exit within {timeout:?}"),
                ));
            }
            Err(error) => {
                terminate_child_with_job(child, job);
                return Err(error);
            }
        }
    }
}

/// Terminate a child spawned as the leader of its own Unix process group
/// (`CommandExt::process_group(0)`), sweeping any grandchild forked mid-signal
/// before reaping the child.
///
/// On Unix (notably macOS XNU), a single `kill(-pid, SIGKILL)` can race with a
/// shell child that is mid-`fork`/`posix_spawn`, leaving the newly created
/// grandchild alive and holding inherited pipes open. Waiting for the direct
/// child with `waitid(..., WEXITED | WNOWAIT)` guarantees it can no longer fork
/// while keeping its PID un-reaped (so it cannot be recycled yet), allowing a
/// second `kill(-pid, SIGKILL)` to sweep the group safely before `child.wait()`
/// reaps it.
#[cfg(unix)]
pub(crate) fn terminate_process_group_child(child: &mut Child) {
    let pgid = i32::try_from(child.id()).ok().filter(|&pid| pid > 0);
    if let Some(pid) = pgid {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    if let Some(pid) = pgid {
        let mut waited_unreaped = false;
        let mut siginfo = MaybeUninit::<libc::siginfo_t>::zeroed();
        loop {
            let status = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    siginfo.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if status == 0 {
                waited_unreaped = true;
                break;
            }
            if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
        if waited_unreaped {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.wait();
}

fn terminate_child_with_job(child: &mut Child, job: &mut ChildJobGuard) {
    #[cfg(unix)]
    {
        job.terminate();
        terminate_process_group_child(child);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        job.terminate();
        let _ = child.wait();
    }
}

struct ShellEnvironmentCapture(PathBuf);

impl ShellEnvironmentCapture {
    fn create() -> Option<Self> {
        for _ in 0..16 {
            let id = SHELL_ENV_CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!(".waku-shell-env-{}-{id}", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&path) {
                Ok(_) => return Some(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            }
        }
        None
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ShellEnvironmentCapture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn command_search_path(command: &Command) -> Vec<PathBuf> {
        let path = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("PATH"))
            .and_then(|(_, value)| value)
            .expect("a provider command sets PATH for its child");
        std::env::split_paths(path).collect()
    }

    /// A CLI resolved from a directory the desktop `PATH` never had must run
    /// with that directory too, or its own launcher — a Bun shim, an npm
    /// `.cmd`, an `env node` shebang — cannot find its runtime.
    #[test]
    fn a_provider_cli_runs_with_the_directories_detection_searched() {
        #[cfg(windows)]
        let program = PathBuf::from("C:\\waku-fixture\\bin\\pi.exe");
        #[cfg(not(windows))]
        let program = PathBuf::from("/opt/waku-fixture/bin/pi");

        let directories = command_search_path(&command(&program));

        assert!(directories.contains(&program.parent().expect("fixture parent").to_path_buf()));
        for searched in executable_search_paths() {
            assert!(
                directories.contains(&searched),
                "{} is searched during detection but missing from the child PATH",
                searched.display()
            );
        }
    }

    #[test]
    fn a_bare_program_name_contributes_no_search_directory() {
        let directories = command_search_path(&command("git"));

        assert_eq!(directories, executable_search_paths());
    }

    #[cfg(unix)]
    #[test]
    fn output_captures_stdout_and_stderr() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf stdout; printf stderr >&2"]);

        let output = output(&mut command).expect("command should run");

        assert_eq!(output.stdout, b"stdout");
        assert_eq!(output.stderr, b"stderr");
    }

    #[test]
    fn child_wait_distinguishes_exit_status_from_timeout_and_reaps_the_child() {
        const CHILD_MODE: &str = "WAKU_CHILD_WAIT_TEST_MODE";
        if let Some(mode) = std::env::var_os(CHILD_MODE) {
            match mode.to_str().expect("child mode") {
                "success" => std::process::exit(0),
                "failure" => std::process::exit(23),
                "hang" => loop {
                    std::thread::park();
                },
                _ => panic!("unknown child mode"),
            }
        }

        for mode in ["success", "failure", "hang"] {
            let mut command = Command::new(std::env::current_exe().expect("test executable"));
            command
                .args([
                    "--exact",
                    "command_env::tests::child_wait_distinguishes_exit_status_from_timeout_and_reaps_the_child",
                ])
                .env(CHILD_MODE, mode)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            command.process_group(0);
            let mut child = spawn(&mut command).expect("spawn child fixture");
            if mode == "hang" {
                let error = wait_for_child(&mut child, Duration::ZERO).expect_err("timeout");
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                assert!(
                    !child
                        .try_wait()
                        .expect("read killed child status")
                        .unwrap()
                        .success()
                );
            } else {
                let status = wait_for_child(&mut child, Duration::from_secs(60))
                    .expect("child fixture should exit");
                assert_eq!(status.code(), Some(if mode == "success" { 0 } else { 23 }));
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn sigchld_is_blocked() -> io::Result<bool> {
        let mut current = MaybeUninit::<libc::sigset_t>::uninit();
        pthread_result(unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), current.as_mut_ptr())
        })?;
        Ok(unsafe { libc::sigismember(current.as_ptr(), libc::SIGCHLD) } == 1)
    }

    #[cfg(target_os = "macos")]
    fn block_sigchld() -> io::Result<SignalMaskRestore> {
        let sigchld = sigchld_set()?;
        let mut previous = MaybeUninit::<libc::sigset_t>::uninit();
        pthread_result(unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &sigchld, previous.as_mut_ptr())
        })?;
        Ok(SignalMaskRestore(unsafe { previous.assume_init() }))
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn spawn_unblocks_sigchld_in_the_child_and_restores_the_caller() {
        if std::env::var_os("WAKU_SIGCHLD_CHILD_PROBE").is_some() {
            assert!(!sigchld_is_blocked().expect("read child signal mask"));
            return;
        }

        let _restore_original = block_sigchld().expect("block SIGCHLD for the fixture");
        assert!(sigchld_is_blocked().expect("read blocked parent mask"));

        let mut command = Command::new(std::env::current_exe().expect("resolve test executable"));
        command
            .args([
                "--exact",
                "command_env::tests::spawn_unblocks_sigchld_in_the_child_and_restores_the_caller",
                "--nocapture",
            ])
            .env("WAKU_SIGCHLD_CHILD_PROBE", "1");
        let output = output(&mut command).expect("spawn child signal probe");

        assert!(
            output.status.success(),
            "child signal probe failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(sigchld_is_blocked().expect("read restored parent mask"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dedicated_provider_thread_can_normalize_sigchld() {
        let _restore_original = block_sigchld().expect("block SIGCHLD for the fixture");

        unblock_sigchld_for_current_thread().expect("unblock provider thread");

        assert!(!sigchld_is_blocked().expect("read normalized signal mask"));
    }

    #[cfg(unix)]
    #[test]
    fn launch_services_path_is_extended_for_script_based_clis() {
        let home = Path::new("/Users/example");
        let paths = search_paths_from(None, Some(OsStr::new("/usr/bin:/bin")), Some(home));

        assert_eq!(paths[0], PathBuf::from("/usr/bin"));
        assert_eq!(paths[1], PathBuf::from("/bin"));
        assert!(paths.contains(&home.join(".bun/bin")));
        assert!(paths.contains(&home.join(".local/share/mise/shims")));
        assert!(paths.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert_eq!(
            paths
                .iter()
                .filter(|path| *path == Path::new("/bin"))
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_precedes_the_inherited_desktop_path() {
        let paths = search_paths_from(
            Some(OsStr::new(
                "/Users/example/.nvm/versions/node/v22.0.0/bin:/Users/example/.local/share/fnm/current/bin",
            )),
            Some(OsStr::new("/usr/bin:/bin")),
            None,
        );

        assert_eq!(
            paths[..4],
            [
                PathBuf::from("/Users/example/.nvm/versions/node/v22.0.0/bin"),
                PathBuf::from("/Users/example/.local/share/fnm/current/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn desktop_path_is_extended_with_the_windows_package_manager_prefixes() {
        let home = Path::new("C:\\Users\\example");
        let paths = search_paths_from(
            None,
            Some(OsStr::new("C:\\Windows\\System32;C:\\Windows")),
            Some(home),
        );

        assert_eq!(paths[0], PathBuf::from("C:\\Windows\\System32"));
        assert_eq!(paths[1], PathBuf::from("C:\\Windows"));
        assert!(paths.contains(&home.join("AppData/Roaming/npm")));
        assert!(paths.contains(&home.join(".bun/bin")));
        assert!(paths.contains(&home.join("scoop/shims")));
        assert!(paths.contains(&home.join(".local/bin")));
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let local_app_data = PathBuf::from(local_app_data);
            assert!(paths.contains(&local_app_data.join("agy/bin")));
            assert!(paths.contains(&local_app_data.join("Volta/bin")));
            assert!(paths.contains(&local_app_data.join("pnpm")));
            assert!(paths.contains(&local_app_data.join("Programs/nodejs")));
        } else {
            assert!(paths.contains(&home.join("AppData/Local/agy/bin")));
        }
        assert_eq!(
            paths
                .iter()
                .filter(|path| *path == Path::new("C:\\Windows"))
                .count(),
            1
        );
    }

    /// The probe script must execute as written against the in-box Windows
    /// PowerShell: `-NoProfile` leaves the child `PATH` untouched, so the
    /// captured value is exactly the one this process inherited.
    #[cfg(windows)]
    #[test]
    fn windows_environment_probe_captures_the_inherited_path_without_a_profile() {
        let capture = ShellEnvironmentCapture::create().expect("create capture file");
        let diagnostics = ShellEnvironmentCapture::create().expect("create diagnostics file");
        let output = fs::File::create(diagnostics.path()).expect("open diagnostics file");
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .arg(WINDOWS_ENV_CAPTURE_COMMAND)
            .env("WAKU_SHELL_ENV_CAPTURE_FILE", capture.path())
            .stdin(Stdio::null())
            // A file preserves diagnostics without a pipe buffer blocking the probe.
            .stdout(output.try_clone().expect("clone diagnostics handle"))
            .stderr(output);
        let mut child = spawn(&mut command).expect("spawn PowerShell probe");
        let started_at = Instant::now();
        // This checks the script's output, not PowerShell's cold-start latency
        // on a busy CI runner. Production probes keep their short deadlines.
        let result = wait_for_child(&mut child, Duration::from_secs(60));
        assert!(
            matches!(&result, Ok(status) if status.success()),
            "PowerShell probe failed after {:?}: {result:?}\n{}",
            started_at.elapsed(),
            String::from_utf8_lossy(&fs::read(diagnostics.path()).expect("read probe diagnostics"))
        );
        let environment =
            parse_shell_environment(&fs::read(capture.path()).expect("capture file written"))
                .expect("parse captured environment");
        let path = environment
            .iter()
            .find(|(name, _)| name == OsStr::new("PATH"))
            .map(|(_, value)| value.clone())
            .expect("captured PATH");
        assert_eq!(path, std::env::var_os("PATH").expect("inherited PATH"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_profile_environment_merges_registry_paths_behind_the_profile_path() {
        let environment = merge_windows_environment(vec![
            (OsString::from("FNM_DIR"), OsString::from("C:\\fnm")),
            (
                OsString::from("PATH"),
                OsString::from("C:\\profile-first;C:\\shared"),
            ),
            (
                OsString::from("WAKU_USER_PATH"),
                OsString::from("C:\\user;C:\\shared"),
            ),
            (
                OsString::from("WAKU_MACHINE_PATH"),
                OsString::from("C:\\machine;C:\\USER;C:\\SHARED"),
            ),
        ])
        .expect("merge captured environment");

        let path = environment
            .iter()
            .find(|(name, _)| name == OsStr::new("PATH"))
            .map(|(_, value)| value.clone())
            .expect("merged PATH");
        assert_eq!(
            std::env::split_paths(&path).collect::<Vec<_>>(),
            vec![
                PathBuf::from("C:\\profile-first"),
                PathBuf::from("C:\\shared"),
                PathBuf::from("C:\\user"),
                PathBuf::from("C:\\machine"),
            ]
        );
        assert!(environment.contains(&(OsString::from("FNM_DIR"), OsString::from("C:\\fnm"))));
        assert!(
            !environment
                .iter()
                .any(|(name, _)| name == OsStr::new("WAKU_USER_PATH"))
        );
        assert!(
            !environment
                .iter()
                .any(|(name, _)| name == OsStr::new("WAKU_MACHINE_PATH"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_bare_name_resolves_through_pathext() {
        let directory = std::env::temp_dir().join(format!("waku-pathext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create fixture directory");
        // Only a suffixed file exists here, so the bare name resolves through PATHEXT.
        // A global npm install also drops an extensionless shim beside it; that layout is
        // covered by a_bare_name_prefers_pathext_over_an_extensionless_shim.
        std::fs::write(directory.join("faux-provider.cmd"), "@echo off\n")
            .expect("write shim fixture");

        assert_eq!(
            resolve_executable_file(&directory.join("faux-provider"))
                .expect("resolve through PATHEXT")
                .to_string_lossy()
                .to_lowercase(),
            directory
                .join("faux-provider.cmd")
                .to_string_lossy()
                .to_lowercase(),
        );
        assert_eq!(resolve_executable_file(&directory.join("absent")), None);

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(windows)]
    #[test]
    fn a_bare_name_prefers_pathext_over_an_extensionless_shim() {
        // A global npm install writes all three of these side by side. Only the `.cmd`
        // can be launched by `CreateProcess`; the extensionless one is a POSIX shim.
        let directory =
            std::env::temp_dir().join(format!("waku-pathext-shim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create fixture directory");
        std::fs::write(directory.join("faux-provider"), "#!/bin/sh\n")
            .expect("write posix shim fixture");
        std::fs::write(directory.join("faux-provider.ps1"), "#!/usr/bin/env pwsh\n")
            .expect("write powershell shim fixture");
        std::fs::write(directory.join("faux-provider.cmd"), "@echo off\n")
            .expect("write cmd shim fixture");

        assert_eq!(
            resolve_executable_file(&directory.join("faux-provider"))
                .expect("resolve through PATHEXT")
                .to_string_lossy()
                .to_lowercase(),
            directory
                .join("faux-provider.cmd")
                .to_string_lossy()
                .to_lowercase(),
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(windows)]
    #[test]
    fn windows_terminal_shells_never_ask_for_a_login_session() {
        assert_eq!(
            default_terminal_shell_args(Path::new("C:\\Program Files\\PowerShell\\7\\pwsh.exe")),
            vec!["-NoLogo".to_owned()]
        );
        assert!(
            default_terminal_shell_args(Path::new("C:\\Windows\\System32\\cmd.exe")).is_empty()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_shell_candidates_have_system_fallbacks() {
        let candidates = default_shell_candidates();

        assert!(candidates.contains(&PathBuf::from("/bin/bash")));
        assert!(candidates.contains(&PathBuf::from("/bin/sh")));
        assert!(default_terminal_shell().is_file());
    }

    #[cfg(unix)]
    #[test]
    fn parses_null_delimited_environment_without_losing_value_contents() {
        let environment = parse_shell_environment(
            b"PATH=/Users/example/.fnm/current/bin:/usr/bin\0TOKEN=line one\nline two=rest\0EMPTY=\0WAKU_SHELL_ENV_CAPTURE_FILE=/tmp/capture\0",
        )
        .expect("parse shell environment");

        assert_eq!(
            environment,
            vec![
                (
                    OsString::from("PATH"),
                    OsString::from("/Users/example/.fnm/current/bin:/usr/bin"),
                ),
                (
                    OsString::from("TOKEN"),
                    OsString::from("line one\nline two=rest"),
                ),
                (OsString::from("EMPTY"), OsString::new()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn captures_environment_from_a_shell_process() {
        let id = SHELL_ENV_CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("waku-command-env-test-{}-{id}", std::process::id()));
        fs::create_dir(&directory).expect("create shell fixture directory");
        let shell = directory.join("fake-shell");
        fs::write(
            &shell,
            "#!/bin/sh\n/usr/bin/printf 'PATH=/Users/example/.fnm/current/bin:/usr/bin\\000TEST_SHELL_TOKEN=from-shell\\000WAKU_DAEMON_TOKEN=secret\\000WAKU_TEST_TOKEN=stripped\\000' > \"$WAKU_SHELL_ENV_CAPTURE_FILE\"\n",
        )
        .expect("write shell fixture");
        let mut permissions = fs::metadata(&shell)
            .expect("read shell fixture")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&shell, permissions).expect("make shell fixture executable");

        let environment =
            capture_shell_environment(&shell, &["-i", "-l", "-c"], LOGIN_SHELL_ENV_TIMEOUT)
                .expect("capture shell environment");

        assert_eq!(
            environment,
            vec![
                (
                    OsString::from("PATH"),
                    OsString::from("/Users/example/.fnm/current/bin:/usr/bin"),
                ),
                (
                    OsString::from("TEST_SHELL_TOKEN"),
                    OsString::from("from-shell"),
                ),
            ]
        );
        let _ = fs::remove_file(shell);
        let _ = fs::remove_dir(directory);
    }

    #[test]
    fn shell_capture_strips_waku_prefixed_variables() {
        let parsed = parse_shell_environment(
            b"PATH=/usr/bin\0OPENCODE_API_KEY=from-shell\0WAKU_DAEMON_TOKEN=secret\0WAKU_DAEMON_ADDRESS=127.0.0.1:9\0WAKU_DATA_DIR=/tmp/waku\0waku_lowercase=1\0WAKU_SHELL_ENV_CAPTURE_FILE=/tmp/cap\0DISABLE_AUTO_UPDATE=true\0",
        )
        .expect("parse filtered environment");

        assert_eq!(
            parsed,
            vec![
                (OsString::from("PATH"), OsString::from("/usr/bin")),
                (
                    OsString::from("OPENCODE_API_KEY"),
                    OsString::from("from-shell"),
                ),
            ]
        );
        assert!(is_shell_capture_variable(OsStr::new("WAKU_DAEMON_TOKEN")));
        assert!(is_shell_capture_variable(OsStr::new("waku_custom")));
        assert!(!is_shell_capture_variable(OsStr::new("OPENCODE_API_KEY")));
        #[cfg(unix)]
        {
            assert!(is_shell_capture_variable(OsStr::new("WAKU_USER_PATH")));
            assert!(is_shell_capture_variable(OsStr::new("WAKU_MACHINE_PATH")));
        }
        #[cfg(windows)]
        {
            assert!(!is_shell_capture_variable(OsStr::new("WAKU_USER_PATH")));
            assert!(!is_shell_capture_variable(OsStr::new("WAKU_MACHINE_PATH")));
        }
    }

    #[test]
    fn env_var_os_prefers_process_environment_over_cached_shell_environment() {
        let shell_env = vec![
            (
                OsString::from("OPENCODE_API_KEY"),
                OsString::from("from-shell"),
            ),
            (
                OsString::from("CLAUDE_CONFIG_DIR"),
                OsString::from("/shell/claude"),
            ),
        ];

        assert_eq!(
            lookup_env_var_os(
                OsStr::new("OPENCODE_API_KEY"),
                Some(OsString::from("from-process")),
                Some(&shell_env),
            ),
            Some(OsString::from("from-process"))
        );
        // An explicit empty process value still shadows the shell cache.
        assert_eq!(
            lookup_env_var_os(
                OsStr::new("OPENCODE_API_KEY"),
                Some(OsString::new()),
                Some(&shell_env),
            ),
            Some(OsString::new())
        );
        // When absent from the process environment, fall back to cached shell env.
        assert_eq!(
            lookup_env_var_os(OsStr::new("OPENCODE_API_KEY"), None, Some(&shell_env)),
            Some(OsString::from("from-shell"))
        );
        assert_eq!(
            lookup_env_var_os(OsStr::new("MISSING_KEY"), None, Some(&shell_env)),
            None
        );
        assert_eq!(
            lookup_env_var_os(OsStr::new("OPENCODE_API_KEY"), None, None),
            None
        );
        #[cfg(windows)]
        assert_eq!(
            lookup_env_var_os(OsStr::new("opencode_api_key"), None, Some(&shell_env)),
            Some(OsString::from("from-shell"))
        );

        // `env_var_os` returns the inherited process env directly and returns
        // None for an absent variable without spawning a login shell.
        assert_eq!(env_var_os("PATH"), std::env::var_os("PATH"));
        assert_eq!(env_var_os("WAKU_DEFINITELY_ABSENT_ENV_VAR_9F3C"), None);
    }

    #[test]
    fn output_with_timeout_captures_streams_and_reaps_hanging_children() {
        const CHILD_MODE: &str = "WAKU_OUTPUT_TIMEOUT_TEST_MODE";
        if let Some(mode) = std::env::var_os(CHILD_MODE) {
            match mode.to_str().expect("child mode") {
                "emit" => {
                    use std::io::Write as _;
                    // Exceed typical 64 KiB OS pipe buffers on both streams to
                    // verify concurrent draining does not deadlock.
                    let payload = vec![b'x'; 96 * 1024];
                    let _ = io::stdout().write_all(b"hello-out:");
                    let _ = io::stdout().write_all(&payload);
                    let _ = io::stderr().write_all(b"hello-err:");
                    let _ = io::stderr().write_all(&payload);
                    std::process::exit(0);
                }
                "hang" => loop {
                    std::thread::park();
                },
                _ => panic!("unknown child mode"),
            }
        }

        let test_bin = std::env::current_exe().expect("test executable");
        let mut emit_cmd = plain_command(&test_bin);
        emit_cmd
            .args([
                "--exact",
                "command_env::tests::output_with_timeout_captures_streams_and_reaps_hanging_children",
                "--nocapture",
            ])
            .env(CHILD_MODE, "emit");
        let output =
            output_with_timeout(&mut emit_cmd, Duration::from_secs(30)).expect("emit child");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("hello-out:"), "stdout missing marker");
        assert!(stdout.len() >= 96 * 1024);
        assert!(stderr.contains("hello-err:"), "stderr missing marker");
        assert!(stderr.len() >= 96 * 1024);

        let mut hang_cmd = plain_command(&test_bin);
        hang_cmd
            .args([
                "--exact",
                "command_env::tests::output_with_timeout_captures_streams_and_reaps_hanging_children",
            ])
            .env(CHILD_MODE, "hang");
        let started = Instant::now();
        let error =
            output_with_timeout(&mut hang_cmd, Duration::from_millis(200)).expect_err("timeout");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout exceeded expected bound: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn resolve_config_dir_expands_tilde_without_requiring_existing_path() {
        assert_eq!(resolve_config_dir("   "), None);
        if let Some(home) = dirs::home_dir() {
            assert_eq!(resolve_config_dir("~"), Some(home.clone()));
            assert_eq!(
                resolve_config_dir("~/.claude_work"),
                Some(home.join(".claude_work"))
            );
            assert_eq!(
                resolve_config_dir("~\\.codex_work"),
                Some(home.join(".codex_work"))
            );
        }
        assert_eq!(
            resolve_config_dir(" /tmp/custom-claude "),
            Some(PathBuf::from("/tmp/custom-claude"))
        );
    }

    #[test]
    fn apply_provider_config_dir_sets_only_custom_account_env() {
        let mut default_claude = plain_command("echo");
        apply_provider_config_dir(
            &mut default_claude,
            crate::model::ProviderKind::Claude,
            None,
        );
        assert!(default_claude.get_envs().count() == 0);

        let mut work_claude = plain_command("echo");
        apply_provider_config_dir(
            &mut work_claude,
            crate::model::ProviderKind::Claude,
            Some(Path::new("/tmp/claude-work")),
        );
        assert_eq!(
            work_claude.get_envs().collect::<Vec<_>>(),
            vec![(
                OsStr::new("CLAUDE_CONFIG_DIR"),
                Some(OsStr::new("/tmp/claude-work"))
            )]
        );

        let mut work_codex = plain_command("echo");
        apply_provider_config_dir(
            &mut work_codex,
            crate::model::ProviderKind::Codex,
            Some(Path::new("/tmp/codex-work")),
        );
        assert_eq!(
            work_codex.get_envs().collect::<Vec<_>>(),
            vec![(
                OsStr::new("CODEX_HOME"),
                Some(OsStr::new("/tmp/codex-work"))
            )]
        );
    }

    #[test]
    fn antigravity_account_env_reads_adc_and_allowlisted_dotenv_only() {
        let dir = std::env::temp_dir().join(format!("waku-agy-env-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(antigravity_settings_arg(Some(&dir)), None);

        let adc = dir.join("application_default_credentials.json");
        std::fs::write(&adc, "{}").unwrap();
        let settings = dir.join("settings.json");
        std::fs::write(&settings, "{\"modelProvider\":\"gemini\"}").unwrap();
        std::fs::write(
            dir.join(".env"),
            "# comment\nGEMINI_API_KEY=\"gk-123\"\nUNRELATED_SECRET=nope\nGOOGLE_CLOUD_PROJECT='proj-1'\nGOOGLE_APPLICATION_CREDENTIALS=/override/adc.json\n",
        )
        .unwrap();

        assert_eq!(antigravity_settings_arg(Some(&dir)), Some(settings));
        let env = antigravity_account_env(&dir);
        assert_eq!(
            env,
            vec![
                ("AGY_ADC_AUTH".into(), "true".into()),
                (
                    "GOOGLE_APPLICATION_CREDENTIALS".into(),
                    "/override/adc.json".into()
                ),
                ("GEMINI_API_KEY".into(), "gk-123".into()),
                ("GOOGLE_CLOUD_PROJECT".into(), "proj-1".into()),
            ]
        );

        let mut cmd = plain_command("echo");
        apply_provider_config_dir(&mut cmd, crate::model::ProviderKind::Antigravity, Some(&dir));
        let mut applied: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_string_lossy().into_owned(), v?.to_string_lossy().into_owned())))
            .collect();
        applied.sort();
        let mut expected = env.clone();
        expected.sort();
        assert_eq!(applied, expected);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
