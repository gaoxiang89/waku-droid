use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use crate::ui::menu::{ContextMenuHandle, MenuItem, context_menu};
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{BidirectionalIterator, Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point as TerminalPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty::{self, Shell};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use anyhow::{Context as _, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use gpui::{
    Action, App, AppContext, Bounds, ClipboardEntry, ClipboardItem, Context, ElementInputHandler,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, FontFallbacks, FontStyle, FontWeight,
    Hsla, ImageFormat, InteractiveElement, IntoElement, KeyBinding, KeyContext, KeyDownEvent,
    Keystroke, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseExitEvent,
    MouseMoveEvent, MouseUpEvent, NoAction, ParentElement, Pixels, Point, Render, ScrollDelta,
    ScrollWheelEvent, SharedString, StrikethroughStyle, Styled, StyledText, Subscription, Task,
    TextRun, UTF16Selection, UnderlineStyle, Window, canvas, div, font, is_no_action,
    prelude::FluentBuilder, px, rgb,
};
use parking_lot::Mutex;

use crate::persistence::DEFAULT_RIGHT_PANEL_WIDTH;
use crate::theme::{Theme, sp};
use crate::ui::scrollbar::{self, ScrollbarState};

/// Fallback advance width, used only until the font has been measured.
const TERMINAL_CELL_WIDTH: f32 = 7.8;
const TERMINAL_CELL_HEIGHT: f32 = 18.0;
const TERMINAL_FONT_SIZE: f32 = 12.5;

#[inline]
fn primary_modifier_pressed(modifiers: &Modifiers) -> bool {
    modifiers.secondary()
}

#[inline]
fn terminal_clipboard_modifier_pressed(modifiers: &Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.secondary() && !modifiers.control && !modifiers.alt
    } else {
        // Preserve Ctrl+C for SIGINT and follow Linux terminal convention.
        modifiers.control && modifiers.shift && !modifiers.alt && !modifiers.platform
    }
}
const TERMINAL_PADDING_X: f32 = 10.0;
const TERMINAL_PADDING_Y: f32 = 8.0;
const TERMINAL_TOOLBAR_HEIGHT: f32 = 34.0;
const TERMINAL_MIN_COLUMNS: usize = 20;
const TERMINAL_MIN_ROWS: usize = 8;
/// Coding agents print long transcripts into the main screen, so keep more
/// than Alacritty's 10k default. Rows are allocated only as history fills.
const TERMINAL_SCROLLBACK_LINES: usize = 30_000;
const TERMINAL_CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);
const TERMINAL_CURSOR_BLINK_PAUSE: Duration = Duration::from_millis(300);

/// Alacritty's default URL hint plus local file paths containing a slash.
/// Requiring a slash keeps ordinary dotted words from becoming links.
#[rustfmt::skip]
const TERMINAL_LINK_REGEX: &str = "((ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file:|git://|ssh:|ftp://)|\
                                    (/|~/|\\./|\\.\\./|[A-Za-z0-9._@%+~-]+/))\
                                   [^\u{0000}-\u{001F}\u{007F}-\u{009F}<>\"\\s{-}\\^⟨⟩`\\\\]+";
const MAX_TERMINAL_LINK_SEARCH_LINES: i32 = 100;

/// Icon glyphs (nerd-font private-use codepoints) resolve through CoreText's
/// cascade rather than run splitting: JetBrains Mono itself keeps the
/// Powerline range it covers, everything else falls through to the bundled
/// symbols face registered in [`crate::assets::register_fonts`].
static TERMINAL_FONT_FALLBACKS: LazyLock<FontFallbacks> = LazyLock::new(|| {
    FontFallbacks::from_fonts(vec![crate::assets::SYMBOLS_FONT_FAMILY.to_owned()])
});

fn terminal_font() -> gpui::Font {
    let mut terminal_font = font("JetBrains Mono");
    terminal_font.fallbacks = Some(TERMINAL_FONT_FALLBACKS.clone());
    terminal_font
}

enum TerminalUiEvent {
    Title(String),
    ResetTitle,
    ClipboardStore(String),
    ClipboardLoad(Arc<dyn Fn(&str) -> String + Send + Sync>),
    /// BEL, or an OSC 9 / OSC 777 desktop notification (`None` for BEL).
    Alert(Option<TerminalAlert>),
    Exited,
}

/// A desktop notification a program asked for, as coding agents do when a
/// turn finishes or an approval is waiting.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalAlert {
    title: Option<String>,
    body: String,
}

/// Finds OSC 9 (iTerm2) and OSC 777 (rxvt `notify`) notifications in PTY
/// output. Alacritty's parser drops both, so the bytes are scanned before
/// they reach it; state carries across reads so a split sequence still
/// matches.
#[derive(Default)]
struct OscAlertScanner {
    state: OscScanState,
    payload: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum OscScanState {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
    /// An OSC longer than any notification; skip to its terminator.
    Overflow,
}

impl OscAlertScanner {
    const MAX_PAYLOAD: usize = 4096;

    fn feed(&mut self, bytes: &[u8], mut alert: impl FnMut(TerminalAlert)) {
        for &byte in bytes {
            self.state = match (self.state, byte) {
                (OscScanState::Ground, 0x1b) => OscScanState::Escape,
                (OscScanState::Ground, _) => OscScanState::Ground,
                (OscScanState::Escape, b']') => {
                    self.payload.clear();
                    OscScanState::Osc
                }
                (OscScanState::Escape, 0x1b) => OscScanState::Escape,
                (OscScanState::Escape, _) => OscScanState::Ground,
                (OscScanState::Osc | OscScanState::OscEscape, 0x07)
                | (OscScanState::OscEscape, b'\\') => {
                    if let Some(found) = osc_alert(&self.payload) {
                        alert(found);
                    }
                    OscScanState::Ground
                }
                (OscScanState::Osc | OscScanState::OscEscape, 0x1b) => OscScanState::OscEscape,
                // Any other byte after ESC inside an OSC aborts it, as in VT parsers.
                (OscScanState::OscEscape, _) => OscScanState::Ground,
                (OscScanState::Osc, _) if self.payload.len() >= Self::MAX_PAYLOAD => {
                    OscScanState::Overflow
                }
                (OscScanState::Osc, _) => {
                    self.payload.push(byte);
                    OscScanState::Osc
                }
                (OscScanState::Overflow, 0x07) => OscScanState::Ground,
                (OscScanState::Overflow, 0x1b) => OscScanState::Escape,
                (OscScanState::Overflow, _) => OscScanState::Overflow,
            };
        }
    }
}

fn osc_alert(payload: &[u8]) -> Option<TerminalAlert> {
    let payload = std::str::from_utf8(payload).ok()?;
    if let Some(body) = payload.strip_prefix("9;") {
        // ConEmu reuses OSC 9 with a numeric subcommand (9;4 progress, 9;9
        // working directory); only free text is an iTerm2 notification.
        let first = body.split(';').next().unwrap_or_default();
        if body.is_empty() || first.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        return Some(TerminalAlert {
            title: None,
            body: body.to_owned(),
        });
    }
    let rest = payload.strip_prefix("777;notify;")?;
    let (title, body) = rest.split_once(';').unwrap_or((rest, ""));
    Some(TerminalAlert {
        title: (!title.is_empty()).then(|| title.to_owned()),
        body: body.to_owned(),
    })
}

/// The local PTY with its output passed through an [`OscAlertScanner`] on the
/// way to Alacritty's event loop.
struct AlertScanningPty {
    pty: tty::Pty,
    scanner: OscAlertScanner,
    ui_events: Sender<TerminalUiEvent>,
}

impl std::io::Read for AlertScanningPty {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use alacritty_terminal::tty::EventedReadWrite as _;
        let read = self.pty.reader().read(buf)?;
        let ui_events = &self.ui_events;
        self.scanner.feed(&buf[..read], |alert| {
            let _ = ui_events.send(TerminalUiEvent::Alert(Some(alert)));
        });
        Ok(read)
    }
}

impl alacritty_terminal::tty::EventedReadWrite for AlertScanningPty {
    type Reader = Self;
    type Writer = <tty::Pty as alacritty_terminal::tty::EventedReadWrite>::Writer;

    unsafe fn register(
        &mut self,
        poll: &Arc<polling::Poller>,
        interest: polling::Event,
        mode: polling::PollMode,
    ) -> std::io::Result<()> {
        // SAFETY: the wrapped PTY lives exactly as long as this wrapper,
        // which the event loop keeps registered for its whole lifetime.
        unsafe { self.pty.register(poll, interest, mode) }
    }

    fn reregister(
        &mut self,
        poll: &Arc<polling::Poller>,
        interest: polling::Event,
        mode: polling::PollMode,
    ) -> std::io::Result<()> {
        self.pty.reregister(poll, interest, mode)
    }

    fn deregister(&mut self, poll: &Arc<polling::Poller>) -> std::io::Result<()> {
        self.pty.deregister(poll)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        self
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.pty.writer()
    }
}

impl alacritty_terminal::tty::EventedPty for AlertScanningPty {
    fn next_child_event(&mut self) -> Option<alacritty_terminal::tty::ChildEvent> {
        self.pty.next_child_event()
    }
}

impl alacritty_terminal::event::OnResize for AlertScanningPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.pty.on_resize(window_size);
    }
}

/// Where keystrokes, resizes, and terminal replies go: a local PTY's event
/// loop, or a PTY the daemon owns on a WSL or SSH host.
#[derive(Clone)]
enum PtyInput {
    Local(EventLoopSender),
    Remote(Arc<RemoteTerminal>),
    #[cfg(test)]
    Test(Sender<Vec<u8>>),
}

impl PtyInput {
    fn input(&self, bytes: Cow<'static, [u8]>) {
        match self {
            Self::Local(sender) => {
                let _ = sender.send(Msg::Input(bytes));
            }
            Self::Remote(remote) => remote.write(bytes.into_owned()),
            #[cfg(test)]
            Self::Test(sender) => sender.send(bytes.into_owned()).unwrap(),
        }
    }

    fn resize(&self, size: WindowSize) {
        match self {
            Self::Local(sender) => {
                let _ = sender.send(Msg::Resize(size));
            }
            Self::Remote(remote) => remote.resize(size.num_cols, size.num_lines),
            #[cfg(test)]
            Self::Test(_) => {}
        }
    }

    fn shutdown(&self) {
        match self {
            Self::Local(sender) => {
                let _ = sender.send(Msg::Shutdown);
            }
            Self::Remote(remote) => remote.close(),
            #[cfg(test)]
            Self::Test(_) => {}
        }
    }
}

/// How a terminal view gets its PTY.
#[derive(Clone)]
pub enum TerminalLauncher {
    /// A shell on this machine.
    Local,
    /// A shell the connected daemon runs on its own host. A desktop PTY would
    /// interpret a WSL or SSH working directory on the wrong machine.
    Daemon(waku_client::DaemonSupervisor),
}

/// A daemon-owned PTY addressed by its own session and runtime ids. Output
/// arrives as ephemeral `terminalOutput` events; input and resizes are
/// fire-and-forget requests in the same runtime mailbox, so they stay ordered.
struct RemoteTerminal {
    daemon: waku_client::DaemonSupervisor,
    session_id: uuid::Uuid,
    runtime_id: uuid::Uuid,
    closed: AtomicBool,
}

impl RemoteTerminal {
    fn write(&self, data: Vec<u8>) {
        if data.is_empty() || self.closed.load(Ordering::Acquire) {
            return;
        }
        let _ = self.daemon.client().notify(
            self.session_id,
            self.runtime_id,
            waku_client::Command::WriteTerminal { data },
        );
    }

    fn resize(&self, cols: u16, rows: u16) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let _ = self.daemon.client().notify(
            self.session_id,
            self.runtime_id,
            waku_client::Command::ResizeTerminal { cols, rows },
        );
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let client = self.daemon.client();
        let _ = client.notify(
            self.session_id,
            self.runtime_id,
            waku_client::Command::CloseTerminal,
        );
        client.unsubscribe(self.session_id, self.runtime_id);
    }

    /// Open the PTY on the daemon host and start feeding its output into
    /// `term`. Blocking: call it off the UI thread.
    fn open(
        daemon: waku_client::DaemonSupervisor,
        working_directory: &Path,
        size: WindowSize,
        term: Arc<FairMutex<Term<TerminalEventProxy>>>,
        dirty: Arc<AtomicBool>,
        ui_events: Sender<TerminalUiEvent>,
    ) -> Result<Arc<Self>> {
        let remote = Arc::new(Self {
            daemon,
            session_id: uuid::Uuid::new_v4(),
            runtime_id: uuid::Uuid::new_v4(),
            closed: AtomicBool::new(false),
        });
        let client = remote.daemon.client();
        // Subscribe before opening so the shell's first prompt is not lost.
        let events = client.subscribe(remote.session_id, remote.runtime_id);
        let opened = client.request(
            remote.session_id,
            remote.runtime_id,
            waku_client::Command::OpenTerminal {
                cwd: working_directory.to_path_buf(),
                cols: size.num_cols,
                rows: size.num_lines,
            },
        );
        if let Err(error) = opened {
            client.unsubscribe(remote.session_id, remote.runtime_id);
            return Err(error).with_context(|| {
                format!(
                    "open terminal in {} on the daemon host",
                    working_directory.display()
                )
            });
        }
        let reader = remote.clone();
        std::thread::Builder::new()
            .name("waku-remote-terminal".into())
            .spawn(move || reader.pump(client, events, term, dirty, ui_events))
            .context("start remote terminal reader")?;
        Ok(remote)
    }

    fn pump(
        &self,
        mut client: waku_client::DaemonClient,
        mut events: Receiver<waku_client::SequencedEvent>,
        term: Arc<FairMutex<Term<TerminalEventProxy>>>,
        dirty: Arc<AtomicBool>,
        ui_events: Sender<TerminalUiEvent>,
    ) {
        use base64::Engine as _;
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        let mut alerts = OscAlertScanner::default();
        let exited = |ui_events: &Sender<TerminalUiEvent>| {
            let _ = ui_events.send(TerminalUiEvent::Exited);
            dirty.store(true, Ordering::Release);
        };
        loop {
            match events.recv() {
                Ok(event) => match event.event.kind.as_str() {
                    "terminalOutput" => {
                        let Some(bytes) = event
                            .event
                            .payload
                            .get("data")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|data| {
                                base64::engine::general_purpose::STANDARD.decode(data).ok()
                            })
                        else {
                            continue;
                        };
                        alerts.feed(&bytes, |alert| {
                            let _ = ui_events.send(TerminalUiEvent::Alert(Some(alert)));
                        });
                        parser.advance(&mut *term.lock(), &bytes);
                        dirty.store(true, Ordering::Release);
                    }
                    "terminalExited" | "terminalError" => {
                        self.closed.store(true, Ordering::Release);
                        exited(&ui_events);
                        return;
                    }
                    _ => {}
                },
                // The transport dropped. The daemon keeps the PTY, so follow
                // the supervisor to its replacement connection and listen
                // again; output produced during the gap is not replayed.
                Err(_) => {
                    if self.closed.load(Ordering::Acquire) {
                        return;
                    }
                    let Some(replacement) = self.await_replacement(&client) else {
                        exited(&ui_events);
                        return;
                    };
                    client = replacement;
                    events = client.subscribe(self.session_id, self.runtime_id);
                    // A daemon that restarted has lost the PTY; a resize is
                    // the cheapest request that proves it still exists.
                    let (cols, rows) = {
                        let term = term.lock();
                        (term.columns() as u16, term.screen_lines() as u16)
                    };
                    let alive = client.request(
                        self.session_id,
                        self.runtime_id,
                        waku_client::Command::ResizeTerminal { cols, rows },
                    );
                    if alive.is_err() {
                        self.closed.store(true, Ordering::Release);
                        exited(&ui_events);
                        return;
                    }
                }
            }
        }
    }

    fn await_replacement(
        &self,
        disconnected: &waku_client::DaemonClient,
    ) -> Option<waku_client::DaemonClient> {
        let updates = self.daemon.subscribe_clients();
        loop {
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            match updates.recv_timeout(Duration::from_secs(1)) {
                Ok(client) if !client.same_connection(disconnected) => return Some(client),
                Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

#[derive(Clone)]
struct TerminalEventProxy {
    dirty: Arc<AtomicBool>,
    sender: Arc<OnceLock<PtyInput>>,
    ui_events: Sender<TerminalUiEvent>,
    window_size: Arc<Mutex<WindowSize>>,
    /// Palette OSC replies run on the PTY thread; `snapshot` refreshes them
    /// so they track the active theme.
    reply_colors: TerminalReplyColors,
}

/// The dynamic colors a program can query with OSC 10/11/12. Some TUIs pick
/// between their light and dark palettes from the reported background, so a
/// reply that does not match the surface makes them paint near-black text
/// onto a dark terminal, where it only shows once selected.
#[derive(Clone)]
struct TerminalReplyColors {
    is_dark: Arc<AtomicBool>,
    foreground: Arc<AtomicU32>,
    background: Arc<AtomicU32>,
    cursor: Arc<AtomicU32>,
}

impl TerminalReplyColors {
    fn new() -> Self {
        let colors = Self {
            is_dark: Arc::new(AtomicBool::new(true)),
            foreground: Arc::new(AtomicU32::new(0)),
            background: Arc::new(AtomicU32::new(0)),
            cursor: Arc::new(AtomicU32::new(0)),
        };
        colors.store(&Theme::dark());
        colors
    }

    fn store(&self, theme: &Theme) {
        self.is_dark.store(theme.is_dark, Ordering::Release);
        self.foreground
            .store(pack_hsla(theme.text), Ordering::Release);
        self.background
            .store(pack_hsla(theme.terminal), Ordering::Release);
        self.cursor.store(pack_hsla(theme.text), Ordering::Release);
    }

    fn is_dark(&self) -> bool {
        self.is_dark.load(Ordering::Acquire)
    }

    /// Named colors owned by the theme; ANSI indices still resolve through
    /// [`terminal_rgb`].
    fn color(&self, index: usize) -> Option<Rgb> {
        let load = |value: &AtomicU32| unpack_rgb(value.load(Ordering::Acquire));
        match index {
            i if i == NamedColor::Foreground as usize
                || i == NamedColor::BrightForeground as usize =>
            {
                Some(load(&self.foreground))
            }
            i if i == NamedColor::Background as usize => Some(load(&self.background)),
            i if i == NamedColor::Cursor as usize => Some(load(&self.cursor)),
            i if i == NamedColor::DimForeground as usize => {
                Some(dim_rgb(load(&self.foreground), self.is_dark()))
            }
            _ => None,
        }
    }
}

impl TerminalEventProxy {
    fn write_pty(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        if let Some(sender) = self.sender.get() {
            sender.input(bytes.into());
        }
    }
}

impl EventListener for TerminalEventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                self.dirty.store(true, Ordering::Release);
            }
            Event::Title(title) => {
                let _ = self.ui_events.send(TerminalUiEvent::Title(title));
            }
            Event::ResetTitle => {
                let _ = self.ui_events.send(TerminalUiEvent::ResetTitle);
            }
            Event::ClipboardStore(_, text) => {
                let _ = self.ui_events.send(TerminalUiEvent::ClipboardStore(text));
            }
            Event::ClipboardLoad(_, formatter) => {
                let _ = self
                    .ui_events
                    .send(TerminalUiEvent::ClipboardLoad(formatter));
            }
            Event::PtyWrite(text) => self.write_pty(text.into_bytes()),
            Event::ColorRequest(index, formatter) => {
                let color = self
                    .reply_colors
                    .color(index)
                    .unwrap_or_else(|| terminal_rgb(index, self.reply_colors.is_dark()));
                self.write_pty(formatter(color).into_bytes());
            }
            Event::TextAreaSizeRequest(formatter) => {
                self.write_pty(formatter(*self.window_size.lock()).into_bytes());
            }
            Event::Bell => {
                let _ = self.ui_events.send(TerminalUiEvent::Alert(None));
            }
            Event::Exit | Event::ChildExit(_) => {
                let _ = self.ui_events.send(TerminalUiEvent::Exited);
                self.dirty.store(true, Ordering::Release);
            }
        }
    }
}

