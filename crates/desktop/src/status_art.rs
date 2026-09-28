//! Live status drawing: the state border every tab wears (working blue with
//! its task-progress fill, needs-you magenta, done green) with its comet,
//! plus the motion clock that paces it and the system "Reduce motion"
//! preference that stills it.
//!
//! Everything is stroked as anti-aliased GPU paths in logical pixels, so it
//! stays crisp at any scale factor. Animation is driven by
//! [`crate::HhApp::ensure_animation_tick`], which only runs while the window
//! keeps drawing frames that contain something animated.
use std::cell::Cell;
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::time::{Duration, Instant};

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Window, point, px, rgb};

use crate::THEME;

/// ~30 fps: the needs-you comet is the one that must catch the eye.
const COMET_FRAME: Duration = Duration::from_millis(33);
/// ~12 fps for working and done comets: they move on the same 2 s loop but
/// every frame redraws the whole window, and they can run for a long time.
const CALM_FRAME: Duration = Duration::from_millis(83);
/// One clockwise loop of a comet.
pub(crate) const COMET_PERIOD_SECS: f32 = 2.0;
/// Share of the perimeter the comet covers, tail included.
pub(crate) const COMET_FRACTION: f32 = 0.2;
/// How often the reduce-motion preference may be re-read.
const REDUCED_MOTION_RECHECK: Duration = Duration::from_secs(1);

pub(crate) const BORDER_WIDTH: f32 = 1.5;
/// The progress fill is a little heavier than its track.
const FILL_WIDTH: f32 = 2.0;
const COMET_BANDS: u8 = 6;
/// How far the comet's tint leans from the state colour toward white.
const COMET_TINT: f32 = 0.55;

/// Border colours (palette A): no red, amber, or orange anywhere, so a
/// working tab, one that needs you, and one that finished can't be confused.
pub(crate) const WORKING_COLOR: u32 = THEME.accent;
pub(crate) const NEEDS_YOU_COLOR: u32 = 0xd6_5cf2;
pub(crate) const DONE_COLOR: u32 = 0x3f_b950;

/// The fastest-moving thing a frame drew, which sets the next frame's delay.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum FrameRate {
    /// Nothing moves: no timer.
    #[default]
    Still,
    /// A working or done comet.
    Calm,
    /// The needs-you comet.
    Comet,
}

impl FrameRate {
    /// Delay before the next frame, or `None` when nothing moves.
    pub(crate) const fn interval(self) -> Option<Duration> {
        match self {
            Self::Still => None,
            Self::Calm => Some(CALM_FRAME),
            Self::Comet => Some(COMET_FRAME),
        }
    }
}

/// Whether the reduce-motion preference last read at `read_at` is due for a
/// re-read at `now`.
pub(crate) fn reduced_motion_recheck_due(read_at: Option<Instant>, now: Instant) -> bool {
    read_at.is_none_or(|read| now.saturating_duration_since(read) >= REDUCED_MOTION_RECHECK)
}

/// The shared animation clock and the reduce-motion switch.
#[derive(Debug)]
pub(crate) struct Motion {
    started: Instant,
    reduced: bool,
    reduced_read_at: Option<Instant>,
    /// What the frame being built (or last built) animates.
    rate: Cell<FrameRate>,
    /// Whether a frame was drawn since the tick last asked for one. gpui
    /// stops drawing a minimised or fully hidden window, so a tick that finds
    /// no frame since its last request stops instead of waking in the dark;
    /// the pending redraw restarts it once the window shows again.
    drawn: Cell<bool>,
    pub(crate) tick_running: bool,
}

