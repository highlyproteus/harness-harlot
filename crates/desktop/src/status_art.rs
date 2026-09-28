//! Live status drawing: task-progress rings, the indeterminate running ring,
//! and the needs-input comet border, plus the motion clock that paces them
//! and the system "Reduce motion" preference that stills them.
//!
//! Everything is stroked as anti-aliased GPU paths in logical pixels, so it
//! stays crisp at any scale factor. Animation is driven by
//! [`crate::HhApp::ensure_animation_tick`], which only runs while a frame
//! actually drew something animated.
use std::cell::Cell;
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::time::{Duration, Instant};

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Window, point, px, rgb};

use crate::THEME;

/// ~30 fps: smooth enough for a small travelling highlight, and half the
/// cost of redrawing at display rate.
pub(crate) const ANIMATION_FRAME: Duration = Duration::from_millis(33);
/// One clockwise loop of the needs-input comet.
pub(crate) const COMET_PERIOD_SECS: f32 = 2.0;
/// Share of the perimeter the comet covers, tail included.
pub(crate) const COMET_FRACTION: f32 = 0.2;
/// One rotation of the indeterminate running ring.
pub(crate) const SPINNER_PERIOD_SECS: f32 = 1.2;
/// How often the reduce-motion preference may be re-read.
const REDUCED_MOTION_RECHECK: Duration = Duration::from_secs(3);

pub(crate) const BORDER_WIDTH: f32 = 1.5;
const RING_RADIUS: f32 = 3.5;
const RING_WIDTH: f32 = 1.5;
/// Arc left open on the indeterminate ring.
const SPINNER_GAP: f32 = FRAC_PI_2;
const COMET_BANDS: u8 = 6;
const COMET_COLOR: u32 = 0xffd49a;

pub(crate) const PROGRESS_RED: u32 = 0xe5484d;
pub(crate) const PROGRESS_AMBER: u32 = 0xf5a524;
pub(crate) const PROGRESS_GREEN: u32 = 0x30a46c;

/// The shared animation clock and the reduce-motion switch.
#[derive(Debug)]
pub(crate) struct Motion {
    started: Instant,
    reduced: bool,
    reduced_read_at: Option<Instant>,
    /// Set while rendering whenever something animated was drawn; the tick
    /// keeps running only while the last frame set it.
    animating: Cell<bool>,
    pub(crate) tick_running: bool,
}

impl Motion {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            reduced: system_prefers_reduced_motion(),
            reduced_read_at: Some(now),
            animating: Cell::new(false),
            tick_running: false,
        }
    }

    pub(crate) const fn reduced(&self) -> bool {
        self.reduced
    }

    /// Seconds since launch; every animation reads the same clock so all
    /// comets and rings move in step.
    pub(crate) fn elapsed_secs(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }

    /// Called at the top of each frame.
    pub(crate) fn begin_frame(&self) {
        self.animating.set(false);
    }

    /// Records that the frame being built draws something animated.
    pub(crate) fn request_frames(&self) {
        self.animating.set(true);
    }

    pub(crate) fn wants_frames(&self) -> bool {
        self.animating.get()
    }

    /// Re-reads the reduce-motion preference unless it was read within the
    /// last few seconds. Returns whether it changed.
    pub(crate) fn refresh_reduced(&mut self, now: Instant) -> bool {
        if !cfg!(target_os = "macos")
            || self
                .reduced_read_at
                .is_some_and(|read| now.duration_since(read) < REDUCED_MOTION_RECHECK)
        {
            return false;
        }
        self.reduced_read_at = Some(now);
        let reduced = system_prefers_reduced_motion();
        let changed = reduced != self.reduced;
        self.reduced = reduced;
        changed
    }
}

#[cfg(target_os = "macos")]
fn system_prefers_reduced_motion() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

/// GNOME's animation switch, read once at startup; absent tools or keys
/// leave motion on.
#[cfg(target_os = "linux")]
fn system_prefers_reduced_motion() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "false")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn system_prefers_reduced_motion() -> bool {
    false
}

/// How a needs-input border is drawn this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum BorderMotion {
    /// The comet's head sits at this fraction of the perimeter.
    Comet { head: f32 },
    /// Reduced motion: a solid border with a steady glow.
    Glow,
}

