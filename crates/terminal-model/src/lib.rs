use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{Config, Osc52, TermMode, point_to_viewport, viewport_to_point};
use alacritty_terminal::vte::ansi::{self, Color, NamedColor};
use hh_protocol::{
    TerminalAttributes, TerminalColor, TerminalCursor, TerminalLine, TerminalModifiers,
    TerminalMouseAction, TerminalMouseButton, TerminalPoint, TerminalRun, TerminalSelection,
    TerminalSelectionKind,
};

pub const SCROLLBACK_HISTORY_LIMIT: usize = 2_000;
const MAX_SAFE_TITLE_CHARS: usize = 80;
const MAX_STYLE_RUNS_PER_LINE: usize = 128;
const MAX_TOTAL_STYLE_RUNS: usize = 3_000;
const MAX_OSC_NOTIFICATION_BYTES: usize = 512;
const MAX_OSC_SEQUENCE_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, Default)]
struct TermEventListener {
    title: Arc<Mutex<Option<String>>>,
    bells: Arc<AtomicU64>,
}

impl EventListener for TermEventListener {
    fn send_event(&self, event: Event) {
        if matches!(event, Event::Bell) {
            self.bells.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let next = match event {
            Event::Title(title)
                if title.chars().count() <= MAX_SAFE_TITLE_CHARS
                    && !title.chars().any(char::is_control) =>
            {
                Some(Some(title))
            }
            Event::Title(_) | Event::ResetTitle => Some(None),
            _ => None,
        };
        if let Some(next) = next
            && let Ok(mut title) = self.title.lock()
        {
            *title = next;
        }
    }
}

#[derive(Debug, Default)]
struct OscNotificationScanner {
    state: OscScanState,
}

#[derive(Debug, Default)]
enum OscScanState {
    #[default]
    Ground,
    Escape,
    Osc {
        bytes: Vec<u8>,
        escape_pending: bool,
    },
}

impl OscNotificationScanner {
    fn scan(&mut self, input: &[u8], messages: &mut Vec<String>) {
        for &byte in input {
            let state = std::mem::take(&mut self.state);
            self.state = match state {
                OscScanState::Ground | OscScanState::Escape if byte == b'\x1b' => {
                    OscScanState::Escape
                }
                OscScanState::Escape if byte == b']' => OscScanState::Osc {
                    bytes: Vec::new(),
                    escape_pending: false,
                },
                OscScanState::Ground | OscScanState::Escape => OscScanState::Ground,
                OscScanState::Osc {
                    bytes,
                    escape_pending: true,
                } if byte == b'\\' => {
                    Self::finish(&bytes, messages);
                    OscScanState::Ground
                }
                OscScanState::Osc {
                    mut bytes,
                    escape_pending,
                } if byte == b'\x07' => {
                    if escape_pending && bytes.len() < MAX_OSC_SEQUENCE_BYTES {
                        bytes.push(b'\x1b');
                    }
                    Self::finish(&bytes, messages);
                    OscScanState::Ground
                }
                OscScanState::Osc {
                    mut bytes,
                    escape_pending,
                } => {
                    if escape_pending {
                        if bytes.len() >= MAX_OSC_SEQUENCE_BYTES {
                            continue;
                        }
                        bytes.push(b'\x1b');
                    }
                    if byte == b'\x1b' {
                        OscScanState::Osc {
                            bytes,
                            escape_pending: true,
                        }
                    } else if bytes.len() >= MAX_OSC_SEQUENCE_BYTES {
                        OscScanState::Ground
                    } else {
                        bytes.push(byte);
                        OscScanState::Osc {
                            bytes,
                            escape_pending: false,
                        }
                    }
                }
            };
        }
    }

