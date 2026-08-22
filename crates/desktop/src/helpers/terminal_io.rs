use crate::typography;

use crate::{
    MAX_PASTE_BYTES, PANE_HEADER_HEIGHT, TERMINAL_BOTTOM_GUARD, TERMINAL_FOCUS_BORDER_WIDTH,
    TERMINAL_HORIZONTAL_PADDING, TERMINAL_VERTICAL_PADDING,
};
use gpui::{Bounds, MouseButton, Pixels, Point};
use hh_protocol::{
    TerminalAttributes, TerminalColor, TerminalLine, TerminalModifiers, TerminalMouseButton,
    TerminalPoint, TerminalRun, TerminalSelection,
};
use unicode_width::UnicodeWidthChar;

pub(crate) fn terminal_point_at(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    row: u16,
    columns: u16,
    cell_width: f32,
) -> TerminalPoint {
    let relative_x = f32::from(position.x - bounds.origin.x).max(0.0);
    let column = if columns == 0 || cell_width <= f32::EPSILON {
        0
    } else {
        (relative_x / cell_width).floor() as u16
    };
    TerminalPoint {
        row,
        column: column.min(columns.saturating_sub(1)),
    }
}

pub(crate) fn terminal_mouse_button(button: MouseButton) -> Option<TerminalMouseButton> {
    match button {
        MouseButton::Left => Some(TerminalMouseButton::Left),
        MouseButton::Middle => Some(TerminalMouseButton::Middle),
        MouseButton::Right => Some(TerminalMouseButton::Right),
        MouseButton::Navigate(_) => None,
    }
}

pub(crate) fn terminal_modifiers(modifiers: gpui::Modifiers) -> TerminalModifiers {
    TerminalModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
    }
}

pub(crate) fn selection_span(
    selection: TerminalSelection,
    row: usize,
    columns: u16,
) -> Option<(u16, u16)> {
    let row = u16::try_from(row).ok()?;
    if row < selection.start.row || row > selection.end.row || columns == 0 {
        return None;
    }
    let start = if selection.is_block || row == selection.start.row {
        selection.start.column.min(columns - 1)
    } else {
        0
    };
    let end = if selection.is_block || row == selection.end.row {
        selection.end.column.min(columns - 1)
    } else {
        columns - 1
    };
    (end >= start).then_some((start, end - start + 1))
}

pub(crate) fn prepare_paste(text: &str, bracketed: bool) -> Result<Vec<u8>, &'static str> {
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r");
    let sanitized = normalized.replace(['\0', '\u{1b}'], "");
    let wrapper_size = if bracketed { 12 } else { 0 };
    if sanitized.len().saturating_add(wrapper_size) > MAX_PASTE_BYTES {
        return Err("paste rejected: clipboard text exceeds 64 KiB");
    }
    if bracketed {
        let mut bytes = Vec::with_capacity(sanitized.len() + wrapper_size);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(sanitized.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        Ok(bytes)
    } else {
        Ok(sanitized.into_bytes())
    }
}

pub(crate) fn plain_history_line(text: &str) -> TerminalLine {
    TerminalLine {
        runs: if text.is_empty() {
            Vec::new()
        } else {
            vec![TerminalRun {
                text: text.to_owned(),
                columns: text.chars().fold(0_u16, |columns, character| {
                    columns.saturating_add(
                        u16::try_from(if character == '\t' {
                            1
                        } else {
                            character.width().unwrap_or(0)
                        })
                        .unwrap_or(u16::MAX),
                    )
                }),
                foreground: TerminalColor::DefaultForeground,
                background: TerminalColor::DefaultBackground,
                attributes: TerminalAttributes::default(),
            }]
        },
    }
}

pub(crate) fn terminal_run_display_text(run: &TerminalRun, _start_column: u16) -> String {
    // The terminal model already represents every occupied grid cell,
    // including the cells skipped by a tab. Render its tab cell as one
    // blank cell instead of asking GPUI to apply proportional tab stops.
    run.text.replace('\t', " ")
}

