//! Paints kitty-graphics images behind Unicode placeholder cells.
//!
//! The session service writes each transmitted PNG to an owner-only file and
//! lists it in `TerminalScreen::images`. Placeholder cells in the grid name
//! the image (foreground color) and their row/column within it (diacritics);
//! each horizontal stretch of such cells becomes one clipped image draw.
//!
//! The GPU samples textures bilinearly without mipmaps, which skips source
//! pixels when shrinking and smears them when enlarging. So each image is
//! resampled on the CPU to the exact device-pixel size it is drawn at and
//! drawn pixel-aligned; the full-size decode is drawn until that is ready.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{Bounds, DevicePixels, ImageId, Pixels, RenderImage, Size, point, px, size};
use hh_protocol::{
    KITTY_PLACEHOLDER, PlaceholderCell, TerminalColor, TerminalImage, TerminalLine,
    placeholder_cells, placeholder_image_id,
};
use image::imageops::FilterType;
use image::{Frame, ImageBuffer, Rgba};

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

/// Snaps an image's logical draw bounds to whole device pixels, returning
/// them with their size in device pixels. GPUI floors a sprite's device
/// origin and ceils its device size; the quarter-pixel nudges keep float
/// error from moving either by a pixel, so a texture resampled to that size
/// maps each texel onto one pixel. `None` when the image covers no pixel.
pub(crate) fn pixel_aligned_bounds(
    bounds: Bounds<Pixels>,
    scale_factor: f32,
) -> Option<(Bounds<Pixels>, Size<DevicePixels>)> {
    let x = (f32::from(bounds.origin.x) * scale_factor).round();
    let y = (f32::from(bounds.origin.y) * scale_factor).round();
    let width = (f32::from(bounds.size.width) * scale_factor).round();
    let height = (f32::from(bounds.size.height) * scale_factor).round();
    if !(width >= 1.0 && height >= 1.0 && width <= 65_536.0 && height <= 65_536.0) {
        return None;
    }
    let snapped = Bounds::new(
        point(px((x + 0.25) / scale_factor), px((y + 0.25) / scale_factor)),
        size(
            px((width - 0.25) / scale_factor),
            px((height - 0.25) / scale_factor),
        ),
    );
    // Checked above: whole numbers in 1..=65536.
    #[allow(clippy::cast_possible_truncation)]
    let device = size(DevicePixels(width as i32), DevicePixels(height as i32));
    Some((snapped, device))
}

/// The placed image painted under one screen cell. Uses the same row segments,
/// placement lookup, and decode state as the painter, so a cell maps to an
/// image exactly when that image is drawn there.
pub(crate) fn painted_image_at<'a>(
    lines: &[TerminalLine],
    images: &'a [TerminalImage],
    cache: &TerminalImageCache,
    row: u16,
    column: u16,
) -> Option<&'a TerminalImage> {
    let line = lines.get(usize::from(row))?;
    let segment = placeholder_segments(row, line)
        .into_iter()
        .find(|segment| {
            column >= segment.start_column && column - segment.start_column < segment.cells
        })?;
    let placement = images.iter().find(|image| image.id == segment.image_id)?;
    matches!(cache.get(&placement.path), Some(ImageLoad::Ready(_))).then_some(placement)
}

pub(crate) enum ImageLoad {
    Loading,
    Ready(DecodedImage),
    Failed,
}

/// A full-size decode and its copy resampled for the size it is drawn at.
pub(crate) struct DecodedImage {
    source: Arc<RenderImage>,
    resampled: Option<(Size<DevicePixels>, Arc<RenderImage>)>,
    /// Size of the resample in flight. One runs at a time, so resizing a
    /// window does not queue a resample for every intermediate size.
    pending: Option<Size<DevicePixels>>,
}

/// What to paint for one image this frame.
pub(crate) struct ImageDraw {
    pub(crate) image: Arc<RenderImage>,
    /// A resample of this source to the drawn size, for the caller to start
    /// and hand to [`TerminalImageCache::finish_resample`].
    pub(crate) resample: Option<Arc<RenderImage>>,
}

impl DecodedImage {
    /// Pixel size of the full-size decode.
    pub(crate) fn size(&self) -> Size<DevicePixels> {
        self.source.size(0)
    }
}

