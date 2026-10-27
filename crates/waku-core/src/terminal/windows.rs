//! ConPTY sessions owned by the daemon, never by the desktop window.
use crate::terminal_state::TerminalEvents;
use crate::{EventSink, WireDriverEvent};
use anyhow::Context as _;
use base64::Engine as _;
use crossbeam_channel::{Sender, bounded};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::io::{Read as _, Write as _};
use std::sync::Arc;

pub struct DaemonTerminal {
    job: Option<crate::process_tree::KillOnCloseJob>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    input: Option<Sender<Vec<u8>>>,
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
        let mut writer = pair.master.take_writer()?;
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
        let (input, rx) = bounded::<Vec<u8>>(256);
        let terminal = Self {
            job,
            master: Mutex::new(pair.master),
            child,
            input: Some(input),
            output: output.clone(),
        };
        let replies = terminal.input.as_ref().unwrap().clone();
        std::thread::Builder::new()
            .name("waku-terminal-input".into())
            .spawn(move || {
                while let Ok(bytes) = rx.recv() {
                    if writer
                        .write_all(&bytes)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
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
                            let data =
                                base64::engine::general_purpose::STANDARD.encode(&buffer[..n]);
                            let _ = output.send_ephemeral(WireDriverEvent::new(
                                "terminalOutput",
                                serde_json::json!({"data": data}),
                            ));
                            let bytes = output.state.lock().take_replies();
                            if !bytes.is_empty() {
                                let _ = replies.try_send(bytes);
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
        self.input
            .as_ref()
            .context("terminal input closed")?
            .try_send(data)
            .context("terminal input unavailable")
    }
    pub fn resize(&self, cols: u16, rows: u16) {
        self.output.state.lock().resize(cols, rows);
        let _ = self.master.lock().resize(size(cols, rows));
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
