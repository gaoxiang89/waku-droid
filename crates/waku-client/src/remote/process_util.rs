//! Small blocking helpers for the `wsl.exe` and `ssh` children.

use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use parking_lot::Mutex;

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
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let status = wait_with_deadline(&mut child, timeout);
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

/// Keeps the tail of a child's stderr so a failed connection can say why
/// without retaining unbounded output from a long-lived bridge.
#[derive(Clone, Default)]
pub struct StderrTail(Arc<Mutex<Vec<u8>>>);

const STDERR_TAIL_BYTES: usize = 8 * 1024;

impl StderrTail {
    pub fn capture(pipe: impl std::io::Read + Send + 'static) -> Self {
        let tail = Self::default();
        let sink = tail.0.clone();
        let _ = std::thread::Builder::new()
            .name("waku-remote-stderr".into())
            .spawn(move || {
                let mut pipe = pipe;
                let mut buffer = [0_u8; 4096];
                while let Ok(read) = pipe.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    let mut bytes = sink.lock();
                    bytes.extend_from_slice(&buffer[..read]);
                    let excess = bytes.len().saturating_sub(STDERR_TAIL_BYTES);
                    bytes.drain(..excess);
                }
            });
        tail
    }

    pub fn text(&self) -> String {
        crate::remote::ssh::stderr_without_askpass_diagnostics(&self.0.lock())
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
