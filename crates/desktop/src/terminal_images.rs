//! Paints kitty-graphics images behind Unicode placeholder cells.
//!
//! The session service writes each transmitted PNG to an owner-only file and
//! lists it in `TerminalScreen::images`. Placeholder cells in the grid name
//! the image (foreground color) and their row/column within it (diacritics);
//! each horizontal stretch of such cells becomes one clipped image draw.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{Bounds, Pixels, RenderImage, Size, point, px, size};
use hh_protocol::{
    KITTY_PLACEHOLDER, PlaceholderCell, TerminalColor, TerminalImage, TerminalLine,
    placeholder_cells, placeholder_image_id,
};
use image::Frame;

use crate::typography::TerminalCellMetrics;

/// Decoded images kept once no screen references them any more.
const MAX_UNREFERENCED_IMAGES: usize = 8;

/// A horizontal stretch of placeholder cells showing one image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageSegment {
    /// Screen row and first grid column of the stretch.
    pub(crate) row: u16,
    pub(crate) start_column: u16,
    pub(crate) cells: u16,
    pub(crate) image_id: u32,
    /// Image row and column shown in the stretch's first cell.
    pub(crate) image_row: u16,
    pub(crate) image_column: u16,
}

/// Whether a run holds placeholder cells rather than drawable text.
pub(crate) fn is_placeholder_run(text: &str) -> bool {
    text.contains(KITTY_PLACEHOLDER)
}

/// Finds the image stretches on one screen row. Cells continue a stretch only
/// while both the grid column and the image column advance together.
pub(crate) fn placeholder_segments(row: u16, line: &TerminalLine) -> Vec<ImageSegment> {
    let mut segments: Vec<ImageSegment> = Vec::new();
    let mut column = 0_u16;
    for run in &line.runs {
        let start = column;
        column = column.saturating_add(run.columns);
        if !is_placeholder_run(&run.text) {
            continue;
        }
        let TerminalColor::Rgb { red, green, blue } = run.foreground else {
            continue;
        };
        let image_id = placeholder_image_id(red, green, blue);
        for (offset, cell) in placeholder_cells(&run.text).into_iter().enumerate() {
            let PlaceholderCell::Image {
                row: image_row,
                column: image_column,
            } = cell
            else {
                continue;
            };
            let grid_column = start.saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
            if let Some(last) = segments.last_mut()
                && last.image_id == image_id
                && last.image_row == image_row
                && last.start_column + last.cells == grid_column
                && last.image_column + last.cells == image_column
            {
                last.cells += 1;
                continue;
            }
            segments.push(ImageSegment {
                row,
                start_column: grid_column,
                cells: 1,
                image_id,
                image_row,
                image_column,
            });
        }
    }
    segments
}

/// Where to draw an image for one segment: the image fit (aspect preserved,
/// centered) into its placement's cell box, and the segment's own cells as
/// the clip. `grid_origin` is the top-left of cell (0, 0).
pub(crate) fn segment_draw_bounds(
    segment: ImageSegment,
    placement: &TerminalImage,
    image_size: Size<f32>,
    metrics: TerminalCellMetrics,
    grid_origin: gpui::Point<Pixels>,
) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
    if image_size.width <= 0.0 || image_size.height <= 0.0 {
        return None;
    }
    let span = metrics.span(segment.start_column, segment.cells);
    let row_top = f32::from(segment.row) * metrics.line_height;
    let clip = Bounds::new(
        point(grid_origin.x + px(span.x), grid_origin.y + px(row_top)),
        size(px(span.width), px(metrics.line_height)),
    );
    let box_width = f32::from(placement.columns) * metrics.cell_width;
    let box_height = f32::from(placement.rows) * metrics.line_height;
    let scale = (box_width / image_size.width).min(box_height / image_size.height);
    let width = image_size.width * scale;
    let height = image_size.height * scale;
    let box_x = span.x - f32::from(segment.image_column) * metrics.cell_width;
    let box_y = row_top - f32::from(segment.image_row) * metrics.line_height;
    let image = Bounds::new(
        point(
            grid_origin.x + px(box_x + (box_width - width) / 2.0),
            grid_origin.y + px(box_y + (box_height - height) / 2.0),
        ),
        size(px(width), px(height)),
    );
    Some((image, clip))
}

pub(crate) enum ImageLoad {
    Loading,
    Ready(Arc<RenderImage>),
    Failed,
}

/// Decoded images keyed by file path (which names pane, id, and generation).
#[derive(Default)]
pub(crate) struct TerminalImageCache {
    entries: HashMap<String, ImageLoad>,
}

impl TerminalImageCache {
    pub(crate) fn get(&self, path: &str) -> Option<&ImageLoad> {
        self.entries.get(path)
    }

    /// Marks `path` as loading; returns false when it is already known.
    pub(crate) fn begin_load(&mut self, path: &str) -> bool {
        if self.entries.contains_key(path) {
            return false;
        }
        self.entries.insert(path.to_owned(), ImageLoad::Loading);
        true
    }

