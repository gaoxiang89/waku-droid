/// Whether `pid` names a process that has not exited. Returns `true` whenever
/// exit cannot be proven (permission denied, failed wait): every caller treats
/// a live process misreported as dead as the worse error.
pub fn is_alive(pid: u32) -> bool {
    imp::is_alive(pid)
}

#[cfg(unix)]
mod imp {
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
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
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
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn is_alive(_pid: u32) -> bool {
        true
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
    fn current_process_is_alive() {
        assert!(is_alive(std::process::id()));
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
