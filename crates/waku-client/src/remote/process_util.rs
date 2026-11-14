//! Small blocking helpers for the `wsl.exe` and `ssh` children.

use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

/// Windows job object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so
/// `ssh.exe` / `wsl.exe` children and any `ProxyJump`/`ProxyCommand`
/// descendants exit when this handle is dropped or the client crashes.
#[cfg(windows)]
#[derive(Debug)]
pub struct ChildJob(windows_sys::Win32::Foundation::HANDLE);

// The job handle is an opaque kernel object with no thread affinity.
#[cfg(windows)]
unsafe impl Send for ChildJob {}
#[cfg(windows)]
unsafe impl Sync for ChildJob {}

#[cfg(windows)]
impl Drop for ChildJob {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

#[cfg(not(windows))]
#[derive(Debug)]
pub struct ChildJob;

/// Best-effort assignment of `child` into a kill-on-close job object.
/// Returns `None` if the OS denies assignment so connections never fail.
#[cfg(windows)]
pub fn adopt_child_into_kill_job(child: &Child) -> Option<ChildJob> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    let pid = child.id();
    if pid == 0 {
        return None;
    }
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let job = ChildJob(job);

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return None;
        }

        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            return None;
        }
        let assigned = AssignProcessToJobObject(job.0, process);
        CloseHandle(process);
        (assigned != 0).then_some(job)
    }
}

#[cfg(not(windows))]
pub fn adopt_child_into_kill_job(_child: &Child) -> Option<ChildJob> {
    None
}

/// The desktop is a GUI-subsystem binary on Windows, so every console child
/// would otherwise flash a window of its own.
pub fn hide_console_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// Run a child to completion with a deadline. A child still running at the
/// deadline is killed so its pipes close and the caller can report why.
pub fn output_with_timeout(
    mut command: Command,
    timeout: Duration,
    label: &str,
) -> anyhow::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console_window(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not run {label}"))?;
    let job = adopt_child_into_kill_job(&child);
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let status = wait_with_deadline(&mut child, timeout);
    drop(job);
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    match status {
        Some(status) => Ok(Output {
            status,
            stdout,
            stderr,
        }),
        None => {
            let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
            if detail.is_empty() {
                bail!("timed out running {label}")
            }
            bail!("timed out running {label}: {detail}")
        }
    }
}

/// Wait for `child`, killing it at the deadline. `None` means it was killed.
pub fn wait_with_deadline(
    child: &mut Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn drain(pipe: Option<impl std::io::Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

struct StderrState {
    bytes: Vec<u8>,
    done: bool,
}

impl Default for StderrState {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            done: true,
        }
    }
}

/// Keeps the tail of a child's stderr so a failed connection can say why
/// without retaining unbounded output from a long-lived bridge.
#[derive(Clone, Default)]
pub struct StderrTail(Arc<(Mutex<StderrState>, Condvar)>);

const STDERR_TAIL_BYTES: usize = 8 * 1024;
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(150);

struct MarkStderrDone(Arc<(Mutex<StderrState>, Condvar)>);

impl Drop for MarkStderrDone {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.0;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.done = true;
        cvar.notify_all();
    }
}

impl StderrTail {
    pub fn capture(pipe: impl std::io::Read + Send + 'static) -> Self {
        let tail = Self(Arc::new((
            Mutex::new(StderrState {
                bytes: Vec::new(),
                done: false,
            }),
            Condvar::new(),
        )));
        let sink = tail.0.clone();
        let spawned = std::thread::Builder::new()
            .name("waku-remote-stderr".into())
            .spawn(move || {
                let _done = MarkStderrDone(sink.clone());
                let mut pipe = pipe;
                let mut buffer = [0_u8; 4096];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            let mut state = sink
                                .0
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            state.bytes.extend_from_slice(&buffer[..read]);
                            let excess = state.bytes.len().saturating_sub(STDERR_TAIL_BYTES);
                            state.bytes.drain(..excess);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            });
        if spawned.is_err() {
            let (lock, cvar) = &*tail.0;
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.done = true;
            cvar.notify_all();
        }
        tail
    }

    pub fn clear(&self) {
        let mut state = self
            .0
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.bytes.clear();
    }

    pub fn text(&self) -> String {
        let state = self
            .0
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::remote::ssh::stderr_without_askpass_diagnostics(&state.bytes)
            .trim()
            .to_owned()
    }

    /// Wait briefly for the stderr reader thread to observe pipe EOF after the
    /// child has exited or been killed, then return the filtered stderr text.
    pub fn finish_and_text(&self) -> String {
        let (lock, cvar) = &*self.0;
        let guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (state, _) = cvar
            .wait_timeout_while(guard, STDERR_DRAIN_TIMEOUT, |state| !state.done)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::remote::ssh::stderr_without_askpass_diagnostics(&state.bytes)
            .trim()
            .to_owned()
    }
}

pub fn with_detail(message: &str, detail: &str) -> String {
    if detail.is_empty() {
        message.to_owned()
    } else {
        format!("{message}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn dropping_child_job_terminates_child_and_grandchild() {
        use std::io::BufRead as _;
        use std::path::PathBuf;

        let comspec =
            std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe"));
        let ping_exe = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("ping.exe"))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("ping.exe"));
        let mut command = Command::new(comspec);
        command
            .args(["/d", "/c"])
            .arg(ping_exe)
            .args(["-n", "60", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        hide_console_window(&mut command);
        let mut child = command.spawn().expect("spawn cmd.exe");
        let job = adopt_child_into_kill_job(&child).expect("adopt cmd.exe into kill job");
        let stdout = child.stdout.take().expect("stdout pipe");

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (eof_tx, eof_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        if String::from_utf8_lossy(&buf).contains("127.0.0.1") {
                            let _ = ready_tx.send(());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = eof_tx.send(());
        });

        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("grandchild ping should start and emit output");
        drop(job);

        let closed = eof_rx.recv_timeout(Duration::from_secs(10));
        if closed.is_err() {
            let _ = child.kill();
        }
        let _ = child.wait();
        closed.expect("dropping ChildJob should terminate child and grandchild holding stdout");
    }

    #[test]
    fn stderr_tail_finish_and_text_drains_on_eof_and_times_out_cleanly() {
        use std::io::Write as _;

        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let tail = StderrTail::capture(reader);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let _ = writer.write_all(b"Permission denied (publickey).\n");
            drop(writer);
        });
        assert_eq!(tail.finish_and_text(), "Permission denied (publickey).");

        // When the writer remains open, finish_and_text stops after the 150ms bound.
        let (open_reader, mut open_writer) = std::io::pipe().expect("open pipe");
        let open_tail = StderrTail::capture(open_reader);
        open_writer.write_all(b"partial line\n").expect("write");
        let started = Instant::now();
        let text = open_tail.finish_and_text();
        assert_eq!(text, "partial line");
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(open_writer);
    }
}
