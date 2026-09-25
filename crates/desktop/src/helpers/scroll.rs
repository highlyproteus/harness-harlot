//! Terminal wheel routing and line conversion.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LiveScrollTarget {
    TerminalMouseReporting,
    LiveBuffer,
}

pub(crate) const fn live_scroll_target(
    mouse_reporting: bool,
    shift_held: bool,
) -> LiveScrollTarget {
    if mouse_reporting && !shift_held {
        LiveScrollTarget::TerminalMouseReporting
    } else {
        LiveScrollTarget::LiveBuffer
    }
}

/// Converts wheel motion into whole terminal lines. Precise deltas accumulate
/// sub-line motion; mouse notches scroll at least one line. A new gesture
/// resets the residual so it cannot inherit motion from the previous one.
pub(crate) fn wheel_delta_lines(
    delta: gpui::ScrollDelta,
    phase: gpui::TouchPhase,
    line_height: f32,
    residual: &mut f32,
) -> i32 {
    if matches!(phase, gpui::TouchPhase::Started) {
        *residual = 0.0;
    }
    match delta {
        gpui::ScrollDelta::Pixels(pixels) => {
            if line_height <= 0.0 {
                return 0;
            }
            *residual += f32::from(pixels.y) / line_height;
            let whole = residual.trunc();
            *residual -= whole;
            whole as i32
        }
        gpui::ScrollDelta::Lines(lines) => {
            *residual = 0.0;
            let rounded = lines.y.round() as i32;
            if rounded == 0 && lines.y != 0.0 {
                if lines.y < 0.0 { -1 } else { 1 }
            } else {
                rounded
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LiveScrollTarget, live_scroll_target, wheel_delta_lines};

    #[test]
    fn wheel_goes_to_mouse_reporting_programs_unless_shift_is_held() {
        assert_eq!(
            live_scroll_target(false, false),
            LiveScrollTarget::LiveBuffer
        );
        assert_eq!(
            live_scroll_target(true, false),
            LiveScrollTarget::TerminalMouseReporting
        );
        assert_eq!(live_scroll_target(true, true), LiveScrollTarget::LiveBuffer);
    }

    #[test]
    fn wheel_delta_lines_accumulates_precise_motion() {
        use gpui::{ScrollDelta, TouchPhase, point, px};
        let mut residual = 0.0;
        let total: i32 = (0..10)
            .map(|_| {
                wheel_delta_lines(
                    ScrollDelta::Pixels(point(px(0.0), px(4.0))),
                    TouchPhase::Moved,
                    18.0,
                    &mut residual,
                )
            })
            .sum();
        assert_eq!(total, 2);
        assert!((residual - 2.0 / 9.0).abs() < 0.0001);
        residual = 0.0;
        assert_eq!(
            wheel_delta_lines(
                ScrollDelta::Pixels(point(px(0.0), px(-40.0))),
                TouchPhase::Moved,
                18.0,
                &mut residual,
            ),
            -2
        );
        assert!((residual + 2.0 / 9.0).abs() < 0.0001);
    }

    #[test]
    fn wheel_delta_lines_preserves_notches_and_resets_gestures() {
        use gpui::{ScrollDelta, TouchPhase, point, px};
        let mut residual = 0.9;
        assert_eq!(
            wheel_delta_lines(
                ScrollDelta::Lines(point(0.0, 1.0)),
                TouchPhase::Moved,
                18.0,
                &mut residual,
            ),
            1
        );
        assert!(residual.abs() < f32::EPSILON);
        assert_eq!(
            wheel_delta_lines(
                ScrollDelta::Lines(point(0.0, 0.3)),
                TouchPhase::Moved,
                18.0,
                &mut residual,
            ),
            1
        );
        residual = 0.9;
        assert_eq!(
            wheel_delta_lines(
                ScrollDelta::Pixels(point(px(0.0), px(2.0))),
                TouchPhase::Started,
                18.0,
                &mut residual,
            ),
            0
        );
        assert!((residual - 1.0 / 9.0).abs() < 0.0001);
    }
}