pub(crate) fn border_motion(reduced: bool, elapsed_secs: f32) -> BorderMotion {
    if reduced {
        BorderMotion::Glow
    } else {
        BorderMotion::Comet {
            head: comet_head(elapsed_secs),
        }
    }
}

/// Where the comet's head is, as a fraction of the perimeter in `0..1`.
pub(crate) fn comet_head(elapsed_secs: f32) -> f32 {
    (elapsed_secs / COMET_PERIOD_SECS).rem_euclid(1.0)
}

/// The indeterminate ring's rotation in radians, or `None` when motion is
/// reduced and the ring stays still.
pub(crate) fn spinner_rotation(reduced: bool, elapsed_secs: f32) -> Option<f32> {
    (!reduced).then(|| (elapsed_secs / SPINNER_PERIOD_SECS).rem_euclid(1.0) * TAU)
}

/// Point at fraction `s` of the perimeter of a `width`×`height` rectangle
/// with corner `radius`, travelling clockwise on screen (y grows downward)
/// from the left end of the top edge. `s` wraps, so any real works.
pub(crate) fn perimeter_point(width: f32, height: f32, radius: f32, s: f32) -> (f32, f32) {
    let width = width.max(0.0);
    let height = height.max(0.0);
    let radius = radius.clamp(0.0, width.min(height) / 2.0);
    let straight_x = width - 2.0 * radius;
    let straight_y = height - 2.0 * radius;
    let corner = FRAC_PI_2 * radius;
    let total = 2.0 * (straight_x + straight_y) + 4.0 * corner;
    if total <= 0.0 {
        return (0.0, 0.0);
    }
    let mut d = s.rem_euclid(1.0) * total;
    // Each corner is an arc from `start` angle, clockwise, around `center`.
    let arc = |center: (f32, f32), start: f32, d: f32| {
        let angle = if radius > 0.0 {
            start + d / radius
        } else {
            start
        };
        (
            center.0 + radius * angle.cos(),
            center.1 + radius * angle.sin(),
        )
    };
    if d <= straight_x {
        return (radius + d, 0.0);
    }
    d -= straight_x;
    if d <= corner {
        return arc((width - radius, radius), -FRAC_PI_2, d);
    }
    d -= corner;
    if d <= straight_y {
        return (width, radius + d);
    }
    d -= straight_y;
    if d <= corner {
        return arc((width - radius, height - radius), 0.0, d);
    }
    d -= corner;
    if d <= straight_x {
        return (width - radius - d, height);
    }
    d -= straight_x;
    if d <= corner {
        return arc((radius, height - radius), FRAC_PI_2, d);
    }
    d -= corner;
    if d <= straight_y {
        return (0.0, height - radius - d);
    }
    d -= straight_y;
    arc((radius, radius), PI, d.min(corner))
}

fn perimeter_length(width: f32, height: f32, radius: f32) -> f32 {
    let radius = radius.clamp(0.0, width.min(height).max(0.0) / 2.0);
    2.0 * (width + height - 4.0 * radius).max(0.0) + TAU * radius
}

/// Task-progress color: red at 0, amber at ½, green at 1, interpolated in
/// HSL along the shorter hue arc so the midway tones stay saturated.
pub(crate) fn progress_color(fraction: f32) -> u32 {
    let fraction = if fraction.is_nan() {
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
    };
    if fraction <= 0.5 {
        lerp_hsl(PROGRESS_RED, PROGRESS_AMBER, fraction * 2.0)
    } else {
        lerp_hsl(PROGRESS_AMBER, PROGRESS_GREEN, (fraction - 0.5) * 2.0)
    }
}

fn lerp_hsl(from: u32, to: u32, t: f32) -> u32 {
    if t <= 0.0 {
        return from;
    }
    if t >= 1.0 {
        return to;
    }
    let (h1, s1, l1) = rgb_to_hsl(from);
    let (h2, s2, l2) = rgb_to_hsl(to);
    let delta = (h2 - h1 + 540.0).rem_euclid(360.0) - 180.0;
    hsl_to_rgb(
        (h1 + delta * t).rem_euclid(360.0),
        s1 + (s2 - s1) * t,
        l1 + (l2 - l1) * t,
    )
}

