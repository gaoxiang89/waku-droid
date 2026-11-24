//! Computer Use helper process integration.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
#[cfg(not(target_os = "macos"))]
use std::time::Instant;
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use uuid::Uuid;

use crate::computer_use;
use crate::driver::DriverEventSender;
use crate::fs_ext;
use crate::model::DriverEvent;

#[cfg(target_os = "macos")]
use std::ffi::OsString;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStringExt as _;

const PREVIEW_POLL_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(not(target_os = "macos"))]
const CANCEL_EXIT_WAIT_TIMEOUT: Duration = Duration::from_millis(750);
#[cfg(not(target_os = "macos"))]
const CANCEL_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone)]
pub(super) struct ComputerUseConfig {
    pub(super) server_path: PathBuf,
    pub(super) repl_path: PathBuf,
    pub(super) skill_path: PathBuf,
    pub(super) process_directory: PathBuf,
}

pub(super) struct ComputerUseRuntime {
    pub(super) config: ComputerUseConfig,
    preview_monitor: Option<ComputerUsePreviewMonitor>,
}

impl ComputerUseRuntime {
    pub(super) fn start(events: DriverEventSender) -> anyhow::Result<Self> {
        let server_path = computer_use::mcp_server_command()?;
        let repl_path = computer_use::js_repl_server_path()?;
        let skill_path = computer_use::skill_root_path()?
            .join("narel-computer-use")
            .join("SKILL.md");
        let process_directory = create_process_directory()?;
        let preview_monitor =
            match ComputerUsePreviewMonitor::start(process_directory.clone(), events) {
                Ok(monitor) => monitor,
                Err(error) => {
                    let _ = fs::remove_dir_all(&process_directory);
                    return Err(error);
                }
            };
        Ok(Self {
            config: ComputerUseConfig {
                server_path,
                repl_path,
                skill_path,
                process_directory,
            },
            preview_monitor: Some(preview_monitor),
        })
    }

    pub(super) fn stop(&self) {
        stop_registered_processes(&self.config.process_directory, &self.config.server_path);
    }
}

impl Drop for ComputerUseRuntime {
    fn drop(&mut self) {
        shutdown_process_directory(
            &self.config.process_directory,
            &self.config.server_path,
            &mut self.preview_monitor,
        );
    }
}

pub(super) struct ComputerUsePreviewMonitor {
    running: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ComputerUsePreviewMonitor {
    pub(super) fn start(directory: PathBuf, events: DriverEventSender) -> anyhow::Result<Self> {
        let running = Arc::new((Mutex::new(true), Condvar::new()));
        let thread_running = running.clone();
        let thread = thread::Builder::new()
            .name("waku-computer-use-preview".into())
            .spawn(move || {
                let mut seen = HashMap::<PathBuf, (SystemTime, u64)>::new();
                loop {
                    {
                        let (lock, _) = &*thread_running;
                        if !*lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
                            break;
                        }
                    }
                    if let Ok(entries) = fs::read_dir(&directory) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                                continue;
                            };
                            if !name.starts_with("preview-") || !name.ends_with(".json") {
                                continue;
                            }
                            let Ok(metadata) = entry.metadata() else {
                                continue;
                            };
                            let Ok(modified) = metadata.modified() else {
                                continue;
                            };
                            let revision = (modified, metadata.len());
                            if seen.get(&path) == Some(&revision) {
                                continue;
                            }
                            seen.insert(path.clone(), revision);
                            let Ok(data) = fs::read(&path) else {
                                continue;
                            };
                            if let Ok(state) = computer_use::decode_preview_update(&data) {
                                let _ = events.send(DriverEvent::ComputerUseUpdated(state));
                            }
                        }
                    }
                    let (lock, condvar) = &*thread_running;
                    let guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let (guard, _) = condvar
                        .wait_timeout_while(guard, PREVIEW_POLL_INTERVAL, |running| *running)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if !*guard {
                        break;
                    }
                }
            })
            .context("failed to start the Computer Use preview monitor")?;
        Ok(Self {
            running,
            thread: Some(thread),
        })
    }
}

impl Drop for ComputerUsePreviewMonitor {
    fn drop(&mut self) {
        {
            let (lock, condvar) = &*self.running;
            let mut running = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *running = false;
            condvar.notify_all();
        }
        if let Some(handle) = self.thread.take()
            && handle.thread().id() != thread::current().id()
        {
            let _ = handle.join();
        }
    }
}

