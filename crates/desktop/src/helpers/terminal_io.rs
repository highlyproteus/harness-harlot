use crate::typography;

use crate::{
    MAX_PASTE_BYTES, PANE_HEADER_HEIGHT, TERMINAL_FOCUS_BORDER_WIDTH, TERMINAL_HORIZONTAL_PADDING,
    TERMINAL_VERTICAL_PADDING,
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

pub(crate) fn terminal_input_bytes(
    key: &str,
    key_char: Option<&str>,
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
    let mut bytes = match key {
        "enter" => vec![b'\r'],
        "backspace" => vec![0x7f],
        "tab" => vec![b'\t'],
        "escape" => vec![0x1b],
        "left" => b"\x1b[D".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "home" => b"\x1b[H".to_vec(),
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
    let content_height = (pane_height - pane_chrome_height - TERMINAL_VERTICAL_PADDING).max(1.0);
    let columns = metrics.columns_for_width(content_width);
    let rows = metrics.rows_for_height(content_height);
    let max_rows_for_columns = (hh_protocol::MAX_TERMINAL_CELLS / u32::from(columns)) as u16;
    (columns, rows.min(max_rows_for_columns.max(1)))
}

#[cfg(test)]
mod tests {
    use super::{
        Bounds, MAX_PASTE_BYTES, TerminalAttributes, TerminalColor, TerminalPoint, TerminalRun,
        TerminalSelection, prepare_paste, selection_span, terminal_input_bytes, terminal_point_at,
        terminal_run_display_text,
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
    fn terminal_input_encodes_unmatched_keys_once_with_control_and_alt_semantics() {
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, false, false),
            Some(vec![b'x'])
        );
        assert_eq!(
            terminal_input_bytes("c", Some("c"), true, false, false),
            Some(vec![0x03])
        );
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, true, false),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(
            terminal_input_bytes("up", None, false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, false, true),
            None
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
}