    fn finish(sequence: &[u8], messages: &mut Vec<String>) {
        let raw = if let Some(message) = sequence.strip_prefix(b"9;") {
            message.to_vec()
        } else if let Some(payload) = sequence.strip_prefix(b"777;notify;") {
            let Some(separator) = payload.iter().position(|byte| *byte == b';') else {
                return;
            };
            let (title, body_with_separator) = payload.split_at(separator);
            let body = &body_with_separator[1..];
            if body.is_empty() {
                title.to_vec()
            } else {
                let mut message = Vec::with_capacity(title.len().saturating_add(body.len() + 2));
                message.extend_from_slice(title);
                message.extend_from_slice(b": ");
                message.extend_from_slice(body);
                message
            }
        } else {
            return;
        };
        let message = String::from_utf8_lossy(&raw);
        let mut cleaned = String::new();
        for character in message.chars().filter(|character| !character.is_control()) {
            if cleaned.len().saturating_add(character.len_utf8()) > MAX_OSC_NOTIFICATION_BYTES {
                break;
            }
            cleaned.push(character);
        }
        messages.push(cleaned);
    }
}

#[derive(Clone, Copy, Debug)]
struct TerminalSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Owns terminal parsing and screen state without tying it to a UI toolkit.
pub struct TerminalModel {
    parser: ansi::Processor,
    terminal: Term<TermEventListener>,
    title: Arc<Mutex<Option<String>>>,
    bells: Arc<AtomicU64>,
    scanner: OscNotificationScanner,
    pending_messages: Vec<String>,
    last_search: Option<(String, Point, Point)>,
}

impl std::fmt::Debug for TerminalModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalModel")
            .field("columns", &self.terminal.columns())
            .field("screen_lines", &self.terminal.screen_lines())
            .finish_non_exhaustive()
    }
}

