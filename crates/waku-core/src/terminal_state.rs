//! Bounded terminal recovery, independent of WebSocket subscriptions.
use crate::{EventSink, WireDriverEvent};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use waku_protocol::TerminalOutput;

const REPLAY_BYTES: usize = 2 * 1024 * 1024;

pub(crate) struct TerminalState {
    parser: vt100::Parser<DeviceReplies>,
    saved_primary: Option<Vec<u8>>,
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
            saved_primary: None,
            bytes: VecDeque::new(),
            offset: 0,
            exited: false,
            modes: RecoveryModes::default(),
            mode_parser: alacritty_terminal::vte::Parser::new(),
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        let was_alternate = self.modes.alternate;
        self.mode_parser.advance(&mut self.modes, bytes);
        self.parser.callbacks_mut().1 = self.modes.keyboard.last().copied().unwrap_or(0);
        if !was_alternate && self.modes.alternate {
            let split = find_alt_screen_enter(bytes).unwrap_or(0);
            if split > 0 {
                self.parser.process(&bytes[..split]);
            }
            self.saved_primary = Some(format_screen_with_scrollback(self.parser.screen()));
            self.parser.process(&bytes[split..]);
        } else {
            self.parser.process(bytes);
            if !self.modes.alternate {
                self.saved_primary = None;
            }
        }
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
            let screen = self.parser.screen();
            let mut data = Vec::new();
            if screen.alternate_screen() {
                if let Some(primary) = &self.saved_primary {
                    data.extend_from_slice(primary);
                }
                data.extend_from_slice(b"\x1b[?1049h");
                data.extend(screen.state_formatted());
            } else {
                data.extend(format_screen_with_scrollback(screen));
            }
            for flags in &self.modes.keyboard {
                data.extend(format!("\x1b[>{flags}u").as_bytes());
            }
            if let Some(title) = &self.parser.callbacks().2 {
                data.extend(b"\x1b]2;");
                data.extend(title.as_bytes());
                data.extend(b"\x1b\\");
            }
            if let Some(cwd_osc) = &self.parser.callbacks().3 {
                data.extend_from_slice(cwd_osc);
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

    /// A dead stream never produces replay bytes again; any attach now gets
    /// the reconstructed final screen instead. Freeing the ring keeps an
    /// exited session from pinning its 2 MiB replay window until the record
    /// itself is dropped.
    fn mark_exited(&mut self) {
        self.exited = true;
        self.bytes = VecDeque::new();
    }
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.parser.callbacks_mut().0)
    }
}

fn format_screen_with_scrollback(screen: &vt100::Screen) -> Vec<u8> {
    let mut screen = screen.clone();
    let (_, cols) = screen.size();
    screen.set_scrollback(usize::MAX);
    let history = screen.scrollback();
    let mut data = Vec::new();
    // Scrollback is restored as readable text; the visible screen retains its
    // formatting. Bound both independently of uptime.
    for offset in (1..=history).rev() {
        screen.set_scrollback(offset);
        if let Some(row) = screen.rows(0, cols).next() {
            data.extend(row.trim_end_matches(' ').as_bytes());
        }
        data.extend(b"\r\n");
    }
    screen.set_scrollback(0);
    if history > 0 {
        let used_rows = screen
            .rows(0, cols)
            .enumerate()
            .filter(|(_, row)| !row.trim_end_matches(' ').is_empty())
            .last()
            .map(|(idx, _)| idx + 1)
            .unwrap_or(0)
            .max(usize::from(screen.cursor_position().0) + 1);
        for _ in 0..used_rows.saturating_sub(1) {
            data.extend(b"\r\n");
        }
    }
    data.extend(screen.state_formatted());
    data
}

fn find_alt_screen_enter(bytes: &[u8]) -> Option<usize> {
    for pattern in [&b"\x1b[?1049h"[..], &b"\x1b[?1047h"[..], &b"\x1b[?47h"[..]] {
        if let Some(pos) = bytes.windows(pattern.len()).position(|w| w == pattern) {
            return Some(pos);
        }
    }
    None
}

#[derive(Default)]
struct DeviceReplies(Vec<u8>, u16, Option<String>, Option<Vec<u8>>);
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
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            [b"7", uri, ..] if !uri.is_empty() && uri.len() <= 2048 => {
                let mut osc = Vec::with_capacity(uri.len() + 6);
                osc.extend_from_slice(b"\x1b]7;");
                osc.extend_from_slice(uri);
                osc.extend_from_slice(b"\x1b\\");
                self.3 = Some(osc);
            }
            [b"9", b"9", path, ..] if !path.is_empty() && path.len() <= 2048 => {
                let mut osc = Vec::with_capacity(path.len() + 8);
                osc.extend_from_slice(b"\x1b]9;9;");
                osc.extend_from_slice(path);
                osc.extend_from_slice(b"\x1b\\");
                self.3 = Some(osc);
            }
            _ => {}
        }
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
            (None, 't', 14) => {
                let (rows, cols) = screen.size();
                format!(
                    "\x1b[4;{};{}t",
                    rows.saturating_mul(16),
                    cols.saturating_mul(8)
                )
            }
            (None, 't', 18) => {
                let (rows, cols) = screen.size();
                format!("\x1b[8;{rows};{cols}t")
            }
            _ => String::new(),
        };
        self.0.extend(response.as_bytes());
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

    /// Appends raw PTY bytes and publishes them as one `terminalOutput`
    /// event. Encoding once here avoids the daemon-side base64 decode that
    /// `send_ephemeral` used to perform on its own output.
    pub fn send_output(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let offset = {
            let mut state = self.state.lock();
            state.append(bytes);
            state.offset
        };
        let event =
            WireDriverEvent::new("terminalOutput", json!({ "data": data, "offset": offset }));
        self.sink.send_ephemeral(event)
    }

    pub fn send_ephemeral(&self, event: WireDriverEvent) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        if matches!(event.kind.as_str(), "terminalExited" | "terminalError") {
            state.mark_exited();
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

    #[test]
    fn exit_frees_the_replay_ring_but_attaches_get_the_final_screen() {
        let mut state = TerminalState::new(80, 24);
        state.append(b"final line\r\n");
        let before = state.snapshot(None);
        state.mark_exited();
        assert!(state.bytes.is_empty());
        let tail = state.snapshot(Some(before.offset));
        assert!(!tail.reset);
        assert!(tail.data.is_empty());
        assert!(tail.exited);
        let fresh = state.snapshot(None);
        assert!(fresh.reset);
        assert!(fresh.exited);
        assert!(String::from_utf8_lossy(&fresh.data).contains("final line"));
    }

    #[test]
    fn alt_screen_snapshot_preserves_primary_screen_and_cwd() {
        let mut state = TerminalState::new(80, 24);
        state.append(b"\x1b]7;file://localhost/tmp/work\x1b\\primary-before-alt\r\n");
        state.append(b"\x1b[?1049halt-screen-view");
        let snapshot = state.snapshot(None);
        assert!(snapshot.reset);
        let text = String::from_utf8_lossy(&snapshot.data);
        assert!(text.contains("primary-before-alt"));
        assert!(text.contains("alt-screen-view"));
        assert!(text.contains("\x1b]7;file://localhost/tmp/work\x1b\\"));
    }
}
