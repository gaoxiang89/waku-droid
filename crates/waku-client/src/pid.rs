/// Whether `pid` names a process that has not exited. Returns `true` whenever
/// exit cannot be proven (permission denied, failed wait): every caller treats
/// a live process misreported as dead as the worse error.
pub fn is_alive(pid: u32) -> bool {
    imp::is_alive(pid)
}

/// Whether `pid` names a live `waku-daemon` (or `waku-debug-daemon`) process.
/// Used before acting on a bare PID from `resident-daemon.json` so an OS-reused
/// PID can never be mistaken for the daemon.
pub fn is_daemon_process(pid: u32) -> bool {
    imp::is_daemon_process(pid)
}

/// Best-effort termination for a process owned by this user. On Unix, sends
/// `SIGTERM` first and only escalates to `SIGKILL` after a grace period.
pub fn terminate(pid: u32) -> bool {
    imp::terminate(pid)
}

/// Best-effort forced termination of a `waku-daemon` process when a graceful
/// shutdown request timed out. Refuses to signal any PID whose executable is
/// not `waku-daemon` / `waku-debug-daemon`.
pub fn terminate_daemon(pid: u32) -> bool {
    imp::terminate_daemon(pid)
}

#[cfg(any(unix, windows))]
fn matches_daemon_stem(stem: &str) -> bool {
    stem.eq_ignore_ascii_case("waku-daemon") || stem.eq_ignore_ascii_case("waku-debug-daemon")
}

#[cfg(unix)]
mod imp {
    use std::path::Path;
    use std::time::{Duration, Instant};

    const GRACEFUL_TERMINATE_TIMEOUT: Duration = Duration::from_secs(2);

    pub fn is_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    pub fn is_daemon_process(pid: u32) -> bool {
        if !is_alive(pid) {
            return false;
        }
        #[cfg(target_os = "linux")]
        if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            let text = exe.to_string_lossy();
            let cleaned = text.strip_suffix(" (deleted)").unwrap_or(&text);
            if let Some(stem) = Path::new(cleaned).file_stem().and_then(|s| s.to_str()) {
                return super::matches_daemon_stem(stem);
            }
        }
        let Ok(output) = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        let comm = String::from_utf8_lossy(&output.stdout);
        let comm = comm.trim();
        Path::new(comm)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(super::matches_daemon_stem)
    }

    pub fn terminate(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        let Ok(raw_pid) = i32::try_from(pid) else {
            return false;
        };
        if unsafe { libc::kill(raw_pid, libc::SIGTERM) } != 0 {
            return false;
        }
        let deadline = Instant::now() + GRACEFUL_TERMINATE_TIMEOUT;
        while Instant::now() < deadline {
            if !is_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        unsafe { libc::kill(raw_pid, libc::SIGKILL) == 0 }
    }

    pub fn terminate_daemon(pid: u32) -> bool {
        if !is_daemon_process(pid) {
            return false;
        }
        terminate(pid)
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    };

    pub fn is_alive(pid: u32) -> bool {
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
            let wait = WaitForSingleObject(handle, 0);
            CloseHandle(handle);
            wait != WAIT_OBJECT_0
        }
    }

    pub fn is_daemon_process(pid: u32) -> bool {
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
                return false;
            }
            let alive = WaitForSingleObject(handle, 0) != WAIT_OBJECT_0;
            let matched = alive && handle_is_daemon(handle);
            CloseHandle(handle);
            matched
        }
    }

    pub fn terminate(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let terminated = TerminateProcess(handle, 1) != 0;
            CloseHandle(handle);
            terminated
        }
    }

    pub fn terminate_daemon(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                pid,
            );
            if handle.is_null() {
                return false;
            }
            let terminated = WaitForSingleObject(handle, 0) != WAIT_OBJECT_0
                && handle_is_daemon(handle)
                && TerminateProcess(handle, 1) != 0;
            CloseHandle(handle);
            terminated
        }
    }

    unsafe fn handle_is_daemon(handle: HANDLE) -> bool {
        let mut buffer = [0_u16; 1024];
        let mut len = buffer.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut len) };
        if ok == 0 || len == 0 {
            return false;
        }
        let path = OsString::from_wide(&buffer[..len as usize]);
        Path::new(&path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(super::matches_daemon_stem)
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn is_alive(_pid: u32) -> bool {
        true
    }

    pub fn is_daemon_process(_pid: u32) -> bool {
        false
    }

    pub fn terminate(_pid: u32) -> bool {
        false
    }

    pub fn terminate_daemon(_pid: u32) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_zero_is_never_alive() {
        assert!(!is_alive(0));
    }

    #[test]
    fn current_process_is_alive_and_not_daemon() {
        let pid = std::process::id();
        assert!(is_alive(pid));
        assert!(!is_daemon_process(pid));
        assert!(!terminate_daemon(pid));
        assert!(is_alive(pid));
    }

    #[test]
    fn daemon_stem_matching_is_exact_and_case_insensitive() {
        assert!(matches_daemon_stem("waku-daemon"));
        assert!(matches_daemon_stem("WAKU-DAEMON"));
        assert!(matches_daemon_stem("waku-debug-daemon"));
        assert!(!matches_daemon_stem("waku"));
        assert!(!matches_daemon_stem("powershell"));
        assert!(!matches_daemon_stem("waku-daemon-other"));
    }

    #[test]
    fn waited_child_is_dead() {
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "exit 3"])
            .spawn()
            .unwrap();
        #[cfg(unix)]
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let _ = child.wait();
        assert!(!is_alive(pid));
    }

    #[cfg(windows)]
    #[test]
    fn exited_259_child_is_dead_while_handle_is_held() {
        // 259 is STILL_ACTIVE; the wait must still report the process dead.
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "exit 259"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "exit-259 child did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(259));
        assert!(!is_alive(pid));
        drop(child);
        assert!(!is_alive(pid));
    }
}