/// Decoded images keyed by file path (which names pane, id, and generation).
#[derive(Default)]
pub(crate) struct TerminalImageCache {
    entries: HashMap<String, ImageLoad>,
    /// Images handed out for painting since their texture was last released:
    /// GPUI's sprite atlas holds a texture for each of them.
    painted: HashSet<ImageId>,
    /// Images whose atlas texture is released at the next prepaint.
    released: Vec<Arc<RenderImage>>,
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
        let load = image.map_or(ImageLoad::Failed, |source| {
            ImageLoad::Ready(DecodedImage {
                source,
                resampled: None,
                pending: None,
            })
        });
        if let Some(previous) = self.entries.insert(path, load) {
            self.release_load(previous);
        }
    }

    /// The image to paint for `path` at `drawn` device pixels: its resample
    /// for that size once ready, the full-size decode until then (and when
    /// they already match). `None` until the image has decoded.
    pub(crate) fn draw(&mut self, path: &str, drawn: Size<DevicePixels>) -> Option<ImageDraw> {
        let Some(ImageLoad::Ready(decoded)) = self.entries.get_mut(path) else {
            return None;
        };
        let draw = match &decoded.resampled {
            Some((resampled_size, image)) if *resampled_size == drawn => ImageDraw {
                image: Arc::clone(image),
                resample: None,
            },
            _ if decoded.source.size(0) == drawn => ImageDraw {
                image: Arc::clone(&decoded.source),
                resample: None,
            },
            _ => {
                let start = decoded.pending.is_none();
                if start {
                    decoded.pending = Some(drawn);
                }
                ImageDraw {
                    image: Arc::clone(&decoded.source),
                    resample: start.then(|| Arc::clone(&decoded.source)),
                }
            }
        };
        self.painted.insert(draw.image.id);
        Some(draw)
    }

    /// Stores a finished resample, releasing the textures it replaces; if the
    /// image is drawn at another size by now, the next [`Self::draw`] starts
    /// a resample for that. A failed resample leaves the full-size decode
    /// drawn at that size.
    pub(crate) fn finish_resample(
        &mut self,
        path: &str,
        drawn: Size<DevicePixels>,
        image: Option<Arc<RenderImage>>,
    ) {
        let Some(ImageLoad::Ready(decoded)) = self.entries.get_mut(path) else {
            return;
        };
        if decoded.pending != Some(drawn) {
            return;
        }
        decoded.pending = None;
        let image = image.unwrap_or_else(|| Arc::clone(&decoded.source));
        let mut replaced = vec![Arc::clone(&decoded.source)];
        replaced.extend(
            decoded
                .resampled
                .replace((drawn, image))
                .map(|(_, previous)| previous),
        );
        for image in replaced {
            self.release(image);
        }
    }

    /// Images whose atlas texture should be dropped now, each at most once
    /// per time it was painted.
    pub(crate) fn take_released(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.released)
    }

    /// Drops decoded images no current screen lists, once enough accumulate.
    pub(crate) fn prune<'a>(&mut self, referenced: impl Iterator<Item = &'a str>) {
        if self.entries.len() <= MAX_UNREFERENCED_IMAGES {
            return;
        }
        let referenced = referenced.collect::<HashSet<_>>();
        let stale = self
            .entries
            .keys()
            .filter(|path| !referenced.contains(path.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for path in stale {
            if let Some(load) = self.entries.remove(&path) {
                self.release_load(load);
            }
        }
    }

    fn release_load(&mut self, load: ImageLoad) {
        if let ImageLoad::Ready(decoded) = load {
            self.release(decoded.source);
            if let Some((_, image)) = decoded.resampled {
                self.release(image);
            }
        }
    }

    /// Queues `image`'s atlas texture for release if it was painted. GPUI's
    /// atlas counts releases, so an unpainted image must not be released.
    fn release(&mut self, image: Arc<RenderImage>) {
        if self.painted.remove(&image.id) {
            self.released.push(image);
        }
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

/// Resamples a decoded image to `drawn` device pixels with a Catmull-Rom
/// filter, which weighs every covered source pixel when shrinking and stays
/// sharper than bilinear when enlarging. The filter treats channels alike,
/// so the BGRA order passes through unchanged.
pub(crate) fn resample_terminal_image(
    source: &RenderImage,
    drawn: Size<DevicePixels>,
) -> Option<Arc<RenderImage>> {
    let from = source.size(0);
    let view = ImageBuffer::<Rgba<u8>, &[u8]>::from_raw(
        u32::try_from(from.width.0).ok()?,
        u32::try_from(from.height.0).ok()?,
        source.as_bytes(0)?,
    )?;
    let resampled = image::imageops::resize(
        &view,
        u32::try_from(drawn.width.0).ok()?,
        u32::try_from(drawn.height.0).ok()?,
        FilterType::CatmullRom,
    );
    Some(Arc::new(RenderImage::new(vec![Frame::new(resampled)])))
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

    fn placed(id: u32, path: &str) -> TerminalImage {
        TerminalImage {
            id,
            generation: 1,
            columns: 4,
            rows: 2,
            path: path.to_owned(),
        }
    }

    fn ready(cache: &mut TerminalImageCache, path: &str) {
        let frame = Frame::new(image::RgbaImage::new(1, 1));
        cache.finish_load(
            path.to_owned(),
            Some(Arc::new(RenderImage::new(vec![frame]))),
        );
    }

    const IMAGE_9: TerminalColor = TerminalColor::Rgb {
        red: 0,
        green: 0,
        blue: 9,
    };

    /// Row 0: `ab`, image 7 in columns 2..5, a space, image 9 in columns 6..8.
    fn two_images() -> (Vec<TerminalLine>, Vec<TerminalImage>) {
        let lines = vec![
            TerminalLine {
                runs: vec![
                    run("ab".to_owned(), 2, TerminalColor::DefaultForeground),
                    run(placeholders(0, 0..3), 3, IMAGE_7),
                    run(" ".to_owned(), 1, TerminalColor::DefaultForeground),
                    run(placeholders(0, 0..2), 2, IMAGE_9),
                ],
            },
            TerminalLine {
                runs: vec![run(placeholders(1, 0..3), 3, IMAGE_7)],
            },
        ];
        (lines, vec![placed(7, "/seven.png"), placed(9, "/nine.png")])
    }

    #[test]
    fn a_cell_inside_a_painted_segment_maps_to_its_image() {
        let (lines, images) = two_images();
        let mut cache = TerminalImageCache::default();
        ready(&mut cache, "/seven.png");
        ready(&mut cache, "/nine.png");
        let at = |row, column| painted_image_at(&lines, &images, &cache, row, column);
        assert_eq!(at(0, 2).map(|image| image.id), Some(7));
        assert_eq!(at(0, 4).map(|image| image.id), Some(7));
        assert_eq!(at(1, 0).map(|image| image.id), Some(7));
        assert_eq!(at(0, 6).map(|image| image.id), Some(9));
        assert_eq!(at(0, 7).map(|image| image.id), Some(9));
    }

    #[test]
    fn cells_outside_every_segment_map_to_no_image() {
        let (lines, images) = two_images();
        let mut cache = TerminalImageCache::default();
        ready(&mut cache, "/seven.png");
        ready(&mut cache, "/nine.png");
        let at = |row, column| painted_image_at(&lines, &images, &cache, row, column);
        // Text before, the gap between, past the last segment, and past the grid.
        assert!(at(0, 1).is_none());
        assert!(at(0, 5).is_none());
        assert!(at(0, 8).is_none());
        assert!(at(1, 3).is_none());
        assert!(at(2, 0).is_none());
    }

    #[test]
    fn only_decoded_placed_images_are_hit() {
        let (lines, images) = two_images();
        let mut cache = TerminalImageCache::default();
        assert!(cache.begin_load("/seven.png"));
        cache.finish_load("/nine.png".to_owned(), None);
        // Still decoding, or failed to decode: nothing is painted there.
        assert!(painted_image_at(&lines, &images, &cache, 0, 2).is_none());
        assert!(painted_image_at(&lines, &images, &cache, 0, 6).is_none());
        ready(&mut cache, "/seven.png");
        assert!(painted_image_at(&lines, &images, &cache, 0, 2).is_some());
        // Placeholder cells for an id the screen no longer lists.
        assert!(painted_image_at(&lines, &images[1..], &cache, 0, 2).is_none());
    }

    fn device(width: i32, height: i32) -> Size<DevicePixels> {
        size(DevicePixels(width), DevicePixels(height))
    }

    fn image_of(pixels: image::RgbaImage) -> Arc<RenderImage> {
        Arc::new(RenderImage::new(vec![Frame::new(pixels)]))
    }

    fn ids(images: &[Arc<RenderImage>]) -> Vec<ImageId> {
        images.iter().map(|image| image.id).collect()
    }

    #[test]
    // Every compared value is a whole number of pixels, so exact equality is the point.
    #[allow(clippy::float_cmp)]
    fn aligned_bounds_cover_whole_device_pixels_after_gpui_rounds_them() {
        let bounds = Bounds::new(point(px(100.3), px(50.7)), size(px(200.45), px(99.9)));
        for scale in [1.0_f32, 1.5, 2.0] {
            let (snapped, drawn) = pixel_aligned_bounds(bounds, scale).unwrap();
            // What GPUI's paint_image does: scale, floor the origin, ceil the size.
            let origin_x = (f32::from(snapped.origin.x) * scale).floor();
            let origin_y = (f32::from(snapped.origin.y) * scale).floor();
            let width = (f32::from(snapped.size.width) * scale).ceil();
            let height = (f32::from(snapped.size.height) * scale).ceil();
            assert_eq!(origin_x, (100.3 * scale).round());
            assert_eq!(origin_y, (50.7 * scale).round());
            #[allow(clippy::cast_precision_loss)]
            let expected = (drawn.width.0 as f32, drawn.height.0 as f32);
            assert_eq!((width, height), expected);
            assert_eq!(expected, ((200.45 * scale).round(), (99.9 * scale).round()));
        }
        let empty = Bounds::new(point(px(0.0), px(0.0)), size(px(0.2), px(40.0)));
        assert!(pixel_aligned_bounds(empty, 2.0).is_none());
    }

    #[test]
    fn the_resample_for_the_drawn_size_replaces_the_full_size_decode() {
        let mut cache = TerminalImageCache::default();
        let source = image_of(image::RgbaImage::new(8, 8));
        cache.finish_load("/a.png".to_owned(), Some(Arc::clone(&source)));

        // Already the drawn size: drawn as is, nothing to resample.
        let draw = cache.draw("/a.png", device(8, 8)).unwrap();
        assert_eq!((draw.image.id, draw.resample.is_none()), (source.id, true));

        // Another size: the decode is drawn while one resample runs, and
        // sizes seen meanwhile (a window being resized) start no more.
        let draw = cache.draw("/a.png", device(4, 4)).unwrap();
        assert_eq!(draw.image.id, source.id);
        assert_eq!(draw.resample.map(|image| image.id), Some(source.id));
        let resample_started = |cache: &mut TerminalImageCache, width, height| {
            cache
                .draw("/a.png", device(width, height))
                .unwrap()
                .resample
                .is_some()
        };
        assert!(!resample_started(&mut cache, 4, 4));
        assert!(!resample_started(&mut cache, 5, 5));

        let small = image_of(image::RgbaImage::new(4, 4));
        cache.finish_resample("/a.png", device(4, 4), Some(Arc::clone(&small)));
        let draw = cache.draw("/a.png", device(4, 4)).unwrap();
        assert_eq!((draw.image.id, draw.resample.is_none()), (small.id, true));

        // Once it finishes, a different drawn size gets its own resample; a
        // result for a size no longer drawn is not shown.
        assert!(resample_started(&mut cache, 6, 6));
        let six = image_of(image::RgbaImage::new(6, 6));
        cache.finish_resample("/a.png", device(6, 6), Some(six));
        let draw = cache.draw("/a.png", device(7, 7)).unwrap();
        assert_eq!(draw.image.id, source.id);
        assert!(draw.resample.is_some());
    }

    #[test]
    fn replaced_and_pruned_textures_are_released_once_and_only_if_painted() {
        let mut cache = TerminalImageCache::default();
        let source = image_of(image::RgbaImage::new(8, 8));
        cache.finish_load("/a.png".to_owned(), Some(Arc::clone(&source)));
        cache.draw("/a.png", device(4, 4));
        let small = image_of(image::RgbaImage::new(4, 4));
        cache.finish_resample("/a.png", device(4, 4), Some(Arc::clone(&small)));
        // The decode was painted while the resample ran; it is not painted any more.
        assert_eq!(ids(&cache.take_released()), vec![source.id]);
        assert!(cache.take_released().is_empty());

        cache.draw("/a.png", device(4, 4));
        cache.draw("/a.png", device(6, 6));
        let medium = image_of(image::RgbaImage::new(6, 6));
        cache.finish_resample("/a.png", device(6, 6), Some(Arc::clone(&medium)));
        let mut released = ids(&cache.take_released());
        released.sort();
        let mut expected = vec![source.id, small.id];
        expected.sort();
        assert_eq!(released, expected);

        // Decoded but never painted: pruning it releases nothing.
        for index in 0..=MAX_UNREFERENCED_IMAGES {
            ready(&mut cache, &format!("/unpainted-{index}.png"));
        }
        cache.draw("/a.png", device(6, 6));
        cache.prune(std::iter::empty());
        assert_eq!(ids(&cache.take_released()), vec![medium.id]);
    }

    #[test]
    fn shrinking_averages_every_source_pixel_instead_of_skipping_them() {
        // One-pixel black and white stripes shrunk 3x must come out an even
        // gray; a sampler that skips pixels turns them into solid bars.
        let stripes = image::RgbaImage::from_fn(90, 12, |x, _| {
            let value = if x % 2 == 0 { 0 } else { 255 };
            image::Rgba([value, value, value, 255])
        });
        let resampled = resample_terminal_image(&image_of(stripes), device(30, 4)).unwrap();
        assert_eq!(resampled.size(0), device(30, 4));
        let pixels = resampled.as_bytes(0).unwrap();
        // Away from the edges, where the filter sees stripes on both sides.
        for column in 2..28 {
            let value = i32::from(pixels[(30 + column) * 4]);
            assert!((value - 127).abs() < 24, "column {column} is {value}");
        }
    }
}
