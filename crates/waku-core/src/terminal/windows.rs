//! ConPTY sessions owned by the daemon, never by the desktop window.
use crate::terminal_state::TerminalEvents;
use crate::{EventSink, WireDriverEvent};
use anyhow::Context as _;
use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

enum DaemonTerminalInput {
    Write(Vec<u8>),
    Resize(PtySize),
    Shutdown,
}

pub struct DaemonTerminal {
    job: Option<crate::process_tree::KillOnCloseJob>,
    /// Kept so ConPTY outlives a writer thread that has already exited, and dropped
    /// during `Drop` to close the pseudo-console and unblock `reader.read()`.
    master: Option<Arc<Mutex<Box<dyn MasterPty + Send>>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    input: Option<Sender<DaemonTerminalInput>>,
    output: TerminalEvents,
    stopped: Arc<AtomicBool>,
}

impl DaemonTerminal {
    pub fn open(
        cwd: &std::path::Path,
        cols: u16,
        rows: u16,
        events: EventSink,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            cwd.is_dir(),
            "terminal working directory does not exist: {}",
            cwd.display()
        );
        let pair = portable_pty::native_pty_system().openpty(size(cols, rows))?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let shell = crate::command_env::default_terminal_shell();
        let mut command = CommandBuilder::new(&shell);
        command.args(crate::command_env::default_terminal_shell_args(&shell));
        command.cwd(cwd);
        for (name, value) in crate::command_env::shell_environment() {
            command.env(name, value);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "waku");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        let child = pair.slave.spawn_command(command)?;
        // Nested job containers (Windows Sandbox, AppX, some enterprise hosts)
        // may deny job assignment; keep the terminal working and rely on
        // direct `child.kill()` in `Drop`.
        let job = child
            .process_id()
            .and_then(|pid| crate::process_tree::KillOnCloseJob::adopt(pid).ok());
        let child = Arc::new(Mutex::new(child));
        drop(pair.slave);
        let output = TerminalEvents::new(events, cols, rows);
        let master = Arc::new(Mutex::new(pair.master));
        let (input, rx) = unbounded::<DaemonTerminalInput>();
        let stopped = Arc::new(AtomicBool::new(false));
        let exit_reported = Arc::new(AtomicBool::new(false));
        let terminal = Self {
            job,
            master: Some(master.clone()),
            child,
            input: Some(input.clone()),
            output: output.clone(),
            stopped: stopped.clone(),
        };
        let replies = input;
        let writer_events = output.clone();
        std::thread::Builder::new()
            .name("waku-terminal-input".into())
            .spawn(move || {
                run_terminal_input(
                    rx,
                    writer,
                    |size| master.lock().resize(size),
                    |error| {
                        let _ = writer_events.send_ephemeral(WireDriverEvent::new(
                            "terminalError",
                            serde_json::Value::String(error.to_string()),
                        ));
                    },
                );
            })
            .context("start terminal input")?;
        let watcher_child = terminal.child.clone();
        let watcher_output = output.clone();
        let watcher_stopped = stopped.clone();
        let watcher_reported = exit_reported.clone();
        std::thread::Builder::new()
            .name("waku-terminal-output".into())
            .spawn(move || {
                let mut buffer = [0u8; 32768];
                while !stopped.load(Ordering::Acquire) {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            let _ = output.send_output(&buffer[..n]);
                            let bytes = output.state.lock().take_replies();
                            if !bytes.is_empty() && !stopped.load(Ordering::Acquire) {
                                let _ = replies.send(DaemonTerminalInput::Write(bytes));
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                report_exit(&output, &exit_reported);
            })
            .context("start terminal output")?;

        // ConPTY keeps its output pipe open while the master handle lives,
        // so a shell that exits on its own never produces the pipe EOF the
        // reader waits for. Watch the child handle instead (Zed waits the
        // same way) and report the exit from here.
        std::thread::Builder::new()
            .name("waku-terminal-child".into())
            .spawn(move || {
                while !watcher_stopped.load(Ordering::Acquire) {
                    let exited = watcher_child
                        .lock()
                        .try_wait()
                        .ok()
                        .flatten()
                        .is_some();
                    if exited {
                        report_exit(&watcher_output, &watcher_reported);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
            .context("start terminal child watcher")?;
        Ok(terminal)
    }

    pub fn write(&self, data: Vec<u8>) -> anyhow::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.input
            .as_ref()
            .context("terminal input closed")?
            .send(DaemonTerminalInput::Write(data))
            .context("terminal input unavailable")
    }
    pub fn resize(&self, cols: u16, rows: u16) {
        self.output.state.lock().resize(cols, rows);
        if let Some(input) = &self.input {
            let _ = input.send(DaemonTerminalInput::Resize(size(cols, rows)));
        }
    }
    pub fn snapshot(&self, after: Option<u64>) -> waku_protocol::TerminalOutput {
        self.output.state.lock().snapshot(after)
    }
    pub fn exited(&self) -> bool {
        self.output.state.lock().exited()
    }
}
impl Drop for DaemonTerminal {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(input) = self.input.take() {
            let _ = input.send(DaemonTerminalInput::Shutdown);
        }
        self.job.take();
        let _ = self.child.lock().kill();
        let _ = self.child.lock().try_wait();
        self.master.take();
    }
}

fn report_exit(output: &TerminalEvents, reported: &AtomicBool) {
    if !reported.swap(true, Ordering::AcqRel) {
        let _ = output.send_ephemeral(WireDriverEvent::new(
            "terminalExited",
            serde_json::Value::Null,
        ));
    }
}
fn size(cols: u16, rows: u16) -> PtySize {
    let cols = cols.max(2);
    let rows = rows.max(1);
    PtySize {
        cols,
        rows,
        pixel_width: cols.saturating_mul(CELL_WIDTH),
        pixel_height: rows.saturating_mul(CELL_HEIGHT),
    }
}

/// Writes and resizes share one thread: overlapping `WriteFile` and
/// `ResizePseudoConsole` breaks the ConPTY input pipe. A resize error is not
/// fatal. Stopping this thread drops `writer`, which delivers EOF and marks
/// the shell exited while output is still alive. A write error means the pipe
/// is already broken, so the thread stops.
fn run_terminal_input(
    rx: crossbeam_channel::Receiver<DaemonTerminalInput>,
    mut writer: impl std::io::Write,
    mut resize: impl FnMut(PtySize) -> anyhow::Result<()>,
    mut on_write_error: impl FnMut(std::io::Error),
) {
    while let Ok(command) = rx.recv() {
        match command {
            DaemonTerminalInput::Write(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
                    on_write_error(error);
                    break;
                }
            }
            DaemonTerminalInput::Resize(mut latest_size) => {
                // Coalesce back-to-back resizes queued during drag before
                // invoking synchronous ConPTY `ResizePseudoConsole`, while
                // preserving strict ordering relative to Write/Shutdown.
                let mut stop = false;
                while let Ok(next) = rx.try_recv() {
                    match next {
                        DaemonTerminalInput::Resize(newer_size) => latest_size = newer_size,
                        DaemonTerminalInput::Write(bytes) => {
                            let _ = resize(latest_size);
                            if !bytes.is_empty() {
                                if let Err(error) =
                                    writer.write_all(&bytes).and_then(|_| writer.flush())
                                {
                                    on_write_error(error);
                                    return;
                                }
                            }
                            // Resume outer loop after flushing the interleaved write.
                            stop = true;
                            break;
                        }
                        DaemonTerminalInput::Shutdown => {
                            return;
                        }
                    }
                }
                if !stop {
                    let _ = resize(latest_size);
                }
            }
            DaemonTerminalInput::Shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_failure_does_not_stop_input() {
        let (tx, rx) = unbounded();
        tx.send(DaemonTerminalInput::Resize(size(0, 0))).unwrap();
        tx.send(DaemonTerminalInput::Write(b"hi".to_vec())).unwrap();
        drop(tx);
        let mut output = Vec::new();
        let mut fatal = None;
        run_terminal_input(
            rx,
            &mut output,
            |_| anyhow::bail!("resize failed"),
            |error| fatal = Some(error.to_string()),
        );
        assert!(fatal.is_none(), "{fatal:?}");
        assert_eq!(output, b"hi");
    }

    #[test]
    fn write_failure_stops_the_writer() {
        let (tx, rx) = unbounded();
        tx.send(DaemonTerminalInput::Write(b"hi".to_vec())).unwrap();
        tx.send(DaemonTerminalInput::Write(b"more".to_vec())).unwrap();
        drop(tx);
        let mut writes = 0;
        let mut fatal = None;
        run_terminal_input(
            rx,
            FailWrite { writes: &mut writes },
            |_| Ok(()),
            |error| fatal = Some(error.kind()),
        );
        assert_eq!(fatal, Some(std::io::ErrorKind::BrokenPipe));
        assert_eq!(writes, 1);
    }

    struct FailWrite<'a> {
        writes: &'a mut usize,
    }

    impl std::io::Write for FailWrite<'_> {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            *self.writes += 1;
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