struct TerminalDimensions {
    columns: usize,
    rows: usize,
}

impl Dimensions for TerminalDimensions {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

struct TerminalSession {
    term: Arc<FairMutex<Term<TerminalEventProxy>>>,
    sender: PtyInput,
    dirty: Arc<AtomicBool>,
    /// Latched by [`Self::take_dirty`] so the next [`Self::snapshot`] rebuilds
    /// even after [`TerminalView::poll`] has cleared `dirty`.
    snapshot_dirty: AtomicBool,
    cached_snapshot: Mutex<Option<(TerminalSnapshotKey, Arc<TerminalSnapshot>)>>,
    ui_events: Receiver<TerminalUiEvent>,
    window_size: Arc<Mutex<WindowSize>>,
    grid_size: (usize, usize),
    reply_colors: TerminalReplyColors,
    url_regex: RegexSearch,
}

impl TerminalSession {
    fn new(
        working_directory: &Path,
        columns: usize,
        rows: usize,
        launcher: TerminalLauncher,
    ) -> Result<Self> {
        let columns = columns.max(TERMINAL_MIN_COLUMNS);
        let rows = rows.max(TERMINAL_MIN_ROWS);
        let window_size = WindowSize {
            num_lines: rows.min(u16::MAX as usize) as u16,
            num_cols: columns.min(u16::MAX as usize) as u16,
            cell_width: TERMINAL_CELL_WIDTH.round() as u16,
            cell_height: TERMINAL_CELL_HEIGHT.round() as u16,
        };
        let shared_window_size = Arc::new(Mutex::new(window_size));
        let dirty = Arc::new(AtomicBool::new(true));
        let sender_slot = Arc::new(OnceLock::new());
        let reply_colors = TerminalReplyColors::new();
        let (ui_event_tx, ui_events) = unbounded();
        let url_regex = RegexSearch::new(TERMINAL_LINK_REGEX)
            .map_err(|error| anyhow::anyhow!("compile terminal link regex: {error}"))?;
        let proxy = TerminalEventProxy {
            dirty: dirty.clone(),
            sender: sender_slot.clone(),
            ui_events: ui_event_tx.clone(),
            window_size: shared_window_size.clone(),
            reply_colors: reply_colors.clone(),
        };

        let config = Config {
            scrolling_history: TERMINAL_SCROLLBACK_LINES,
            // Agent TUIs (Codex, Claude Code) push the kitty keyboard
            // protocol to tell Shift-Enter from Enter; see `kitty_key_bytes`.
            kitty_keyboard: true,
            ..Default::default()
        };
        let dimensions = TerminalDimensions { columns, rows };
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &dimensions,
            proxy.clone(),
        )));

        if let TerminalLauncher::Daemon(daemon) = launcher {
            let remote = RemoteTerminal::open(
                daemon,
                working_directory,
                window_size,
                term.clone(),
                dirty.clone(),
                ui_event_tx,
            )?;
            let sender = PtyInput::Remote(remote);
            sender_slot
                .set(sender.clone())
                .map_err(|_| anyhow::anyhow!("initialize remote terminal input"))?;
            return Ok(Self {
                term,
                sender,
                dirty,
                snapshot_dirty: AtomicBool::new(true),
                cached_snapshot: Mutex::new(None),
                ui_events,
                window_size: shared_window_size,
                grid_size: (columns, rows),
                reply_colors,
                url_regex,
            });
        }

        let shell = crate::command_env::default_terminal_shell();
        let shell_args = crate::command_env::default_terminal_shell_args(&shell);
        let mut options = tty::Options {
            shell: Some(Shell::new(shell.to_string_lossy().into_owned(), shell_args)),
            working_directory: Some(working_directory.to_path_buf()),
            drain_on_exit: false,
            ..Default::default()
        };
        options.env.insert("TERM".into(), "xterm-256color".into());
        options.env.insert("COLORTERM".into(), "truecolor".into());
        options.env.insert("TERM_PROGRAM".into(), "waku".into());
        options.env.insert(
            "TERM_PROGRAM_VERSION".into(),
            env!("CARGO_PKG_VERSION").into(),
        );
        if let Some(path) = crate::command_env::executable_search_path() {
            options
                .env
                .insert("PATH".into(), path.to_string_lossy().into_owned());
        }

        let pty = AlertScanningPty {
            pty: tty::new(&options, window_size, 0)
                .with_context(|| format!("spawn terminal in {}", working_directory.display()))?,
            scanner: OscAlertScanner::default(),
            ui_events: ui_event_tx,
        };
        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)
            .context("create Alacritty PTY event loop")?;
        let sender = PtyInput::Local(event_loop.channel());
        sender_slot
            .set(sender.clone())
            .map_err(|_| anyhow::anyhow!("initialize Alacritty PTY sender"))?;
        event_loop.spawn();

        Ok(Self {
            term,
            sender,
            dirty,
            snapshot_dirty: AtomicBool::new(true),
            cached_snapshot: Mutex::new(None),
            ui_events,
            window_size: shared_window_size,
            grid_size: (columns, rows),
            reply_colors,
            url_regex,
        })
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        if !bytes.is_empty() {
            self.sender.input(bytes);
        }
    }

    fn resize(&mut self, columns: usize, rows: usize, cell_width: f32) {
        let columns = columns.max(TERMINAL_MIN_COLUMNS);
        let rows = rows.max(TERMINAL_MIN_ROWS);
        if self.grid_size == (columns, rows) {
            return;
        }

        self.grid_size = (columns, rows);
        let dimensions = TerminalDimensions { columns, rows };
        self.term.lock().resize(dimensions);
        let size = WindowSize {
            num_lines: rows.min(u16::MAX as usize) as u16,
            num_cols: columns.min(u16::MAX as usize) as u16,
            cell_width: cell_width.round() as u16,
            cell_height: TERMINAL_CELL_HEIGHT.round() as u16,
        };
        *self.window_size.lock() = size;
        self.sender.resize(size);
        self.dirty.store(true, Ordering::Release);
    }

    fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    fn scroll(&self, lines: i32) {
        if lines == 0 {
            return;
        }
        self.term.lock().scroll_display(Scroll::Delta(lines));
        self.dirty.store(true, Ordering::Release);
    }

    fn take_dirty(&self) -> bool {
        let dirty = self.dirty.swap(false, Ordering::AcqRel);
        if dirty {
            self.snapshot_dirty.store(true, Ordering::Release);
        }
        dirty
    }

    fn link_at(&mut self, point: TerminalPoint) -> Option<TerminalLink> {
        let TerminalSession {
            term, url_regex, ..
        } = self;
        let term = term.lock();
        let (value, bounds) =
            hyperlink_at(&term, point).or_else(|| plain_link_at(&term, url_regex, point))?;
        Some(TerminalLink { value, bounds })
    }

    fn snapshot(
        &self,
        theme: Theme,
        selection_color: Hsla,
        cursor_style: TerminalCursorStyle,
        hovered_link: Option<&Match>,
    ) -> Arc<TerminalSnapshot> {
        self.reply_colors.store(&theme);
        let columns = self.grid_size.0;
        let rows = self.grid_size.1;
        let key = TerminalSnapshotKey {
            columns,
            rows,
            is_dark: theme.is_dark,
            text_color: pack_hsla(theme.text),
            terminal_color: pack_hsla(theme.terminal),
            selection_color: pack_hsla(selection_color),
            cursor_style,
            hovered_link: hovered_link.cloned(),
        };
        let dirty =
            self.dirty.load(Ordering::Acquire) || self.snapshot_dirty.swap(false, Ordering::AcqRel);
        if !dirty
            && let Some((cached_key, cached_snapshot)) = self.cached_snapshot.lock().as_ref()
            && *cached_key == key
        {
            return cached_snapshot.clone();
        }

        let term = self.term.lock();
        let content = term.renderable_content();
        let selection = content.selection;
        let cursor_row = content.cursor.point.line.0 + content.display_offset as i32;
        let cursor_column = content.cursor.point.column.0;
        let cursor_style = if matches!(
            content.cursor.shape,
            alacritty_terminal::vte::ansi::CursorShape::Hidden
        ) {
            TerminalCursorStyle::Hidden
        } else {
            cursor_style
        };
        let outline_cursor = (cursor_style == TerminalCursorStyle::Outline
            && (0..rows as i32).contains(&cursor_row)
            && cursor_column < columns)
            .then_some((cursor_row as usize, cursor_column));
        let mut cells = vec![TerminalCell::blank(theme); columns * rows];

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + content.display_offset as i32;
            let column = indexed.point.column.0;
            if row < 0 || row as usize >= rows || column >= columns {
                continue;
            }
            let cell = indexed.cell;
            let mut text = if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::HIDDEN)
            {
                " ".to_owned()
            } else {
                cell.c.to_string()
            };
            if let Some(zerowidth) = cell.zerowidth() {
                text.extend(zerowidth);
            }

            let mut foreground = resolve_color(cell.fg, content.colors, theme, true);
            let mut background = resolve_color(cell.bg, content.colors, theme, false);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut foreground, &mut background);
            }
            // `Flags::DIM_BOLD` and `Flags::BOLD_ITALIC` are unions of the
            // individual bits, so testing them with `intersects` would also
            // match plain bold or italic cells.
            if cell.flags.contains(Flags::DIM) {
                if theme.is_dark {
                    foreground.l *= 0.7;
                } else {
                    foreground.l = 1.0 - (1.0 - foreground.l) * 0.7;
                }
            }
            if selection.is_some_and(|selection| selection.contains(indexed.point)) {
                background = selection_color;
                foreground = theme.text;
            }
            if cursor_style == TerminalCursorStyle::Solid
                && row == cursor_row
                && column == cursor_column
            {
                background = theme.text;
                foreground = theme.terminal;
            }

            cells[row as usize * columns + column] = TerminalCell {
                text,
                foreground,
                background,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES)
                    || hovered_link.is_some_and(|bounds| bounds.contains(&indexed.point)),
                strikeout: cell.flags.contains(Flags::STRIKEOUT),
            };
        }

        let mut rendered_rows = Vec::with_capacity(rows);
        for row in cells.chunks(columns) {
            let mut text = String::new();
            let mut runs: Vec<TerminalRun> = Vec::new();
            for cell in row {
                let len = cell.text.len();
                text.push_str(&cell.text);
                let style = TerminalRunStyle {
                    foreground: cell.foreground,
                    background: cell.background,
                    bold: cell.bold,
                    italic: cell.italic,
                    underline: cell.underline,
                    strikeout: cell.strikeout,
                };
                if let Some(run) = runs.last_mut().filter(|run| run.style == style) {
                    run.len += len;
                } else {
                    runs.push(TerminalRun { len, style });
                }
            }
            rendered_rows.push(TerminalRow {
                text: SharedString::from(text),
                runs,
            });
        }

        let snapshot = Arc::new(TerminalSnapshot {
            rows: rendered_rows,
            outline_cursor,
            cursor_position: ((0..rows as i32).contains(&cursor_row) && cursor_column < columns)
                .then_some((cursor_row as usize, cursor_column)),
        });
        *self.cached_snapshot.lock() = Some((key, snapshot.clone()));
        snapshot
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.sender.shutdown();
    }
}

/// Adapts the alacritty grid to the overlay scrollbar: the scrollback history
/// is the content above the viewport, and `display_offset` is how far back up
/// into it the view currently sits (0 = pinned to the live bottom).
#[derive(Clone)]
struct TerminalScrollbarTarget {
    term: Arc<FairMutex<Term<TerminalEventProxy>>>,
    dirty: Arc<AtomicBool>,
    viewport_rows: usize,
}

impl scrollbar::Scrollable for TerminalScrollbarTarget {
    fn viewport_height(&self) -> Pixels {
        px(self.viewport_rows as f32 * TERMINAL_CELL_HEIGHT)
    }

    fn max_offset(&self) -> Pixels {
        px(self.term.lock().grid().history_size() as f32 * TERMINAL_CELL_HEIGHT)
    }

    fn scrolled(&self) -> Pixels {
        let term = self.term.lock();
        let grid = term.grid();
        let lines_above = grid.history_size().saturating_sub(grid.display_offset());
        px(lines_above as f32 * TERMINAL_CELL_HEIGHT)
    }

