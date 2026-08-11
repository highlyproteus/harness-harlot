use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{self, Color, NamedColor};
use rust_mux_protocol::{
    TerminalAttributes, TerminalColor, TerminalCursor, TerminalLine, TerminalRun,
};

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
                    scrolling_history: 2_000,
                    ..Config::default()
                },
                &size,
                VoidListener,
            ),
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
        (0..self.terminal.screen_lines())
            .map(|line| {
                let line = i32::try_from(line).unwrap_or(i32::MAX);
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
        if !self.terminal.mode().contains(TermMode::SHOW_CURSOR) {
            return None;
        }
        let point = self.terminal.grid().cursor.point;
        Some(TerminalCursor {
            row: u16::try_from(point.line.0).ok()?,
            column: u16::try_from(point.column.0).ok()?,
        })
    }
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
}
