//! Bounded terminal recovery, independent of WebSocket subscriptions.
use crate::{EventSink, WireDriverEvent};
use base64::Engine as _;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use waku_protocol::TerminalOutput;

const REPLAY_BYTES: usize = 2 * 1024 * 1024;

pub(crate) struct TerminalState {
    parser: vt100::Parser<DeviceReplies>,
    bytes: VecDeque<u8>,
    offset: u64,
    exited: bool,
    modes: RecoveryModes,
    mode_parser: alacritty_terminal::vte::Parser,
}

impl TerminalState {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows.max(1),
                cols.max(2),
                2000,
                DeviceReplies::default(),
            ),
            bytes: VecDeque::new(),
            offset: 0,
            exited: false,
            modes: RecoveryModes::default(),
            mode_parser: alacritty_terminal::vte::Parser::new(),
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.mode_parser.advance(&mut self.modes, bytes);
        self.parser.callbacks_mut().1 = self.modes.keyboard.last().copied().unwrap_or(0);
        self.parser.process(bytes);
        self.offset += bytes.len() as u64;
        self.bytes.extend(bytes);
        let excess = self.bytes.len().saturating_sub(REPLAY_BYTES);
        self.bytes.drain(..excess);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(2));
    }

    pub fn snapshot(&self, after: Option<u64>) -> TerminalOutput {
        let start = self.offset - self.bytes.len() as u64;
        let cursor = after.unwrap_or(0);
        let reset = after.is_none() || cursor < start || cursor > self.offset;
        let data = if !reset {
            self.bytes
                .iter()
                .skip((cursor - start) as usize)
                .copied()
                .collect()
        } else {
            let mut screen = self.parser.screen().clone();
            let (rows, cols) = screen.size();
            screen.set_scrollback(usize::MAX);
            let history = screen.scrollback();
            let mut data = Vec::new();
            // Scrollback is restored as readable text; the visible screen
            // retains its formatting. Bound both independently of uptime.
            for offset in (1..=history).rev() {
                screen.set_scrollback(offset);
                if let Some(row) = screen.rows(0, cols).next() {
                    data.extend(row.as_bytes());
                }
                data.extend(b"\r\n");
            }
            if history > 0 {
                for _ in 0..rows {
                    data.extend(b"\r\n");
                }
            }
            screen.set_scrollback(0);
            if screen.alternate_screen() {
                data.extend(b"\x1b[?1049h");
            }
            data.extend(screen.state_formatted());
            for flags in &self.modes.keyboard {
                data.extend(format!("\x1b[>{flags}u").as_bytes());
            }
            if let Some(title) = &self.parser.callbacks().2 {
                data.extend(b"\x1b]2;");
                data.extend(title.as_bytes());
                data.extend(b"\x1b\\");
            }
            data
        };
        let (rows, cols) = self.parser.screen().size();
        TerminalOutput {
            offset: self.offset,
            reset,
            rows,
            cols,
            exited: self.exited,
            data,
        }
    }
    pub fn exited(&self) -> bool {
        self.exited
    }
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.parser.callbacks_mut().0)
    }
}

#[derive(Default)]
struct DeviceReplies(Vec<u8>, u16, Option<String>);
impl vt100::Callbacks for DeviceReplies {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.2 = Some(
            String::from_utf8_lossy(title)
                .chars()
                .filter(|c| !c.is_control())
                .take(512)
                .collect(),
        );
    }
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let response = match (i1, c, first) {
            (None, 'n', 5) => "\x1b[0n".to_owned(),
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                format!("\x1b[{};{}R", row + 1, col + 1)
            }
            (None, 'c', _) => "\x1b[?6c".to_owned(),
            (Some(b'>'), 'c', _) => "\x1b[>0;1;0c".to_owned(),
            (Some(b'?'), 'u', _) => format!("\x1b[?{}u", self.1),
            (None, 't', 18) => {
                let (rows, cols) = screen.size();
                format!("\x1b[8;{rows};{cols}t")
            }
            _ => String::new(),
        };
        self.0.extend(response.as_bytes());
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.len() >= 2 && params[1] == b"?" && matches!(params[0], b"10" | b"11" | b"12") {
            let color = if params[0] == b"11" {
                "1010/1010/1010"
            } else {
                "dddd/dddd/dddd"
            };
            self.0.extend(
                format!(
                    "\x1b]{};rgb:{color}\x1b\\",
                    String::from_utf8_lossy(params[0])
                )
                .as_bytes(),
            );
        }
    }
}