/// Extracts the URL under one grid column of a terminal row, if any.
///
/// Runs are concatenated first so links recolored mid-token still match
/// whole. The clicked column maps to a character via display width, the
/// surrounding token expands over non-whitespace characters, and surrounding
/// quotes/brackets plus trailing punctuation and unbalanced parentheses are
/// trimmed before the `http(s)://` (or `www.`) prefix check.
pub(crate) fn url_at_column(line: &TerminalLine, column: u16) -> Option<String> {
    let text = line
        .runs
        .iter()
        .flat_map(|run| run.text.chars())
        .collect::<String>();

    // Locate the character whose display cells contain the clicked column.
    let mut cell = 0_u16;
    let mut clicked_byte = text.len();
    for (byte, character) in text.char_indices() {
        let width = u16::try_from(character.width().unwrap_or(0)).unwrap_or(0);
        if width == 0 || cell >= column || cell + width > column {
            clicked_byte = byte;
            break;
        }
        cell += width;
    }

    let token_character =
        |character: char| !character.is_whitespace() && character.width().is_some_and(|w| w > 0);
    if !text[clicked_byte..]
        .chars()
        .next()
        .is_some_and(token_character)
    {
        return None;
    }

    let mut start = clicked_byte;
    while start > 0
        && let Some(previous) = text[..start].chars().next_back()
        && token_character(previous)
    {
        start -= previous.len_utf8();
    }
    let mut end = clicked_byte;
    while let Some(character) = text[end..].chars().next()
        && token_character(character)
    {
        end += character.len_utf8();
    }

    let token = text[start..end]
        .trim_matches(|c: char| matches!(c, '(' | ')' | '<' | '>' | '"' | '\'' | '`'))
        .trim_end_matches(['.', ',', ';', ':', '!', '?']);
    let mut token = token
        .trim_start_matches(|c: char| !c.is_ascii())
        .trim_end_matches(|c: char| !c.is_ascii())
        .to_owned();
    while token.ends_with(')') && token.matches('(').count() < token.matches(')').count() {
        token.pop();
    }
    if token.is_empty() || token.len() > 2_048 {
        return None;
    }

    let lower = token.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        let host = token
            .split_once("://")
            .map_or(token.as_str(), |(_, rest)| rest);
        host.chars()
            .any(|c| c.is_ascii_alphanumeric())
            .then_some(token)
    } else if lower.starts_with("www.") && token[4..].contains(['.', '/']) {
        Some(format!("https://{token}"))
    } else {
        None
    }
}