fn channels(color: u32) -> [f32; 3] {
    [16, 8, 0].map(|shift| f32::from(((color >> shift) & 0xff) as u8) / 255.0)
}

fn rgb_to_hsl(color: u32) -> (f32, f32, f32) {
    let [red, green, blue] = channels(color);
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let lightness = f32::midpoint(max, min);
    let chroma = max - min;
    if chroma == 0.0 {
        return (0.0, 0.0, lightness);
    }
    let saturation = chroma / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = if (max - red).abs() < f32::EPSILON {
        60.0 * ((green - blue) / chroma).rem_euclid(6.0)
    } else if (max - green).abs() < f32::EPSILON {
        60.0 * ((blue - red) / chroma + 2.0)
    } else {
        60.0 * ((red - green) / chroma + 4.0)
    };
    (hue, saturation, lightness)
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> u32 {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = hue / 60.0;
    let second = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
    let (red, green, blue) = match sector as u8 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let offset = lightness - chroma / 2.0;
    let byte = |value: f32| u32::from(((value + offset).clamp(0.0, 1.0) * 255.0).round() as u8);
    (byte(red) << 16) | (byte(green) << 8) | byte(blue)
}

fn color(rgb_value: u32, alpha: f32) -> Hsla {
    let mut color = Hsla::from(rgb(rgb_value));
    color.a = alpha;
    color
}

/// Strokes an open (or `closed`) polyline of logical-pixel points offset by
/// `origin`.
fn stroke_polyline(
    window: &mut Window,
    origin: gpui::Point<Pixels>,
    points: &[(f32, f32)],
    closed: bool,
    width: f32,
    stroke: Hsla,
) {
    let Some((first, rest)) = points.split_first() else {
        return;
    };
    let at = |(x, y): (f32, f32)| point(origin.x + px(x), origin.y + px(y));
    let mut builder = PathBuilder::stroke(px(width));
    builder.move_to(at(*first));
    for point in rest {
        builder.line_to(at(*point));
    }
    if closed {
        builder.close();
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, stroke);
    }
}

/// Points along the ring of `radius` around `center`, from `start` radians
/// clockwise through `sweep` radians.
fn arc_points(center: (f32, f32), radius: f32, start: f32, sweep: f32) -> Vec<(f32, f32)> {
    // One segment per ~6°: smooth at the ring's 7 px diameter even at 2x.
    let segments = ((sweep.abs() / TAU) * 60.0).ceil().max(2.0) as u16;
    (0..=segments)
        .map(|index| {
            let angle = start + sweep * f32::from(index) / f32::from(segments);
            (
                center.0 + radius * angle.cos(),
                center.1 + radius * angle.sin(),
            )
        })
        .collect()
}

/// A ring whose filled arc is `fraction` of the way around from 12 o'clock,
/// over a faint full track.
pub(crate) fn paint_progress_ring(bounds: Bounds<Pixels>, window: &mut Window, fraction: f32) {
    let center = (
        f32::from(bounds.size.width) / 2.0,
        f32::from(bounds.size.height) / 2.0,
    );
    let track = arc_points(center, RING_RADIUS, 0.0, TAU);
    stroke_polyline(
        window,
        bounds.origin,
        &track,
        true,
        RING_WIDTH,
        color(THEME.dim, 0.45),
    );
    let fraction = fraction.clamp(0.0, 1.0);
    if fraction <= 0.0 {
        return;
    }
    let arc = arc_points(center, RING_RADIUS, -FRAC_PI_2, fraction * TAU);
    stroke_polyline(
        window,
        bounds.origin,
        &arc,
        fraction >= 1.0,
        RING_WIDTH,
        color(progress_color(fraction), 1.0),
    );
}

/// The blue running ring without task data: open by a quarter that rotates
/// clockwise, or closed and still when `rotation` is `None`.
pub(crate) fn paint_spinner_ring(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    rotation: Option<f32>,
) {
    let center = (
        f32::from(bounds.size.width) / 2.0,
        f32::from(bounds.size.height) / 2.0,
    );
    let blue = color(THEME.accent, 1.0);
    if let Some(rotation) = rotation {
        let points = arc_points(center, RING_RADIUS, rotation - FRAC_PI_2, TAU - SPINNER_GAP);
        stroke_polyline(window, bounds.origin, &points, false, RING_WIDTH, blue);
    } else {
        let points = arc_points(center, RING_RADIUS, 0.0, TAU);
        stroke_polyline(window, bounds.origin, &points, true, RING_WIDTH, blue);
    }
}