/// VT100 screen snapshots do not include the kitty keyboard stack used by
/// coding-agent TUIs. Track it incrementally, including split escape codes.
#[derive(Default)]
struct RecoveryModes {
    keyboard: Vec<u16>,
    inactive_keyboard: Vec<u16>,
    alternate: bool,
}
impl alacritty_terminal::vte::Perform for RecoveryModes {
    fn csi_dispatch(
        &mut self,
        params: &alacritty_terminal::vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            return;
        }
        let mut args = params.iter();
        let first = args.next().and_then(|p| p.first()).copied().unwrap_or(0);
        if intermediates == b"?" && matches!(action, 'h' | 'l') && matches!(first, 47 | 1047 | 1049)
        {
            let alternate = action == 'h';
            if alternate != self.alternate {
                std::mem::swap(&mut self.keyboard, &mut self.inactive_keyboard);
                self.alternate = alternate;
            }
        }
        if action != 'u' {
            return;
        }
        match intermediates {
            b">" => {
                if self.keyboard.len() == 32 {
                    self.keyboard.remove(0);
                }
                self.keyboard.push(first);
            }
            b"<" => {
                self.keyboard
                    .truncate(self.keyboard.len().saturating_sub(first.max(1) as usize));
            }
            b"=" => {
                let behavior = args.next().and_then(|p| p.first()).copied().unwrap_or(1);
                if self.keyboard.is_empty() {
                    self.keyboard.push(0);
                }
                let mode = self.keyboard.last_mut().unwrap();
                *mode = match behavior {
                    2 => *mode | first,
                    3 => *mode & !first,
                    _ => first,
                };
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
pub(crate) struct TerminalEvents {
    pub state: Arc<Mutex<TerminalState>>,
    sink: EventSink,
}

impl TerminalEvents {
    pub fn new(sink: EventSink, cols: u16, rows: u16) -> Self {
        Self {
            sink,
            state: Arc::new(Mutex::new(TerminalState::new(cols, rows))),
        }
    }

    pub fn send_ephemeral(&self, mut event: WireDriverEvent) -> anyhow::Result<()> {
        // Publish under the same lock as the snapshot, so its offset is an
        // exact boundary between replay and queued live output.
        let mut state = self.state.lock();
        match event.kind.as_str() {
            "terminalOutput" => {
                if let Some(data) = event.payload.get("data").and_then(|v| v.as_str()) {
                    let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
                    state.append(&bytes);
                    event.payload["offset"] = state.offset.into();
                }
            }
            "terminalExited" | "terminalError" => state.exited = true,
            _ => {}
        }
        self.sink.send_ephemeral(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnect_replays_only_missing_bytes() {
        let mut state = TerminalState::new(80, 24);
        state.append(b"hello");
        let first = state.snapshot(None);
        state.append(b" world");
        let next = state.snapshot(Some(first.offset));
        assert_eq!(next.data, b" world");
        assert!(!next.reset);
        assert!(state.snapshot(Some(next.offset)).data.is_empty());
    }
    #[test]
    fn evicted_cursor_gets_bounded_screen_instead_of_broken_byte_tail() {
        let mut state = TerminalState::new(80, 24);
        state.append(&vec![b'x'; REPLAY_BYTES + 100]);
        state.append(b"\r\nrecovered");
        let snapshot = state.snapshot(Some(1));
        assert!(snapshot.reset);
        assert!(snapshot.data.len() < 256 * 1024);
        assert!(String::from_utf8_lossy(&snapshot.data).contains("recovered"));
        assert_eq!(state.bytes.len(), REPLAY_BYTES);
    }
    #[test]
    fn detached_terminal_answers_cursor_queries_and_restores_keyboard_mode() {
        let mut state = TerminalState::new(80, 24);
        state.append(b"abc\x1b[6n");
        assert_eq!(state.take_replies(), b"\x1b[1;4R");
        state.append(b"\x1b[?1049h\x1b[>");
        state.append(b"3u");
        let snapshot = state.snapshot(Some(u64::MAX));
        assert!(snapshot.data.windows(8).any(|b| b == b"\x1b[?1049h"));
        assert!(snapshot.data.ends_with(b"\x1b[>3u"));
    }
}