    fn scroll_to(&self, offset: Pixels) {
        let mut term = self.term.lock();
        let target_offset = (term.grid().history_size() as f32
            - f32::from(offset) / TERMINAL_CELL_HEIGHT)
            .round()
            .max(0.0) as usize;
        let delta = target_offset as i32 - term.grid().display_offset() as i32;
        if delta != 0 {
            term.scroll_display(Scroll::Delta(delta));
            self.dirty.store(true, Ordering::Release);
        }
    }
}

#[derive(Clone)]
struct TerminalCell {
    text: String,
    foreground: Hsla,
    background: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikeout: bool,
}

impl TerminalCell {
    fn blank(theme: Theme) -> Self {
        Self {
            text: " ".into(),
            foreground: theme.text,
            background: theme.terminal,
            bold: false,
            italic: false,
            underline: false,
            strikeout: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct TerminalRunStyle {
    foreground: Hsla,
    background: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikeout: bool,
}

struct TerminalRun {
    len: usize,
    style: TerminalRunStyle,
}

struct TerminalRow {
    text: SharedString,
    runs: Vec<TerminalRun>,
}

struct TerminalSnapshot {
    rows: Vec<TerminalRow>,
    outline_cursor: Option<(usize, usize)>,
    cursor_position: Option<(usize, usize)>,
}

#[derive(Clone, PartialEq, Eq)]
struct TerminalSnapshotKey {
    columns: usize,
    rows: usize,
    is_dark: bool,
    text_color: u32,
    terminal_color: u32,
    selection_color: u32,
    cursor_style: TerminalCursorStyle,
    hovered_link: Option<Match>,
}

struct TerminalComposition {
    text: String,
    selection: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalLink {
    value: String,
    bounds: Match,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TerminalLinkTarget {
    Url(String),
    File(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalCursorStyle {
    Solid,
    Outline,
    Hidden,
}

struct TerminalCursorBlink {
    visible: bool,
    enabled: bool,
    epoch: usize,
    _task: Task<()>,
}

impl TerminalCursorBlink {
    fn new() -> Self {
        Self {
            visible: true,
            enabled: false,
            epoch: 0,
            _task: Task::ready(()),
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if self.enabled {
            return;
        }

        self.enabled = true;
        self.visible = true;
        let epoch = self.next_epoch();
        self.schedule_blink(epoch, TERMINAL_CURSOR_BLINK_INTERVAL, cx);
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if !self.enabled && self.visible {
            return;
        }

        self.enabled = false;
        self.visible = true;
        self.next_epoch();
        cx.notify();
    }

    fn pause(&mut self, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }

        self.visible = true;
        let epoch = self.next_epoch();
        self.schedule_blink(epoch, TERMINAL_CURSOR_BLINK_PAUSE, cx);
        cx.notify();
    }

    fn blink(&mut self, epoch: usize, cx: &mut Context<Self>) {
        if !self.enabled || epoch != self.epoch {
            return;
        }

        self.visible = !self.visible;
        let epoch = self.next_epoch();
        self.schedule_blink(epoch, TERMINAL_CURSOR_BLINK_INTERVAL, cx);
        cx.notify();
    }

    fn schedule_blink(&mut self, epoch: usize, delay: Duration, cx: &mut Context<Self>) {
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.blink(epoch, cx));
            }
        });
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch
    }

    fn visible(&self) -> bool {
        self.visible
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TerminalPasteContent {
    Text(String),
    Image { format: ImageFormat, bytes: Vec<u8> },
    Paths(Vec<PathBuf>),
}

fn extract_terminal_paste_content(clipboard: &ClipboardItem) -> Option<TerminalPasteContent> {
    if let Some(ClipboardEntry::Image(image)) = clipboard.entries().first() {
        if !image.bytes.is_empty() {
            return Some(TerminalPasteContent::Image {
                format: image.format,
                bytes: image.bytes.clone(),
            });
        }
    }

    if let Some(ClipboardEntry::ExternalPaths(external)) = clipboard.entries().first() {
        if !external.paths().is_empty() {
            return Some(TerminalPasteContent::Paths(external.paths().to_vec()));
        }
    }

    if let Some(text) = clipboard.text().filter(|t| !t.is_empty()) {
        return Some(TerminalPasteContent::Text(text));
    }

    for entry in clipboard.entries() {
        match entry {
            ClipboardEntry::Image(image) if !image.bytes.is_empty() => {
                return Some(TerminalPasteContent::Image {
                    format: image.format,
                    bytes: image.bytes.clone(),
                });
            }
            ClipboardEntry::ExternalPaths(external) if !external.paths().is_empty() => {
                return Some(TerminalPasteContent::Paths(external.paths().to_vec()));
            }
            _ => {}
        }
    }

    None
}

fn clipboard_has_paste_content(clipboard: &ClipboardItem) -> bool {
    extract_terminal_paste_content(clipboard).is_some()
}

fn shell_quote_path(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    let is_safe = !path_str.is_empty()
        && path_str
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '\\' | ':' | '.' | '_' | '-'));
    if is_safe {
        return path_str.into_owned();
    }

    if cfg!(windows) || path_str.contains('\\') {
        let escaped = path_str.replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        let escaped = path_str.replace('\'', "'\\''");
        format!("'{escaped}'")
    }
}

/// Agent CLIs accept only PNG and JPEG, while clipboards also carry BMP (every
/// Windows screenshot) and TIFF. Re-encode those as PNG; anything that does not
/// decode is passed through unchanged.
fn paste_image_as_png(format: ImageFormat, bytes: Vec<u8>) -> (ImageFormat, Vec<u8>) {
    if matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Svg
    ) {
        return (format, bytes);
    }
    let mut png = Vec::new();
    let encoded = image::load_from_memory(&bytes).and_then(|decoded| {
        decoded.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    });
    match encoded {
        Ok(()) => (ImageFormat::Png, png),
        Err(_) => (format, bytes),
    }
}

fn save_pasted_image(
    launcher: TerminalLauncher,
    format: ImageFormat,
    bytes: Vec<u8>,
) -> Result<PathBuf> {
    let (format, bytes) = paste_image_as_png(format, bytes);
    // A daemon terminal's shell runs on the daemon host, so the image has to
    // land there. A desktop temp path would mean nothing to that shell, so a
    // failed upload is an error rather than a local fallback.
    if let TerminalLauncher::Daemon(daemon) = launcher {
        let response = daemon.client().request(
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            waku_client::Command::StoreBlob {
                mime_type: format.mime_type().to_owned(),
                bytes,
            },
        )?;
        return match response {
            waku_client::ResponsePayload::BlobStored { path, .. } => Ok(path),
            _ => anyhow::bail!("Waku daemon returned an invalid blob response"),
        };
    }

    let ext = match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        _ => "png",
    };
    let file_name = format!(
        "waku-paste-{}-{}.{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..6],
        ext
    );
    let temp_dir = std::env::temp_dir();
    let temp_path = temp_dir.join(file_name);
    std::fs::write(&temp_path, bytes)?;
    Ok(temp_path)
}

fn join_shell_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| shell_quote_path(path))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The image format a copied file's extension names, for files that can be
/// uploaded to a daemon host the way a clipboard image is.
fn image_format_for_path(path: &Path) -> Option<ImageFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => return None,
    };
    ImageFormat::from_mime_type(mime)
}

/// What pasting copied files into a daemon terminal produced: host paths for
/// the images it uploaded, and how many files it had to leave behind.
#[derive(Debug, Default)]
struct UploadedPastePaths {
    paths: Vec<PathBuf>,
    skipped: usize,
}

/// Copied files live on this machine, not on the daemon host. Images are
/// uploaded like a clipboard image; anything else cannot be reached from the
/// remote shell and is skipped.
fn upload_pasted_paths(launcher: TerminalLauncher, paths: Vec<PathBuf>) -> UploadedPastePaths {
    let mut uploaded = UploadedPastePaths::default();
    for path in paths {
        let stored = image_format_for_path(&path).and_then(|format| {
            let bytes = std::fs::read(&path).ok()?;
            save_pasted_image(launcher.clone(), format, bytes).ok()
        });
        match stored {
            Some(stored) => uploaded.paths.push(stored),
            None => uploaded.skipped += 1,
        }
    }
    uploaded
}

/// Something a paste could not do that the user should hear about; the app
/// shows it as a toast.
pub struct TerminalNotice(pub String);

impl EventEmitter<TerminalNotice> for TerminalView {}

/// A program in an unfocused terminal rang the bell or posted a desktop
/// notification; the app relays it as a system notification.
pub struct TerminalAttention {
    pub title: String,
    pub body: String,
}

impl EventEmitter<TerminalAttention> for TerminalView {}

/// A file path link clicked in the terminal; the app opens workspace files
/// in its Files panel and reveals anything else in the system file manager.
pub struct TerminalOpenFile(pub PathBuf);

impl EventEmitter<TerminalOpenFile> for TerminalView {}

pub struct TerminalView {
    session: Option<TerminalSession>,
    launcher: TerminalLauncher,
    error: Option<String>,
    focus_handle: FocusHandle,
    working_directory: PathBuf,
    title: String,
    exited: bool,
    scroll_accumulator: f32,
    panel_width: f32,
    /// Height the terminal is allowed to fill. `None` keeps the right panel's
    /// assumption that the grid stretches from below the app header to the
    /// window bottom; the bottom panel supplies its own height so the PTY's
    /// row count matches what is actually visible.
    panel_height: Option<f32>,
    /// Advance width of one grid cell, measured from the terminal font on
    /// first render so grid math matches what `StyledText` actually lays out.
    measured_cell_width: Option<f32>,
    scrollbar_state: Rc<ScrollbarState>,
    grid_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    cursor_position: Option<(usize, usize)>,
    composition: Option<TerminalComposition>,
    selecting: bool,
    /// Viewport `(column, row)` last sent for an active mouse-reported press
    /// or drag when the running TUI has enabled [`TermMode::MOUSE_MODE`].
    mouse_reported_cell: Option<(usize, usize)>,
    hovered_link: Option<TerminalLink>,
    cursor_blink: gpui::Entity<TerminalCursorBlink>,
    cursor_focus_tracking_started: bool,
    /// Window active and this terminal focused, as last reported to the
    /// program through focus events (`CSI I` / `CSI O`).
    focused: bool,
    /// A bell or notification arrived while unfocused; cleared on focus.
    needs_attention: bool,
    context_menu: ContextMenuHandle,
    /// The bottom panel hosts this terminal behind its own tab strip: the
    /// built-in toolbar is dropped (the tab carries status and title) and
    /// ctrl+pageup/pagedown cycle the panel's terminals instead of reaching
    /// the shell.
    bottom_panel_tabs: bool,
    _subscriptions: Vec<Subscription>,
}

impl TerminalView {
    pub fn new(
        working_directory: PathBuf,
        launcher: TerminalLauncher,
        cx: &mut Context<Self>,
    ) -> Self {
        let terminal_cwd = working_directory.clone();
        let session_launcher = launcher.clone();
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async move { TerminalSession::new(&terminal_cwd, 52, 36, session_launcher) })
                .await;
            if this
                .update(cx, |this, cx| {
                    match started {
                        Ok(session) => this.session = Some(session),
                        Err(error) => this.error = Some(error.to_string()),
                    }
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(24))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.poll(cx) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let context_menu = ContextMenuHandle::new(cx);
        let cursor_blink = cx.new(|_| TerminalCursorBlink::new());
        let subscriptions = vec![cx.observe(&cursor_blink, |_, _, cx| cx.notify())];

        Self {
            session: None,
            launcher,
            error: None,
            focus_handle: cx.focus_handle(),
            title: tr!("right_panel.terminal"),
            working_directory,
            exited: false,
            scroll_accumulator: 0.0,
            panel_width: DEFAULT_RIGHT_PANEL_WIDTH,
            panel_height: None,
            measured_cell_width: None,
            scrollbar_state: ScrollbarState::new(),
            grid_bounds: Rc::new(Cell::new(None)),
            cursor_position: None,
            composition: None,
            selecting: false,
            mouse_reported_cell: None,
            hovered_link: None,
            cursor_blink,
            cursor_focus_tracking_started: false,
            focused: false,
            needs_attention: false,
            context_menu,
            bottom_panel_tabs: false,
            _subscriptions: subscriptions,
        }
    }

    /// An entity with no session and no subscriptions, for tests outside
    /// this module that need terminals without a live PTY.
    #[cfg(test)]
    pub(crate) fn inert_test_view_entity(cx: &mut App) -> gpui::Entity<Self> {
        cx.new(|cx| Self {
            session: None,
            launcher: TerminalLauncher::Local,
            error: None,
            focus_handle: cx.focus_handle(),
            title: String::new(),
            working_directory: PathBuf::new(),
            exited: false,
            scroll_accumulator: 0.0,
            panel_width: 320.0,
            panel_height: Some(160.0),
            measured_cell_width: Some(8.0),
            scrollbar_state: ScrollbarState::new(),
            grid_bounds: Rc::new(Cell::new(None)),
            cursor_position: None,
            composition: None,
            selecting: false,
            mouse_reported_cell: None,
            hovered_link: None,
            cursor_blink: cx.new(|_| TerminalCursorBlink::new()),
            cursor_focus_tracking_started: false,
            focused: false,
            needs_attention: false,
            context_menu: ContextMenuHandle::new(cx),
            bottom_panel_tabs: true,
            _subscriptions: Vec::new(),
        })
    }

