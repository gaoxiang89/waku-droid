//! Daemon-owned pseudoterminals for remote clients.
//!
//! The daemon owns the shell, cwd, and PTY. Clients only render the byte
//! stream and send input/resize controls, so a browser can operate against a
//! daemon on another machine without interpreting any daemon-side paths.

#[cfg(not(unix))]
use std::path::Path;

#[cfg(not(unix))]
use anyhow::bail;

#[cfg(not(unix))]
use crate::EventSink;

#[cfg(unix)]
mod platform {
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use alacritty_terminal::event::{OnResize as _, WindowSize};
    use alacritty_terminal::tty::{self, EventedPty as _, EventedReadWrite as _, Shell};
    use anyhow::{Context as _, bail};
    use base64::Engine as _;
    use crossbeam_channel::{Sender, unbounded};
    use parking_lot::Mutex;
    use polling::{Event as PollEvent, Events, PollMode, Poller};
    use serde_json::json;

    use crate::{EventSink, WireDriverEvent};

    const CELL_WIDTH: u16 = 8;
    const CELL_HEIGHT: u16 = 16;
    const MIN_COLUMNS: u16 = 2;
    const MIN_ROWS: u16 = 1;
    const SHELL_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

    enum DaemonTerminalInput {
        Write(Vec<u8>),
        Resize(WindowSize),
    }

    pub struct DaemonTerminal {
        pty: Arc<Mutex<tty::Pty>>,
        stopped: Arc<AtomicBool>,
        reader_poller: Arc<Poller>,
        writer_poller: Arc<Poller>,
        input_tx: Option<Sender<DaemonTerminalInput>>,
        reader: Option<JoinHandle<()>>,
        writer: Option<JoinHandle<()>>,
    }

    impl DaemonTerminal {
        pub fn open(
            cwd: &std::path::Path,
            cols: u16,
            rows: u16,
            events: EventSink,
        ) -> anyhow::Result<Self> {
            let shell = crate::command_env::default_terminal_shell();
            let shell_args = crate::command_env::default_terminal_shell_args(&shell);
            Self::open_with_shell(
                cwd,
                cols,
                rows,
                events,
                Shell::new(shell.to_string_lossy().into_owned(), shell_args),
            )
        }