#[allow(clippy::fn_params_excessive_bools)]
pub(crate) fn terminal_input_bytes(
    key: &str,
    key_char: Option<&str>,
    shift: bool,
    control: bool,
    alt: bool,
    platform: bool,
) -> Option<Vec<u8>> {
    // Command/Super is an application modifier, not a PTY modifier. Unmatched
    // platform shortcuts remain available to the OS instead of becoming text.
    if platform {
        return None;
    }
    if control && key.len() == 1 {
        return key
            .as_bytes()
            .first()
            .map(|byte| vec![byte.to_ascii_lowercase() & 0x1f]);
    }
    // Shift uses the standard xterm legacy encodings: CSI Z for backtab and
    // the modifier parameter (2 = shift) on navigation keys. Shifted
    // printables already arrive shifted in `key_char` and ctrl combinations
    // are handled above, so shift never affects those arms.
    let mut bytes = match key {
        "enter" => vec![b'\r'],
        "backspace" => vec![0x7f],
        "tab" if shift => b"\x1b[Z".to_vec(),
        "tab" => vec![b'\t'],
        "escape" => vec![0x1b],
        "left" if shift => b"\x1b[1;2D".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "right" if shift => b"\x1b[1;2C".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "up" if shift => b"\x1b[1;2A".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" if shift => b"\x1b[1;2B".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "home" if shift => b"\x1b[1;2H".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" if shift => b"\x1b[1;2F".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        _ => key_char?.as_bytes().to_vec(),
    };
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

pub(crate) fn terminal_grid_for_pane(
    pane_width: f32,
    pane_height: f32,
    metrics: typography::TerminalCellMetrics,
    show_pane_header: bool,
) -> (u16, u16) {
    let content_width =
        (pane_width - TERMINAL_HORIZONTAL_PADDING - TERMINAL_FOCUS_BORDER_WIDTH).max(1.0);
    let pane_chrome_height = if show_pane_header {
        PANE_HEADER_HEIGHT
    } else {
        0.0
    };
    let content_height =
        (pane_height - pane_chrome_height - TERMINAL_VERTICAL_PADDING - TERMINAL_BOTTOM_GUARD)
            .max(1.0);
    let columns = metrics.columns_for_width(content_width);
    let rows = metrics.rows_for_height(content_height);
    let max_rows_for_columns = (hh_protocol::MAX_TERMINAL_CELLS / u32::from(columns)) as u16;
    (columns, rows.min(max_rows_for_columns.max(1)))
}

#[cfg(test)]
mod tests {
    use super::{
        Bounds, MAX_PASTE_BYTES, PANE_HEADER_HEIGHT, TERMINAL_BOTTOM_GUARD,
        TERMINAL_VERTICAL_PADDING, TerminalAttributes, TerminalColor, TerminalLine, TerminalPoint,
        TerminalRun, TerminalSelection, prepare_paste, selection_span, terminal_grid_for_pane,
        terminal_input_bytes, terminal_point_at, terminal_run_display_text, typography,
        url_at_column,
    };
    use gpui::point;
    use gpui::px;
    use gpui::size;

    #[test]
    fn multi_column_terminal_rows_keep_spaces_and_wide_cells_on_one_grid() {
        let modeled_cells = TerminalRun {
            text: "A\t  B".to_owned(),
            columns: 5,
            foreground: TerminalColor::DefaultForeground,
            background: TerminalColor::DefaultBackground,
            attributes: TerminalAttributes::default(),
        };

        assert_eq!(terminal_run_display_text(&modeled_cells, 0), "A   B");
        assert_eq!(modeled_cells.columns, 5);
    }

    #[test]
    fn one_row_hit_surface_maps_pointer_positions_to_terminal_cells() {
        let bounds = Bounds {
            origin: point(px(100.0), px(40.0)),
            size: size(px(80.0), px(18.0)),
        };
        assert_eq!(
            terminal_point_at(point(px(100.0), px(49.0)), bounds, 7, 10, 8.0),
            TerminalPoint { row: 7, column: 0 }
        );
        assert_eq!(
            terminal_point_at(point(px(139.9), px(49.0)), bounds, 7, 10, 8.0),
            TerminalPoint { row: 7, column: 4 }
        );
        assert_eq!(
            terminal_point_at(point(px(190.0), px(49.0)), bounds, 7, 10, 8.0),
            TerminalPoint { row: 7, column: 9 }
        );
    }

    #[test]
    fn terminal_grid_reserves_the_bottom_guard_before_adding_a_row() {
        let metrics = typography::TerminalCellMetrics {
            font_size: 13.5,
            cell_width: 8.0,
            ascent: 10.0,
            descent: 3.0,
            baseline: 13.0,
            line_height: 19.0,
        };
        let exact_two_rows_without_guard =
            PANE_HEADER_HEIGHT + TERMINAL_VERTICAL_PADDING + metrics.line_height * 2.0;

        assert_eq!(
            terminal_grid_for_pane(320.0, exact_two_rows_without_guard, metrics, true).1,
            1
        );
        assert_eq!(
            terminal_grid_for_pane(
                320.0,
                exact_two_rows_without_guard + TERMINAL_BOTTOM_GUARD,
                metrics,
                true,
            )
            .1,
            2
        );
    }

    #[test]
    fn terminal_input_encodes_unmatched_keys_once_with_control_and_alt_semantics() {
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, false, false, false),
            Some(vec![b'x'])
        );
        assert_eq!(
            terminal_input_bytes("c", Some("c"), false, true, false, false),
            Some(vec![0x03])
        );
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, false, true, false),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(
            terminal_input_bytes("up", None, false, false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, false, false, true),
            None
        );
    }

    #[test]
    fn shifted_keys_use_xterm_backtab_and_modifier_sequences() {
        assert_eq!(
            terminal_input_bytes("tab", None, true, false, false, false),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            terminal_input_bytes("tab", None, false, false, false, false),
            Some(vec![b'\t'])
        );
        assert_eq!(
            terminal_input_bytes("left", None, true, false, false, false),
            Some(b"\x1b[1;2D".to_vec())
        );
        assert_eq!(
            terminal_input_bytes("a", Some("A"), true, false, false, false),
            Some(b"A".to_vec())
        );
        assert_eq!(
            terminal_input_bytes("tab", None, true, false, true, false),
            Some(b"\x1b\x1b[Z".to_vec())
        );
    }

    #[test]
    fn bracketed_paste_normalizes_newlines_and_cannot_inject_an_early_end_marker() {
        let bytes = prepare_paste("one\n\x1b[201~two\r\n", true).unwrap();
        assert_eq!(bytes, b"\x1b[200~one\r[201~two\r\x1b[201~");
        assert_eq!(
            bytes
                .windows(b"\x1b[201~".len())
                .filter(|window| *window == b"\x1b[201~")
                .count(),
            1
        );
    }

    #[test]
    fn oversized_paste_is_rejected_before_it_reaches_the_protocol() {
        let text = "x".repeat(MAX_PASTE_BYTES + 1);
        assert_eq!(
            prepare_paste(&text, false),
            Err("paste rejected: clipboard text exceeds 64 KiB")
        );
    }

    #[test]
    fn selection_highlight_spans_exact_grid_cells_across_rows() {
        let selection = TerminalSelection {
            start: TerminalPoint { row: 1, column: 3 },
            end: TerminalPoint { row: 2, column: 4 },
            is_block: false,
        };
        assert_eq!(selection_span(selection, 0, 10), None);
        assert_eq!(selection_span(selection, 1, 10), Some((3, 7)));
        assert_eq!(selection_span(selection, 2, 10), Some((0, 5)));
    }

    fn url_line(text: &str) -> TerminalLine {
        TerminalLine {
            runs: vec![TerminalRun {
                text: text.to_owned(),
                columns: text
                    .chars()
                    .map(|c| {
                        unicode_width::UnicodeWidthChar::width(c)
                            .unwrap_or(0)
                            .max(1) as u16
                    })
                    .sum(),
                foreground: TerminalColor::DefaultForeground,
                background: TerminalColor::DefaultBackground,
                attributes: TerminalAttributes::default(),
            }],
        }
    }

    #[test]
    fn url_clicks_extract_the_link_under_the_pointed_column() {
        let line = url_line("see https://example.com/docs now");
        assert_eq!(
            url_at_column(&line, 7),
            Some("https://example.com/docs".to_owned())
        );
        assert_eq!(
            url_at_column(&line, 20),
            Some("https://example.com/docs".to_owned())
        );
        assert_eq!(url_at_column(&line, 3), None);
        assert_eq!(url_at_column(&line, 31), None);
    }

    #[test]
    fn url_extraction_trims_wrapping_punctuation_and_keeps_balanced_parens() {
        let line = url_line("(https://en.wikipedia.org/wiki/Kernel_(computing)).");
        assert_eq!(
            url_at_column(&line, 5),
            Some("https://en.wikipedia.org/wiki/Kernel_(computing)".to_owned())
        );

        let quoted = url_line("\"https://example.com,\"");
        assert_eq!(
            url_at_column(&quoted, 5),
            Some("https://example.com".to_owned())
        );
    }

    #[test]
    fn url_extraction_accepts_bare_www_and_stops_at_wide_characters() {
        let line = url_line("www.github.com/rust-lang/rust界");
        assert_eq!(
            url_at_column(&line, 4),
            Some("https://www.github.com/rust-lang/rust".to_owned())
        );

        let plain = url_line("go to example.com for docs");
        assert_eq!(url_at_column(&plain, 8), None);
    }
}