    /// Let the bottom panel's tab strip own this terminal's chrome.
    pub fn with_bottom_panel_tabs(mut self) -> Self {
        self.bottom_panel_tabs = true;
        self
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    /// The shell's OSC title, or the localized default while the shell has
    /// not set one.
    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn exited(&self) -> bool {
        self.exited
    }

    /// Whether a bell or notification arrived since the terminal last had
    /// focus.
    pub fn needs_attention(&self) -> bool {
        self.needs_attention
    }

    pub fn set_panel_width(&mut self, width: f32) {
        self.panel_width = width;
    }

    pub fn set_panel_height(&mut self, height: f32) {
        self.panel_height = Some(height);
    }

    pub fn refresh_localized_text(&mut self, cx: &mut Context<Self>) {
        if matches!(self.title.as_str(), "Terminal" | "终端") {
            self.title = tr!("right_panel.terminal");
            cx.notify();
        }
    }

    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        let mut changed = session.take_dirty();
        while let Ok(event) = session.ui_events.try_recv() {
            changed = true;
            match event {
                TerminalUiEvent::Title(title) => self.title = title,
                TerminalUiEvent::ResetTitle => self.title = tr!("right_panel.terminal"),
                TerminalUiEvent::ClipboardStore(text) => {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                }
                TerminalUiEvent::ClipboardLoad(formatter) => {
                    let text = cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .unwrap_or_default();
                    session.write(formatter(&text).into_bytes());
                }
                TerminalUiEvent::Alert(alert) => {
                    // Programs ring and notify for whoever is not watching;
                    // repeated bells while already flagged stay quiet.
                    if self.focused || (alert.is_none() && self.needs_attention) {
                        continue;
                    }
                    self.needs_attention = true;
                    let alert = alert.unwrap_or_else(|| TerminalAlert {
                        title: None,
                        body: tr!("terminal.needs_attention"),
                    });
                    cx.emit(TerminalAttention {
                        title: alert.title.unwrap_or_else(|| self.title.clone()),
                        body: alert.body,
                    });
                }
                TerminalUiEvent::Exited => self.exited = true,
            }
        }
        changed
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.pause_cursor_blink(cx);
        let keystroke = &event.keystroke;

        // Split, close-pane, and resize chords reach the panel's actions from
        // the focused pane. Only the bottom panel can split, so other
        // terminals leave these chords to the shell.
        if self.bottom_panel_tabs
            && let Some(chord) = bottom_panel_pane_chord(keystroke)
        {
            match chord {
                PaneChord::SplitRight => cx.dispatch_action(&crate::SplitTerminalRight),
                PaneChord::SplitDown => cx.dispatch_action(&crate::SplitTerminalDown),
                PaneChord::ClosePane => cx.dispatch_action(&crate::CloseTerminalPane),
                PaneChord::ResizeLeft => cx.dispatch_action(&crate::ResizeTerminalPaneLeft),
                PaneChord::ResizeRight => cx.dispatch_action(&crate::ResizeTerminalPaneRight),
                PaneChord::ResizeUp => cx.dispatch_action(&crate::ResizeTerminalPaneUp),
                PaneChord::ResizeDown => cx.dispatch_action(&crate::ResizeTerminalPaneDown),
            }
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        // The platform owns candidate navigation/confirmation while composing.
        // Printable input must also reach its text handler, otherwise dead keys
        // and IMEs are bypassed (or ordinary characters get sent twice).
        if self.composition.is_some() || terminal_uses_text_input(event) {
            return;
        }
        // The bottom panel's terminals leave ctrl+pageup/pagedown unhandled so
        // the panel's tab-cycle key bindings (bound in the `Terminal` context)
        // receive them instead of the shell.
        if self.bottom_panel_tabs
            && keystroke.modifiers.control
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.shift
            && !keystroke.modifiers.platform
            && !keystroke.modifiers.function
            && matches!(keystroke.key.as_str(), "pageup" | "pagedown")
        {
            return;
        }
        if terminal_clipboard_modifier_pressed(&keystroke.modifiers)
            && keystroke.key.eq_ignore_ascii_case("c")
        {
            self.copy_selection(cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if terminal_clipboard_modifier_pressed(&keystroke.modifiers)
            && keystroke.key.eq_ignore_ascii_case("a")
        {
            self.select_all(cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        let Some(session) = &self.session else {
            return;
        };
        if terminal_clipboard_modifier_pressed(&keystroke.modifiers)
            && keystroke.key.eq_ignore_ascii_case("v")
        {
            self.paste_from_clipboard(cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        let mode = session.mode();
        if let Some(bytes) = kitty_key_bytes(keystroke, event.is_held, mode)
            .or_else(|| terminal_key_bytes(keystroke, mode))
        {
            session.term.lock().selection = None;
            session.write(bytes);
            session.dirty.store(true, Ordering::Release);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    fn cell_width(&self) -> f32 {
        self.measured_cell_width.unwrap_or(TERMINAL_CELL_WIDTH)
    }

    fn grid_point_for_position(
        &self,
        position: Point<Pixels>,
        clamp_to_grid: bool,
    ) -> Option<(TerminalPoint, Side)> {
        let bounds = self.grid_bounds.get()?;
        let session = self.session.as_ref()?;
        let display_offset = session.term.lock().grid().display_offset();
        terminal_grid_point(
            bounds,
            position,
            self.cell_width(),
            session.grid_size.0,
            session.grid_size.1,
            display_offset,
            clamp_to_grid,
        )
    }

    fn viewport_cell_for_position(
        &self,
        position: Point<Pixels>,
        clamp_to_grid: bool,
    ) -> Option<(usize, usize)> {
        let bounds = self.grid_bounds.get()?;
        let session = self.session.as_ref()?;
        terminal_grid_point(
            bounds,
            position,
            self.cell_width(),
            session.grid_size.0,
            session.grid_size.1,
            0,
            clamp_to_grid,
        )
        .map(|(point, _)| (point.column.0, point.line.0.max(0) as usize))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The overlay scrollbar's listeners already ran (bubble order is
        // reverse registration and it paints above the grid) but do not stop
        // propagation; a grab or track click must not also start a selection
        // underneath the bar.
        if self.scrollbar_state.engaged() {
            return;
        }
        window.focus(&self.focus_handle, cx);
        let Some((point, side)) = self.grid_point_for_position(event.position, false) else {
            return;
        };

        if primary_modifier_pressed(&event.modifiers)
            && let Some(link) = self
                .session
                .as_mut()
                .and_then(|session| session.link_at(point))
            && let Some(target) =
                terminal_link_target(&link.value, self.working_directory.as_path())
        {
            self.selecting = false;
            self.mouse_reported_cell = None;
            self.hovered_link = Some(link);
            match target {
                TerminalLinkTarget::Url(url) => cx.open_url(&url),
                TerminalLinkTarget::File(path) => cx.emit(TerminalOpenFile(path)),
            }
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if let Some(cell) = self.viewport_cell_for_position(event.position, false)
            && let Some(session) = &self.session
            && let Some(bytes) =
                terminal_mouse_button_input(0, true, cell, &event.modifiers, session.mode())
        {
            self.selecting = false;
            self.mouse_reported_cell = Some(cell);
            session.write(bytes);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        let Some(session) = &self.session else {
            return;
        };

        let mut term = session.term.lock();
        if event.modifiers.shift
            && let Some(selection) = term.selection.as_mut()
        {
            selection.update(point, side);
        } else {
            let selection_type = match event.click_count {
                2 => SelectionType::Semantic,
                count if count >= 3 => SelectionType::Lines,
                _ => SelectionType::Simple,
            };
            term.selection = Some(Selection::new(selection_type, point, side));
        }
        drop(term);

        self.selecting = true;
        self.mouse_reported_cell = None;
        session.dirty.store(true, Ordering::Release);
        cx.stop_propagation();
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.mouse_reported_cell.is_some() {
            if event.pressed_button == Some(MouseButton::Left)
                && let Some(cell) = self.viewport_cell_for_position(event.position, true)
                && let Some(session) = &self.session
            {
                if self.mouse_reported_cell != Some(cell) {
                    self.mouse_reported_cell = Some(cell);
                    if let Some(bytes) =
                        terminal_mouse_move_input(0, cell, &event.modifiers, session.mode())
                    {
                        session.write(bytes);
                    }
                }
                cx.stop_propagation();
            }
            return;
        }

        let hover_changed = if self.selecting {
            self.set_hovered_link(None)
        } else {
            self.refresh_hovered_link(primary_modifier_pressed(&event.modifiers), event.position)
        };
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            if hover_changed {
                cx.notify();
            }
            return;
        }
        let Some((point, side)) = self.grid_point_for_position(event.position, true) else {
            return;
        };
        let Some(session) = &self.session else {
            return;
        };

        if let Some(selection) = session.term.lock().selection.as_mut() {
            selection.update(point, side);
            session.dirty.store(true, Ordering::Release);
            cx.stop_propagation();
            cx.notify();
        } else if hover_changed {
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
        if let Some(last_cell) = self.mouse_reported_cell.take()
            && let Some(session) = &self.session
        {
            let cell = self
                .viewport_cell_for_position(event.position, true)
                .unwrap_or(last_cell);
            if let Some(bytes) =
                terminal_mouse_button_input(0, false, cell, &event.modifiers, session.mode())
            {
                session.write(bytes);
            }
        }
    }

    fn on_mouse_exit(&mut self, _: &MouseExitEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.set_hovered_link(None) {
            cx.notify();
        }
    }

    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refresh_hovered_link(
            primary_modifier_pressed(&event.modifiers),
            window.mouse_position(),
        ) {
            cx.notify();
        }
    }

    fn refresh_hovered_link(&mut self, command_pressed: bool, position: Point<Pixels>) -> bool {
        let link = if command_pressed {
            self.grid_point_for_position(position, false)
                .and_then(|(point, _)| self.session.as_mut()?.link_at(point))
        } else {
            None
        };
        self.set_hovered_link(link)
    }

    fn set_hovered_link(&mut self, link: Option<TerminalLink>) -> bool {
        if self.hovered_link == link {
            return false;
        }
        self.hovered_link = link;
        true
    }

    fn selected_text(&self) -> Option<String> {
        self.session
            .as_ref()?
            .term
            .lock()
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    fn copy_selection(&self, cx: &mut App) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, text: String, cx: &mut Context<Self>) {
        self.pause_cursor_blink(cx);
        let Some(session) = &self.session else {
            return;
        };
        session.term.lock().selection = None;
        session.write(bracketed_paste(text, session.mode()));
        session.dirty.store(true, Ordering::Release);
        cx.notify();
    }

    fn paste_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() || self.session.is_none() {
            return;
        }
        if matches!(self.launcher, TerminalLauncher::Local) {
            self.paste(join_shell_paths(&paths), cx);
            return;
        }
        self.pause_cursor_blink(cx);
        let launcher = self.launcher.clone();
        cx.spawn(async move |this, cx| {
            let uploaded = cx
                .background_executor()
                .spawn(async move { upload_pasted_paths(launcher, paths) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !uploaded.paths.is_empty() {
                    this.paste(join_shell_paths(&uploaded.paths), cx);
                }
                if uploaded.skipped > 0 {
                    cx.emit(TerminalNotice(tr!(
                        "terminal.paste_files_skipped",
                        count = uploaded.skipped
                    )));
                }
            });
        })
        .detach();
    }

    pub fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        let Some(content) = extract_terminal_paste_content(&clipboard) else {
            return;
        };

        match content {
            TerminalPasteContent::Text(text) => {
                self.paste(text, cx);
            }
            TerminalPasteContent::Paths(paths) => {
                self.paste_paths(paths, cx);
            }
            TerminalPasteContent::Image { format, bytes } => {
                self.pause_cursor_blink(cx);
                let launcher = self.launcher.clone();
                cx.spawn(async move |this, cx| {
                    let saved_path = cx
                        .background_executor()
                        .spawn(async move { save_pasted_image(launcher, format, bytes) })
                        .await;
                    let _ = this.update(cx, |this, cx| match saved_path {
                        Ok(path) => this.paste(shell_quote_path(&path), cx),
                        Err(error) => cx.emit(TerminalNotice(tr!(
                            "terminal.paste_image_failed",
                            error = error.to_string()
                        ))),
                    });
                })
                .detach();
            }
        }
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let mut term = session.term.lock();
        let start = TerminalPoint::new(term.topmost_line(), Column(0));
        let end = TerminalPoint::new(term.bottommost_line(), term.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
        drop(term);
        session.dirty.store(true, Ordering::Release);
        cx.notify();
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        let delta = match event.delta {
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / TERMINAL_CELL_HEIGHT,
            ScrollDelta::Lines(delta) => delta.y,
        };
        self.scroll_accumulator += delta;
        let lines = self.scroll_accumulator.trunc() as i32;
        if lines != 0 {
            self.scroll_accumulator -= lines as f32;
            let cell = self.viewport_cell_for_position(event.position, true);
            match terminal_wheel_input(lines, cell, &event.modifiers, session.mode()) {
                Some(bytes) => session.write(bytes),
                None => session.scroll(lines),
            }
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn ensure_cursor_focus_tracking(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cursor_focus_tracking_started {
            return;
        }
        self.cursor_focus_tracking_started = true;

        let focus_handle = self.focus_handle.clone();
        self._subscriptions.extend([
            cx.observe_window_activation(window, |terminal, window, cx| {
                terminal.update_cursor_blinking(window, cx);
            }),
            cx.on_focus(&focus_handle, window, |terminal, window, cx| {
                terminal.update_cursor_blinking(window, cx);
            }),
            cx.on_blur(&focus_handle, window, |terminal, window, cx| {
                terminal.composition = None;
                terminal.update_cursor_blinking(window, cx);
                cx.notify();
            }),
        ]);
        self.update_cursor_blinking(window, cx);
    }

    fn update_cursor_blinking(&mut self, window: &Window, cx: &mut Context<Self>) {
        let focused = window.is_window_active() && self.focus_handle.is_focused(window);
        if focused != self.focused {
            self.focused = focused;
            if focused && self.needs_attention {
                self.needs_attention = false;
                cx.notify();
            }
            if let Some(session) = &self.session
                && session.mode().contains(TermMode::FOCUS_IN_OUT)
            {
                session.write(if focused {
                    &b"\x1b[I"[..]
                } else {
                    &b"\x1b[O"[..]
                });
            }
        }
        self.cursor_blink.update(cx, |cursor, cx| {
            if focused {
                cursor.start(cx);
            } else {
                cursor.stop(cx);
            }
        });
    }

    fn pause_cursor_blink(&mut self, cx: &mut Context<Self>) {
        self.cursor_blink.update(cx, |cursor, cx| cursor.pause(cx));
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        // The shell owns committed text; the terminal has no editable document.
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self
                .composition
                .as_ref()
                .map_or(0..0, |ime| ime.selection.clone()),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.composition
            .as_ref()
            .map(|ime| 0..ime.text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.composition = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composition = None;
        self.pause_cursor_blink(cx);
        if !text.is_empty()
            && let Some(session) = &self.session
        {
            session.term.lock().selection = None;
            session.write(text.as_bytes().to_vec());
            session.dirty.store(true, Ordering::Release);
        }
        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Pre-edit stays local until the IME commits it. Sending pinyin to
        // the PTY here would let the shell execute unconfirmed input.
        self.composition = (!text.is_empty()).then(|| {
            let len = text.encode_utf16().count();
            TerminalComposition {
                text: text.to_owned(),
                selection: selection
                    .map_or(len..len, |range| range.start.min(len)..range.end.min(len)),
            }
        });
        self.pause_cursor_blink(cx);
        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let grid = self.grid_bounds.get()?;
        let (row, column) = self.cursor_position?;
        Some(Bounds::new(
            grid.origin
                + gpui::point(
                    px((column + range.start) as f32 * self.cell_width()),
                    px(row as f32 * TERMINAL_CELL_HEIGHT),
                ),
            gpui::size(px(self.cell_width()), px(TERMINAL_CELL_HEIGHT)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        self.session.is_some() && !self.exited
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_cursor_focus_tracking(window, cx);
        let theme = Theme::current(cx);
        let selection_color = theme.selection;
        let viewport = window.viewport_size();
        let panel_width = self.panel_width;
        let toolbar_height = if self.bottom_panel_tabs {
            0.0
        } else {
            TERMINAL_TOOLBAR_HEIGHT
        };
        let body_height = match self.panel_height {
            Some(height) => (height - toolbar_height).max(120.0),
            None => (f32::from(viewport.height) - 48.0 - toolbar_height).max(120.0),
        };
        // The rows are laid out by `StyledText` at the font's own advance, so
        // the grid must be sized from that same measured advance or the text
        // wraps short of (or past) the panel edge.
        let cell_width = *self.measured_cell_width.get_or_insert_with(|| {
            let text_system = cx.text_system();
            let font_id = text_system.resolve_font(&terminal_font());
            text_system
                .advance(font_id, px(TERMINAL_FONT_SIZE), 'm')
                .map_or(TERMINAL_CELL_WIDTH, |advance| f32::from(advance.width))
        });
        let columns = ((panel_width - TERMINAL_PADDING_X * 2.0) / cell_width)
            .floor()
            .max(TERMINAL_MIN_COLUMNS as f32) as usize;
        let rows = ((body_height - TERMINAL_PADDING_Y * 2.0) / TERMINAL_CELL_HEIGHT)
            .floor()
            .max(TERMINAL_MIN_ROWS as f32) as usize;

        let terminal_focused = window.is_window_active() && self.focus_handle.is_focused(window);
        let cursor_style =
            terminal_cursor_style(terminal_focused, self.cursor_blink.read(cx).visible());
        if let Some(session) = self.session.as_mut() {
            session.resize(columns, rows, cell_width);
        }
        if self.selecting {
            self.set_hovered_link(None);
        } else {
            self.refresh_hovered_link(
                primary_modifier_pressed(&window.modifiers()),
                window.mouse_position(),
            );
        }
        let hovered_link = self.hovered_link.as_ref().map(|link| &link.bounds);
        let snapshot = self
            .session
            .as_ref()
            .map(|session| session.snapshot(theme, selection_color, cursor_style, hovered_link));
        self.cursor_position = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.cursor_position);
        let title = if self.title.trim().is_empty() {
            tr!("right_panel.terminal")
        } else {
            self.title.clone()
        };
        let directory = self
            .working_directory
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| tr!("workspace.workspace"));

        let mut screen = div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .relative()
            .cursor_text()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_exit(cx.listener(Self::on_mouse_exit))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed));

        if self.hovered_link.is_some() {
            screen = screen.cursor_pointer();
        }

        if let Some(snapshot) = snapshot {
            let TerminalSnapshot {
                rows: snapshot_rows,
                outline_cursor,
                cursor_position,
            } = &*snapshot;
            for row in snapshot_rows {
                let runs = row
                    .runs
                    .iter()
                    .map(|run| {
                        let mut run_font = terminal_font();
                        if run.style.bold {
                            run_font.weight = FontWeight::BOLD;
                        }
                        if run.style.italic {
                            run_font.style = FontStyle::Italic;
                        }
                        TextRun {
                            len: run.len,
                            font: run_font,
                            color: run.style.foreground,
                            background_color: Some(run.style.background),
                            underline: run.style.underline.then_some(UnderlineStyle {
                                thickness: px(1.0),
                                color: Some(run.style.foreground),
                                wavy: false,
                            }),
                            strikethrough: run.style.strikeout.then_some(StrikethroughStyle {
                                thickness: px(1.0),
                                color: Some(run.style.foreground),
                            }),
                        }
                    })
                    .collect::<Vec<_>>();
                screen = screen.child(
                    div()
                        .h(px(TERMINAL_CELL_HEIGHT))
                        .flex_none()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(TERMINAL_FONT_SIZE))
                        .line_height(px(TERMINAL_CELL_HEIGHT))
                        .child(StyledText::new(row.text.clone()).with_runs(runs)),
                );
            }
            if let Some((row, column)) = *outline_cursor {
                screen = screen.child(
                    div()
                        .absolute()
                        .left(px(column as f32 * cell_width))
                        .top(px(row as f32 * TERMINAL_CELL_HEIGHT))
                        .w(px(cell_width))
                        .h(px(TERMINAL_CELL_HEIGHT))
                        .border_1()
                        .border_color(theme.text),
                );
            }
            if terminal_focused
                && let Some(ime) = &self.composition
                && let Some((row, column)) = *cursor_position
            {
                screen = screen.child(
                    div()
                        .absolute()
                        .left(px(column as f32 * cell_width))
                        .top(px(row as f32 * TERMINAL_CELL_HEIGHT))
                        .h(px(TERMINAL_CELL_HEIGHT))
                        .bg(theme.terminal)
                        .text_size(px(TERMINAL_FONT_SIZE))
                        .line_height(px(TERMINAL_CELL_HEIGHT))
                        .whitespace_nowrap()
                        .child(StyledText::new(ime.text.clone()).with_runs(vec![TextRun {
                            len: ime.text.len(),
                            font: terminal_font(),
                            color: theme.text,
                            background_color: None,
                            underline: Some(UnderlineStyle {
                                thickness: px(1.0),
                                color: Some(theme.text),
                                wavy: false,
                            }),
                            strikethrough: None,
                        }])),
                );
            }
        } else {
            screen = screen.child(
                div()
                    .p(px(12.0))
                    .text_size(sp(12.5))
                    .line_height(sp(17.0))
                    .text_color(if self.error.is_some() {
                        theme.danger
                    } else {
                        theme.text_tertiary
                    })
                    .child(
                        self.error
                            .clone()
                            .unwrap_or_else(|| tr!("terminal.starting")),
                    ),
            );
        }

        let grid_bounds = self.grid_bounds.clone();
        let input_view = cx.entity();
        let input_focus = self.focus_handle.clone();
        screen = screen.child(
            canvas(
                move |bounds, _, _| grid_bounds.set(Some(bounds)),
                move |bounds, _, window, cx| {
                    window.handle_input(
                        &input_focus,
                        ElementInputHandler::new(bounds, input_view),
                        cx,
                    );
                },
            )
            .absolute()
            .inset_0(),
        );

        let context_terminal = cx.entity();
        let screen = context_menu(
            div().size_full().child(screen),
            "terminal-context-menu",
            &self.context_menu,
            move |cx| {
                let has_selection = context_terminal.read(cx).selected_text().is_some();
                let can_paste = cx
                    .read_from_clipboard()
                    .as_ref()
                    .is_some_and(clipboard_has_paste_content);
                let has_session = context_terminal.read(cx).session.is_some();
                let bottom_panel_tabs = context_terminal.read(cx).bottom_panel_tabs;

                let copy_terminal = context_terminal.clone();
                let paste_terminal = context_terminal.clone();
                let select_all_terminal = context_terminal.clone();
                let mut items = vec![
                    MenuItem::new(tr!("menu.copy"), move |_, cx| {
                        let selected_text = { copy_terminal.read(cx).selected_text() };
                        if let Some(text) = selected_text {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    })
                    .disabled(!has_selection),
                    MenuItem::new(tr!("menu.paste"), move |_, cx| {
                        paste_terminal.update(cx, |terminal, cx| terminal.paste_from_clipboard(cx));
                    })
                    .disabled(!can_paste),
                    MenuItem::Separator,
                    MenuItem::new(tr!("menu.select_all"), move |_, cx| {
                        select_all_terminal.update(cx, |terminal, cx| terminal.select_all(cx));
                    })
                    .disabled(!has_session),
                ];
                if bottom_panel_tabs {
                    let split_right_terminal = context_terminal.clone();
                    let split_down_terminal = context_terminal.clone();
                    let close_pane_terminal = context_terminal.clone();
                    items.extend([
                        MenuItem::Separator,
                        MenuItem::new(tr!("bottom_panel.split_right"), move |window, cx| {
                            let focus = split_right_terminal.read(cx).focus_handle(cx);
                            window.focus(&focus, cx);
                            cx.dispatch_action(&crate::SplitTerminalRight);
                        })
                        .icon("icons/split.svg"),
                        MenuItem::new(tr!("bottom_panel.split_down"), move |window, cx| {
                            let focus = split_down_terminal.read(cx).focus_handle(cx);
                            window.focus(&focus, cx);
                            cx.dispatch_action(&crate::SplitTerminalDown);
                        })
                        .icon("icons/panel-bottom.svg"),
                        MenuItem::new(tr!("bottom_panel.close_pane"), move |window, cx| {
                            let focus = close_pane_terminal.read(cx).focus_handle(cx);
                            window.focus(&focus, cx);
                            cx.dispatch_action(&crate::CloseTerminalPane);
                        })
                        .icon("icons/x.svg"),
                    ]);
                }
                items
            },
        );

        let scrollbar = self.session.as_ref().map(|session| {
            scrollbar::vertical(
                &TerminalScrollbarTarget {
                    term: session.term.clone(),
                    dirty: session.dirty.clone(),
                    viewport_rows: session.grid_size.1,
                },
                &self.scrollbar_state,
            )
        });

        let grid = div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .px(px(TERMINAL_PADDING_X))
            .py(px(TERMINAL_PADDING_Y))
            .bg(theme.terminal)
            .overflow_hidden()
            .flex()
            .flex_col()
            .relative()
            .child(screen)
            .children(scrollbar);

        div()
            .id("alacritty-terminal")
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .size_full()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme.terminal)
            .when(!self.bottom_panel_tabs, |element| {
                element.child(
                    div()
                        .h(px(TERMINAL_TOOLBAR_HEIGHT))
                        .flex_none()
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .child(
                            div()
                                .w(px(6.0))
                                .h(px(6.0))
                                .rounded_full()
                                .bg(if self.exited {
                                    theme.danger
                                } else if self.needs_attention {
                                    theme.warning
                                } else {
                                    theme.accent
                                }),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(sp(12.5))
                                .text_color(theme.text_secondary)
                                .child(SharedString::from(title.to_owned())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(sp(12.5))
                                .text_color(theme.text_tertiary)
                                .child(directory),
                        ),
                )
            })
            .child(grid)
            .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| {
                style.bg(theme.accent.opacity(0.08))
            })
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    this.paste_paths(paths.paths().to_vec(), cx);
                }),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
    }
}

fn terminal_cursor_style(focused: bool, blink_visible: bool) -> TerminalCursorStyle {
    if !focused {
        TerminalCursorStyle::Outline
    } else if blink_visible {
        TerminalCursorStyle::Solid
    } else {
        TerminalCursorStyle::Hidden
    }
}

fn terminal_grid_point(
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    cell_width: f32,
    columns: usize,
    rows: usize,
    display_offset: usize,
    clamp_to_grid: bool,
) -> Option<(TerminalPoint, Side)> {
    if columns == 0 || rows == 0 || (!clamp_to_grid && !bounds.contains(&position)) {
        return None;
    }

    let x = f32::from(position.x - bounds.origin.x);
    let y = f32::from(position.y - bounds.origin.y);
    let max_x = columns as f32 * cell_width;
    let max_y = rows as f32 * TERMINAL_CELL_HEIGHT;
    let x = x.clamp(0.0, max_x);
    let y = y.clamp(0.0, max_y);
    let column = ((x / cell_width).floor() as usize).min(columns - 1);
    let viewport_row = ((y / TERMINAL_CELL_HEIGHT).floor() as usize).min(rows - 1) as i32;
    let side = if x >= max_x || x % cell_width >= cell_width / 2.0 {
        Side::Right
    } else {
        Side::Left
    };
    let line = Line(viewport_row - display_offset.min(i32::MAX as usize) as i32);

    Some((TerminalPoint::new(line, Column(column)), side))
}

/// Apply the same delimiter heuristics as Alacritty's hint system.
fn post_process_terminal_link<T: EventListener>(
    term: &Term<T>,
    regex_match: &Match,
) -> Option<Match> {
    let mut iter = term.grid().iter_from(*regex_match.start());
    let mut character = iter.cell().c;
    let end = *regex_match.end();
    let mut open_parens = 0;
    let mut open_brackets = 0;

    loop {
        match character {
            '(' => open_parens += 1,
            '[' => open_brackets += 1,
            ')' if open_parens == 0 => {
                iter.prev();
                break;
            }
            ')' => open_parens -= 1,
            ']' if open_brackets == 0 => {
                iter.prev();
                break;
            }
            ']' => open_brackets -= 1,
            _ => {}
        }

        if iter.point() == end {
            break;
        }
        let Some(indexed) = iter.next() else {
            break;
        };
        character = indexed.cell.c;
    }

    let start = *regex_match.start();
    while iter.point() != start {
        if !matches!(
            character,
            '.' | ',' | ':' | ';' | '?' | '!' | '(' | '[' | '\''
        ) {
            break;
        }
        let Some(indexed) = iter.prev() else {
            break;
        };
        character = indexed.cell.c;
    }

    (start <= iter.point()).then(|| start..=iter.point())
}

fn plain_link_at<T: EventListener>(
    term: &Term<T>,
    regex: &mut RegexSearch,
    point: TerminalPoint,
) -> Option<(String, Match)> {
    let mut start = term.line_search_left(point);
    let mut end = term.line_search_right(point);
    start.line = start.line.max(point.line - MAX_TERMINAL_LINK_SEARCH_LINES);
    end.line = end.line.min(point.line + MAX_TERMINAL_LINK_SEARCH_LINES);

    let raw_match = RegexIter::new(start, end, Direction::Right, term, regex)
        .find(|bounds| bounds.contains(&point))?;
    let raw_end = *raw_match.end();
    let mut next_match = Some(raw_match);

    // Post-processing can split a greedy match at an unmatched closing
    // bracket. Continue inside the original range in case the clicked URL is
    // a later segment of that match.
    while let Some(regex_match) = next_match {
        let processed = post_process_terminal_link(term, &regex_match);
        if processed
            .as_ref()
            .is_some_and(|bounds| bounds.contains(&point))
        {
            let bounds = processed.unwrap();
            let value = term.bounds_to_string(*bounds.start(), *bounds.end());
            return Some((value, bounds));
        }

        let next_start = processed
            .as_ref()
            .map_or_else(|| *regex_match.start(), |bounds| *bounds.end())
            .add(term, Boundary::Grid, 1);
        if next_start > raw_end {
            return None;
        }
        next_match = term.regex_search_right(regex, next_start, raw_end);
    }

    None
}

fn hyperlink_at<T: EventListener>(term: &Term<T>, point: TerminalPoint) -> Option<(String, Match)> {
    let hyperlink = term.grid()[point].hyperlink()?;
    let grid = term.grid();

    let mut end = point;
    for cell in grid.iter_from(point) {
        if cell.hyperlink().as_ref() == Some(&hyperlink) {
            end = cell.point;
        } else {
            break;
        }
    }

    let mut start = point;
    let mut iter = grid.iter_from(point);
    while let Some(cell) = iter.prev() {
        if cell.hyperlink().as_ref() == Some(&hyperlink) {
            start = cell.point;
        } else {
            break;
        }
    }

    Some((hyperlink.uri().to_owned(), start..=end))
}

fn terminal_link_target(value: &str, working_directory: &Path) -> Option<TerminalLinkTarget> {
    if let Some(path) = existing_terminal_file_path(value, working_directory) {
        return Some(TerminalLinkTarget::File(path));
    }

    let url = url::Url::parse(value).ok()?;
    if url.scheme() == "file" {
        return None;
    }
    Some(TerminalLinkTarget::Url(url.to_string()))
}

fn existing_terminal_file_path(value: &str, working_directory: &Path) -> Option<PathBuf> {
    let mut path = match url::Url::parse(value) {
        Ok(url) if url.scheme() == "file" => url.to_file_path().ok()?,
        Ok(_) => return None,
        Err(_) => {
            if let Some(relative) = value.strip_prefix("~/") {
                dirs::home_dir()?.join(relative)
            } else {
                let path = Path::new(value);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    working_directory.join(path)
                }
            }
        }
    };

    loop {
        if path.exists() {
            return Some(path);
        }
        let value = path.to_string_lossy();
        let (prefix, suffix) = value.rsplit_once(':')?;
        if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        path = PathBuf::from(prefix);
    }
}

/// Full-screen TUIs that request mouse reporting or run on the alternate
/// screen have no local scrollback to move, so the wheel must reach the
/// program instead, matching Alacritty and Zed. `None` scrolls the local
/// history; Shift forces that as the conventional escape hatch. `cell` is the
/// pointer's (column, viewport row).
fn terminal_wheel_input(
    lines: i32,
    cell: Option<(usize, usize)>,
    modifiers: &Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if lines == 0 || modifiers.shift {
        return None;
    }
    let count = lines.unsigned_abs() as usize;
    if mode.intersects(TermMode::MOUSE_MODE) {
        // An unencodable position is dropped rather than scrolling history
        // the application cannot see.
        let report = cell
            .and_then(|(column, row)| {
                let button = if lines > 0 { 64 } else { 65 }
                    + if modifiers.alt { 8 } else { 0 }
                    + if modifiers.control { 16 } else { 0 };
                mouse_report(button, column, row, mode)
            })
            .unwrap_or_default();
        return Some(report.repeat(count));
    }
    if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        let arrow: &[u8] = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
        return Some(arrow.repeat(count));
    }
    None
}

/// Mouse click and release sequences for TUIs that enable mouse reporting.
/// Shift bypasses reporting so the user can still select text locally.
fn terminal_mouse_button_input(
    button: u8,
    pressed: bool,
    (column, row): (usize, usize),
    modifiers: &Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if modifiers.shift || !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let mods = if modifiers.alt { 8 } else { 0 } + if modifiers.control { 16 } else { 0 };
    let code = if pressed || mode.contains(TermMode::SGR_MOUSE) {
        button + mods
    } else {
        3 + mods
    };
    mouse_report_with_state(code, pressed, column, row, mode)
}

/// Mouse drag sequences for TUIs that enable button-drag or any-motion
/// reporting (`1002` / `1003`).
fn terminal_mouse_move_input(
    button: u8,
    (column, row): (usize, usize),
    modifiers: &Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if modifiers.shift || !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
        return None;
    }
    let mods = if modifiers.alt { 8 } else { 0 } + if modifiers.control { 16 } else { 0 };
    mouse_report_with_state(button + 32 + mods, true, column, row, mode)
}

fn mouse_report(button: u8, column: usize, row: usize, mode: TermMode) -> Option<Vec<u8>> {
    mouse_report_with_state(button, true, column, row, mode)
}

fn mouse_report_with_state(
    button: u8,
    pressed: bool,
    column: usize,
    row: usize,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{button};{};{}{suffix}", column + 1, row + 1).into_bytes());
    }

    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    let max_position = if utf8 { 2015 } else { 223 };
    if column >= max_position || row >= max_position {
        return None;
    }
    let mut report = vec![0x1b, b'[', b'M', 32 + button];
    for position in [column, row] {
        let encoded = 32 + 1 + position as u32;
        if utf8 && encoded >= 0x80 {
            report.push((0xC0 + encoded / 64) as u8);
            report.push((0x80 + (encoded & 63)) as u8);
        } else {
            report.push(encoded as u8);
        }
    }
    Some(report)
}

fn bracketed_paste(text: String, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

fn terminal_uses_text_input(event: &KeyDownEvent) -> bool {
    let modifiers = event.keystroke.modifiers;
    event.prefer_character_input
        || (event
            .keystroke
            .key_char
            .as_ref()
            .is_some_and(|text| !text.is_empty() && !text.chars().any(char::is_control))
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform)
}

/// A split, close, or resize chord for the bottom panel's pane tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneChord {
    SplitRight,
    SplitDown,
    ClosePane,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
}

/// The pane chord a keydown carries, whatever spelling the platform reports.
///
/// Windows folds a shifted punctuation key into the character it produces and
/// clears the shift flag before the event reaches us: shift+= arrives as `+`
/// and shift+- as `_`, both without a shift modifier. Only that platform may
/// treat the unshifted `+`/`_` as the chord; on other layouts they are plain
/// shell input. Everywhere, the unshifted `=` and `-` stay with the shell.
fn bottom_panel_pane_chord(keystroke: &Keystroke) -> Option<PaneChord> {
    let modifiers = keystroke.modifiers;
    if modifiers.function {
        return None;
    }
    let key = keystroke.key.as_str();
    let key_char = keystroke.key_char.as_deref().unwrap_or("");

    if (modifiers.secondary() || modifiers.control) && modifiers.shift {
        if key.eq_ignore_ascii_case("d") {
            return Some(if modifiers.alt {
                PaneChord::SplitDown
            } else {
                PaneChord::SplitRight
            });
        }
        if key.eq_ignore_ascii_case("w") && !modifiers.alt {
            return Some(PaneChord::ClosePane);
        }
    }

    if !modifiers.alt {
        return None;
    }

    let shifted = modifiers.shift;
    // Only Windows folds a shifted punctuation key into an unshifted "+"/"_"
    // keystroke; on other layouts those keys are legitimate shell input
    // (e.g. alt+plus is a plain meta key on layouts where + is unshifted).
    let folded = cfg!(windows);
    let split_right = folded && matches!(key, "+" | "plus" | "add")
        || (shifted && (matches!(key, "=" | "equal") || key_char == "+"));
    let split_down = folded && matches!(key, "_" | "underscore")
        || (shifted && (matches!(key, "-" | "minus" | "subtract") || key_char == "_"));
    if split_right {
        return Some(PaneChord::SplitRight);
    }
    if split_down {
        return Some(PaneChord::SplitDown);
    }
    if shifted {
        return match key {
            "left" => Some(PaneChord::ResizeLeft),
            "right" => Some(PaneChord::ResizeRight),
            "up" => Some(PaneChord::ResizeUp),
            "down" => Some(PaneChord::ResizeDown),
            _ => None,
        };
    }
    None
}

/// Key bindings outrank key-down listeners, so an app shortcut on a key the
/// shell also uses (Escape for vim, Ctrl-L, Ctrl-W, Ctrl-[ ...) would never
/// reach the terminal. Unbind every such shortcut inside the `Terminal`
/// context, except the actions in `keep`, so the terminal's own key handler
/// sees them. Call it once, after all other bindings are registered.
pub fn shield_shell_keystrokes(cx: &mut App, keep: &[&dyn Action]) {
    let shields = shell_keystroke_shields(cx.key_bindings().borrow().bindings(), keep);
    cx.bind_keys(shields);
}

fn shell_keystroke_shields<'a>(
    bindings: impl IntoIterator<Item = &'a KeyBinding>,
    keep: &[&dyn Action],
) -> Vec<KeyBinding> {
    // Bindings scoped to the terminal itself (or deeper) were written for it;
    // only the ones that already apply above it can shadow the shell.
    let ancestors = [KeyContext::parse("Waku").expect("valid key context")];
    let mut seen = HashSet::new();
    bindings
        .into_iter()
        .filter_map(|binding| {
            let [keystroke] = binding.keystrokes() else {
                return None;
            };
            let keystroke = keystroke.inner();
            let shadows_shell = binding
                .predicate()
                .is_none_or(|predicate| predicate.eval(&ancestors))
                && !is_no_action(binding.action())
                && !keep
                    .iter()
                    .any(|action| action.partial_eq(binding.action()))
                && terminal_claims_keystroke(keystroke);
            let source = keystroke.unparse();
            (shadows_shell && seen.insert(source.clone()))
                .then(|| KeyBinding::new(&source, NoAction, Some("Terminal")))
        })
        .collect()
}

/// Whether the focused terminal would act on `keystroke` itself: its
/// clipboard chords, or any key it encodes for the shell.
fn terminal_claims_keystroke(keystroke: &Keystroke) -> bool {
    let modifiers = keystroke.modifiers;
    if terminal_clipboard_modifier_pressed(&modifiers)
        && ["a", "c", "v"]
            .iter()
            .any(|key| keystroke.key.eq_ignore_ascii_case(key))
    {
        return true;
    }
    // Ctrl+Shift+key reaches the shell as plain Ctrl+key, so off macOS those
    // chords stay app shortcuts, as in Windows Terminal and VS Code.
    if !cfg!(target_os = "macos") && modifiers.control && modifiers.shift {
        return false;
    }
    terminal_key_bytes(keystroke, TermMode::empty()).is_some()
}

/// The kitty keyboard protocol's `CSI code ; modifiers u` form for keys whose
/// legacy bytes are ambiguous, once a program has pushed the protocol;
/// `None` leaves the key to the legacy encoding, which the protocol keeps for
/// cursor and function keys and plain Enter/Tab/Backspace.
///
/// Implements disambiguation (flag 1), repeat events (2), shifted alternate
/// keys (4), and all-keys-as-escapes (8) for keys that reach `on_key_down`.
/// Plain printable keys still arrive as text through the input handler (so
/// IMEs keep working), and release events are not reported.
fn kitty_key_bytes(keystroke: &Keystroke, repeat: bool, mode: TermMode) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) {
        return None;
    }
    let modifiers = keystroke.modifiers;
    if modifiers.platform || modifiers.function {
        return None;
    }
    let all_as_escapes = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let modifier_value = 1
        + u32::from(modifiers.shift)
        + u32::from(modifiers.alt) * 2
        + u32::from(modifiers.control) * 4;
    let key = keystroke.key.as_str();
    let code = match key {
        "escape" => 27,
        "enter" | "return" | "tab" | "backspace" if modifier_value == 1 && !all_as_escapes => {
            return None;
        }
        "enter" | "return" => 13,
        "tab" => 9,
        "backspace" => 127,
        _ => {
            let character = if key == "space" {
                ' '
            } else {
                let mut chars = key.chars();
                let (Some(character), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                character
            };
            // Shift alone still types text; only Ctrl/Alt chords are ambiguous.
            if !all_as_escapes && !modifiers.control && !modifiers.alt {
                return None;
            }
            u32::from(character.to_ascii_lowercase())
        }
    };
    let shifted = (mode.contains(TermMode::REPORT_ALTERNATE_KEYS) && modifiers.shift)
        .then_some(keystroke.key_char.as_deref())
        .flatten()
        .and_then(|text| {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(character), None) if u32::from(character) != code => Some(character),
                _ => None,
            }
        });
    let event_type = (repeat && mode.contains(TermMode::REPORT_EVENT_TYPES)).then_some(":2");