        pub(crate) fn open_with_shell(
            cwd: &std::path::Path,
            cols: u16,
            rows: u16,
            events: EventSink,
            shell: Shell,
        ) -> anyhow::Result<Self> {
            if !cwd.is_dir() {
                bail!(
                    "terminal working directory does not exist: {}",
                    cwd.display()
                );
            }

            let mut options = tty::Options {
                shell: Some(shell),
                working_directory: Some(cwd.to_owned()),
                drain_on_exit: false,
                ..Default::default()
            };
            for (name, value) in crate::command_env::shell_environment() {
                options.env.insert(
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                );
            }
            options.env.insert("TERM".into(), "xterm-256color".into());
            options.env.insert("COLORTERM".into(), "truecolor".into());
            // Same identity as a local terminal: the app's own emulator
            // renders this PTY, so programs may rely on its features.
            options.env.insert("TERM_PROGRAM".into(), "waku".into());
            options.env.insert(
                "TERM_PROGRAM_VERSION".into(),
                env!("CARGO_PKG_VERSION").into(),
            );

            let size = window_size(cols, rows);
            let pty = tty::new(&options, size, 0)
                .with_context(|| format!("spawn terminal in {}", cwd.display()))?;
            let pty = Arc::new(Mutex::new(pty));
            let stopped = Arc::new(AtomicBool::new(false));

            let reader_poller = Arc::new(Poller::new().context("create terminal reader poller")?);
            unsafe {
                pty.lock().register(
                    &reader_poller,
                    PollEvent::readable(0).with_interrupt(),
                    PollMode::Level,
                )
            }
            .context("register terminal reader")?;

            // The input thread starts first so no fallible setup remains after
            // the output thread is running; an error there would otherwise
            // leave the reader polling a PTY nobody owns.
            let mut input = pty
                .lock()
                .writer()
                .try_clone()
                .context("clone terminal input")?;
            let writer_poller = Arc::new(Poller::new().context("create terminal writer poller")?);
            unsafe {
                writer_poller.add_with_mode(
                    &input,
                    PollEvent::writable(0).with_interrupt(),
                    PollMode::Level,
                )
            }
            .context("register terminal writer")?;
            let (input_tx, input_rx) = unbounded::<DaemonTerminalInput>();
            let writer_stopped = stopped.clone();
            let writer_events = events.clone();
            let writer_poller_thread = writer_poller.clone();
            let writer_pty = pty.clone();
            let writer = std::thread::Builder::new()
                .name("waku-daemon-terminal-input".into())
                .spawn(move || {
                    while let Ok(command) = input_rx.recv() {
                        if writer_stopped.load(Ordering::Acquire) {
                            break;
                        }
                        let result = match command {
                            DaemonTerminalInput::Write(data) => write_all_evented(
                                &mut input,
                                &writer_poller_thread,
                                &data,
                                &writer_stopped,
                            ),
                            DaemonTerminalInput::Resize(size) => {
                                writer_pty.lock().on_resize(size);
                                Ok(())
                            }
                        };
                        if let Err(error) = result {
                            if !writer_stopped.load(Ordering::Acquire) {
                                let _ = writer_events.send_ephemeral(WireDriverEvent::new(
                                    "terminalError",
                                    serde_json::Value::String(error.to_string()),
                                ));
                            }
                            break;
                        }
                    }
                    let _ = writer_poller_thread.delete(&input);
                })
                .context("start terminal input thread")?;

            let reader_pty = pty.clone();
            let reader_stopped = stopped.clone();
            let reader_events = events.clone();
            let reader_poller_thread = reader_poller.clone();
            let reader = match std::thread::Builder::new()
                .name("waku-daemon-terminal-output".into())
                .spawn(move || {
                    let mut buffer = [0_u8; 32 * 1024];
                    let mut events = Events::new();
                    while !reader_stopped.load(Ordering::Acquire) {
                        events.clear();
                        if let Err(error) = reader_poller_thread.wait(&mut events, None) {
                            if error.kind() == std::io::ErrorKind::Interrupted
                                && !reader_stopped.load(Ordering::Acquire)
                            {
                                continue;
                            }
                            if !reader_stopped.load(Ordering::Acquire) {
                                let _ = reader_events.send_ephemeral(WireDriverEvent::new(
                                    "terminalError",
                                    serde_json::Value::String(error.to_string()),
                                ));
                            }
                            break;
                        }

                        loop {
                            let read_result = {
                                let mut pty = reader_pty.lock();
                                pty.reader().read(&mut buffer)
                            };
                            match read_result {
                                Ok(0) => {
                                    let _ = reader_events.send_ephemeral(WireDriverEvent::new(
                                        "terminalExited",
                                        serde_json::Value::Null,
                                    ));
                                    reader_stopped.store(true, Ordering::Release);
                                    break;
                                }
                                Ok(read) => {
                                    let data = base64::engine::general_purpose::STANDARD
                                        .encode(&buffer[..read]);
                                    let _ = reader_events.send_ephemeral(WireDriverEvent::new(
                                        "terminalOutput",
                                        json!({ "data": data }),
                                    ));
                                }
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                    ) =>
                                {
                                    break;
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                    continue;
                                }
                                Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                                    // A PTY master may report EIO while the child is
                                    // attaching or exiting. The registered SIGCHLD
                                    // pipe wakes the poller when the exit is known.
                                    if reader_pty.lock().next_child_event().is_some() {
                                        let _ = reader_events.send_ephemeral(WireDriverEvent::new(
                                            "terminalExited",
                                            serde_json::Value::Null,
                                        ));
                                        reader_stopped.store(true, Ordering::Release);
                                    } else {
                                        // Level-triggered polling would otherwise
                                        // spin on the EIO fd until SIGCHLD lands.
                                        std::thread::sleep(Duration::from_millis(1));
                                    }
                                    break;
                                }
                                Err(error) => {
                                    let _ = reader_events.send_ephemeral(WireDriverEvent::new(
                                        "terminalError",
                                        serde_json::Value::String(error.to_string()),
                                    ));
                                    reader_stopped.store(true, Ordering::Release);
                                    break;
                                }
                            }
                        }

                        if !reader_stopped.load(Ordering::Acquire)
                            && reader_pty.lock().next_child_event().is_some()
                        {
                            let _ = reader_events.send_ephemeral(WireDriverEvent::new(
                                "terminalExited",
                                serde_json::Value::Null,
                            ));
                            reader_stopped.store(true, Ordering::Release);
                        }
                    }
                    let _ = reader_pty.lock().deregister(&reader_poller_thread);
                }) {
                Ok(reader) => reader,
                Err(error) => {
                    stopped.store(true, Ordering::Release);
                    drop(input_tx);
                    let _ = writer_poller.notify();
                    let _ = writer.join();
                    return Err(anyhow::Error::from(error).context("start terminal output thread"));
                }
            };