impl TerminalModel {
    /// Creates an empty terminal grid.
    ///
    /// # Panics
    ///
    /// Panics when either dimension is zero because Alacritty's grid requires
    /// at least one visible row and column.
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        assert!(columns > 0, "terminal must have at least one column");
        assert!(screen_lines > 0, "terminal must have at least one line");
        let size = TerminalSize {
            columns,
            screen_lines,
        };
        let listener = TermEventListener::default();
        let title = Arc::clone(&listener.title);
        let bells = Arc::clone(&listener.bells);
        Self {
            parser: ansi::Processor::new(),
            terminal: Term::new(
                Config {
                    scrolling_history: SCROLLBACK_HISTORY_LIMIT,
                    osc52: Osc52::Disabled,
                    ..Config::default()
                },
                &size,
                listener,
            ),
            title,
            bells,
            scanner: OscNotificationScanner::default(),
            pending_messages: Vec::new(),
            last_search: None,
        }
    }

    pub fn process_output(&mut self, bytes: &[u8]) {
        self.scanner.scan(bytes, &mut self.pending_messages);
        self.parser.advance(&mut self.terminal, bytes);
    }

    pub fn bell_count(&self) -> u64 {
        self.bells.load(Ordering::Relaxed)
    }

    pub fn take_notification_messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_messages)
    }

    /// Resizes the terminal's visible grid while preserving its parser state.
    ///
    /// # Panics
    ///
    /// Panics when either dimension is zero because Alacritty's grid requires
    /// at least one visible row and column.
    pub fn resize(&mut self, columns: usize, screen_lines: usize) {
        assert!(columns > 0, "terminal must have at least one column");
        assert!(screen_lines > 0, "terminal must have at least one line");
        self.terminal.resize(TerminalSize {
            columns,
            screen_lines,
        });
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.terminal.columns(), self.terminal.screen_lines())
    }

    #[cfg(test)]
    fn terminal(&self) -> &Term<TermEventListener> {
        &self.terminal
    }

    /// Returns the latest bounded OSC window-title signal. The title is never
    /// derived from grid text and callers must treat it as ephemeral metadata.
    pub fn terminal_title(&self) -> Option<String> {
        self.title.lock().ok().and_then(|title| title.clone())
    }

    /// Returns the visible terminal grid as compact styled runs.
    pub fn styled_lines(&self) -> Vec<TerminalLine> {
        let display_offset =
            i32::try_from(self.terminal.grid().display_offset()).unwrap_or(i32::MAX);
        let mut styled_runs_remaining = MAX_TOTAL_STYLE_RUNS;
        (0..self.terminal.screen_lines())
            .map(|line| {
                let line = i32::try_from(line).unwrap_or(i32::MAX) - display_offset;
                let row = &self.terminal.grid()[Line(line)];
                let end = (0..self.terminal.columns())
                    .rposition(|column| {
                        let cell = &row[Column(column)];
                        cell.c != ' '
                            || cell.bg != Color::Named(NamedColor::Background)
                            || cell.flags.contains(Flags::INVERSE)
                            || cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    })
                    .map_or(0, |index| index + 1);
                let mut runs: Vec<TerminalRun> = Vec::new();
                for column in 0..end {
                    let cell = &row[Column(column)];
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        if let Some(previous) = runs.last_mut() {
                            previous.columns = previous.columns.saturating_add(1);
                        }
                        continue;
                    }
                    let mut foreground = terminal_color(cell.fg);
                    let mut background = terminal_color(cell.bg);
                    if cell.flags.contains(Flags::INVERSE) {
                        std::mem::swap(&mut foreground, &mut background);
                    }
                    let hidden = cell.flags.contains(Flags::HIDDEN);
                    let mut attributes = 0;
                    for (present, flag) in [
                        (cell.flags.contains(Flags::BOLD), TerminalAttributes::BOLD),
                        (cell.flags.contains(Flags::DIM), TerminalAttributes::DIM),
                        (
                            cell.flags.contains(Flags::ITALIC),
                            TerminalAttributes::ITALIC,
                        ),
                        (
                            cell.flags.intersects(Flags::ALL_UNDERLINES),
                            TerminalAttributes::UNDERLINE,
                        ),
                        (
                            cell.flags.contains(Flags::STRIKEOUT),
                            TerminalAttributes::STRIKETHROUGH,
                        ),
                    ] {
                        if present {
                            attributes |= flag;
                        }
                    }
                    let attributes = TerminalAttributes::new(attributes);
                    let extends_previous = runs.last().is_some_and(|previous| {
                        previous.foreground == foreground
                            && previous.background == background
                            && previous.attributes == attributes
                    });
                    let coarsen_style = !runs.is_empty()
                        && (runs.len() >= MAX_STYLE_RUNS_PER_LINE || styled_runs_remaining == 0);
                    if extends_previous || coarsen_style {
                        if let Some(previous) = runs.last_mut() {
                            previous.text.push(if hidden { ' ' } else { cell.c });
                            if !hidden && let Some(zerowidth) = cell.zerowidth() {
                                previous.text.extend(zerowidth);
                            }
                            previous.columns = previous.columns.saturating_add(1);
                        }
                    } else {
                        let mut text = String::from(if hidden { ' ' } else { cell.c });
                        if !hidden && let Some(zerowidth) = cell.zerowidth() {
                            text.extend(zerowidth);
                        }
                        runs.push(TerminalRun {
                            text,
                            columns: 1,
                            foreground,
                            background,
                            attributes,
                        });
                        styled_runs_remaining = styled_runs_remaining.saturating_sub(1);
                    }
                }
                TerminalLine { runs }
            })
            .collect()
    }

    /// Returns the visible terminal cursor position when the application has
    /// not hidden it.
    pub fn cursor(&self) -> Option<TerminalCursor> {
        if self.terminal.grid().display_offset() != 0
            || !self.terminal.mode().contains(TermMode::SHOW_CURSOR)
        {
            return None;
        }
        let point = self.terminal.grid().cursor.point;
        Some(TerminalCursor {
            row: u16::try_from(point.line.0).ok()?,
            column: u16::try_from(point.column.0).ok()?,
        })
    }

    pub fn selection(&self) -> Option<TerminalSelection> {
        let range = self
            .terminal
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.terminal))?;
        let display_offset = self.terminal.grid().display_offset();
        let start = point_to_viewport(display_offset, range.start)?;
        let end = point_to_viewport(display_offset, range.end)?;
        let last_row = self.terminal.screen_lines().saturating_sub(1);
        if start.line > last_row || end.line > last_row {
            return None;
        }
        Some(TerminalSelection {
            start: TerminalPoint {
                row: u16::try_from(start.line).ok()?,
                column: u16::try_from(start.column.0).ok()?,
            },
            end: TerminalPoint {
                row: u16::try_from(end.line).ok()?,
                column: u16::try_from(end.column.0).ok()?,
            },
            is_block: range.is_block,
        })
    }

    pub fn display_offset(&self) -> usize {
        self.terminal.grid().display_offset()
    }

    pub fn history_size(&self) -> usize {
        self.terminal.grid().history_size()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.terminal.mode().contains(TermMode::BRACKETED_PASTE)
    }

    pub fn mouse_reporting(&self) -> bool {
        self.terminal.mode().intersects(TermMode::MOUSE_MODE)
    }

    pub fn mouse_motion(&self) -> bool {
        self.terminal
            .mode()
            .intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
    }

    pub fn sgr_mouse(&self) -> bool {
        self.terminal.mode().contains(TermMode::SGR_MOUSE)
    }

    pub fn begin_selection(&mut self, point: TerminalPoint, kind: TerminalSelectionKind) {
        self.last_search = None;
        let point = self.viewport_point(point);
        let ty = match kind {
            TerminalSelectionKind::Simple => SelectionType::Simple,
            TerminalSelectionKind::Block => SelectionType::Block,
            TerminalSelectionKind::Semantic => SelectionType::Semantic,
            TerminalSelectionKind::Lines => SelectionType::Lines,
        };
        self.terminal.selection = Some(Selection::new(ty, point, Side::Left));
    }

    pub fn update_selection(&mut self, point: TerminalPoint) {
        let point = self.viewport_point(point);
        if let Some(selection) = self.terminal.selection.as_mut() {
            selection.update(point, Side::Right);
        }
    }

    pub fn clear_selection(&mut self) {
        self.terminal.selection = None;
        self.last_search = None;
    }

    pub fn selected_text(&self) -> Option<String> {
        self.terminal.selection_to_string()
    }

    pub fn scroll(&mut self, lines: i32) {
        self.terminal.scroll_display(Scroll::Delta(lines));
    }

    /// Returns the viewport to the live bottom. `Grid::scroll_up` anchors to
    /// old content whenever `display_offset` is nonzero, so streaming output
    /// otherwise leaves the typed line below the fold until this is called.
    pub fn scroll_bottom(&mut self) {
        self.terminal.scroll_display(Scroll::Bottom);
    }

    pub fn search_literal(&mut self, query: &str, forward: bool) -> bool {
        if query.is_empty() {
            return false;
        }
        let Ok(mut regex) = RegexSearch::new(&escape_regex_literal(query)) else {
            return false;
        };
        let display_offset = self.terminal.grid().display_offset();
        let origin = match self
            .last_search
            .as_ref()
            .filter(|(previous, _, _)| previous == query)
        {
            Some((_, _, end)) if forward => end.add(&self.terminal, Boundary::None, 1),
            Some((_, start, _)) => start.sub(&self.terminal, Boundary::None, 1),
            None if forward => viewport_to_point(display_offset, Point::new(0, Column(0))),
            None => viewport_to_point(
                display_offset,
                Point::new(
                    self.terminal.screen_lines().saturating_sub(1),
                    self.terminal.last_column(),
                ),
            ),
        };
        let direction = if forward {
            Direction::Right
        } else {
            Direction::Left
        };
        let side = if forward { Side::Right } else { Side::Left };
        let Some(found) = self
            .terminal
            .search_next(&mut regex, origin, direction, side, None)
        else {
            return false;
        };
        let (start, end) = (*found.start(), *found.end());
        self.last_search = Some((query.to_owned(), start, end));
        self.terminal.scroll_to_point(start);
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        self.terminal.selection = Some(selection);
        true
    }

    pub fn mouse_report(
        &self,
        point: TerminalPoint,
        button: TerminalMouseButton,
        action: TerminalMouseAction,
        modifiers: TerminalModifiers,
    ) -> Option<Vec<u8>> {
        if !self.mouse_reporting() {
            return None;
        }
        let base = match button {
            TerminalMouseButton::Left => 0,
            TerminalMouseButton::Middle => 1,
            TerminalMouseButton::Right => 2,
            TerminalMouseButton::WheelUp => 64,
            TerminalMouseButton::WheelDown => 65,
        };
        let modifier_bits = u8::from(modifiers.shift) * 4
            + u8::from(modifiers.alt) * 8
            + u8::from(modifiers.control) * 16;
        let motion = matches!(action, TerminalMouseAction::Move);
        if motion && !self.mouse_motion() {
            return None;
        }
        let code = base + modifier_bits + u8::from(motion) * 32;
        let column = point.column.saturating_add(1);
        let row = point.row.saturating_add(1);
        if self.sgr_mouse() {
            let suffix = if matches!(action, TerminalMouseAction::Release) {
                'm'
            } else {
                'M'
            };
            return Some(format!("\x1b[<{code};{column};{row}{suffix}").into_bytes());
        }

        let legacy_code = if matches!(action, TerminalMouseAction::Release) {
            3 + modifier_bits
        } else {
            code
        };
        let x = u8::try_from(column).ok()?.checked_add(32)?;
        let y = u8::try_from(row).ok()?.checked_add(32)?;
        Some(vec![0x1b, b'[', b'M', legacy_code.checked_add(32)?, x, y])
    }

    fn viewport_point(&self, point: TerminalPoint) -> Point {
        let row = usize::from(point.row).min(self.terminal.screen_lines().saturating_sub(1));
        let column =
            Column(usize::from(point.column).min(self.terminal.columns().saturating_sub(1)));
        viewport_to_point(
            self.terminal.grid().display_offset(),
            Point::new(row, column),
        )
    }
}