    let mut sequence = format!("\x1b[{code}");
    if let Some(shifted) = shifted {
        sequence.push_str(&format!(":{}", u32::from(shifted)));
    }
    if modifier_value != 1 || event_type.is_some() {
        sequence.push_str(&format!(
            ";{modifier_value}{}",
            event_type.unwrap_or_default()
        ));
    }
    sequence.push('u');
    Some(sequence.into_bytes())
}

fn terminal_key_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    let key = keystroke.key.as_str();

    if modifiers.platform {
        #[cfg(target_os = "macos")]
        return match key {
            "left" => Some(vec![0x01]),
            "right" => Some(vec![0x05]),
            "backspace" => Some(vec![0x15]),
            _ => None,
        };

        #[cfg(not(target_os = "macos"))]
        return None;
    }

    let modifier = 1
        + u8::from(modifiers.shift)
        + u8::from(modifiers.alt) * 2
        + u8::from(modifiers.control) * 4;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    // Alone, each of Shift, Alt, and Ctrl gives Enter and Backspace the
    // distinct byte xterm and Alacritty send, so readline and TUIs can tell
    // them apart (Shift-Enter newline, Alt/Ctrl-Backspace word delete).
    let only = (modifiers.shift, modifiers.alt, modifiers.control);
    let special = match key {
        "enter" | "return" => Some(
            match only {
                (true, false, false) => "\n",
                (false, true, false) => "\x1b\r",
                _ => "\r",
            }
            .to_owned(),
        ),
        "tab" if modifiers.shift => Some("\x1b[Z".to_owned()),
        "tab" => Some("\t".to_owned()),
        "backspace" => Some(
            match only {
                (false, true, false) => "\x1b\x7f",
                (false, false, true) => "\x08",
                _ => "\x7f",
            }
            .to_owned(),
        ),
        "escape" => Some("\x1b".to_owned()),
        "up" => Some(cursor_sequence('A', modifier, app_cursor)),
        "down" => Some(cursor_sequence('B', modifier, app_cursor)),
        "right" => Some(cursor_sequence('C', modifier, app_cursor)),
        "left" => Some(cursor_sequence('D', modifier, app_cursor)),
        "home" => Some(cursor_sequence('H', modifier, app_cursor)),
        "end" => Some(cursor_sequence('F', modifier, app_cursor)),
        "insert" => Some(tilde_sequence(2, modifier)),
        "delete" | "forwarddelete" => Some(tilde_sequence(3, modifier)),
        "pageup" => Some(tilde_sequence(5, modifier)),
        "pagedown" => Some(tilde_sequence(6, modifier)),
        "f1" => Some(function_sequence('P', modifier)),
        "f2" => Some(function_sequence('Q', modifier)),
        "f3" => Some(function_sequence('R', modifier)),
        "f4" => Some(function_sequence('S', modifier)),
        "f5" => Some(tilde_sequence(15, modifier)),
        "f6" => Some(tilde_sequence(17, modifier)),
        "f7" => Some(tilde_sequence(18, modifier)),
        "f8" => Some(tilde_sequence(19, modifier)),
        "f9" => Some(tilde_sequence(20, modifier)),
        "f10" => Some(tilde_sequence(21, modifier)),
        "f11" => Some(tilde_sequence(23, modifier)),
        "f12" => Some(tilde_sequence(24, modifier)),
        "f13" => Some(tilde_sequence(25, modifier)),
        "f14" => Some(tilde_sequence(26, modifier)),
        "f15" => Some(tilde_sequence(28, modifier)),
        "f16" => Some(tilde_sequence(29, modifier)),
        "f17" => Some(tilde_sequence(31, modifier)),
        "f18" => Some(tilde_sequence(32, modifier)),
        "f19" => Some(tilde_sequence(33, modifier)),
        "f20" => Some(tilde_sequence(34, modifier)),
        _ => None,
    };
    if let Some(special) = special {
        return Some(special.into_bytes());
    }

    // Parsed keystrokes (key bindings) carry no key_char; spell out space so
    // Ctrl-Space still encodes as NUL.
    let text = keystroke
        .key_char
        .as_deref()
        .or_else(|| (key == "space").then_some(" "))
        .or_else(|| (key.chars().count() == 1).then_some(key))?;
    let mut bytes = if modifiers.control {
        control_bytes(text)?
    } else {
        text.as_bytes().to_vec()
    };
    if modifiers.alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn control_bytes(text: &str) -> Option<Vec<u8>> {
    let character = text.chars().next()?.to_ascii_lowercase();
    let byte = match character {
        ' ' | '@' => 0,
        'a'..='z' => character as u8 - b'a' + 1,
        '[' => 27,
        '\\' => 28,
        ']' => 29,
        '^' => 30,
        '_' => 31,
        '?' => 127,
        _ => return None,
    };
    Some(vec![byte])
}