            Ok(Self {
                pty,
                stopped,
                reader_poller,
                writer_poller,
                input_tx: Some(input_tx),
                reader: Some(reader),
                writer: Some(writer),
            })
        }

        pub fn write(&self, data: Vec<u8>) -> anyhow::Result<()> {
            if data.is_empty() {
                return Ok(());
            }
            self.input_tx
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("terminal input writer is not running"))?
                .send(DaemonTerminalInput::Write(data))
                .context("terminal input writer is not running")
        }

        pub fn resize(&self, cols: u16, rows: u16) {
            if let Some(input_tx) = &self.input_tx {
                let _ = input_tx.send(DaemonTerminalInput::Resize(window_size(cols, rows)));
            }
        }
    }

    impl Drop for DaemonTerminal {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            drop(self.input_tx.take());
            let _ = self.writer_poller.notify();
            let _ = self.reader_poller.notify();

            // Alacritty's PTY destructor sends SIGHUP and then waits for the
            // child without a timeout. A shell can ignore or defer SIGHUP,
            // so bound its grace period before either join or PTY drop.
            // Hold the lock so the reader cannot reap the child between our
            // exit check and signal delivery.
            terminate_shell(&self.pty.lock());
            if let Some(writer) = self.writer.take() {
                let _ = writer.join();
            }
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    fn write_all_evented<W: std::io::Write + ?Sized>(
        writer: &mut W,
        poller: &Poller,
        data: &[u8],
        stopped: &AtomicBool,
    ) -> std::io::Result<()> {
        let mut written = 0;
        let mut events = Events::new();
        while written < data.len() {
            if stopped.load(Ordering::Acquire) {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            match writer.write(&data[written..]) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    loop {
                        if stopped.load(Ordering::Acquire) {
                            return Err(std::io::ErrorKind::BrokenPipe.into());
                        }
                        events.clear();
                        match poller.wait(&mut events, None) {
                            Ok(_) if !events.is_empty() => break,
                            Ok(_) => {}
                            Err(error)
                                if error.kind() == std::io::ErrorKind::Interrupted
                                    && !stopped.load(Ordering::Acquire) => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn terminate_shell(pty: &tty::Pty) {
        let child_pid = pty.child().id() as libc::pid_t;
        if child_has_exited(child_pid) {
            return;
        }
        unsafe {
            libc::kill(child_pid, libc::SIGHUP);
        }
        let deadline = Instant::now() + SHELL_SHUTDOWN_GRACE;
        while !child_has_exited(child_pid) {
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(child_pid, libc::SIGKILL);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    fn child_has_exited(child_pid: libc::pid_t) -> bool {
        // WNOWAIT leaves reaping to Alacritty's Child, keeping the PID owned
        // until its destructor has finished sending signals and waiting.
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child_pid as libc::id_t,
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            unsafe { status.si_pid() != 0 }
        } else {
            // The output reader may already have reaped a naturally exited
            // shell via next_child_event before shutdown began.
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
        }
    }

    fn window_size(cols: u16, rows: u16) -> WindowSize {
        WindowSize {
            num_lines: rows.max(MIN_ROWS),
            num_cols: cols.max(MIN_COLUMNS),
            cell_width: CELL_WIDTH,
            cell_height: CELL_HEIGHT,
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::unix::net::UnixStream;

        use super::*;

        #[test]
        fn evented_write_completes_after_backpressure() {
            let (mut read_end, mut write_end) = UnixStream::pair().unwrap();
            // Only the writer is non-blocking; `read_to_end` on a non-blocking
            // stream fails with WouldBlock as soon as it catches up.
            write_end.set_nonblocking(true).unwrap();

            let poller = Poller::new().unwrap();
            unsafe {
                poller.add_with_mode(
                    &write_end,
                    PollEvent::writable(0).with_interrupt(),
                    PollMode::Level,
                )
            }
            .unwrap();

            let filler = [b'x'; 1024];
            let mut filler_len = 0;
            while let Ok(count) = write_end.write(&filler) {
                if count == 0 {
                    break;
                }
                filler_len += count;
            }

            let payload = vec![b'y'; 64 * 1024];
            let reader = std::thread::spawn(move || {
                let mut received = Vec::new();
                read_end.read_to_end(&mut received).unwrap();
                received
            });

            let stopped = AtomicBool::new(false);
            write_all_evented(&mut write_end, &poller, &payload, &stopped).unwrap();
            drop(write_end);
            drop(poller);

            let received = reader.join().unwrap();
            assert_eq!(received.len(), filler_len + payload.len());
            assert!(received[..filler_len].iter().all(|byte| *byte == b'x'));
            assert!(received.ends_with(&payload));
        }
    }
}

#[cfg(unix)]
pub use platform::DaemonTerminal;

#[cfg(not(unix))]
pub struct DaemonTerminal;

#[cfg(not(unix))]
impl DaemonTerminal {
    pub fn open(_cwd: &Path, _cols: u16, _rows: u16, _events: EventSink) -> anyhow::Result<Self> {
        bail!("daemon terminals are not supported on this platform")
    }

    pub fn write(&self, _data: Vec<u8>) -> anyhow::Result<()> {
        bail!("daemon terminals are not supported on this platform")
    }

    pub fn resize(&self, _cols: u16, _rows: u16) {}
}
