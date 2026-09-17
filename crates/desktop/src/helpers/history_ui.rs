use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px, rgb};

use crate::THEME;
use hh_protocol::{HistoryClearScope, HistoryWarning};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LiveScrollTarget {
    TerminalMouseReporting,
    LiveBuffer,
}

pub(crate) const fn live_scroll_target(
    mouse_reporting: bool,
    shift_held: bool,
    _at_live_top: bool,
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

pub(crate) fn history_label(label: &'static str) -> AnyElement {
    div()
        .w(px(76.0))
        .font_family(".SystemUIFont")
        .text_xs()
        .text_color(rgb(THEME.muted))
        .child(label)
        .into_any_element()
}

pub(crate) fn history_scope_key(scope: HistoryClearScope) -> usize {
    match scope {
        HistoryClearScope::Terminal { .. } => 0,
        HistoryClearScope::Workspace { .. } => 1,
        HistoryClearScope::All => 2,
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    const KIB: u64 = 1024;
    if bytes >= GIB {
        format!("{}.{} GiB", bytes / GIB, (bytes % GIB) * 10 / GIB)
    } else if bytes >= MIB {
        format!("{}.{} MiB", bytes / MIB, (bytes % MIB) * 10 / MIB)
    } else if bytes >= KIB {
        format!("{}.{} KiB", bytes / KIB, (bytes % KIB) * 10 / KIB)
    } else {
        format!("{bytes} B")
    }
}

pub(crate) fn format_history_date(milliseconds: u64) -> String {
    let days = i64::try_from(milliseconds / 1_000 / 86_400).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

pub(crate) fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

pub(crate) fn history_warning_text(
    warning: Option<HistoryWarning>,
    dropped_bytes: u64,
) -> Option<String> {
    match warning {
    Some(HistoryWarning::ApproachingCapacity) => Some(
        "Archive is nearing its quota. Increase the limit or clear selected history before it fills."
            .to_owned(),
    ),
    Some(HistoryWarning::PausedAtCapacity) => Some(format!(
        "Archive is full and paused; the terminal is still live. {} could not be archived. Increase the quota or clear selected history.",
        format_bytes(dropped_bytes)
    )),
    Some(HistoryWarning::QueueOverflow) => Some(format!(
        "The storage queue could not keep up; {} is marked as an archive gap. Terminal input and output continued normally.",
        format_bytes(dropped_bytes)
    )),
    Some(HistoryWarning::CorruptChunk) => Some(
        "A local archive chunk failed integrity checks. It is shown as a gap; other chunks remain available."
            .to_owned(),
    ),
    None => None,
}
}

#[cfg(test)]
mod tests {
    use super::{LiveScrollTarget, format_bytes, live_scroll_target, wheel_delta_lines};

    #[test]
    fn byte_sizes_use_the_largest_meaningful_binary_unit() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1_536), "1.5 KiB");
        assert_eq!(format_bytes(1_572_864), "1.5 MiB");
        assert_eq!(format_bytes(1_610_612_736), "1.5 GiB");
    }

    #[test]
    fn scrolling_past_live_history_never_enters_the_archive_implicitly() {
        assert_eq!(
            live_scroll_target(false, false, true),
            LiveScrollTarget::LiveBuffer
        );
        assert_eq!(
            live_scroll_target(true, false, true),
            LiveScrollTarget::TerminalMouseReporting
        );
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
