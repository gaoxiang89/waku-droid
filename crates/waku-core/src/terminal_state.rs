//! Bounded terminal recovery, independent of WebSocket subscriptions.
use crate::{EventSink, WireDriverEvent};
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb, Timeout};
use alacritty_terminal::vte::{Params, Perform};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::json;
use std::collections::VecDeque;
use std::io::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use waku_protocol::TerminalOutput;

const REPLAY_BYTES: usize = 2 * 1024 * 1024;
const SCROLLBACK_LINES: usize = 2000;
const MAX_PENDING_REPLIES: usize = 64 * 1024;
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

const VISUAL_FLAGS: Flags = Flags::INVERSE
    .union(Flags::BOLD)
    .union(Flags::ITALIC)
    .union(Flags::DIM)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT)
    .union(Flags::ALL_UNDERLINES);

/// Disables DEC 2026 buffering in the daemon's shadow emulator so device queries
/// and reconnect snapshots never stall behind an unclosed synchronized update.
#[derive(Default)]
struct NoSyncTimeout;

impl Timeout for NoSyncTimeout {
    fn set_timeout(&mut self, _: std::time::Duration) {}

    fn clear_timeout(&mut self) {}

    fn pending_timeout(&self) -> bool {
        false
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

struct ShadowEvents {
    replies: Vec<u8>,
    title: Option<String>,
    window_size: WindowSize,
}

impl ShadowEvents {
    fn push_reply(&mut self, bytes: &[u8]) {
        if self.replies.len().saturating_add(bytes.len()) <= MAX_PENDING_REPLIES {
            self.replies.extend_from_slice(bytes);
        }
    }
}

#[derive(Clone)]
struct ShadowEventProxy {
    shared: Arc<Mutex<ShadowEvents>>,
}

impl EventListener for ShadowEventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                self.shared.lock().push_reply(text.as_bytes());
            }
            Event::TextAreaSizeRequest(formatter) => {
                let mut shared = self.shared.lock();
                let text = formatter(shared.window_size);
                shared.push_reply(text.as_bytes());
            }
            Event::Title(title) => {
                let clean: String = title
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(512)
                    .collect();
                self.shared.lock().title = Some(clean);
            }
            Event::ResetTitle => {
                self.shared.lock().title = None;
            }
            // Client renderers answer OSC 10/11/12 from their active theme.
            Event::ColorRequest(_, _) => {}
            _ => {}
        }
    }
}

struct ShadowEmulator {
    term: Term<ShadowEventProxy>,
    processor: Processor<NoSyncTimeout>,
    shared: Arc<Mutex<ShadowEvents>>,
    saved_primary: Option<Vec<u8>>,
    modes: RecoveryModes,
    mode_parser: alacritty_terminal::vte::Parser,
}

impl ShadowEmulator {
    fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(1);
        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: CELL_WIDTH,
            cell_height: CELL_HEIGHT,
        };
        let shared = Arc::new(Mutex::new(ShadowEvents {
            replies: Vec::new(),
            title: None,
            window_size,
        }));
        let proxy = ShadowEventProxy {
            shared: shared.clone(),
        };
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            kitty_keyboard: true,
            ..Default::default()
        };
        let dimensions = TerminalDimensions {
            columns: usize::from(cols),
            rows: usize::from(rows),
        };
        Self {
            term: Term::new(config, &dimensions, proxy),
            processor: Processor::new(),
            shared,
            saved_primary: None,
            modes: RecoveryModes::default(),
            mode_parser: alacritty_terminal::vte::Parser::new(),
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        let was_alternate = self.term.mode().contains(TermMode::ALT_SCREEN);
        self.mode_parser.advance(&mut self.modes, bytes);
        if std::mem::take(&mut self.modes.reset_seen) {
            self.shared.lock().title = None;
        }
        if !was_alternate && self.modes.alternate {
            let split = find_alt_screen_enter(bytes).unwrap_or(0);
            if split > 0 {
                self.processor.advance(&mut self.term, &bytes[..split]);
            }
            let mut primary = Vec::new();
            format_screen_contents(&self.term, true, &mut primary);
            self.saved_primary = Some(primary);
            self.processor.advance(&mut self.term, &bytes[split..]);
        } else {
            self.processor.advance(&mut self.term, bytes);
        }
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.saved_primary = None;
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        self.shared.lock().window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: CELL_WIDTH,
            cell_height: CELL_HEIGHT,
        };
        self.term.resize(TerminalDimensions {
            columns: usize::from(cols),
            rows: usize::from(rows),
        });
    }

    fn format_snapshot(&self) -> Vec<u8> {
        let mut data = Vec::new();
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            if let Some(primary) = &self.saved_primary {
                data.extend_from_slice(primary);
            }
            data.extend_from_slice(b"\x1b[?1049h");
            format_screen_contents(&self.term, false, &mut data);
        } else {
            format_screen_contents(&self.term, true, &mut data);
        }
        format_terminal_modes(&self.term, &mut data);
        for flags in &self.modes.keyboard {
            let _ = write!(data, "\x1b[>{flags}u");
        }
        if let Some(title) = &self.shared.lock().title {
            data.extend_from_slice(b"\x1b]2;");
            data.extend_from_slice(title.as_bytes());
            data.extend_from_slice(b"\x1b\\");
        }
        if let Some(cwd_osc) = &self.modes.cwd_osc {
            data.extend_from_slice(cwd_osc);
        }
        data
    }
}