impl Motion {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            reduced: system_prefers_reduced_motion(),
            reduced_read_at: Some(now),
            rate: Cell::new(FrameRate::Still),
            drawn: Cell::new(false),
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
        self.rate.set(FrameRate::Still);
        self.drawn.set(true);
    }

    /// Records that the frame being built draws something moving at `rate`.
    pub(crate) fn request_frames(&self, rate: FrameRate) {
        self.rate.set(self.rate.get().max(rate));
    }

    /// Delay before the first frame of a new tick, or `None` when the last
    /// frame drew nothing moving.
    pub(crate) fn wanted_interval(&self) -> Option<Duration> {
        self.rate.get().interval()
    }

    /// One tick of the animation timer: re-reads reduce motion (throttled)
    /// and returns the delay before the next frame, or `None` to stop because
    /// the last frame drew nothing moving or no frame was drawn since the
    /// last tick (the window is hidden or minimised). A `Some` asks the
    /// caller to redraw; a frame drawn after reduce motion turns on draws
    /// everything still, so the tick after it stops.
    pub(crate) fn next_tick(&mut self, now: Instant) -> Option<Duration> {
        if !self.drawn.replace(false) {
            return None;
        }
        self.refresh_reduced(now);
        self.wanted_interval()
    }

    /// Re-reads the reduce-motion preference unless it was read within the
    /// last second. Returns whether it changed.
    pub(crate) fn refresh_reduced(&mut self, now: Instant) -> bool {
        if !cfg!(target_os = "macos") || !reduced_motion_recheck_due(self.reduced_read_at, now) {
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

/// How a state border is drawn this frame.
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

/// What one state border draws: its colour, the base border's opacity, the
/// task-progress fill (a fraction clockwise from the top-left corner) when
/// the agent reports a list, and the motion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BorderArt {
    pub(crate) color: u32,
    pub(crate) base_alpha: f32,
    pub(crate) fill: Option<f32>,
    pub(crate) motion: BorderMotion,
}

/// Opacity of the dim track under a progress fill.
const FILL_TRACK_ALPHA: f32 = 0.18;

/// The comet's colour: the state colour leaning toward white.
pub(crate) fn comet_tint(color: u32) -> u32 {
    let lift = |shift: u32| {
        let channel = f32::from(((color >> shift) & 0xff) as u8);
        let lifted = channel + (255.0 - channel) * COMET_TINT;
        (lifted.round() as u32).min(255) << shift
    };
    lift(16) | lift(8) | lift(0)
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

#[cfg(test)]
fn channels(color: u32) -> [f32; 3] {
    [16, 8, 0].map(|shift| f32::from(((color >> shift) & 0xff) as u8) / 255.0)
}

/// Hue in degrees, saturation, lightness.
#[cfg(test)]
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

/// Length of the run a progress fill covers, as a share of the perimeter:
/// the fraction clamped to `0..=1`, with NaN treated as nothing done.
pub(crate) fn fill_extent(fraction: f32) -> f32 {
    if fraction.is_nan() {
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
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

/// A tab's state border: the base border in the state colour (or, with a
/// task list, a dim track under a bright fill running clockwise from the
/// top-left corner), then either the comet at `head` or, with reduced
/// motion, a steady glow.
pub(crate) fn paint_status_border(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    radius: f32,
    art: BorderArt,
) {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if width < 2.0 * BORDER_WIDTH || height < 2.0 * BORDER_WIDTH {
        return;
    }
    let inset = BORDER_WIDTH / 2.0;
    let ring = perimeter_run(width, height, radius, inset, 0.0, 1.0);
    match art.fill {
        Some(fraction) => {
            stroke_polyline(
                window,
                bounds.origin,
                &ring,
                true,
                BORDER_WIDTH,
                color(art.color, FILL_TRACK_ALPHA),
            );
            let extent = fill_extent(fraction);
            if extent > 0.0 {
                let fill = perimeter_run(width, height, radius, inset, 0.0, extent);
                stroke_polyline(
                    window,
                    bounds.origin,
                    &fill,
                    extent >= 1.0,
                    FILL_WIDTH,
                    color(art.color, 1.0),
                );
            }
        }
        None => stroke_polyline(
            window,
            bounds.origin,
            &ring,
            true,
            BORDER_WIDTH,
            color(art.color, art.base_alpha),
        ),
    }
    match art.motion {
        BorderMotion::Glow => {
            for (depth, alpha) in [(2.0, 0.35), (3.25, 0.15)] {
                let halo = perimeter_run(width, height, radius, inset + depth, 0.0, 1.0);
                stroke_polyline(
                    window,
                    bounds.origin,
                    &halo,
                    true,
                    BORDER_WIDTH,
                    color(art.color, alpha),
                );
            }
        }
        BorderMotion::Comet { head } => {
            // The tail fades in bands from faint to the bright head.
            let tint = comet_tint(art.color);
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
                    color(tint, alpha),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BorderMotion, COMET_PERIOD_SECS, DONE_COLOR, FrameRate, Motion, NEEDS_YOU_COLOR,
        WORKING_COLOR, border_motion, comet_head, comet_tint, fill_extent, perimeter_point,
        reduced_motion_recheck_due, rgb_to_hsl,
    };
    use std::time::{Duration, Instant};

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
    }

    /// Palette A: the three state colours are far apart in hue, and none of
    /// them sits in the red/amber/orange band that reads as a warning.
    #[test]
    fn the_state_colours_are_distinct_and_none_is_red_amber_or_orange() {
        let hues = [WORKING_COLOR, NEEDS_YOU_COLOR, DONE_COLOR].map(|color| {
            let (hue, saturation, _) = rgb_to_hsl(color);
            assert!(saturation > 0.4, "{color:06x} is a clear colour");
            assert!(
                (60.0..=330.0).contains(&hue),
                "{color:06x} at {hue}° is in the red–orange–amber band"
            );
            hue
        });
        for (index, first) in hues.iter().enumerate() {
            for second in &hues[index + 1..] {
                let gap = (first - second).abs().min(360.0 - (first - second).abs());
                assert!(gap > 60.0, "{first}° and {second}° are too close");
            }
        }
    }

    #[test]
    fn the_comet_is_a_lighter_tint_of_its_state_colour() {
        for color in [WORKING_COLOR, NEEDS_YOU_COLOR, DONE_COLOR] {
            let (hue, _, lightness) = rgb_to_hsl(color);
            let (tint_hue, _, tint_lightness) = rgb_to_hsl(comet_tint(color));
            assert!(tint_lightness > lightness, "{color:06x} tint is lighter");
            assert!((tint_hue - hue).abs() < 8.0, "{color:06x} keeps its hue");
        }
    }

    #[test]
    fn the_progress_fill_covers_the_done_share_of_the_border() {
        assert!(fill_extent(0.0).abs() < f32::EPSILON);
        assert!((fill_extent(0.5) - 0.5).abs() < f32::EPSILON);
        assert!((fill_extent(1.0) - 1.0).abs() < f32::EPSILON);
        assert!((fill_extent(7.0) - 1.0).abs() < f32::EPSILON, "clamped");
        assert!(fill_extent(-1.0).abs() < f32::EPSILON, "clamped");
        assert!(
            fill_extent(f32::NAN).abs() < f32::EPSILON,
            "NaN is nothing done"
        );
        // Half the tasks cover half the perimeter: from the top-left start to
        // the point diametrically opposite on a symmetric box.
        let (width, height, radius) = (100.0, 20.0, 4.0);
        let start = perimeter_point(width, height, radius, 0.0);
        let half = perimeter_point(width, height, radius, fill_extent(0.5));
        assert!(close(half, (width - start.0, height)), "{half:?}");
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
    fn reduced_motion_stills_every_border() {
        assert_eq!(border_motion(true, 0.7), BorderMotion::Glow);
        assert!(matches!(
            border_motion(false, 0.7),
            BorderMotion::Comet { head } if (head - comet_head(0.7)).abs() < 1e-6
        ));
    }

    #[test]
    fn only_the_needs_you_comet_runs_at_thirty_fps_and_the_calm_ones_at_twelve() {
        assert_eq!(FrameRate::Still.interval(), None);
        assert_eq!(FrameRate::Calm.interval(), Some(Duration::from_millis(83)));
        assert_eq!(FrameRate::Comet.interval(), Some(Duration::from_millis(33)));

        let motion = Motion::new();
        motion.begin_frame();
        assert_eq!(
            motion.wanted_interval(),
            None,
            "a still frame wants no timer"
        );
        motion.request_frames(FrameRate::Calm);
        assert_eq!(motion.wanted_interval(), FrameRate::Calm.interval());
        motion.request_frames(FrameRate::Comet);
        motion.request_frames(FrameRate::Calm);
        assert_eq!(
            motion.wanted_interval(),
            FrameRate::Comet.interval(),
            "the fastest animation on screen sets the pace"
        );
    }

    #[test]
    fn the_tick_stops_when_the_window_stops_drawing_frames() {
        let mut motion = Motion::new();
        let now = Instant::now();
        motion.begin_frame();
        motion.request_frames(FrameRate::Calm);
        assert_eq!(motion.next_tick(now), FrameRate::Calm.interval());
        // Hidden or minimised: gpui draws no frame after the redraw request.
        assert_eq!(motion.next_tick(now), None);

        // Shown again: the pending redraw draws a frame that restarts it, and
        // a frame with nothing moving stops it.
        motion.begin_frame();
        motion.request_frames(FrameRate::Comet);
        assert_eq!(motion.next_tick(now), FrameRate::Comet.interval());
        motion.begin_frame();
        assert_eq!(motion.next_tick(now), None);
    }

    #[test]
    fn reduce_motion_is_rechecked_at_most_once_a_second() {
        let read = Instant::now();
        assert!(reduced_motion_recheck_due(None, read), "never read yet");
        assert!(!reduced_motion_recheck_due(Some(read), read));
        assert!(!reduced_motion_recheck_due(
            Some(read),
            read + Duration::from_millis(999)
        ));
        assert!(reduced_motion_recheck_due(
            Some(read),
            read + Duration::from_secs(1)
        ));
    }
}
