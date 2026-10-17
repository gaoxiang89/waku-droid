//! Whole-tree lifetime for provider processes on Windows.
//!
//! Unix gives each ACP agent its own process group, so killing the group
//! reaches anything a wrapper launcher started. Windows has no such group:
//! terminating a `.cmd` shim leaves the PowerShell and agent it started
//! running, and a busy agent may keep executing tools after Waku hung up.
//! A job object with `KILL_ON_JOB_CLOSE` is the Windows equivalent — every
//! process the child creates joins the job, and closing the last handle
//! (including when the daemon itself dies) terminates all of them.

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

/// A job that terminates every member process when dropped.
#[derive(Debug)]
pub(crate) struct KillOnCloseJob(HANDLE);

// The handle is an opaque kernel object reference with no thread affinity.
unsafe impl Send for KillOnCloseJob {}
unsafe impl Sync for KillOnCloseJob {}

impl KillOnCloseJob {
    /// Put the running process `pid` in a new kill-on-close job.
    ///
    /// The child is already running when this is called, so a descendant it
    /// starts before the assignment lands escapes the job. Launchers need far
    /// longer than that window to start anything — `cmd.exe` still has to
    /// read its script — and everything created afterwards is contained.
    pub(crate) fn adopt(pid: u32) -> io::Result<Self> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(job);

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }

        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let assigned = unsafe { AssignProcessToJobObject(job.0, process) };
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(process) };
        if assigned == 0 {
            return Err(error);
        }
        Ok(job)
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::KillOnCloseJob;

    /// `ping` inherits the shell's stdout, so the pipe reaches EOF only once
    /// the grandchild is gone too. Killing just `cmd.exe` would leave it open
    /// for the whole minute.
    #[test]
    fn dropping_the_job_terminates_grandchildren() {
        let mut child = Command::new("cmd.exe")
            .args(["/d", "/c", "ping -n 60 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start cmd.exe");
        let job = KillOnCloseJob::adopt(child.id()).expect("adopt cmd.exe");
        let mut stdout = child.stdout.take().expect("stdout pipe");
        // Let `cmd.exe` start `ping` inside the job before tearing it down.
        std::thread::sleep(Duration::from_millis(1500));

        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let mut output = Vec::new();
            let _ = stdout.read_to_end(&mut output);
            let _ = done.send(());
        });
        let _ = child.kill();
        drop(job);

        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the grandchild kept the pipe open after the job closed");
        let _ = child.wait();
    }
}