fn cursor_sequence(final_byte: char, modifier: u8, app_cursor: bool) -> String {
    if modifier == 1 {
        format!("\x1b{}{}", if app_cursor { 'O' } else { '[' }, final_byte)
    } else {
        format!("\x1b[1;{modifier}{final_byte}")
    }
}

fn function_sequence(final_byte: char, modifier: u8) -> String {
    if modifier == 1 {
        format!("\x1bO{final_byte}")
    } else {
        format!("\x1b[1;{modifier}{final_byte}")
    }
}

fn tilde_sequence(number: u8, modifier: u8) -> String {
    if modifier == 1 {
        format!("\x1b[{number}~")
    } else {
        format!("\x1b[{number};{modifier}~")
    }
}

fn resolve_color(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    theme: Theme,
    foreground: bool,
) -> Hsla {
    let rgb = match color {
        Color::Spec(color) => Some(color),
        Color::Indexed(index) => {
            colors[index as usize].or_else(|| Some(terminal_rgb(index as usize, theme.is_dark)))
        }
        Color::Named(NamedColor::Foreground | NamedColor::BrightForeground) => None,
        Color::Named(NamedColor::Background) => return theme.terminal,
        Color::Named(NamedColor::Cursor) => return theme.text,
        Color::Named(named) => {
            colors[named as usize].or_else(|| Some(terminal_rgb(named as usize, theme.is_dark)))
        }
    };
    rgb.map(rgb_to_hsla).unwrap_or(if foreground {
        theme.text
    } else {
        theme.terminal
    })
}

fn rgb_to_hsla(color: Rgb) -> Hsla {
    rgb((u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)).into()
}

fn pack_hsla(color: Hsla) -> u32 {
    let color = color.to_rgb();
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(color.r) << 16) | (channel(color.g) << 8) | channel(color.b)
}

