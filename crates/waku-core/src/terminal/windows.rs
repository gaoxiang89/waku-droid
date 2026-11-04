//! ConPTY sessions owned by the daemon, never by the desktop window.
use crate::terminal_state::TerminalEvents;
use crate::{EventSink, WireDriverEvent};
use anyhow::Context as _;
use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::io::Read as _;
use std::sync::Arc;

enum DaemonTerminalInput {
    Write(Vec<u8>),
    Resize(PtySize),
}

pub struct DaemonTerminal {
    job: Option<crate::process_tree::KillOnCloseJob>,
    /// Kept so ConPTY outlives a writer thread that has already exited.
    _master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    input: Option<Sender<DaemonTerminalInput>>,
    output: TerminalEvents,
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
        let mut child = pair.slave.spawn_command(command)?;
        let job = match child
            .process_id()
            .map(crate::process_tree::KillOnCloseJob::adopt)
            .transpose()
        {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                return Err(error.into());
            }
        };
        let child = Arc::new(Mutex::new(child));
        drop(pair.slave);
        let output = TerminalEvents::new(events, cols, rows);
        let master = Arc::new(Mutex::new(pair.master));
        let (input, rx) = unbounded::<DaemonTerminalInput>();
        let terminal = Self {
            job,
            _master: master.clone(),
            child,
            input: Some(input),
            output: output.clone(),
        };
        let replies = terminal.input.as_ref().unwrap().clone();
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
        std::thread::Builder::new()
            .name("waku-terminal-output".into())
            .spawn(move || {
                let mut buffer = [0u8; 32768];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            let _ = output.send_output(&buffer[..n]);
                            let bytes = output.state.lock().take_replies();
                            if !bytes.is_empty() {
                                let _ = replies.send(DaemonTerminalInput::Write(bytes));
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = output.send_ephemeral(WireDriverEvent::new(
                    "terminalExited",
                    serde_json::Value::Null,
                ));
            })
            .context("start terminal output")?;
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
        self.job.take();
        self.input.take();
        let _ = self.child.lock().kill();
    }
}
fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols: cols.max(2),
        rows: rows.max(1),
        pixel_width: 0,
        pixel_height: 0,
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
            DaemonTerminalInput::Resize(size) => {
                let _ = resize(size);
            }
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
