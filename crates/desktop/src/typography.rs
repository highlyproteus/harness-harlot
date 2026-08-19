use gpui::{Font, FontFallbacks, FontFeatures, TextSystem, font, px};

const TERMINAL_FONT_SIZE: f32 = 13.5;
const TERMINAL_ZOOM_STEP: f32 = 1.0;
pub(super) const TERMINAL_ZOOM_MIN_LEVEL: i8 = -5;
pub(super) const TERMINAL_ZOOM_MAX_LEVEL: i8 = 18;
const LINE_HEIGHT_RATIO: f32 = 1.4;

#[cfg(target_os = "macos")]
const MONOSPACE_CANDIDATES: &[&str] = &[
    "SF Mono",
    "Menlo",
    "JetBrains Mono",
    "IBM Plex Mono",
    "Cascadia Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
];

#[cfg(not(target_os = "macos"))]
const MONOSPACE_CANDIDATES: &[&str] = &[
    "JetBrains Mono",
    "IBM Plex Mono",
    "Cascadia Mono",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Ubuntu Mono",
];

const SYMBOL_FALLBACK_CANDIDATES: &[&str] = &[
    "Apple Color Emoji",
    ".Apple Symbols Fallback",
    "Noto Color Emoji",
    "Noto Sans Symbols 2",
    "DejaVu Sans",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalCellMetrics {
    pub font_size: f32,
    pub cell_width: f32,
    pub ascent: f32,
    pub descent: f32,
    pub baseline: f32,
    pub line_height: f32,
}

impl TerminalCellMetrics {
    fn from_measurements(font_size: f32, cell_width: f32, ascent: f32, descent: f32) -> Self {
        let font_size = font_size.max(1.0);
        let cell_width = cell_width.max(1.0);
        let ascent = ascent.max(0.0);
        let descent = descent.max(0.0);
        let natural_height = ascent + descent;
        let line_height = round_to_half((font_size * LINE_HEIGHT_RATIO).max(natural_height));
        let baseline = (line_height - natural_height).max(0.0) * 0.5 + ascent;
        Self {
            font_size,
            cell_width,
            ascent,
            descent,
            baseline,
            line_height,
        }
    }

    fn with_font_size(self, font_size: f32) -> Self {
        let scale = font_size / self.font_size;
        Self::from_measurements(
            font_size,
            self.cell_width * scale,
            self.ascent * scale,
            self.descent * scale,
        )
    }

    fn for_zoom_level(self, level: i8) -> Self {
        let level = level.clamp(TERMINAL_ZOOM_MIN_LEVEL, TERMINAL_ZOOM_MAX_LEVEL);
        self.with_font_size(self.font_size + f32::from(level) * TERMINAL_ZOOM_STEP)
    }

    pub fn span(self, start_column: u16, columns: u16) -> TerminalCellSpan {
        TerminalCellSpan {
            x: f32::from(start_column) * self.cell_width,
            width: f32::from(columns) * self.cell_width,
            height: self.line_height,
        }
    }

    pub fn columns_for_width(self, width: f32) -> u16 {
        (width.max(0.0) / self.cell_width).floor().clamp(
            f32::from(hh_protocol::MIN_TERMINAL_COLUMNS),
            f32::from(hh_protocol::MAX_TERMINAL_COLUMNS),
        ) as u16
    }

    pub fn rows_for_height(self, height: f32) -> u16 {
        (height.max(0.0) / self.line_height).floor().clamp(
            f32::from(hh_protocol::MIN_TERMINAL_ROWS),
            f32::from(hh_protocol::MAX_TERMINAL_ROWS),
        ) as u16
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalCellSpan {
    pub x: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug)]
pub struct TerminalFontProfile {
    pub family: String,
    pub metrics: TerminalCellMetrics,
    base_font: Font,
}

impl TerminalFontProfile {
    pub fn resolve(text_system: &TextSystem) -> Self {
        let available = text_system.all_font_names();
        let family = select_font_family(&available, MONOSPACE_CANDIDATES)
            .unwrap_or_else(|| MONOSPACE_CANDIDATES[0].to_owned());
        let mut fallbacks = resolve_available_fonts(&available, MONOSPACE_CANDIDATES);
        fallbacks.retain(|fallback| !fallback.eq_ignore_ascii_case(&family));
        fallbacks.extend(resolve_available_fonts(
            &available,
            SYMBOL_FALLBACK_CANDIDATES,
        ));
        fallbacks.dedup_by(|left, right| left.eq_ignore_ascii_case(right));

        let mut base_font = font(family.clone());
        base_font.features = FontFeatures::disable_ligatures();
        base_font.fallbacks = Some(FontFallbacks::from_fonts(fallbacks.clone()));
        let font_id = text_system.resolve_font(&base_font);
        let font_size = px(TERMINAL_FONT_SIZE);
        let cell_width = text_system
            .ch_advance(font_id, font_size)
            .or_else(|_| {
                text_system
                    .advance(font_id, font_size, 'M')
                    .map(|advance| advance.width)
            })
            .map_or(TERMINAL_FONT_SIZE * 0.6, f32::from);
        let metrics = TerminalCellMetrics::from_measurements(
            TERMINAL_FONT_SIZE,
            cell_width,
            f32::from(text_system.ascent(font_id, font_size)),
            f32::from(text_system.descent(font_id, font_size)),
        );

        Self {
            family,
            metrics,
            base_font,
        }
    }

    pub fn metrics_for_zoom_level(&self, level: i8) -> TerminalCellMetrics {
        self.metrics.for_zoom_level(level)
    }

    pub fn font(&self, bold: bool, italic: bool) -> Font {
        let mut font = self.base_font.clone();
        if bold {
            font = font.bold();
        }
        if italic {
            font = font.italic();
        }
        font
    }
}

pub(super) fn adjusted_terminal_zoom_level(current: i8, delta: i8) -> i8 {
    current
        .saturating_add(delta)
        .clamp(TERMINAL_ZOOM_MIN_LEVEL, TERMINAL_ZOOM_MAX_LEVEL)
}

fn select_font_family(available: &[String], candidates: &[&str]) -> Option<String> {
    candidates.iter().find_map(|candidate| {
        available
            .iter()
            .find(|available| available.eq_ignore_ascii_case(candidate))
            .cloned()
    })
}

fn resolve_available_fonts(available: &[String], candidates: &[&str]) -> Vec<String> {
    candidates
        .iter()
        .filter_map(|candidate| {
            available
                .iter()
                .find(|available| available.eq_ignore_ascii_case(candidate))
                .cloned()
        })
        .collect()
}

fn round_to_half(value: f32) -> f32 {
    (value * 2.0).ceil() * 0.5
}

#[cfg(test)]
mod tests {
    use super::{
        TERMINAL_ZOOM_MAX_LEVEL, TERMINAL_ZOOM_MIN_LEVEL, TerminalCellMetrics,
        adjusted_terminal_zoom_level, select_font_family,
    };

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.0001, "{actual} != {expected}");
    }

    #[test]
    fn font_selection_respects_order_and_case_without_inventing_a_family() {
        let available = vec!["menlo".to_owned(), "JetBrains Mono".to_owned()];
        let candidates = ["SF Mono", "Menlo", "JetBrains Mono"];
        assert_eq!(
            select_font_family(&available, &candidates),
            Some("menlo".to_owned())
        );
        assert_eq!(select_font_family(&available, &["Iosevka"]), None);
    }

    #[test]
    fn measured_metrics_share_one_baseline_and_cell_grid() {
        let metrics = TerminalCellMetrics::from_measurements(13.5, 8.125, 10.25, 3.0);
        assert_close(metrics.line_height, 19.0);
        assert_close(metrics.baseline, 13.125);

        let styled_run = metrics.span(3, 5);
        let background = metrics.span(3, 5);
        let cursor = metrics.span(7, 1);
        assert_eq!(styled_run, background);
        assert_close(styled_run.x, 24.375);
        assert_close(styled_run.width, 40.625);
        assert_close(cursor.x, 56.875);
        assert_close(cursor.width, metrics.cell_width);
        assert_close(cursor.height, metrics.line_height);
    }

    #[test]
    fn viewport_dimensions_use_the_measured_cell_geometry() {
        let metrics = TerminalCellMetrics::from_measurements(13.5, 8.0, 10.0, 3.0);
        assert_eq!(metrics.columns_for_width(800.0), 100);
        assert_eq!(metrics.rows_for_height(380.0), 20);
    }

    #[test]
    fn terminal_zoom_scales_every_cell_measurement_and_is_bounded() {
        let base = TerminalCellMetrics::from_measurements(13.5, 8.0, 10.0, 3.0);
        let zoomed = base.for_zoom_level(3);
        assert_close(zoomed.font_size, 16.5);
        assert!(zoomed.cell_width > base.cell_width);
        assert!(zoomed.line_height > base.line_height);
        assert!(zoomed.columns_for_width(800.0) < base.columns_for_width(800.0));
        assert!(zoomed.rows_for_height(380.0) < base.rows_for_height(380.0));

        assert_eq!(
            adjusted_terminal_zoom_level(TERMINAL_ZOOM_MAX_LEVEL, 1),
            TERMINAL_ZOOM_MAX_LEVEL
        );
        assert_eq!(
            adjusted_terminal_zoom_level(TERMINAL_ZOOM_MIN_LEVEL, -1),
            TERMINAL_ZOOM_MIN_LEVEL
        );
        assert_eq!(
            adjusted_terminal_zoom_level(TERMINAL_ZOOM_MAX_LEVEL - 1, 1),
            TERMINAL_ZOOM_MAX_LEVEL
        );
        assert_eq!(adjusted_terminal_zoom_level(1, -1), 0);
    }
}