pub(crate) struct TerminalState {
    cols: u16,
    rows: u16,
    shadow: ShadowEmulator,
    bytes: VecDeque<u8>,
    offset: u64,
    exited: bool,
}

impl TerminalState {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            shadow: ShadowEmulator::new(cols, rows),
            bytes: VecDeque::new(),
            offset: 0,
            exited: false,
        }
    }

    fn update_shadow(&mut self, op: impl FnOnce(&mut ShadowEmulator)) {
        if catch_unwind(AssertUnwindSafe(|| op(&mut self.shadow))).is_err() {
            eprintln!("waku-daemon shadow terminal emulator panicked; resetting shadow state");
            self.shadow = ShadowEmulator::new(self.cols, self.rows);
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.update_shadow(|shadow| shadow.append(bytes));
        self.offset += bytes.len() as u64;
        self.bytes.extend(bytes);
        let excess = self.bytes.len().saturating_sub(REPLAY_BYTES);
        self.bytes.drain(..excess);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        self.cols = cols;
        self.rows = rows;
        self.update_shadow(|shadow| shadow.resize(cols, rows));
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
            catch_unwind(AssertUnwindSafe(|| self.shadow.format_snapshot())).unwrap_or_else(|_| {
                eprintln!(
                    "waku-daemon shadow terminal snapshot panicked; returning blank snapshot"
                );
                Vec::new()
            })
        };
        let grid = self.shadow.term.grid();
        TerminalOutput {
            offset: self.offset,
            reset,
            rows: grid.screen_lines().min(usize::from(u16::MAX)) as u16,
            cols: grid.columns().min(usize::from(u16::MAX)) as u16,
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
        std::mem::take(&mut self.shadow.shared.lock().replies)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: Color,
    bg: Color,
    underline_color: Option<Color>,
    flags: Flags,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            underline_color: None,
            flags: Flags::empty(),
        }
    }
}

impl CellStyle {
    fn from_cell(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            underline_color: cell.underline_color(),
            flags: cell.flags & VISUAL_FLAGS,
        }
    }
}

struct AnsiWriter<'a> {
    out: &'a mut Vec<u8>,
    cur_style: CellStyle,
    cur_hyperlink: Option<String>,
}

