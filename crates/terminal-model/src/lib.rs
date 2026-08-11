use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{Config, Osc52, TermMode, point_to_viewport, viewport_to_point};
use alacritty_terminal::vte::ansi::{self, Color, NamedColor};
use rust_mux_protocol::{
    TerminalAttributes, TerminalColor, TerminalCursor, TerminalLine, TerminalModifiers,
    TerminalMouseAction, TerminalMouseButton, TerminalPoint, TerminalRun, TerminalSelection,
    TerminalSelectionKind,
};

pub const SCROLLBACK_HISTORY_LIMIT: usize = 2_000;

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
    terminal: Term<VoidListener>,
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
        Self {
            parser: ansi::Processor::new(),
            terminal: Term::new(
                Config {
                    scrolling_history: SCROLLBACK_HISTORY_LIMIT,
                    osc52: Osc52::Disabled,
                    ..Config::default()
                },
                &size,
                VoidListener,
            ),
            last_search: None,
        }
    }

    pub fn process_output(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.terminal, bytes);
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

    pub fn terminal(&self) -> &Term<VoidListener> {
        &self.terminal
    }

    /// Returns a plain-text view of the visible screen for toolkit spikes and
    /// accessibility fallbacks. Styling remains available through `terminal`.
    ///
    /// # Panics
    ///
    /// Panics only if the visible line count cannot fit in Alacritty's signed
    /// line index. Rust Mux caps PTY rows far below that bound.
    pub fn visible_lines(&self) -> Vec<String> {
        (0..self.terminal.screen_lines())
            .map(|line| {
                let line = i32::try_from(line).unwrap_or(i32::MAX);
                let mut text = (0..self.terminal.columns())
                    .map(|column| self.terminal.grid()[Line(line)][Column(column)].c)
                    .collect::<String>();
                while text.ends_with(' ') {
                    text.pop();
                }
                text
            })
            .collect()
    }

    /// Returns the visible terminal grid as compact styled runs.
    pub fn styled_lines(&self) -> Vec<TerminalLine> {
        let display_offset =
            i32::try_from(self.terminal.grid().display_offset()).unwrap_or(i32::MAX);
        (0..self.terminal.screen_lines())
            .map(|line| {
                let line = i32::try_from(line).unwrap_or(i32::MAX) - display_offset;
                let cells = (0..self.terminal.columns())
                    .map(|column| &self.terminal.grid()[Line(line)][Column(column)])
                    .collect::<Vec<_>>();
                let end = cells
                    .iter()
                    .rposition(|cell| {
                        cell.c != ' '
                            || cell.bg != Color::Named(NamedColor::Background)
                            || cell.flags.contains(Flags::INVERSE)
                            || cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    })
                    .map_or(0, |index| index + 1);
                let mut runs: Vec<TerminalRun> = Vec::new();
                for cell in cells.into_iter().take(end) {
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
                    let mut text = if cell.flags.contains(Flags::HIDDEN) {
                        " ".to_owned()
                    } else {
                        cell.c.to_string()
                    };
                    if !cell.flags.contains(Flags::HIDDEN)
                        && let Some(zerowidth) = cell.zerowidth()
                    {
                        text.extend(zerowidth);
                    }
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
                    let run = TerminalRun {
                        text,
                        columns: 1,
                        foreground,
                        background,
                        attributes: TerminalAttributes::new(attributes),
                    };
                    if let Some(previous) = runs.last_mut()
                        && same_style(previous, &run)
                    {
                        previous.text.push_str(&run.text);
                        previous.columns = previous.columns.saturating_add(run.columns);
                    } else {
                        runs.push(run);
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

fn same_style(left: &TerminalRun, right: &TerminalRun) -> bool {
    left.foreground == right.foreground
        && left.background == right.background
        && left.attributes == right.attributes
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
    fn exposes_trimmed_visible_text_without_reimplementing_parsing() {
        let mut model = TerminalModel::new(12, 3);
        model.process_output(b"one\r\ntwo");

        assert_eq!(model.visible_lines(), ["one", "two", ""]);
    }

    #[test]
    fn resizes_the_underlying_terminal_grid() {
        let mut model = TerminalModel::new(12, 3);
        model.resize(80, 24);
        assert_eq!(model.dimensions(), (80, 24));
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
