use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px, rgb};

use crate::THEME;
use hh_protocol::{HistoryClearScope, HistoryWarning};

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
    use super::format_bytes;

    #[test]
    fn byte_sizes_use_the_largest_meaningful_binary_unit() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1_536), "1.5 KiB");
        assert_eq!(format_bytes(1_572_864), "1.5 MiB");
        assert_eq!(format_bytes(1_610_612_736), "1.5 GiB");
    }
}