impl<'a> AnsiWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        out.extend_from_slice(b"\x1b[0m");
        Self {
            out,
            cur_style: CellStyle::default(),
            cur_hyperlink: None,
        }
    }

    fn set_style(&mut self, style: CellStyle) {
        if style == self.cur_style {
            return;
        }
        self.cur_style = style;
        if style == CellStyle::default() {
            self.out.extend_from_slice(b"\x1b[0m");
            return;
        }
        self.out.extend_from_slice(b"\x1b[0");
        if style.flags.contains(Flags::BOLD) {
            self.out.extend_from_slice(b";1");
        }
        if style.flags.contains(Flags::DIM) {
            self.out.extend_from_slice(b";2");
        }
        if style.flags.contains(Flags::ITALIC) {
            self.out.extend_from_slice(b";3");
        }
        if style.flags.contains(Flags::DOUBLE_UNDERLINE) {
            self.out.extend_from_slice(b";4:2");
        } else if style.flags.contains(Flags::UNDERCURL) {
            self.out.extend_from_slice(b";4:3");
        } else if style.flags.contains(Flags::DOTTED_UNDERLINE) {
            self.out.extend_from_slice(b";4:4");
        } else if style.flags.contains(Flags::DASHED_UNDERLINE) {
            self.out.extend_from_slice(b";4:5");
        } else if style.flags.contains(Flags::UNDERLINE) {
            self.out.extend_from_slice(b";4");
        }
        if style.flags.contains(Flags::INVERSE) {
            self.out.extend_from_slice(b";7");
        }
        if style.flags.contains(Flags::HIDDEN) {
            self.out.extend_from_slice(b";8");
        }
        if style.flags.contains(Flags::STRIKEOUT) {
            self.out.extend_from_slice(b";9");
        }
        write_fg_sgr(self.out, style.fg);
        write_bg_sgr(self.out, style.bg);
        if let Some(ul_color) = style.underline_color {
            write_ul_sgr(self.out, ul_color);
        }
        self.out.push(b'm');
    }

    fn set_hyperlink(&mut self, uri: Option<&str>) {
        let clean = uri.map(sanitize_osc_text).filter(|text| !text.is_empty());
        if clean.as_deref() == self.cur_hyperlink.as_deref() {
            return;
        }
        match clean {
            Some(uri) => {
                self.out.extend_from_slice(b"\x1b]8;;");
                self.out.extend_from_slice(uri.as_bytes());
                self.out.extend_from_slice(b"\x1b\\");
                self.cur_hyperlink = Some(uri);
            }
            None => {
                self.out.extend_from_slice(b"\x1b]8;;\x1b\\");
                self.cur_hyperlink = None;
            }
        }
    }

    fn emit_cell(&mut self, cell: &Cell, col: usize, cols: usize) -> usize {
        self.set_style(CellStyle::from_cell(cell));
        let hyperlink = cell.hyperlink();
        self.set_hyperlink(hyperlink.as_ref().map(|link| link.uri()));
        if cell.flags.contains(Flags::WIDE_CHAR) && col + 1 < cols {
            push_sanitized_cell_text(self.out, cell);
            2
        } else if cell.flags.intersects(
            Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
        ) {
            self.out.push(b' ');
            1
        } else {
            push_sanitized_cell_text(self.out, cell);
            1
        }
    }
}

fn sanitize_osc_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

fn sanitize_cell_char(ch: char) -> char {
    if ch.is_control() { ' ' } else { ch }
}