/// Samples the perimeter inset by half the stroke width from `from` to `to`
/// (fractions, `to` may exceed 1 to wrap).
fn perimeter_run(
    width: f32,
    height: f32,
    radius: f32,
    inset: f32,
    from: f32,
    to: f32,
) -> Vec<(f32, f32)> {
    let inner_w = width - 2.0 * inset;
    let inner_h = height - 2.0 * inset;
    let inner_r = (radius - inset).max(0.0);
    let length = perimeter_length(inner_w, inner_h, inner_r) * (to - from);
    // ~1.5 px per segment keeps the corners round and the run smooth.
    let segments = (length / 1.5).ceil().max(2.0) as u16;
    (0..=segments)
        .map(|index| {
            let s = from + (to - from) * f32::from(index) / f32::from(segments);
            let (x, y) = perimeter_point(inner_w, inner_h, inner_r, s);
            (x + inset, y + inset)
        })
        .collect()
}

/// The needs-input frame: an orange border with either a bright comet at
/// `head` running clockwise, or (reduced motion) a steady inner glow.
pub(crate) fn paint_needs_input_border(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    radius: f32,
    motion: BorderMotion,
) {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if width < 2.0 * BORDER_WIDTH || height < 2.0 * BORDER_WIDTH {
        return;
    }
    let inset = BORDER_WIDTH / 2.0;
    let ring = perimeter_run(width, height, radius, inset, 0.0, 1.0);
    stroke_polyline(
        window,
        bounds.origin,
        &ring,
        true,
        BORDER_WIDTH,
        color(THEME.warning, 1.0),
    );
    match motion {
        BorderMotion::Glow => {
            for (depth, alpha) in [(2.0, 0.35), (3.25, 0.15)] {
                let halo = perimeter_run(width, height, radius, inset + depth, 0.0, 1.0);
                stroke_polyline(
                    window,
                    bounds.origin,
                    &halo,
                    true,
                    BORDER_WIDTH,
                    color(THEME.warning, alpha),
                );
            }
        }
        BorderMotion::Comet { head } => {
            // The tail fades in bands from faint to the bright head.
            let band = COMET_FRACTION / f32::from(COMET_BANDS);
            for index in 0..COMET_BANDS {
                let from = head - COMET_FRACTION + band * f32::from(index);
                let alpha = f32::from(index + 1) / f32::from(COMET_BANDS);
                let points = perimeter_run(width, height, radius, inset, from, from + band);
                stroke_polyline(
                    window,
                    bounds.origin,
                    &points,
                    false,
                    BORDER_WIDTH,
                    color(COMET_COLOR, alpha),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BorderMotion, COMET_PERIOD_SECS, PROGRESS_AMBER, PROGRESS_GREEN, PROGRESS_RED,
        SPINNER_PERIOD_SECS, border_motion, channels, comet_head, perimeter_point, progress_color,
        rgb_to_hsl, spinner_rotation,
    };

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
    }

    #[test]
    fn progress_color_runs_red_to_amber_to_green() {
        assert_eq!(progress_color(0.0), PROGRESS_RED);
        assert_eq!(progress_color(0.5), PROGRESS_AMBER);
        assert_eq!(progress_color(1.0), PROGRESS_GREEN);
        assert_eq!(progress_color(-3.0), PROGRESS_RED, "clamped below");
        assert_eq!(progress_color(7.0), PROGRESS_GREEN, "clamped above");
        assert_eq!(progress_color(f32::NAN), PROGRESS_RED);

        // A quarter of the way sits between red and amber in hue, going
        // through orange rather than the long way round through blue.
        let (red_hue, _, _) = rgb_to_hsl(PROGRESS_RED);
        let (amber_hue, _, _) = rgb_to_hsl(PROGRESS_AMBER);
        let (quarter_hue, saturation, _) = rgb_to_hsl(progress_color(0.25));
        let unwrapped = |hue: f32| if hue > 180.0 { hue - 360.0 } else { hue };
        assert!(
            unwrapped(red_hue) < unwrapped(quarter_hue)
                && unwrapped(quarter_hue) < unwrapped(amber_hue),
            "{red_hue} < {quarter_hue} < {amber_hue}"
        );
        assert!(saturation > 0.6, "midway tones stay saturated");
        let [r, g, b] = channels(progress_color(0.75));
        assert!(g > r && g > b, "three quarters leans green");
    }

    #[test]
    fn perimeter_runs_clockwise_from_the_top_left() {
        let (width, height, radius) = (100.0, 20.0, 4.0);
        assert!(close(
            perimeter_point(width, height, radius, 0.0),
            (4.0, 0.0)
        ));
        // Straight runs: top →, right ↓, bottom ←, left ↑.
        let sample = |s: f32| perimeter_point(width, height, radius, s);
        let total = 2.0 * (92.0 + 12.0) + std::f32::consts::TAU * 4.0;
        let top = sample(40.0 / total);
        let right = sample((92.0 + std::f32::consts::FRAC_PI_2 * 4.0 + 6.0) / total);
        let bottom = sample((92.0 + 12.0 + std::f32::consts::PI * 4.0 + 40.0) / total);
        let left =
            sample((2.0 * 92.0 + 12.0 + 3.0 * std::f32::consts::FRAC_PI_2 * 4.0 + 6.0) / total);
        assert!(close(top, (44.0, 0.0)), "{top:?}");
        assert!(close(right, (100.0, 10.0)), "{right:?}");
        assert!(close(bottom, (56.0, 20.0)), "{bottom:?}");
        assert!(close(left, (0.0, 10.0)), "{left:?}");

        // Walking forward, the point always turns right (clockwise on a
        // y-down screen): the cross product of successive steps is ≥ 0.
        let points = (0..=400_u16)
            .map(|index| sample(f32::from(index) / 400.0))
            .collect::<Vec<_>>();
        for window in points.windows(3) {
            let (before, at, after) = (window[0], window[1], window[2]);
            let cross = (at.0 - before.0) * (after.1 - at.1) - (at.1 - before.1) * (after.0 - at.0);
            assert!(cross >= -1e-3, "turned counter-clockwise at {at:?}");
        }
        assert!(close(sample(1.0), sample(0.0)), "closed loop");
        assert!(close(sample(1.25), sample(0.25)), "wraps");
        assert!(close(sample(-0.25), sample(0.75)), "wraps backwards");
    }

    #[test]
    fn square_corners_and_degenerate_boxes_stay_on_the_edge() {
        assert!(close(perimeter_point(10.0, 10.0, 0.0, 0.25), (10.0, 0.0)));
        assert!(close(perimeter_point(10.0, 10.0, 0.0, 0.5), (10.0, 10.0)));
        assert!(close(perimeter_point(0.0, 0.0, 3.0, 0.3), (0.0, 0.0)));
        // An oversized radius is clamped to a pill.
        let (x, y) = perimeter_point(20.0, 10.0, 50.0, 0.1);
        assert!((0.0..=20.0).contains(&x) && (0.0..=10.0).contains(&y));
    }

    #[test]
    fn the_comet_loops_once_per_period() {
        assert!(comet_head(0.0).abs() < 1e-6);
        let quarter = comet_head(COMET_PERIOD_SECS / 4.0);
        assert!((quarter - 0.25).abs() < 1e-5);
        for t in [0.3_f32, 1.1, 5.7] {
            assert!((comet_head(t) - comet_head(t + COMET_PERIOD_SECS)).abs() < 1e-4);
            assert!((0.0..1.0).contains(&comet_head(t)));
        }
        assert!(
            comet_head(0.5) > comet_head(0.25),
            "head advances clockwise"
        );
    }

    #[test]
    fn reduced_motion_stills_the_border_and_the_running_ring() {
        assert_eq!(border_motion(true, 0.7), BorderMotion::Glow);
        assert!(matches!(
            border_motion(false, 0.7),
            BorderMotion::Comet { head } if (head - comet_head(0.7)).abs() < 1e-6
        ));
        assert_eq!(spinner_rotation(true, 0.4), None);
        let rotation = spinner_rotation(false, SPINNER_PERIOD_SECS / 2.0).expect("animated");
        assert!((rotation - std::f32::consts::PI).abs() < 1e-4);
    }
}