fn escape_regex_literal(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for character in query.chars() {
        if matches!(
            character,
            '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn terminal_color(color: Color) -> TerminalColor {
    match color {
        Color::Spec(color) => TerminalColor::Rgb {
            red: color.r,
            green: color.g,
            blue: color.b,
        },
        Color::Indexed(index) => TerminalColor::Indexed { index },
        Color::Named(color) => match color {
            NamedColor::Foreground
            | NamedColor::BrightForeground
            | NamedColor::DimForeground
            | NamedColor::Cursor => TerminalColor::DefaultForeground,
            NamedColor::Background => TerminalColor::DefaultBackground,
            NamedColor::Black | NamedColor::DimBlack => TerminalColor::Ansi { index: 0 },
            NamedColor::Red | NamedColor::DimRed => TerminalColor::Ansi { index: 1 },
            NamedColor::Green | NamedColor::DimGreen => TerminalColor::Ansi { index: 2 },
            NamedColor::Yellow | NamedColor::DimYellow => TerminalColor::Ansi { index: 3 },
            NamedColor::Blue | NamedColor::DimBlue => TerminalColor::Ansi { index: 4 },
            NamedColor::Magenta | NamedColor::DimMagenta => TerminalColor::Ansi { index: 5 },
            NamedColor::Cyan | NamedColor::DimCyan => TerminalColor::Ansi { index: 6 },
            NamedColor::White | NamedColor::DimWhite => TerminalColor::Ansi { index: 7 },
            NamedColor::BrightBlack => TerminalColor::Ansi { index: 8 },
            NamedColor::BrightRed => TerminalColor::Ansi { index: 9 },
            NamedColor::BrightGreen => TerminalColor::Ansi { index: 10 },
            NamedColor::BrightYellow => TerminalColor::Ansi { index: 11 },
            NamedColor::BrightBlue => TerminalColor::Ansi { index: 12 },
            NamedColor::BrightMagenta => TerminalColor::Ansi { index: 13 },
            NamedColor::BrightCyan => TerminalColor::Ansi { index: 14 },
            NamedColor::BrightWhite => TerminalColor::Ansi { index: 15 },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use alacritty_terminal::index::{Column, Line};

    use super::*;

    #[test]
    fn delegates_escape_sequence_parsing_to_alacritty() {
        let mut model = TerminalModel::new(20, 4);
        model.process_output(b"plain\x1b[31m red");

        assert_eq!(model.terminal().grid()[Line(0)][Column(0)].c, 'p');
        assert_eq!(model.terminal().grid()[Line(0)][Column(6)].c, 'r');
        let styled = model.styled_lines();
        assert_eq!(styled[0].runs[1].text, " red");
        assert_eq!(
            styled[0].runs[1].foreground,
            TerminalColor::Ansi { index: 1 }
        );
    }

    #[test]
    fn adversarial_per_cell_colors_are_coarsened_to_a_bounded_run_count() {
        let columns = 200;
        let rows = 30;
        let mut model = TerminalModel::new(columns, rows);
        let mut output = String::new();
        for row in 0..rows {
            for column in 0..columns {
                write!(
                    output,
                    "\u{1b}[38;2;{};{};{}mX",
                    row % 256,
                    column % 256,
                    (row + column) % 256
                )
                .unwrap();
            }
            if row + 1 < rows {
                output.push_str("\r\n");
            }
        }
        model.process_output(output.as_bytes());

        let lines = model.styled_lines();
        let run_count = lines.iter().map(|line| line.runs.len()).sum::<usize>();
        assert!(run_count <= MAX_TOTAL_STYLE_RUNS + rows);
        assert!(
            lines
                .iter()
                .all(|line| line.runs.len() <= MAX_STYLE_RUNS_PER_LINE)
        );
        assert!(hh_protocol::encode_frame(&lines).is_ok());
        assert!(lines.iter().all(|line| !line.runs.is_empty()));
    }

    #[test]
    fn resizes_the_underlying_terminal_grid() {
        let mut model = TerminalModel::new(12, 3);
        model.resize(80, 24);
        assert_eq!(model.dimensions(), (80, 24));
    }

    #[test]
    fn captures_only_bounded_osc_title_metadata_without_reading_grid_text() {
        let mut model = TerminalModel::new(20, 3);
        model.process_output(b"conversation text\r\n\x1b]0;Claude Code\x07");

        assert_eq!(model.terminal_title().as_deref(), Some("Claude Code"));

        let oversized = format!("\x1b]0;{}\x07", "x".repeat(MAX_SAFE_TITLE_CHARS + 1));
        model.process_output(oversized.as_bytes());
        assert_eq!(model.terminal_title(), None);
    }
    #[test]
    fn counts_terminal_bells() {
        let mut model = TerminalModel::new(20, 3);
        model.process_output(b"\x07");

        assert_eq!(model.bell_count(), 1);
    }

    #[test]
    fn captures_split_osc_9_notifications() {
        let mut model = TerminalModel::new(20, 3);
        model.process_output(b"\x1b]9;approval ");
        assert!(model.take_notification_messages().is_empty());

        model.process_output(b"needed\x07");
        assert_eq!(
            model.take_notification_messages(),
            vec!["approval needed".to_owned()]
        );
    }

    #[test]
    fn captures_osc_777_title_and_body() {
        let mut model = TerminalModel::new(20, 3);
        model.process_output(b"\x1b]777;notify;Claude;needs approval\x1b\\");

        assert_eq!(
            model.take_notification_messages(),
            vec!["Claude: needs approval".to_owned()]
        );
    }

    #[test]
    fn abandons_oversized_osc_notifications_and_recovers() {
        let mut model = TerminalModel::new(20, 3);
        let oversized = format!("\x1b]9;{}", "x".repeat(MAX_OSC_SEQUENCE_BYTES + 1));
        model.process_output(oversized.as_bytes());
        model.process_output(b"\x07");
        assert!(model.take_notification_messages().is_empty());

        model.process_output(b"\x1b]9;recovered\x07");
        assert_eq!(
            model.take_notification_messages(),
            vec!["recovered".to_owned()]
        );
    }

    #[test]
    fn exposes_cursor_and_sgr_attributes() {
        let mut model = TerminalModel::new(20, 3);
        model.process_output(b"\x1b[1;3;4;38;2;12;34;56mhi");

        let run = &model.styled_lines()[0].runs[0];
        assert!(run.attributes.contains(TerminalAttributes::BOLD));
        assert!(run.attributes.contains(TerminalAttributes::ITALIC));
        assert!(run.attributes.contains(TerminalAttributes::UNDERLINE));
        assert_eq!(
            run.foreground,
            TerminalColor::Rgb {
                red: 12,
                green: 34,
                blue: 56
            }
        );
        assert_eq!(model.cursor(), Some(TerminalCursor { row: 0, column: 2 }));
    }

    #[test]
    fn styled_runs_preserve_terminal_cell_spans() {
        let mut model = TerminalModel::new(12, 3);
        model.process_output("A界B".as_bytes());

        let run = &model.styled_lines()[0].runs[0];
        assert_eq!(run.text, "A界B");
        assert_eq!(run.columns, 4);
    }

    #[test]
    fn selection_uses_alacritty_grid_coordinates_and_text_extraction() {
        let mut model = TerminalModel::new(12, 3);
        model.process_output(b"hello world");
        model.begin_selection(
            TerminalPoint { row: 0, column: 1 },
            TerminalSelectionKind::Simple,
        );
        model.update_selection(TerminalPoint { row: 0, column: 3 });

        assert_eq!(model.selected_text().as_deref(), Some("ell"));
        assert_eq!(
            model.selection(),
            Some(TerminalSelection {
                start: TerminalPoint { row: 0, column: 1 },
                end: TerminalPoint { row: 0, column: 3 },
                is_block: false,
            })
        );
    }

    #[test]
    fn scrollback_is_bounded_and_changes_the_authoritative_display() {
        let mut model = TerminalModel::new(20, 3);
        for line in 0..(SCROLLBACK_HISTORY_LIMIT + 100) {
            model.process_output(format!("line {line}\r\n").as_bytes());
        }
        assert_eq!(model.history_size(), SCROLLBACK_HISTORY_LIMIT);

        let bottom = model.styled_lines();
        model.scroll(1);
        assert_eq!(model.display_offset(), 1);
        assert_ne!(model.styled_lines(), bottom);
        assert_eq!(model.cursor(), None);
    }

    #[test]
    fn scroll_bottom_returns_the_viewport_to_the_live_display() {
        let mut model = TerminalModel::new(20, 3);
        for line in 0..40 {
            model.process_output(format!("line {line}\r\n").as_bytes());
        }
        assert!(model.history_size() > 0);

        model.scroll(10);
        assert_eq!(model.display_offset(), 10);
        assert_eq!(model.cursor(), None);

        model.scroll_bottom();
        assert_eq!(model.display_offset(), 0);
        assert!(model.cursor().is_some());
    }

    #[test]
    fn literal_search_does_not_treat_regex_metacharacters_as_patterns() {
        let mut model = TerminalModel::new(20, 4);
        model.process_output(b"alpha\r\nneedle.\r\nomega");

        assert!(model.search_literal(".", true));
        assert_eq!(model.selected_text().as_deref(), Some("."));
    }

    #[test]
    fn repeated_literal_search_advances_to_the_next_match() {
        let mut model = TerminalModel::new(20, 4);
        model.process_output(b"needle\r\nneedle");

        assert!(model.search_literal("needle", true));
        assert_eq!(model.selection().unwrap().start.row, 0);
        assert!(model.search_literal("needle", true));
        assert_eq!(model.selection().unwrap().start.row, 1);
    }

    #[test]
    fn terminal_modes_drive_bracketed_paste_and_sgr_mouse_reports() {
        let mut model = TerminalModel::new(20, 4);
        model.process_output(b"\x1b[?2004h\x1b[?1000h\x1b[?1006h");

        assert!(model.bracketed_paste());
        assert!(model.mouse_reporting());
        assert!(model.sgr_mouse());
        assert_eq!(
            model.mouse_report(
                TerminalPoint { row: 1, column: 2 },
                TerminalMouseButton::Left,
                TerminalMouseAction::Press,
                TerminalModifiers::default(),
            ),
            Some(b"\x1b[<0;3;2M".to_vec())
        );
    }
}