pub(super) fn create_process_directory() -> anyhow::Result<PathBuf> {
    let directory = std::env::temp_dir()
        .join("waku-computer-use")
        .join(Uuid::new_v4().simple().to_string());
    fs::create_dir_all(&directory).with_context(|| {
        format!(
            "could not create Computer Use process directory {}",
            directory.display()
        )
    })?;
    fs_ext::restrict_to_owner(&directory).with_context(|| {
        format!(
            "could not secure Computer Use process directory {}",
            directory.display()
        )
    })?;
    Ok(directory)
}

/// Stop registered helpers, join the preview monitor cleanly, wait briefly on
/// non-macOS for cancelled helpers to observe their `cancel-{pid}` markers, and
/// remove the process directory without racing either the monitor or helpers.
pub(super) fn shutdown_process_directory(
    directory: &Path,
    helper_executable: &Path,
    preview_monitor: &mut Option<ComputerUsePreviewMonitor>,
) {
    #[cfg(not(target_os = "macos"))]
    let wait_timeout = CANCEL_EXIT_WAIT_TIMEOUT;
    #[cfg(target_os = "macos")]
    let wait_timeout = Duration::ZERO;
    shutdown_process_directory_with_timeout(
        directory,
        helper_executable,
        preview_monitor,
        wait_timeout,
    );
}

fn shutdown_process_directory_with_timeout(
    directory: &Path,
    helper_executable: &Path,
    preview_monitor: &mut Option<ComputerUsePreviewMonitor>,
    wait_timeout: Duration,
) {
    stop_registered_processes(directory, helper_executable);
    drop(preview_monitor.take());
    #[cfg(not(target_os = "macos"))]
    wait_for_cancelled_processes(directory, helper_executable, wait_timeout);
    #[cfg(target_os = "macos")]
    let _ = wait_timeout;
    let _ = fs::remove_dir_all(directory);
}

pub(super) fn stop_registered_processes(directory: &Path, helper_executable: &Path) {
    let expected_executable = canonicalize_executable(helper_executable);
    for (pid, registration) in registered_processes(directory) {
        if process_executable(pid).as_deref() == Some(expected_executable.as_path()) {
            // macOS owns a Launch Services bridge; closing it interrupts the
            // native SDK. Portable hosts poll a cancellation marker so they
            // can cancel the operation and await SDK shutdown on both OSes.
            #[cfg(target_os = "macos")]
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = fs::write(directory.join(format!("cancel-{pid}")), b"");
        }
        let _ = fs::remove_file(registration);
    }
}

#[cfg(not(target_os = "macos"))]
fn cancelling_processes(directory: &Path, expected_executable: &Path) -> Vec<i32> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            let pid = entry
                .file_name()
                .to_str()?
                .strip_prefix("cancel-")?
                .parse::<i32>()
                .ok()?;
            (pid > 1 && process_executable(pid).as_deref() == Some(expected_executable))
                .then_some(pid)
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn wait_for_cancelled_processes(directory: &Path, helper_executable: &Path, timeout: Duration) {
    if timeout.is_zero() {
        return;
    }
    let expected_executable = canonicalize_executable(helper_executable);
    let mut tracked = cancelling_processes(directory, &expected_executable);
    if tracked.is_empty() {
        return;
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        tracked.retain(|&pid| {
            directory.join(format!("cancel-{pid}")).exists()
                && process_executable(pid).as_deref() == Some(expected_executable.as_path())
        });
        if tracked.is_empty() {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(CANCEL_EXIT_POLL_INTERVAL.min(remaining));
    }
}

pub(super) fn registered_processes(directory: &Path) -> Vec<(i32, PathBuf)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            (pid > 1).then_some((pid, entry.path()))
        })
        .collect()
}

fn strip_linux_deleted_suffix(path: PathBuf) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = path.as_os_str().as_bytes();
        if let Some(stripped) = bytes.strip_suffix(b" (deleted)") {
            return PathBuf::from(std::ffi::OsStr::from_bytes(stripped));
        }
        path
    }
    #[cfg(not(unix))]
    {
        match path.to_str() {
            Some(text) => match text.strip_suffix(" (deleted)") {
                Some(stripped) => PathBuf::from(stripped),
                None => path,
            },
            None => path,
        }
    }
}