fn push_sanitized_cell_text(out: &mut Vec<u8>, cell: &Cell) {
    let mut buf = [0u8; 4];
    let ch = sanitize_cell_char(cell.c);
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    if let Some(zerowidth) = cell.zerowidth() {
        for &zw in zerowidth {
            if !zw.is_control() {
                out.extend_from_slice(zw.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
}

fn write_fg_sgr(out: &mut Vec<u8>, color: Color) {
    match color {
        Color::Named(named) => match named {
            NamedColor::Black | NamedColor::DimBlack => out.extend_from_slice(b";30"),
            NamedColor::Red | NamedColor::DimRed => out.extend_from_slice(b";31"),
            NamedColor::Green | NamedColor::DimGreen => out.extend_from_slice(b";32"),
            NamedColor::Yellow | NamedColor::DimYellow => out.extend_from_slice(b";33"),
            NamedColor::Blue | NamedColor::DimBlue => out.extend_from_slice(b";34"),
            NamedColor::Magenta | NamedColor::DimMagenta => out.extend_from_slice(b";35"),
            NamedColor::Cyan | NamedColor::DimCyan => out.extend_from_slice(b";36"),
            NamedColor::White | NamedColor::DimWhite => out.extend_from_slice(b";37"),
            NamedColor::BrightBlack => out.extend_from_slice(b";90"),
            NamedColor::BrightRed => out.extend_from_slice(b";91"),
            NamedColor::BrightGreen => out.extend_from_slice(b";92"),
            NamedColor::BrightYellow => out.extend_from_slice(b";93"),
            NamedColor::BrightBlue => out.extend_from_slice(b";94"),
            NamedColor::BrightMagenta => out.extend_from_slice(b";95"),
            NamedColor::BrightCyan => out.extend_from_slice(b";96"),
            NamedColor::BrightWhite => out.extend_from_slice(b";97"),
            NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::BrightForeground
            | NamedColor::DimForeground
            | NamedColor::Cursor => {}
        },
        Color::Indexed(index) => {
            let _ = write!(out, ";38;5;{index}");
        }
        Color::Spec(Rgb { r, g, b }) => {
            let _ = write!(out, ";38;2;{r};{g};{b}");
        }
    }
}

fn write_bg_sgr(out: &mut Vec<u8>, color: Color) {
    match color {
        Color::Named(named) => match named {
            NamedColor::Black | NamedColor::DimBlack => out.extend_from_slice(b";40"),
            NamedColor::Red | NamedColor::DimRed => out.extend_from_slice(b";41"),
            NamedColor::Green | NamedColor::DimGreen => out.extend_from_slice(b";42"),
            NamedColor::Yellow | NamedColor::DimYellow => out.extend_from_slice(b";43"),
            NamedColor::Blue | NamedColor::DimBlue => out.extend_from_slice(b";44"),
            NamedColor::Magenta | NamedColor::DimMagenta => out.extend_from_slice(b";45"),
            NamedColor::Cyan | NamedColor::DimCyan => out.extend_from_slice(b";46"),
            NamedColor::White | NamedColor::DimWhite => out.extend_from_slice(b";47"),
            NamedColor::BrightBlack => out.extend_from_slice(b";100"),
            NamedColor::BrightRed => out.extend_from_slice(b";101"),
            NamedColor::BrightGreen => out.extend_from_slice(b";102"),
            NamedColor::BrightYellow => out.extend_from_slice(b";103"),
            NamedColor::BrightBlue => out.extend_from_slice(b";104"),
            NamedColor::BrightMagenta => out.extend_from_slice(b";105"),
            NamedColor::BrightCyan => out.extend_from_slice(b";106"),
            NamedColor::BrightWhite => out.extend_from_slice(b";107"),
            NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::BrightForeground
            | NamedColor::DimForeground
            | NamedColor::Cursor => {}
        },
        Color::Indexed(index) => {
            let _ = write!(out, ";48;5;{index}");
        }
        Color::Spec(Rgb { r, g, b }) => {
            let _ = write!(out, ";48;2;{r};{g};{b}");
        }
    }
}

fn write_ul_sgr(out: &mut Vec<u8>, color: Color) {
    match color {
        Color::Named(named) => match named {
            NamedColor::Black
            | NamedColor::Red
            | NamedColor::Green
            | NamedColor::Yellow
            | NamedColor::Blue
            | NamedColor::Magenta
            | NamedColor::Cyan
            | NamedColor::White
            | NamedColor::BrightBlack
            | NamedColor::BrightRed
            | NamedColor::BrightGreen
            | NamedColor::BrightYellow
            | NamedColor::BrightBlue
            | NamedColor::BrightMagenta
            | NamedColor::BrightCyan
            | NamedColor::BrightWhite => {
                let _ = write!(out, ";58;5;{}", named as u8);
            }
            NamedColor::DimBlack => out.extend_from_slice(b";58;5;0"),
            NamedColor::DimRed => out.extend_from_slice(b";58;5;1"),
            NamedColor::DimGreen => out.extend_from_slice(b";58;5;2"),
            NamedColor::DimYellow => out.extend_from_slice(b";58;5;3"),
            NamedColor::DimBlue => out.extend_from_slice(b";58;5;4"),
            NamedColor::DimMagenta => out.extend_from_slice(b";58;5;5"),
            NamedColor::DimCyan => out.extend_from_slice(b";58;5;6"),
            NamedColor::DimWhite => out.extend_from_slice(b";58;5;7"),
            NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::BrightForeground
            | NamedColor::DimForeground
            | NamedColor::Cursor => {}
        },
        Color::Indexed(index) => {
            let _ = write!(out, ";58;5;{index}");
        }
        Color::Spec(Rgb { r, g, b }) => {
            let _ = write!(out, ";58;2;{r};{g};{b}");
        }
    }
}

fn is_default_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.fg == Color::Named(NamedColor::Foreground)
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell.flags.intersects(
            VISUAL_FLAGS
                | Flags::WIDE_CHAR
                | Flags::WIDE_CHAR_SPACER
                | Flags::LEADING_WIDE_CHAR_SPACER,
        )
        && cell.underline_color().is_none()
        && cell.hyperlink().is_none()
        && cell.zerowidth().is_none_or(<[char]>::is_empty)
}

fn format_screen_contents<T>(term: &Term<T>, include_scrollback: bool, out: &mut Vec<u8>) {
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    if cols == 0 || rows == 0 {
        return;
    }

    if include_scrollback {
        let history = grid.history_size();
        if history > 0 {
            let mut line_text = String::new();
            for line_idx in -(history as i32)..0 {
                line_text.clear();
                let row = &grid[Line(line_idx)];
                let mut col = 0;
                while col < cols {
                    let cell = &row[Column(col)];
                    if cell.flags.contains(Flags::WIDE_CHAR) && col + 1 < cols {
                        line_text.push(sanitize_cell_char(cell.c));
                        if let Some(zerowidth) = cell.zerowidth() {
                            for &zw in zerowidth {
                                if !zw.is_control() {
                                    line_text.push(zw);
                                }
                            }
                        }
                        col += 2;
                    } else if cell.flags.intersects(
                        Flags::WIDE_CHAR
                            | Flags::WIDE_CHAR_SPACER
                            | Flags::LEADING_WIDE_CHAR_SPACER,
                    ) {
                        line_text.push(' ');
                        col += 1;
                    } else {
                        line_text.push(sanitize_cell_char(cell.c));
                        if let Some(zerowidth) = cell.zerowidth() {
                            for &zw in zerowidth {
                                if !zw.is_control() {
                                    line_text.push(zw);
                                }
                            }
                        }
                        col += 1;
                    }
                }
                out.extend_from_slice(line_text.trim_end_matches(' ').as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            for _ in 0..rows.saturating_sub(1) {
                out.extend_from_slice(b"\r\n");
            }
        }
    }

    let mut writer = AnsiWriter::new(out);
    let mut prev_row_wrapped = false;

    for row_idx in 0..rows {
        let row = &grid[Line(row_idx as i32)];
        let row_wrapped =
            row_idx + 1 < rows && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let last_cell = &row[Column(cols - 1)];
        let last_is_leading_spacer = last_cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER);
        let defer_leading_spacer = row_wrapped && last_is_leading_spacer && row_idx + 1 < rows && {
            let next_first = &grid[Line((row_idx + 1) as i32)][Column(0)];
            next_first.flags.contains(Flags::WIDE_CHAR) && last_cell.bg == next_first.bg
        };

        let end_col = if defer_leading_spacer {
            cols - 1
        } else if row_wrapped {
            cols
        } else {
            (0..cols)
                .rfind(|&col| !is_default_blank(&row[Column(col)]))
                .map_or(0, |col| col + 1)
        };

        if end_col == 0 {
            if prev_row_wrapped {
                writer.set_style(CellStyle::default());
                writer.set_hyperlink(None);
                writer.out.push(b' ');
            }
            prev_row_wrapped = false;
            continue;
        }

        if !prev_row_wrapped {
            let _ = write!(writer.out, "\x1b[{};1H", row_idx + 1);
        }

        let mut col = 0;
        while col < end_col {
            let cell = &row[Column(col)];
            if !(prev_row_wrapped && col == 0) && !row_wrapped && is_default_blank(cell) {
                let mut skip = 1;
                while col + skip < end_col && is_default_blank(&row[Column(col + skip)]) {
                    skip += 1;
                }
                if skip >= 6 {
                    let _ = write!(writer.out, "\x1b[{skip}C");
                    col += skip;
                    continue;
                }
            }
            col += writer.emit_cell(cell, col, cols);
        }

        prev_row_wrapped = row_wrapped;
    }

    let cursor_row = grid
        .cursor
        .point
        .line
        .0
        .clamp(0, rows.saturating_sub(1) as i32) as usize;
    let cursor_col = grid.cursor.point.column.0.min(cols.saturating_sub(1));
    let _ = write!(writer.out, "\x1b[{};{}H", cursor_row + 1, cursor_col + 1);

    if grid.cursor.input_needs_wrap && cursor_col + 1 == cols {
        let cell = &grid[Line(cursor_row as i32)][Column(cursor_col)];
        if !cell
            .flags
            .intersects(Flags::LEADING_WIDE_CHAR_SPACER | Flags::WIDE_CHAR)
        {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                if cols >= 2 {
                    let prev = &grid[Line(cursor_row as i32)][Column(cols - 2)];
                    if prev.flags.contains(Flags::WIDE_CHAR) {
                        let _ = write!(writer.out, "\x1b[{};{}H", cursor_row + 1, cols - 1);
                        writer.emit_cell(prev, cols - 2, cols);
                    }
                }
            } else {
                writer.emit_cell(cell, cursor_col, cols);
            }
        }
    }

    writer.set_hyperlink(None);
    writer.set_style(CellStyle::from_cell(&grid.cursor.template));
}

fn format_terminal_modes<T>(term: &Term<T>, out: &mut Vec<u8>) {
    let mode = *term.mode();
    if !mode.contains(TermMode::SHOW_CURSOR) {
        out.extend_from_slice(b"\x1b[?25l");
    }
    let cursor_style = term.cursor_style();
    if cursor_style.shape != CursorShape::Block || cursor_style.blinking {
        let code = match (cursor_style.shape, cursor_style.blinking) {
            (CursorShape::Block, true) => Some(1),
            (CursorShape::Block, false) => Some(2),
            (CursorShape::Underline, true) => Some(3),
            (CursorShape::Underline, false) => Some(4),
            (CursorShape::Beam, true) => Some(5),
            (CursorShape::Beam, false) => Some(6),
            (CursorShape::HollowBlock | CursorShape::Hidden, _) => None,
        };
        if let Some(code) = code {
            let _ = write!(out, "\x1b[{code} q");
        }
    }
    if mode.contains(TermMode::APP_CURSOR) {
        out.extend_from_slice(b"\x1b[?1h");
    }
    if mode.contains(TermMode::APP_KEYPAD) {
        out.extend_from_slice(b"\x1b=");
    }
    if mode.contains(TermMode::BRACKETED_PASTE) {
        out.extend_from_slice(b"\x1b[?2004h");
    }
    if mode.contains(TermMode::FOCUS_IN_OUT) {
        out.extend_from_slice(b"\x1b[?1004h");
    }
    if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        out.extend_from_slice(b"\x1b[?1000h");
    } else if mode.contains(TermMode::MOUSE_DRAG) {
        out.extend_from_slice(b"\x1b[?1002h");
    } else if mode.contains(TermMode::MOUSE_MOTION) {
        out.extend_from_slice(b"\x1b[?1003h");
    }
    if mode.contains(TermMode::SGR_MOUSE) {
        out.extend_from_slice(b"\x1b[?1006h");
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        out.extend_from_slice(b"\x1b[?1005h");
    }
    if !mode.contains(TermMode::ALTERNATE_SCROLL) {
        out.extend_from_slice(b"\x1b[?1007l");
    }
    if !mode.contains(TermMode::LINE_WRAP) {
        out.extend_from_slice(b"\x1b[?7l");
    }
    if mode.contains(TermMode::INSERT) {
        out.extend_from_slice(b"\x1b[4h");
    }
    if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
        out.extend_from_slice(b"\x1b[20h");
    }
}

fn find_alt_screen_enter(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(b"\x1b[?1049h".len())
        .position(|w| w == b"\x1b[?1049h")
}

/// Tracks kitty keyboard stack, cwd OSCs, and full resets that `Term` keeps
/// private or ignores.
#[derive(Default)]
struct RecoveryModes {
    keyboard: Vec<u16>,
    inactive_keyboard: Vec<u16>,
    alternate: bool,
    cwd_osc: Option<Vec<u8>>,
    reset_seen: bool,
}

impl Perform for RecoveryModes {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        if intermediates == b"?"
            && matches!(action, 'h' | 'l')
            && params.iter().any(|p| p.first() == Some(&1049))
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
        let mut args = params.iter();
        let first = args.next().and_then(|p| p.first()).copied().unwrap_or(0);
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

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        match params {
            [b"7", uri, ..]
                if !uri.is_empty()
                    && uri.len() <= 2048
                    && !uri.iter().any(|&b| b < 0x20 || b == 0x7f) =>
            {
                let mut osc = Vec::with_capacity(uri.len() + 6);
                osc.extend_from_slice(b"\x1b]7;");
                osc.extend_from_slice(uri);
                osc.extend_from_slice(b"\x1b\\");
                self.cwd_osc = Some(osc);
            }
            [b"9", b"9", path, ..]
                if !path.is_empty()
                    && path.len() <= 2048
                    && !path.iter().any(|&b| b < 0x20 || b == 0x7f) =>
            {
                let mut osc = Vec::with_capacity(path.len() + 8);
                osc.extend_from_slice(b"\x1b]9;9;");
                osc.extend_from_slice(path);
                osc.extend_from_slice(b"\x1b\\");
                self.cwd_osc = Some(osc);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.keyboard.clear();
            self.inactive_keyboard.clear();
            self.alternate = false;
            self.cwd_osc = None;
            self.reset_seen = true;
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
    use alacritty_terminal::event::VoidListener;

    #[derive(Default)]
    struct ReplayProbe {
        title: Option<String>,
        cwd_osc: Option<String>,
    }

    impl ReplayProbe {
        fn parse(&mut self, bytes: &[u8]) {
            let mut parser = alacritty_terminal::vte::Parser::new();
            parser.advance(self, bytes);
        }
    }

    impl Perform for ReplayProbe {
        fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
            match params {
                [b"0" | b"2", title, ..] => {
                    self.title = Some(String::from_utf8_lossy(title).into_owned());
                }
                [b"7", uri, ..] => {
                    self.cwd_osc = Some(format!("7:{}", String::from_utf8_lossy(uri)));
                }
                [b"9", b"9", path, ..] => {
                    self.cwd_osc = Some(format!("9;9:{}", String::from_utf8_lossy(path)));
                }
                _ => {}
            }
        }
    }

    fn replay_snapshot(snapshot: &TerminalOutput) -> (Term<VoidListener>, ReplayProbe) {
        let dims = TerminalDimensions {
            columns: usize::from(snapshot.cols),
            rows: usize::from(snapshot.rows),
        };
        let mut term = Term::new(
            Config {
                scrolling_history: SCROLLBACK_LINES,
                kitty_keyboard: true,
                ..Default::default()
            },
            &dims,
            VoidListener,
        );
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, b"\x1bc");
        term.resize(dims);
        processor.advance(&mut term, &snapshot.data);
        let mut probe = ReplayProbe::default();
        probe.parse(&snapshot.data);
        (term, probe)
    }

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

    #[test]
    fn wide_char_shrink_resize_and_overwrite_never_panics() {
        let widths = [110_u16, 107, 98, 76, 75, 48, 20, 3, 2];
        let mut state = TerminalState::new(120, 24);
        let cjk_line = "中文字符边界测试🙂宽度截断恢复验证🚀".repeat(4);
        for row in 0..24 {
            state.append(format!("row{row:02}:{cjk_line}\r\n").as_bytes());
        }

        for &width in &widths {
            state.resize(width, 24);
            // Overwrite and erase near the truncated right edge at odd & even widths.
            state.append(
                format!(
                    "\x1b[1;{}H\x1b[K\x1b[2;1H\x1b[2K\x1b[3;{}H中\x1b[1X\x1b[1P\x1b[1@好",
                    width,
                    width.saturating_sub(1)
                )
                .as_bytes(),
            );
            let snapshot = state.snapshot(None);
            assert_eq!(snapshot.cols, width);
            let (replayed, _) = replay_snapshot(&snapshot);
            assert_eq!(replayed.grid().columns(), usize::from(width));
        }

        // Repeat on the alternate screen (reflow == false).
        let mut alt_state = TerminalState::new(120, 24);
        alt_state.append(b"\x1b[?1049h");
        for row in 1..=24 {
            alt_state.append(format!("\x1b[{row};1H{cjk_line}").as_bytes());
        }
        for &width in &widths {
            alt_state.resize(width, 24);
            alt_state.append(
                format!(
                    "\x1b[5;{}H界\x1b[K\x1b[6;{}H🚀\x1b[1X\x1b[1P\x1b[1@测",
                    width,
                    width.saturating_sub(1)
                )
                .as_bytes(),
            );
            let snapshot = alt_state.snapshot(None);
            assert_eq!(snapshot.cols, width);
            let (replayed, _) = replay_snapshot(&snapshot);
            assert!(replayed.mode().contains(TermMode::ALT_SCREEN));
            assert_eq!(replayed.grid().columns(), usize::from(width));
        }
    }

    #[test]
    fn snapshot_round_trips_colors_styles_wide_chars_scrollback_and_modes() {
        let mut state = TerminalState::new(30, 8);
        for i in 0..35 {
            state.append(format!("history-{i:02}: 中文滚动行\r\n").as_bytes());
        }
        // Styled row with named, indexed, truecolor, bold, dim, italic, underline,
        // undercurl, inverse, strikeout.
        state.append(
            b"\x1b[2J\x1b[1;1H\x1b[1;31;44mR\x1b[0;2;3;38;5;214;48;2;12;34;56mI\x1b[0;4;9mU\x1b[0;4:3;7mC\x1b[0m",
        );
        // Wide CJK + combining mark on row 2.
        state.append("\x1b[2;1H汉e\u{0301}字".as_bytes());
        // Wrapped line across row 3 -> row 4, including wide char pushed to row 4.
        state.append("\x1b[3;1H".as_bytes());
        state.append("a".repeat(29).as_bytes());
        state.append("宽".as_bytes());
        // Colored blank status bar on row 6.
        state.append(b"\x1b[6;1H\x1b[48;5;24m");
        state.append(&[b' '; 30]);
        state.append(b"\x1b[0m");
        // Modes, cursor style/position, title, Windows cwd.
        state.append(
            b"\x1b[?25l\x1b[6 q\x1b[?1h\x1b=\x1b[?2004h\x1b[?1004h\x1b[?1002h\x1b[?1006h\x1b]2;waku-test-title\x1b\\\x1b]9;9;C:\\Users\\gaoxi\\git\\waku\x1b\\\x1b[5;12H",
        );

        let snapshot = state.snapshot(None);
        let (replayed, probe) = replay_snapshot(&snapshot);
        let orig = state.shadow.term.grid();
        let copy = replayed.grid();

        assert_eq!(copy.history_size(), orig.history_size());
        for line_idx in -(orig.history_size() as i32)..0 {
            let orig_line = Line(line_idx);
            let copy_line = Line(line_idx);
            for col in 0..30 {
                assert_eq!(
                    copy[copy_line][Column(col)].c,
                    orig[orig_line][Column(col)].c,
                    "history mismatch at line {line_idx} col {col}"
                );
            }
        }

        for row in 0..8_i32 {
            for col in 0..30 {
                let expected = &orig[Line(row)][Column(col)];
                let actual = &copy[Line(row)][Column(col)];
                assert_eq!(
                    (
                        actual.c,
                        actual.fg,
                        actual.bg,
                        actual.flags,
                        actual.zerowidth(),
                    ),
                    (
                        expected.c,
                        expected.fg,
                        expected.bg,
                        expected.flags,
                        expected.zerowidth(),
                    ),
                    "cell mismatch at row {row} col {col}"
                );
            }
        }

        assert_eq!(copy.cursor.point, orig.cursor.point);
        assert_eq!(replayed.cursor_style(), state.shadow.term.cursor_style());
        assert_eq!(*replayed.mode(), *state.shadow.term.mode());
        assert_eq!(probe.title.as_deref(), Some("waku-test-title"));
        assert_eq!(
            probe.cwd_osc.as_deref(),
            Some("9;9:C:\\Users\\gaoxi\\git\\waku")
        );
    }

    #[test]
    fn device_queries_answer_dsr_cpr_da_kitty_and_xtwinops() {
        let mut state = TerminalState::new(80, 24);
        state.append(b"\x1b[5n");
        assert_eq!(state.take_replies(), b"\x1b[0n");

        state.append(b"\x1b[3;7H\x1b[6n");
        assert_eq!(state.take_replies(), b"\x1b[3;7R");

        state.append(b"\x1b[c");
        assert_eq!(state.take_replies(), b"\x1b[?6c");

        state.append(b"\x1b[>c");
        let da2 = state.take_replies();
        assert!(da2.starts_with(b"\x1b[>0;") && da2.ends_with(b";1c"));

        state.append(b"\x1b[>5u\x1b[?u");
        assert_eq!(state.take_replies(), b"\x1b[?5u");
        state.append(b"\x1b[<u\x1b[?u");
        assert_eq!(state.take_replies(), b"\x1b[?0u");

        state.append(b"\x1b[14t\x1b[18t");
        assert_eq!(state.take_replies(), b"\x1b[4;384;640t\x1b[8;24;80t");

        state.resize(100, 30);
        state.append(b"\x1b[14t\x1b[18t");
        assert_eq!(state.take_replies(), b"\x1b[4;480;800t\x1b[8;30;100t");

        // Palette queries belong to the client renderer, not the daemon.
        state.append(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
        assert!(state.take_replies().is_empty());
    }

    #[test]
    fn synchronized_update_does_not_stall_shadow_state_or_replies() {
        let mut state = TerminalState::new(80, 24);
        state.append(b"\x1b[?2026hhello\x1b[6n");
        assert_eq!(state.take_replies(), b"\x1b[1;6R");
        let snapshot = state.snapshot(None);
        assert!(String::from_utf8_lossy(&snapshot.data).contains("hello"));
    }

    #[test]
    fn shadow_panic_isolation_preserves_offset_and_recovers() {
        let mut state = TerminalState::new(80, 24);
        state.append(b"before");
        let before = state.snapshot(None);
        state.update_shadow(|_| panic!("simulated parser panic"));
        state.append(b"alive");
        let inc = state.snapshot(Some(before.offset));
        assert!(!inc.reset);
        assert_eq!(inc.data, b"alive");
        let full = state.snapshot(None);
        assert!(full.reset);
        assert_eq!(full.offset, before.offset + 5);
        assert!(String::from_utf8_lossy(&full.data).contains("alive"));
    }
}