    pub(crate) fn finish_load(&mut self, path: String, image: Option<Arc<RenderImage>>) {
        self.entries
            .insert(path, image.map_or(ImageLoad::Failed, ImageLoad::Ready));
    }

    /// Drops decoded images no current screen lists, once enough accumulate.
    pub(crate) fn prune<'a>(&mut self, referenced: impl Iterator<Item = &'a str>) {
        if self.entries.len() <= MAX_UNREFERENCED_IMAGES {
            return;
        }
        let referenced = referenced.collect::<HashSet<_>>();
        self.entries
            .retain(|path, _| referenced.contains(path.as_str()));
    }
}

/// Decodes one service-written PNG into GPUI's BGRA frame format.
pub(crate) fn decode_terminal_image(path: &Path) -> Result<Arc<RenderImage>> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut rgba = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .with_context(|| format!("decode {}", path.display()))?
        .into_rgba8();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![Frame::new(rgba)])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hh_protocol::{TerminalAttributes, TerminalRun, placeholder_diacritic};

    fn placeholders(row: u16, columns: std::ops::Range<u16>) -> String {
        columns
            .map(|column| {
                [
                    KITTY_PLACEHOLDER,
                    placeholder_diacritic(row).unwrap(),
                    placeholder_diacritic(column).unwrap(),
                ]
                .into_iter()
                .collect::<String>()
            })
            .collect()
    }

    fn run(text: String, columns: u16, foreground: TerminalColor) -> TerminalRun {
        TerminalRun {
            text,
            columns,
            foreground,
            background: TerminalColor::DefaultBackground,
            attributes: TerminalAttributes::new(0),
        }
    }

    const IMAGE_7: TerminalColor = TerminalColor::Rgb {
        red: 0,
        green: 0,
        blue: 7,
    };

    #[test]
    fn a_row_of_placeholders_after_text_becomes_one_segment() {
        let line = TerminalLine {
            runs: vec![
                run("ab".to_owned(), 2, TerminalColor::DefaultForeground),
                run(placeholders(3, 0..5), 5, IMAGE_7),
            ],
        };
        assert_eq!(
            placeholder_segments(9, &line),
            vec![ImageSegment {
                row: 9,
                start_column: 2,
                cells: 5,
                image_id: 7,
                image_row: 3,
                image_column: 0,
            }]
        );
    }

    #[test]
    fn a_jump_in_image_columns_starts_a_new_segment() {
        let text = placeholders(0, 0..2) + &placeholders(0, 5..6);
        let line = TerminalLine {
            runs: vec![run(text, 3, IMAGE_7)],
        };
        let segments = placeholder_segments(0, &line);
        assert_eq!(segments.len(), 2);
        assert_eq!((segments[1].start_column, segments[1].image_column), (2, 5));
    }

    #[test]
    fn placeholders_without_an_rgb_image_color_are_ignored() {
        let line = TerminalLine {
            runs: vec![run(
                placeholders(0, 0..2),
                2,
                TerminalColor::DefaultForeground,
            )],
        };
        assert!(placeholder_segments(0, &line).is_empty());
    }

    #[test]
    fn an_image_is_fit_and_centered_in_its_placement_box_and_clipped_to_the_segment() {
        let metrics = TerminalCellMetrics {
            font_size: 10.0,
            cell_width: 10.0,
            ascent: 0.0,
            descent: 0.0,
            baseline: 0.0,
            line_height: 20.0,
        };
        let placement = TerminalImage {
            id: 7,
            generation: 1,
            columns: 10,
            rows: 5,
            path: String::new(),
        };
        // Second image row, cells 2..6 of the 10-cell-wide box.
        let segment = ImageSegment {
            row: 4,
            start_column: 12,
            cells: 4,
            image_id: 7,
            image_row: 1,
            image_column: 2,
        };
        // A square image in a 100x100 box fills it exactly.
        let (image, clip) = segment_draw_bounds(
            segment,
            &placement,
            size(50.0, 50.0),
            metrics,
            point(px(0.0), px(0.0)),
        )
        .unwrap();
        assert_eq!(image.origin, point(px(100.0), px(60.0)));
        assert_eq!(image.size, size(px(100.0), px(100.0)));
        assert_eq!(clip.origin, point(px(120.0), px(80.0)));
        assert_eq!(clip.size, size(px(40.0), px(20.0)));

        // A wide image is letterboxed vertically within the same box.
        let (image, _) = segment_draw_bounds(
            segment,
            &placement,
            size(200.0, 50.0),
            metrics,
            point(px(0.0), px(0.0)),
        )
        .unwrap();
        assert_eq!(image.size, size(px(100.0), px(25.0)));
        assert_eq!(image.origin, point(px(100.0), px(97.5)));
    }
}