fn unpack_rgb(value: u32) -> Rgb {
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

fn dim_rgb(color: Rgb, is_dark: bool) -> Rgb {
    // Faint fades toward the surface: black on dark, white on light.
    let dim = |channel: u8| {
        if is_dark {
            channel * 2 / 3
        } else {
            channel + (255 - channel) / 3
        }
    };
    Rgb {
        r: dim(color.r),
        g: dim(color.g),
        b: dim(color.b),
    }
}

fn terminal_rgb(index: usize, is_dark: bool) -> Rgb {
    // Tomorrow Night on dark surfaces, Tomorrow on light — same hues tuned
    // for the opposite background so ANSI text stays legible on white.
    const ANSI_DARK: [u32; 16] = [
        0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6, 0x666666,
        0xd54e53, 0xb9ca4a, 0xe7c547, 0x7aa6da, 0xc397d8, 0x70c0b1, 0xeaeaea,
    ];
    const ANSI_LIGHT: [u32; 16] = [
        0x000000, 0xc82829, 0x718c00, 0xeab700, 0x4271ae, 0x8959a8, 0x3e999f, 0xc7c7c7, 0x8e908c,
        0xc82829, 0x718c00, 0xeab700, 0x4271ae, 0x8959a8, 0x3e999f, 0xffffff,
    ];
    let ansi = if is_dark { &ANSI_DARK } else { &ANSI_LIGHT };
    let value = match index {
        0..=15 => ansi[index],
        16..=231 => {
            let index = index - 16;
            let channel = |value: usize| {
                if value == 0 {
                    0
                } else {
                    55 + value as u32 * 40
                }
            };
            let red = channel(index / 36);
            let green = channel((index / 6) % 6);
            let blue = channel(index % 6);
            (red << 16) | (green << 8) | blue
        }
        232..=255 => {
            let value = 8 + (index as u32 - 232) * 10;
            (value << 16) | (value << 8) | value
        }
        value
            if value >= NamedColor::DimBlack as usize && value <= NamedColor::DimWhite as usize =>
        {
            let base = ansi[value - NamedColor::DimBlack as usize];
            let dim = dim_rgb(
                Rgb {
                    r: (base >> 16) as u8,
                    g: (base >> 8) as u8,
                    b: base as u8,
                },
                is_dark,
            );
            (u32::from(dim.r) << 16) | (u32::from(dim.g) << 8) | u32::from(dim.b)
        }
        _ => {
            if is_dark {
                0xe5e5e5
            } else {
                0x242424
            }
        }
    };
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Processor;
    use gpui::{ExternalPaths, Image, Modifiers, actions, point, size};

    /// TUIs pick their palette from the reported background, so the reply has
    /// to match the surface or dark text lands on the dark terminal.
    #[test]
    fn palette_queries_report_the_active_theme_surface() {
        let (sender, written) = unbounded();
        let sender_slot = Arc::new(OnceLock::new());
        assert!(sender_slot.set(PtyInput::Test(sender)).is_ok());
        let (ui_tx, _ui_events) = unbounded();
        let reply_colors = TerminalReplyColors::new();
        let proxy = TerminalEventProxy {
            dirty: Arc::new(AtomicBool::new(false)),
            sender: sender_slot,
            ui_events: ui_tx,
            window_size: Arc::new(Mutex::new(WindowSize {
                num_cols: 40,
                num_lines: 8,
                cell_width: 8,
                cell_height: 18,
            })),
            reply_colors: reply_colors.clone(),
        };
        let query = |index: usize| {
            proxy.send_event(Event::ColorRequest(
                index,
                Arc::new(|color: Rgb| format!("{:02x}{:02x}{:02x}", color.r, color.g, color.b)),
            ));
            String::from_utf8(written.recv().unwrap()).unwrap()
        };

        reply_colors.store(&Theme::dark());
        assert_eq!(query(NamedColor::Background as usize), "151515");
        assert_eq!(query(NamedColor::Foreground as usize), "e2e2e2");
        assert_eq!(query(NamedColor::Cursor as usize), "e2e2e2");

        reply_colors.store(&Theme::light());
        assert_eq!(query(NamedColor::Background as usize), "ffffff");
        assert_eq!(query(NamedColor::Foreground as usize), "242424");
    }

    #[test]
    fn printable_keys_use_platform_text_input_but_terminal_controls_do_not() {
        let event = |key_name, text, modifiers, prefer_character_input| KeyDownEvent {
            keystroke: key(key_name, text, modifiers),
            is_held: false,
            prefer_character_input,
        };
        assert!(terminal_uses_text_input(&event(
            "a",
            Some("a"),
            Modifiers::default(),
            false
        )));
        assert!(terminal_uses_text_input(&event(
            "space",
            Some(" "),
            Modifiers::default(),
            false
        )));
        assert!(terminal_uses_text_input(&event(
            "e",
            Some("€"),
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
            true
        )));
        for key_name in ["enter", "backspace", "escape", "up", "tab"] {
            assert!(!terminal_uses_text_input(&event(
                key_name,
                None,
                Modifiers::default(),
                false
            )));
        }
        // macOS supplies key_char for Return/Tab too; keep their terminal
        // sequences (CR and Shift-Tab) instead of sending text input.
        assert!(!terminal_uses_text_input(&event(
            "enter",
            Some("\n"),
            Modifiers::default(),
            false
        )));
        assert!(!terminal_uses_text_input(&event(
            "tab",
            Some("\t"),
            Modifiers {
                shift: true,
                ..Default::default()
            },
            false
        )));
        for modifiers in [
            Modifiers {
                control: true,
                ..Default::default()
            },
            Modifiers {
                alt: true,
                ..Default::default()
            },
        ] {
            assert!(!terminal_uses_text_input(&event(
                "c",
                Some("c"),
                modifiers,
                false
            )));
        }
    }

    /// Exercise the input protocol without launching a shell or rendering a UI.
    #[gpui::test]
    fn ime_preedit_commit_and_cancel_only_write_confirmed_text(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            let (sender, written) = unbounded();
            let dirty = Arc::new(AtomicBool::new(false));
            let (ui_tx, ui_events) = unbounded();
            let window_size = Arc::new(Mutex::new(WindowSize {
                num_cols: 40,
                num_lines: 8,
                cell_width: 8,
                cell_height: 18,
            }));
            let reply_colors = TerminalReplyColors::new();
            let proxy = TerminalEventProxy {
                dirty: dirty.clone(),
                sender: Arc::new(OnceLock::new()),
                ui_events: ui_tx,
                window_size: window_size.clone(),
                reply_colors: reply_colors.clone(),
            };
            let view = cx.new(|cx| TerminalView {
                session: Some(TerminalSession {
                    term: Arc::new(FairMutex::new(Term::new(
                        Config::default(),
                        &TerminalDimensions {
                            columns: 40,
                            rows: 8,
                        },
                        proxy,
                    ))),
                    sender: PtyInput::Test(sender),
                    dirty,
                    snapshot_dirty: AtomicBool::new(true),
                    cached_snapshot: Mutex::new(None),
                    ui_events,
                    window_size,
                    grid_size: (40, 8),
                    reply_colors,
                    url_regex: RegexSearch::new(TERMINAL_LINK_REGEX).unwrap(),
                }),
                launcher: TerminalLauncher::Local,
                error: None,
                focus_handle: cx.focus_handle(),
                working_directory: PathBuf::new(),
                title: String::new(),
                exited: false,
                scroll_accumulator: 0.0,
                panel_width: 320.0,
                panel_height: Some(160.0),
                measured_cell_width: Some(8.0),
                scrollbar_state: ScrollbarState::new(),
                grid_bounds: Rc::new(Cell::new(Some(Bounds::new(
                    point(px(10.), px(20.)),
                    size(px(320.), px(144.)),
                )))),
                cursor_position: Some((2, 3)),
                composition: None,
                selecting: false,
                mouse_reported_cell: None,
                hovered_link: None,
                cursor_blink: cx.new(|_| TerminalCursorBlink::new()),
                cursor_focus_tracking_started: false,
                focused: false,
                needs_attention: false,
                context_menu: ContextMenuHandle::new(cx),
                bottom_panel_tabs: true,
                _subscriptions: Vec::new(),
            });
            view.update(cx, |view, cx| {
                assert!(view.accepts_text_input(window, cx));
                assert_eq!(
                    view.selected_text_range(false, window, cx).unwrap().range,
                    0..0
                );
                // Candidate windows stay anchored even when the terminal caret blinks off.
                assert_eq!(
                    view.bounds_for_range(0..0, Bounds::default(), window, cx)
                        .unwrap()
                        .origin,
                    point(px(34.), px(56.))
                );
                view.replace_and_mark_text_in_range(None, "nihao", Some(5..5), window, cx);
                assert_eq!(view.marked_text_range(window, cx), Some(0..5));
                assert!(written.is_empty());
                // Confirmation/navigation must never escape to the shell during pre-edit.
                for key_name in ["enter", "escape", "up", "backspace"] {
                    view.on_key_down(
                        &KeyDownEvent {
                            keystroke: key(key_name, None, Modifiers::default()),
                            is_held: false,
                            prefer_character_input: false,
                        },
                        window,
                        cx,
                    );
                }
                assert!(written.is_empty());
                view.replace_and_mark_text_in_range(None, "你好😀", Some(4..4), window, cx);
                assert_eq!(view.marked_text_range(window, cx), Some(0..4));
                assert_eq!(
                    view.selected_text_range(false, window, cx).unwrap().range,
                    4..4
                );
                view.replace_text_in_range(None, "你好😀", window, cx);
                assert_eq!(written.try_recv().unwrap(), "你好😀".as_bytes());
                assert!(view.composition.is_none());
                assert!(written.is_empty());
                view.replace_and_mark_text_in_range(None, "quxiao", None, window, cx);
                view.replace_text_in_range(None, "", window, cx);
                assert!(view.composition.is_none());
                assert!(written.is_empty());
                view.replace_and_mark_text_in_range(None, "ni", None, window, cx);
                view.unmark_text(window, cx);
                assert!(view.composition.is_none());
                assert!(written.is_empty());
                view.replace_and_mark_text_in_range(None, "ni", None, window, cx);
                view.replace_and_mark_text_in_range(None, "", None, window, cx);
                assert!(view.composition.is_none());
                assert!(written.is_empty());
                // Ordinary text goes through this same path, with no duplicate key write.
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: key("a", Some("a"), Modifiers::default()),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
                assert!(written.is_empty());
                view.replace_text_in_range(None, "a", window, cx);
                assert_eq!(written.try_recv().unwrap(), b"a");
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: key(
                            "c",
                            Some("c"),
                            Modifiers {
                                control: true,
                                ..Default::default()
                            },
                        ),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
                assert_eq!(written.try_recv().unwrap(), vec![3]);
                view.exited = true;
                assert!(!view.accepts_text_input(window, cx));
            });
        });
    }

    fn key(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: key_char.map(str::to_owned),
            modifiers,
        }
    }

    fn parse_terminal(input: &[u8]) -> Term<VoidListener> {
        let dimensions = TerminalDimensions {
            columns: 40,
            rows: 3,
        };
        let mut term = Term::new(Config::default(), &dimensions, VoidListener);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, input);
        term
    }

    fn terminal_point_for(content: &str, needle: &str) -> TerminalPoint {
        let offset = content.find(needle).unwrap();
        TerminalPoint::new(Line((offset / 40) as i32), Column(offset % 40))
    }

    fn plain_link_value(term: &Term<VoidListener>, point: TerminalPoint) -> Option<String> {
        let mut regex = RegexSearch::new(TERMINAL_LINK_REGEX).unwrap();
        plain_link_at(term, &mut regex, point).map(|(value, _)| value)
    }

    #[test]
    fn detects_plain_and_osc8_terminal_links() {
        let content = "visit (https://example.com/docs). next";
        let term = parse_terminal(content.as_bytes());
        assert_eq!(
            plain_link_value(&term, terminal_point_for(content, "example")),
            Some("https://example.com/docs".to_owned())
        );
        assert_eq!(
            plain_link_value(&term, terminal_point_for(content, ").")),
            None
        );

        let term = parse_terminal(b"x\x1b]8;;https://waku.gg\x1b\\Waku\x1b]8;;\x1b\\ y");
        let (value, bounds) = hyperlink_at(&term, TerminalPoint::new(Line(0), Column(2))).unwrap();
        assert_eq!(value, "https://waku.gg");
        assert_eq!(
            bounds,
            TerminalPoint::new(Line(0), Column(1))..=TerminalPoint::new(Line(0), Column(4))
        );
    }

    #[test]
    fn detects_links_across_soft_wrapped_lines() {
        let content = "prefix https://example.com/a/very/long/path/that/wraps suffix";
        let term = parse_terminal(content.as_bytes());
        let mut regex = RegexSearch::new(TERMINAL_LINK_REGEX).unwrap();
        let (value, bounds) =
            plain_link_at(&term, &mut regex, terminal_point_for(content, "that")).unwrap();

        assert_eq!(value, "https://example.com/a/very/long/path/that/wraps");
        assert_eq!(*bounds.start(), terminal_point_for(content, "https"));
        assert_eq!(bounds.end().line, Line(1));
    }

    #[test]
    fn resolves_terminal_urls_and_existing_project_paths() {
        let working_directory = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            terminal_link_target("https://example.com/docs", working_directory),
            Some(TerminalLinkTarget::Url(
                "https://example.com/docs".to_owned()
            ))
        );
        assert_eq!(
            terminal_link_target("src/terminal.rs:42:8", working_directory),
            Some(TerminalLinkTarget::File(
                working_directory.join("src/terminal.rs")
            ))
        );
        assert_eq!(
            terminal_link_target("src/does-not-exist.rs", working_directory),
            None
        );
    }

    #[test]
    fn encodes_terminal_control_and_cursor_keys() {
        assert_eq!(
            terminal_key_bytes(
                &key(
                    "c",
                    Some("c"),
                    Modifiers {
                        control: true,
                        ..Default::default()
                    }
                ),
                TermMode::empty()
            ),
            Some(vec![3])
        );
        assert_eq!(
            terminal_key_bytes(&key("up", None, Modifiers::default()), TermMode::APP_CURSOR),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            terminal_key_bytes(
                &key(
                    "left",
                    None,
                    Modifiers {
                        control: true,
                        ..Default::default()
                    }
                ),
                TermMode::empty()
            ),
            Some(b"\x1b[1;5D".to_vec())
        );
    }

    #[test]
    fn encodes_modified_enter_backspace_and_app_cursor_home() {
        let bytes = |name: &str, modifiers: Modifiers, mode: TermMode| {
            terminal_key_bytes(&key(name, None, modifiers), mode)
        };
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        let control = Modifiers {
            control: true,
            ..Default::default()
        };
        let plain = Modifiers::default();
        let none = TermMode::empty();

        assert_eq!(bytes("enter", plain, none), Some(b"\r".to_vec()));
        assert_eq!(bytes("enter", shift, none), Some(b"\n".to_vec()));
        assert_eq!(bytes("enter", alt, none), Some(b"\x1b\r".to_vec()));
        assert_eq!(bytes("backspace", plain, none), Some(b"\x7f".to_vec()));
        assert_eq!(bytes("backspace", alt, none), Some(b"\x1b\x7f".to_vec()));
        assert_eq!(bytes("backspace", control, none), Some(b"\x08".to_vec()));
        assert_eq!(bytes("home", plain, none), Some(b"\x1b[H".to_vec()));
        assert_eq!(
            bytes("end", plain, TermMode::APP_CURSOR),
            Some(b"\x1bOF".to_vec())
        );
        assert_eq!(
            bytes("home", shift, TermMode::APP_CURSOR),
            Some(b"\x1b[1;2H".to_vec())
        );
        assert_eq!(bytes("f13", plain, none), Some(b"\x1b[25~".to_vec()));
        assert_eq!(bytes("space", control, none), Some(vec![0]));
    }

    #[test]
    fn kitty_protocol_disambiguates_only_ambiguous_keys() {
        let bytes = |name: &str, modifiers: Modifiers, mode: TermMode| {
            kitty_key_bytes(&key(name, None, modifiers), false, mode)
        };
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let control = Modifiers {
            control: true,
            ..Default::default()
        };
        let plain = Modifiers::default();
        let flag1 = TermMode::DISAMBIGUATE_ESC_CODES;

        // Off until a program pushes the protocol.
        assert_eq!(bytes("escape", plain, TermMode::empty()), None);
        assert_eq!(bytes("escape", plain, flag1), Some(b"\x1b[27u".to_vec()));
        assert_eq!(bytes("enter", shift, flag1), Some(b"\x1b[13;2u".to_vec()));
        assert_eq!(bytes("c", control, flag1), Some(b"\x1b[99;5u".to_vec()));
        // Unambiguous keys keep their legacy bytes.
        assert_eq!(bytes("enter", plain, flag1), None);
        assert_eq!(bytes("a", shift, flag1), None);
        assert_eq!(bytes("space", plain, flag1), None);
        assert_eq!(bytes("up", control, flag1), None);

        let all = flag1 | TermMode::REPORT_ALL_KEYS_AS_ESC;
        assert_eq!(bytes("enter", plain, all), Some(b"\x1b[13u".to_vec()));
        let repeat = flag1 | TermMode::REPORT_EVENT_TYPES;
        assert_eq!(
            kitty_key_bytes(&key("escape", None, plain), true, repeat),
            Some(b"\x1b[27;1:2u".to_vec())
        );
        let alternate = flag1 | TermMode::REPORT_ALTERNATE_KEYS;
        let control_shift = Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            kitty_key_bytes(&key("a", Some("A"), control_shift), false, alternate),
            Some(b"\x1b[97:65;6u".to_vec())
        );
    }

    #[test]
    fn scans_osc_notifications_across_reads_and_skips_conemu() {
        let mut scanner = OscAlertScanner::default();
        let mut found = Vec::new();
        for chunk in [
            &b"out\x1b]9;Agent turn com"[..],
            b"plete\x07\x1b]9;4;3;0\x07\x1b]9;9;C:\\\x1b\\",
            b"\x1b]777;notify;Codex;Approval needed\x1b\\\x1b]0;title\x07",
        ] {
            scanner.feed(chunk, |alert| found.push(alert));
        }
        assert_eq!(
            found,
            vec![
                TerminalAlert {
                    title: None,
                    body: "Agent turn complete".to_owned(),
                },
                TerminalAlert {
                    title: Some("Codex".to_owned()),
                    body: "Approval needed".to_owned(),
                },
            ]
        );
    }

    /// App shortcuts must not swallow keys the shell needs (Escape for vim,
    /// Ctrl-L, Ctrl-[), while deliberate app chords keep working in the pane.
    #[test]
    fn terminal_shields_shell_keys_from_app_shortcuts() {
        actions!(terminal_shield_test, [AppWide, Kept, TerminalOnly]);
        let bindings = [
            KeyBinding::new("escape", AppWide, Some("Waku")),
            KeyBinding::new("secondary-c", AppWide, Some("Waku")),
            KeyBinding::new("ctrl-l", AppWide, None),
            KeyBinding::new("ctrl-[", AppWide, Some("Waku")),
            KeyBinding::new("ctrl-escape", AppWide, Some("Waku")),
            KeyBinding::new("ctrl-j", Kept, None),
            KeyBinding::new("ctrl-shift-e", AppWide, None),
            KeyBinding::new("secondary-,", AppWide, None),
            KeyBinding::new("alt-shift-left", TerminalOnly, Some("Terminal")),
            KeyBinding::new("escape", AppWide, Some("Browser")),
            KeyBinding::new("escape", AppWide, Some("Waku > TextInput")),
        ];
        let shielded = shell_keystroke_shields(&bindings, &[&Kept])
            .iter()
            .map(|binding| {
                assert!(is_no_action(binding.action()));
                binding.keystrokes()[0].inner().unparse()
            })
            .collect::<Vec<_>>();
        let mut expected = vec!["escape", "ctrl-l", "ctrl-[", "ctrl-escape"];
        // Cmd-C is the terminal's own copy on macOS; elsewhere Ctrl-C is SIGINT.
        expected.insert(
            1,
            if cfg!(target_os = "macos") {
                "cmd-c"
            } else {
                "ctrl-c"
            },
        );
        if cfg!(target_os = "macos") {
            // Ctrl-Shift-E is plain Ctrl-E to the shell, which owns it on macOS.
            expected.push("ctrl-shift-e");
        }
        assert_eq!(shielded, expected);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_terminal_clipboard_shortcuts_preserve_ctrl_c_for_sigint() {
        let control = Modifiers {
            control: true,
            ..Default::default()
        };
        let control_shift = Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };

        assert!(primary_modifier_pressed(&control));
        assert!(!terminal_clipboard_modifier_pressed(&control));
        assert!(terminal_clipboard_modifier_pressed(&control_shift));
    }

    #[test]
    fn wheel_reaches_mouse_reporting_and_alternate_screen_apps() {
        let plain = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };

        assert_eq!(
            terminal_wheel_input(3, Some((0, 0)), &plain, TermMode::default()),
            None
        );
        assert_eq!(
            terminal_wheel_input(
                2,
                Some((4, 1)),
                &plain,
                TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE
            ),
            Some(b"\x1b[<64;5;2M\x1b[<64;5;2M".to_vec())
        );
        assert_eq!(
            terminal_wheel_input(-1, Some((0, 0)), &plain, TermMode::MOUSE_DRAG),
            Some(vec![0x1b, b'[', b'M', 32 + 65, 33, 33])
        );
        assert_eq!(
            terminal_wheel_input(
                -1,
                Some((100, 0)),
                &plain,
                TermMode::MOUSE_MOTION | TermMode::UTF8_MOUSE
            ),
            Some(vec![0x1b, b'[', b'M', 32 + 65, 0xC2, 0x85, 33])
        );
        assert_eq!(
            terminal_wheel_input(
                1,
                Some((0, 0)),
                &shift,
                TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE
            ),
            None
        );

        let alt_screen = TermMode::default() | TermMode::ALT_SCREEN;
        assert_eq!(
            terminal_wheel_input(-2, None, &plain, alt_screen),
            Some(b"\x1bOB\x1bOB".to_vec())
        );
        assert_eq!(
            terminal_wheel_input(1, None, &plain, TermMode::ALT_SCREEN),
            None
        );
    }

    #[test]
    fn mouse_clicks_and_drags_report_to_tuis_unless_shift_selects() {
        let plain = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let sgr_drag = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;

        assert_eq!(
            terminal_mouse_button_input(0, true, (4, 1), &plain, TermMode::default()),
            None
        );
        assert_eq!(
            terminal_mouse_button_input(0, true, (4, 1), &plain, sgr_drag),
            Some(b"\x1b[<0;5;2M".to_vec())
        );
        assert_eq!(
            terminal_mouse_move_input(0, (6, 2), &plain, sgr_drag),
            Some(b"\x1b[<32;7;3M".to_vec())
        );
        assert_eq!(
            terminal_mouse_button_input(0, false, (6, 2), &plain, sgr_drag),
            Some(b"\x1b[<0;7;3m".to_vec())
        );
        // Legacy X10 release encodes button 3.
        assert_eq!(
            terminal_mouse_button_input(0, false, (0, 0), &plain, TermMode::MOUSE_REPORT_CLICK),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 33, 33])
        );
        // Shift forces local selection instead of reporting to the TUI.
        assert_eq!(
            terminal_mouse_button_input(0, true, (4, 1), &shift, sgr_drag),
            None
        );
    }

    #[test]
    fn snapshot_cache_reuses_clean_frames_and_invalidates_on_dirty_or_style_change() {
        let (sender, _written) = unbounded();
        let dirty = Arc::new(AtomicBool::new(true));
        let (ui_tx, ui_events) = unbounded();
        let window_size = Arc::new(Mutex::new(WindowSize {
            num_cols: 20,
            num_lines: 4,
            cell_width: 8,
            cell_height: 18,
        }));
        let reply_colors = TerminalReplyColors::new();
        let proxy = TerminalEventProxy {
            dirty: dirty.clone(),
            sender: Arc::new(OnceLock::new()),
            ui_events: ui_tx,
            window_size: window_size.clone(),
            reply_colors: reply_colors.clone(),
        };
        let session = TerminalSession {
            term: Arc::new(FairMutex::new(Term::new(
                Config::default(),
                &TerminalDimensions {
                    columns: 20,
                    rows: 4,
                },
                proxy,
            ))),
            sender: PtyInput::Test(sender),
            dirty,
            snapshot_dirty: AtomicBool::new(true),
            cached_snapshot: Mutex::new(None),
            ui_events,
            window_size,
            grid_size: (20, 4),
            reply_colors,
            url_regex: RegexSearch::new(TERMINAL_LINK_REGEX).unwrap(),
        };
        let theme = Theme::dark();
        assert!(session.take_dirty());

        let first = session.snapshot(theme, theme.selection, TerminalCursorStyle::Outline, None);
        let second = session.snapshot(theme, theme.selection, TerminalCursorStyle::Outline, None);
        assert!(Arc::ptr_eq(&first, &second));

        // Cursor style or dirty flag invalidates the cached snapshot.
        let solid = session.snapshot(theme, theme.selection, TerminalCursorStyle::Solid, None);
        assert!(!Arc::ptr_eq(&first, &solid));

        session.dirty.store(true, Ordering::Release);
        assert!(session.take_dirty());
        let rebuilt = session.snapshot(theme, theme.selection, TerminalCursorStyle::Solid, None);
        assert!(!Arc::ptr_eq(&solid, &rebuilt));
    }

    #[test]
    fn wraps_bracketed_paste_only_when_requested() {
        assert_eq!(
            bracketed_paste("hello".into(), TermMode::BRACKETED_PASTE),
            b"\x1b[200~hello\x1b[201~"
        );
        assert_eq!(bracketed_paste("hello".into(), TermMode::empty()), b"hello");
    }

    #[test]
    fn cursor_blinks_while_focused_and_outlines_when_unfocused() {
        assert_eq!(
            terminal_cursor_style(false, false),
            TerminalCursorStyle::Outline
        );
        assert_eq!(
            terminal_cursor_style(false, true),
            TerminalCursorStyle::Outline
        );
        assert_eq!(
            terminal_cursor_style(true, false),
            TerminalCursorStyle::Hidden
        );
        assert_eq!(
            terminal_cursor_style(true, true),
            TerminalCursorStyle::Solid
        );
    }

    #[test]
    fn maps_pointer_positions_into_scrollback_grid_coordinates() {
        let bounds = Bounds::new(point(px(10.0), px(20.0)), size(px(72.0), px(64.0)));
        let position = point(
            px(10.0 + TERMINAL_CELL_WIDTH * 2.0 + 5.0),
            px(20.0 + TERMINAL_CELL_HEIGHT + 8.0),
        );

        assert_eq!(
            terminal_grid_point(bounds, position, TERMINAL_CELL_WIDTH, 10, 4, 3, false),
            Some((TerminalPoint::new(Line(-2), Column(2)), Side::Right))
        );
        assert_eq!(
            terminal_grid_point(
                bounds,
                point(px(0.0), px(0.0)),
                TERMINAL_CELL_WIDTH,
                10,
                4,
                3,
                false
            ),
            None
        );
    }

    #[test]
    fn clamps_selection_drags_to_the_terminal_grid_edges() {
        let bounds = Bounds::new(point(px(10.0), px(20.0)), size(px(72.0), px(64.0)));

        assert_eq!(
            terminal_grid_point(
                bounds,
                point(px(500.0), px(500.0)),
                TERMINAL_CELL_WIDTH,
                10,
                4,
                0,
                true
            ),
            Some((TerminalPoint::new(Line(3), Column(9)), Side::Right))
        );
        assert_eq!(
            terminal_grid_point(
                bounds,
                point(px(-50.0), px(-50.0)),
                TERMINAL_CELL_WIDTH,
                10,
                4,
                3,
                true
            ),
            Some((TerminalPoint::new(Line(-3), Column(0)), Side::Left))
        );
    }

    // Linux builds `image` with PNG alone, so BMP falls through unchanged there.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn pasted_bmp_is_re_encoded_as_png() {
        let mut bmp = Vec::new();
        image::RgbImage::from_pixel(2, 2, image::Rgb([200, 30, 30]))
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .unwrap();
        let (format, bytes) = paste_image_as_png(ImageFormat::Bmp, bmp);
        assert_eq!(format, ImageFormat::Png);
        assert!(bytes.starts_with(b"\x89PNG"));

        let (format, bytes) = paste_image_as_png(ImageFormat::Bmp, b"not an image".to_vec());
        assert_eq!(format, ImageFormat::Bmp, "undecodable bytes pass through");
        assert_eq!(bytes, b"not an image");
    }

    #[test]
    fn copied_image_files_upload_and_other_files_are_skipped() {
        assert_eq!(
            image_format_for_path(Path::new("shot.PNG")),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            image_format_for_path(Path::new("a/b.jpeg")),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(image_format_for_path(Path::new("notes.txt")), None);
        assert_eq!(image_format_for_path(Path::new("Makefile")), None);

        let dir =
            std::env::temp_dir().join(format!("waku-paste-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("shot.png");
        let mut png = Vec::new();
        image::RgbImage::from_pixel(1, 1, image::Rgb([0, 0, 0]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&image, &png).unwrap();
        let text = dir.join("notes.txt");
        std::fs::write(&text, "hi").unwrap();
        let missing = dir.join("gone.png");

        // The local launcher stands in for the daemon upload: same sorting.
        let uploaded = upload_pasted_paths(TerminalLauncher::Local, vec![image, text, missing]);
        assert_eq!(uploaded.paths.len(), 1);
        assert_eq!(std::fs::read(&uploaded.paths[0]).unwrap(), png);
        assert_eq!(uploaded.skipped, 2);
        let _ = std::fs::remove_file(&uploaded.paths[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quotes_terminal_paths_only_when_needed() {
        let clean_path = PathBuf::from("/tmp/waku-paste-20261020.png");
        assert_eq!(
            shell_quote_path(&clean_path),
            "/tmp/waku-paste-20261020.png"
        );

        let space_posix = PathBuf::from("/Users/alice/my images/photo.png");
        if !cfg!(windows) {
            assert_eq!(
                shell_quote_path(&space_posix),
                "'/Users/alice/my images/photo.png'"
            );
        }

        let quoted_posix = PathBuf::from("/tmp/bob's photo.png");
        if !cfg!(windows) {
            assert_eq!(
                shell_quote_path(&quoted_posix),
                "'/tmp/bob'\\''s photo.png'"
            );
        }
    }

    #[test]
    fn extracts_terminal_paste_content_with_correct_priority() {
        // Text-only clipboard
        let text_item = ClipboardItem::new_string("echo hello".to_string());
        assert_eq!(
            extract_terminal_paste_content(&text_item),
            Some(TerminalPasteContent::Text("echo hello".into()))
        );
        assert!(clipboard_has_paste_content(&text_item));

        // Image clipboard
        let image = Image {
            format: ImageFormat::Png,
            bytes: vec![137, 80, 78, 71],
            id: 1,
        };
        let image_item = ClipboardItem::new_image(&image);
        assert_eq!(
            extract_terminal_paste_content(&image_item),
            Some(TerminalPasteContent::Image {
                format: ImageFormat::Png,
                bytes: vec![137, 80, 78, 71],
            })
        );
        assert!(clipboard_has_paste_content(&image_item));

        // External paths clipboard
        let paths = ExternalPaths(vec![PathBuf::from("/tmp/file.png")].into());
        let paths_item = ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(paths)],
        };
        assert_eq!(
            extract_terminal_paste_content(&paths_item),
            Some(TerminalPasteContent::Paths(vec![PathBuf::from(
                "/tmp/file.png"
            )]))
        );
        assert!(clipboard_has_paste_content(&paths_item));

        // Empty clipboard
        let empty_item = ClipboardItem { entries: vec![] };
        assert_eq!(extract_terminal_paste_content(&empty_item), None);
        assert!(!clipboard_has_paste_content(&empty_item));
    }

    #[test]
    fn bottom_panel_chords_accept_windows_folded_symbols() {
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        let alt_shift = Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        // gpui_windows reports shift+= as "+" and shift+- as "_" with the
        // shift flag cleared; those must still split there. Other platforms
        // keep the shift flag, so an unshifted "+"/"_" is real shell input.
        let folded_right = cfg!(windows).then_some(PaneChord::SplitRight);
        let folded_down = cfg!(windows).then_some(PaneChord::SplitDown);
        assert_eq!(
            bottom_panel_pane_chord(&key("+", Some("+"), alt)),
            folded_right
        );
        assert_eq!(
            bottom_panel_pane_chord(&key("_", Some("_"), alt)),
            folded_down
        );
        // The unshifted keys stay with the shell.
        assert_eq!(bottom_panel_pane_chord(&key("=", Some("="), alt)), None);
        assert_eq!(bottom_panel_pane_chord(&key("-", Some("-"), alt)), None);
        // Platforms that keep the shift flag report the unshifted key.
        assert_eq!(
            bottom_panel_pane_chord(&key("=", Some("+"), alt_shift)),
            Some(PaneChord::SplitRight)
        );
        assert_eq!(
            bottom_panel_pane_chord(&key("-", Some("_"), alt_shift)),
            Some(PaneChord::SplitDown)
        );
    }

    #[test]
    fn bottom_panel_chords_cover_split_close_and_resize() {
        let ctrl_shift = Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        let ctrl_alt_shift = Modifiers {
            control: true,
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            bottom_panel_pane_chord(&key("d", Some("d"), ctrl_shift)),
            Some(PaneChord::SplitRight)
        );
        assert_eq!(
            bottom_panel_pane_chord(&key("d", Some("d"), ctrl_alt_shift)),
            Some(PaneChord::SplitDown)
        );
        assert_eq!(
            bottom_panel_pane_chord(&key("w", None, ctrl_shift)),
            Some(PaneChord::ClosePane)
        );

        let alt_shift = Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        for (name, chord) in [
            ("left", PaneChord::ResizeLeft),
            ("right", PaneChord::ResizeRight),
            ("up", PaneChord::ResizeUp),
            ("down", PaneChord::ResizeDown),
        ] {
            assert_eq!(
                bottom_panel_pane_chord(&key(name, None, alt_shift)),
                Some(chord)
            );
        }
        // Alt+arrow without shift belongs to the shell.
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(bottom_panel_pane_chord(&key("left", None, alt)), None);
    }
}