fn canonicalize_executable(path: &Path) -> PathBuf {
    let stripped = strip_linux_deleted_suffix(path.to_path_buf());
    fs::canonicalize(&stripped).unwrap_or(stripped)
}

#[cfg(target_os = "macos")]
pub(super) fn process_executable(pid: i32) -> Option<PathBuf> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast(),
            libc::PROC_PIDPATHINFO_MAXSIZE as u32,
        )
    };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(canonicalize_executable(&PathBuf::from(OsString::from_vec(
        buffer,
    ))))
}

#[cfg(target_os = "linux")]
pub(super) fn process_executable(pid: i32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|path| canonicalize_executable(&path))
}

#[cfg(target_os = "windows")]
pub(super) fn process_executable(pid: i32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if process.is_null() {
            return None;
        }
        let mut buffer = vec![0u16; 32768];
        let mut len = buffer.len() as u32;
        let success = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut len);
        CloseHandle(process);
        (success != 0)
            .then(|| PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len as usize])))
            .map(|path| canonicalize_executable(&path))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub(super) fn process_executable(_: i32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use std::time::Instant;

    #[test]
    fn strip_linux_deleted_suffix_handles_replaced_binaries() {
        assert_eq!(
            strip_linux_deleted_suffix(PathBuf::from("/opt/waku/helper (deleted)")),
            PathBuf::from("/opt/waku/helper")
        );
        assert_eq!(
            strip_linux_deleted_suffix(PathBuf::from("/opt/waku/helper")),
            PathBuf::from("/opt/waku/helper")
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[test]
    fn process_executable_canonicalizes_current_process() {
        let expected = canonicalize_executable(&std::env::current_exe().unwrap());
        assert_eq!(
            process_executable(std::process::id() as i32),
            Some(expected)
        );
    }

    #[test]
    fn preview_monitor_stops_and_joins_promptly_on_drop() {
        let directory = create_process_directory().unwrap();
        let (events, _rx) = crate::driver::test_event_channel();
        let monitor = ComputerUsePreviewMonitor::start(directory.clone(), events).unwrap();

        let started = Instant::now();
        drop(monitor);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "dropping the preview monitor must wake and join its thread promptly"
        );
        fs::remove_dir_all(&directory).unwrap();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn shutdown_waits_for_helper_to_observe_cancel_marker_before_removing_directory() {
        let directory = create_process_directory().unwrap();
        let pid = std::process::id() as i32;
        let current_exe = std::env::current_exe().unwrap();
        fs::write(directory.join(pid.to_string()), b"").unwrap();

        let cancel_path = directory.join(format!("cancel-{pid}"));
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_observed = observed.clone();
        let watcher = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if cancel_path.exists() {
                    thread_observed.store(true, std::sync::atomic::Ordering::Release);
                    // Simulate the helper observing cancel-{pid} and removing it on shutdown.
                    thread::sleep(Duration::from_millis(40));
                    let _ = fs::remove_file(&cancel_path);
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });

        let (events, _rx) = crate::driver::test_event_channel();
        let mut monitor = Some(ComputerUsePreviewMonitor::start(directory.clone(), events).unwrap());
        let started = Instant::now();
        shutdown_process_directory_with_timeout(
            &directory,
            &current_exe,
            &mut monitor,
            Duration::from_secs(2),
        );
        watcher.join().unwrap();

        assert!(
            observed.load(std::sync::atomic::Ordering::Acquire),
            "helper must observe cancel-{{pid}} before process directory is deleted"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "shutdown should return promptly once helper clears its cancel marker"
        );
        assert!(!directory.exists(), "process directory must be cleaned up");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn shutdown_wait_is_bounded_when_helper_never_clears_cancel_marker() {
        let directory = create_process_directory().unwrap();
        let pid = std::process::id() as i32;
        let current_exe = std::env::current_exe().unwrap();
        fs::write(directory.join(pid.to_string()), b"").unwrap();

        let mut monitor = None;
        let started = Instant::now();
        shutdown_process_directory_with_timeout(
            &directory,
            &current_exe,
            &mut monitor,
            Duration::from_millis(80),
        );

        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(60) && elapsed < Duration::from_secs(1),
            "wait must honor bounded timeout, elapsed={elapsed:?}"
        );
        assert!(!directory.exists(), "directory must still be removed after timeout");
    }
}
