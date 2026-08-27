use crate::helpers::split_control_id;
use crate::typography;

use hh_protocol::{AppearanceColor, PaneLayout, SplitAxis};
use std::collections::HashMap;

use crate::helpers::terminal_grid_for_pane;
use crate::view_models::{PixelRect, SplitControlId};
use crate::{
    APP_CHROME_HEIGHT, DEFAULT_SIDEBAR_WIDTH, DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH,
    MIN_PANE_HEIGHT, MIN_PANE_WIDTH, MIN_SIDEBAR_WIDTH, MIN_TERMINAL_AREA_WIDTH,
    SPLIT_DIVIDER_SIZE, WORKSTATION_BANNER_ASPECT_RATIO, WORKSTATION_BANNER_MAX_HEIGHT,
    WORKSTATION_BANNER_MIN_HEIGHT, development_build,
};
use uuid::Uuid;

pub(crate) fn default_sidebar_width() -> f32 {
    default_sidebar_width_for(development_build())
}

pub(crate) const fn default_sidebar_width_for(development: bool) -> f32 {
    if development {
        DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH
    } else {
        DEFAULT_SIDEBAR_WIDTH
    }
}

/// Restore only the short-lived 104 px Dev migration introduced by the
/// compact-rail experiment. Any other persisted width is user-resized data.
pub(crate) fn migrated_sidebar_width(stored_width: Option<f32>) -> f32 {
    migrated_sidebar_width_for(stored_width, development_build())
}

pub(crate) fn migrated_sidebar_width_for(stored_width: Option<f32>, development: bool) -> f32 {
    if development && stored_width.is_some_and(|width| (width - 104.0).abs() < f32::EPSILON) {
        DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH
    } else {
        stored_width.unwrap_or_else(|| default_sidebar_width_for(development))
    }
}

pub(crate) fn constrained_sidebar_width(preferred_width: f32, window_width: f32) -> f32 {
    let preferred_width = if preferred_width.is_finite() {
        preferred_width
    } else {
        default_sidebar_width()
    };
    let window_width = if window_width.is_finite() {
        window_width
    } else {
        MIN_TERMINAL_AREA_WIDTH + default_sidebar_width()
    };
    let maximum_for_window =
        (window_width - MIN_TERMINAL_AREA_WIDTH).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    preferred_width.clamp(MIN_SIDEBAR_WIDTH, maximum_for_window)
}

pub(crate) fn sidebar_width_for_visibility(
    preferred_width: f32,
    window_width: f32,
    visible: bool,
) -> f32 {
    if visible {
        constrained_sidebar_width(preferred_width, window_width)
    } else {
        0.0
    }
}

pub(crate) fn workspace_pixel_size(
    window_width: f32,
    window_height: f32,
    sidebar_width: f32,
) -> (f32, f32) {
    (
        (window_width - sidebar_width).max(1.0),
        (window_height - APP_CHROME_HEIGHT).max(1.0),
    )
}

pub(crate) const fn rgba_with_alpha(color: u32, alpha: u8) -> u32 {
    (color << 8) | alpha as u32
}

pub(crate) fn composite_rgb(foreground: u32, background: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let inverse = 255 - alpha;
    let channel = |shift| {
        let foreground = (foreground >> shift) & 0xff_u32;
        let background = (background >> shift) & 0xff_u32;
        (foreground * alpha + background * inverse + 127_u32) / 255_u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

pub(crate) fn readable_text_color(background: u32) -> u32 {
    let red = (background >> 16) & 0xff;
    let green = (background >> 8) & 0xff;
    let blue = background & 0xff;
    if red * 299 + green * 587 + blue * 114 > 150_000 {
        0x111318
    } else {
        0xffffff
    }
}

pub(crate) fn parse_hex_color(value: &str) -> Option<AppearanceColor> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let rgb = u32::from_str_radix(value, 16).ok()?;
    Some(AppearanceColor::new(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    ))
}
/// Converts HSV channels to an RGB value encoded as `0xRRGGBB`.
///
/// Hue is clamped to `0..=360`; saturation and value are clamped to `0..=1`.
pub(crate) fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> u32 {
    let hue = hue.clamp(0.0, 360.0) / 60.0;
    let saturation = saturation.clamp(0.0, 1.0);
    let value = value.clamp(0.0, 1.0);
    let sector = hue.floor().rem_euclid(6.0);
    let fraction = hue - hue.floor();
    let p = value * (1.0 - saturation);
    let q = value * (1.0 - saturation * fraction);
    let t = value * (1.0 - saturation * (1.0 - fraction));
    let (red, green, blue) = if sector < 1.0 {
        (value, t, p)
    } else if sector < 2.0 {
        (q, value, p)
    } else if sector < 3.0 {
        (p, value, t)
    } else if sector < 4.0 {
        (p, q, value)
    } else if sector < 5.0 {
        (t, p, value)
    } else {
        (value, p, q)
    };
    let channel = |component: f32| (component * 255.0 + 0.5) as u32;
    (channel(red) << 16) | (channel(green) << 8) | channel(blue)
}

/// Converts an RGB value encoded as `0xRRGGBB` to HSV channels.
///
/// Hue is in `0..=360`; saturation and value are in `0..=1`. Gray inputs use
/// hue zero.
pub(crate) fn rgb_to_hsv(rgb: u32) -> (f32, f32, f32) {
    let red_channel = u8::try_from((rgb >> 16) & 0xff).unwrap_or_default();
    let green_channel = u8::try_from((rgb >> 8) & 0xff).unwrap_or_default();
    let blue_channel = u8::try_from(rgb & 0xff).unwrap_or_default();
    let red = f32::from(red_channel) / 255.0;
    let green = f32::from(green_channel) / 255.0;
    let blue = f32::from(blue_channel) / 255.0;
    let maximum_channel = red_channel.max(green_channel).max(blue_channel);
    let maximum = red.max(green).max(blue);
    let minimum = red.min(green).min(blue);
    let delta = maximum - minimum;
    if maximum_channel == red_channel.min(green_channel).min(blue_channel) {
        return (0.0, 0.0, maximum);
    }

    let hue = if maximum_channel == red_channel {
        60.0 * ((green - blue) / delta).rem_euclid(6.0)
    } else if maximum_channel == green_channel {
        60.0 * ((blue - red) / delta + 2.0)
    } else {
        60.0 * ((red - green) / delta + 4.0)
    };
    let saturation = if maximum_channel == 0 {
        0.0
    } else {
        delta / maximum
    };
    (hue, saturation, maximum)
}

pub(crate) fn effective_split_ratio(axis: SplitAxis, width: f32, height: f32, ratio: f32) -> f32 {
    let extent = match axis {
        SplitAxis::Horizontal => width,
        SplitAxis::Vertical => height,
    }
    .max(1.0);
    let minimum = match axis {
        SplitAxis::Horizontal => MIN_PANE_WIDTH,
        SplitAxis::Vertical => MIN_PANE_HEIGHT,
    };
    if extent < minimum * 2.0 + SPLIT_DIVIDER_SIZE {
        return 0.5;
    }
    let low = minimum / extent;
    let high = (extent - SPLIT_DIVIDER_SIZE - minimum) / extent;
    ratio.clamp(low, high)
}

pub(crate) fn split_child_dimensions(
    axis: SplitAxis,
    width: f32,
    height: f32,
    ratio: f32,
) -> (f32, f32, f32, f32) {
    match axis {
        SplitAxis::Horizontal => {
            let first_width = (width * ratio).floor().max(1.0);
            let second_width = (width - first_width - SPLIT_DIVIDER_SIZE).max(1.0);
            (first_width, height, second_width, height)
        }
        SplitAxis::Vertical => {
            let first_height = (height * ratio).floor().max(1.0);
            let second_height = (height - first_height - SPLIT_DIVIDER_SIZE).max(1.0);
            (width, first_height, width, second_height)
        }
    }
}

pub(crate) fn find_split_rect(
    layout: &PaneLayout,
    target_split_id: SplitControlId,
    rect: PixelRect,
    ratios: &HashMap<SplitControlId, f32>,
) -> Option<PixelRect> {
    let PaneLayout::Split {
        axis,
        ratio,
        first,
        second,
    } = layout
    else {
        return None;
    };
    let split_id = split_control_id(first, second);
    if split_id == target_split_id {
        return Some(rect);
    }
    let ratio = effective_split_ratio(
        *axis,
        rect.width,
        rect.height,
        ratios.get(&split_id).copied().unwrap_or(*ratio),
    );
    let (first_width, first_height, second_width, second_height) =
        split_child_dimensions(*axis, rect.width, rect.height, ratio);
    let first_rect = PixelRect {
        width: first_width,
        height: first_height,
        ..rect
    };
    let second_rect = match axis {
        SplitAxis::Horizontal => PixelRect {
            x: rect.x + first_width + SPLIT_DIVIDER_SIZE,
            y: rect.y,
            width: second_width,
            height: second_height,
        },
        SplitAxis::Vertical => PixelRect {
            x: rect.x,
            y: rect.y + first_height + SPLIT_DIVIDER_SIZE,
            width: second_width,
            height: second_height,
        },
    };
    find_split_rect(first, target_split_id, first_rect, ratios)
        .or_else(|| find_split_rect(second, target_split_id, second_rect, ratios))
}

pub(crate) fn collect_pane_sizes(
    layout: &PaneLayout,
    width: f32,
    height: f32,
    metrics_for_pane: &impl Fn(Uuid) -> typography::TerminalCellMetrics,
    ratios: &HashMap<SplitControlId, f32>,
    show_root_header: bool,
    output: &mut Vec<(Uuid, u16, u16)>,
) {
    match layout {
        PaneLayout::Leaf { pane } => {
            let (columns, rows) =
                terminal_grid_for_pane(width, height, metrics_for_pane(pane.id), show_root_header);
            output.push((pane.id, columns, rows));
        }
        PaneLayout::Stack { active, .. } => {
            let (columns, rows) =
                terminal_grid_for_pane(width, height, metrics_for_pane(*active), show_root_header);
            output.push((*active, columns, rows));
        }
        PaneLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = effective_split_ratio(
                *axis,
                width,
                height,
                ratios
                    .get(&split_control_id(first, second))
                    .copied()
                    .unwrap_or(*ratio),
            );
            let (first_width, first_height, second_width, second_height) =
                split_child_dimensions(*axis, width, height, ratio);
            collect_pane_sizes(
                first,
                first_width,
                first_height,
                metrics_for_pane,
                ratios,
                true,
                output,
            );
            collect_pane_sizes(
                second,
                second_width,
                second_height,
                metrics_for_pane,
                ratios,
                true,
                output,
            );
        }
    }
}

/// Degenerate aspect ratios (zero, negative, non-finite) fall back to the
/// bundled 3:1 artwork shape rather than producing a zero-height header.
pub(crate) fn sanitized_banner_aspect_ratio(aspect_ratio: f32) -> f32 {
    if aspect_ratio.is_finite() && aspect_ratio > 0.0 {
        aspect_ratio
    } else {
        WORKSTATION_BANNER_ASPECT_RATIO
    }
}

/// Rail-header height that matches the banner's own aspect at the current rail
/// width, clamped so no image shape can collapse or dominate the rail.
pub(crate) fn workstation_banner_header_height(
    sidebar_content_width: f32,
    aspect_ratio: f32,
) -> f32 {
    (sidebar_content_width.max(0.0) / sanitized_banner_aspect_ratio(aspect_ratio))
        .clamp(WORKSTATION_BANNER_MIN_HEIGHT, WORKSTATION_BANNER_MAX_HEIGHT)
}

/// Largest aspect-preserving size that fits the given box. Both render sites
/// pass the result as explicit pixel width/height on the image element, so no
/// percentage sizing or layout-injected aspect ratio can crop the artwork.
pub(crate) fn banner_fit_size(
    available_width: f32,
    available_height: f32,
    aspect_ratio: f32,
) -> (f32, f32) {
    let aspect_ratio = sanitized_banner_aspect_ratio(aspect_ratio);
    let available_width = available_width.max(0.0);
    let available_height = available_height.max(0.0);
    if available_width / aspect_ratio <= available_height {
        (available_width, available_width / aspect_ratio)
    } else {
        (available_height * aspect_ratio, available_height)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppearanceColor, DEFAULT_SIDEBAR_WIDTH, DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH, HashMap,
        MAX_SIDEBAR_WIDTH, MIN_PANE_WIDTH, MIN_SIDEBAR_WIDTH, MIN_TERMINAL_AREA_WIDTH, PaneLayout,
        SPLIT_DIVIDER_SIZE, SplitAxis, Uuid, WORKSTATION_BANNER_ASPECT_RATIO,
        WORKSTATION_BANNER_MAX_HEIGHT, WORKSTATION_BANNER_MIN_HEIGHT, banner_fit_size,
        collect_pane_sizes, composite_rgb, constrained_sidebar_width, default_sidebar_width,
        default_sidebar_width_for, effective_split_ratio, hsv_to_rgb, migrated_sidebar_width_for,
        parse_hex_color, rgb_to_hsv, rgba_with_alpha, sanitized_banner_aspect_ratio,
        sidebar_width_for_visibility, split_child_dimensions, typography, workspace_pixel_size,
        workstation_banner_header_height,
    };

    use hh_protocol::Pane;

    #[test]
    fn color_picker_accepts_exact_hex_and_rejects_partial_or_non_hex_input() {
        assert_eq!(
            parse_hex_color("#67C8C6"),
            Some(AppearanceColor::new(0x67, 0xc8, 0xc6))
        );
        assert_eq!(
            parse_hex_color("62adff"),
            Some(AppearanceColor::HARBOR_BLUE)
        );
        assert_eq!(parse_hex_color("FFF"), None);
        assert_eq!(parse_hex_color("GGADFF"), None);
    }

    #[test]
    fn hsv_conversion_covers_primary_and_achromatic_colors() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), 0xff0000);
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), 0x00ff00);
        assert_eq!(hsv_to_rgb(0.0, 0.0, 1.0), 0xffffff);
        assert_eq!(hsv_to_rgb(0.0, 0.0, 0.0), 0x000000);
    }

    #[test]
    fn hsv_conversion_round_trips_rgb_and_uses_zero_hue_for_gray() {
        let (hue, saturation, value) = rgb_to_hsv(0x67c8c6);
        assert_eq!(hsv_to_rgb(hue, saturation, value), 0x67c8c6);
        assert_eq!(rgb_to_hsv(0x808080), (0.0, 0.0, 128.0 / 255.0));
    }

    #[test]
    fn alpha_color_helpers_encode_and_composite_exact_channels() {
        assert_eq!(rgba_with_alpha(0x3b424f, 0xd0), 0x3b424fd0);
        assert_eq!(composite_rgb(0xffffff, 0x000000, 0x80), 0x808080);
        assert_eq!(composite_rgb(0x3b424f, 0x15171c, 0xff), 0x3b424f);
    }

    #[test]
    fn pane_geometry_tracks_narrow_medium_and_wide_windows_without_fixed_columns() {
        let pane = Pane {
            id: Uuid::from_u128(10),
            kind: hh_protocol::PaneKind::Terminal,
            title: "Terminal 1".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let layout = PaneLayout::Leaf { pane };
        let metrics = typography::TerminalCellMetrics {
            font_size: 13.5,
            cell_width: 8.0,
            ascent: 10.0,
            descent: 3.0,
            baseline: 13.0,
            line_height: 19.0,
        };
        let ratios = HashMap::new();

        let dimensions = [(720.0, 460.0), (1280.0, 820.0), (1800.0, 1000.0)]
            .into_iter()
            .map(|(window_width, window_height)| {
                let workspace =
                    workspace_pixel_size(window_width, window_height, DEFAULT_SIDEBAR_WIDTH);
                let mut sizes = Vec::new();
                collect_pane_sizes(
                    &layout,
                    workspace.0,
                    workspace.1,
                    &|_| metrics,
                    &ratios,
                    false,
                    &mut sizes,
                );
                sizes[0]
            })
            .collect::<Vec<_>>();

        assert_eq!(dimensions[0], (Uuid::from_u128(10), 69, 19));
        assert_eq!(dimensions[1], (Uuid::from_u128(10), 139, 38));
        assert_eq!(dimensions[2], (Uuid::from_u128(10), 204, 48));
        assert!(
            dimensions
                .windows(2)
                .all(|pair| { pair[0].1 < pair[1].1 && pair[0].2 < pair[1].2 })
        );
    }

    #[test]
    fn workspace_geometry_excludes_both_navigation_rows() {
        assert_eq!(
            workspace_pixel_size(1280.0, 820.0, DEFAULT_SIDEBAR_WIDTH),
            (1136.0, 750.0),
            "PTY sizing must exclude the 38 px global navigation and 32 px workspace tab strip"
        );
    }

    #[test]
    fn sidebar_width_is_bounded_without_forgetting_a_wider_preference() {
        assert!((constrained_sidebar_width(80.0, 1280.0) - MIN_SIDEBAR_WIDTH).abs() < 0.0001);
        assert!((constrained_sidebar_width(900.0, 1280.0) - 420.0).abs() < 0.0001);

        let preferred = 390.0;
        let compact = constrained_sidebar_width(preferred, 640.0);
        assert!((compact - 320.0).abs() < 0.0001);
        assert!((workspace_pixel_size(640.0, 460.0, compact).0 - 320.0).abs() < 0.0001);
        assert!((constrained_sidebar_width(preferred, 1280.0) - preferred).abs() < 0.0001);

        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                (constrained_sidebar_width(preferred, invalid) - default_sidebar_width()).abs()
                    < 0.0001
            );
        }
    }

    #[test]
    fn development_sidebar_restores_the_normal_width_after_the_compact_experiment() {
        assert!(
            (default_sidebar_width_for(true) - DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH).abs()
                < f32::EPSILON
        );
        assert!(
            (migrated_sidebar_width_for(Some(104.0), true) - DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH)
                .abs()
                < f32::EPSILON
        );
        assert!((migrated_sidebar_width_for(Some(356.0), true) - 356.0).abs() < f32::EPSILON);
        assert!(
            (migrated_sidebar_width_for(None, false) - DEFAULT_SIDEBAR_WIDTH).abs() < f32::EPSILON
        );
    }

    #[test]
    fn workstation_banner_header_matches_the_artwork_aspect_at_every_rail_width() {
        for sidebar_content_width in [136.0_f32, 217.0, 412.0] {
            let height = workstation_banner_header_height(sidebar_content_width, 3.0);
            assert!((height - sidebar_content_width / 3.0).abs() < 0.0001);
            let (width, fitted_height) = banner_fit_size(sidebar_content_width, height, 3.0);
            assert!((width - sidebar_content_width).abs() < 0.0001);
            assert!((fitted_height - height).abs() < 0.0001);
        }
    }

    #[test]
    fn tall_and_wide_banners_stay_whole_inside_the_clamped_header() {
        // Square artwork is capped, and still fits whole inside the capped box.
        let square_height = workstation_banner_header_height(412.0, 1.0);
        assert!((square_height - WORKSTATION_BANNER_MAX_HEIGHT).abs() < f32::EPSILON);
        let (width, height) = banner_fit_size(412.0, square_height, 1.0);
        assert!((width - WORKSTATION_BANNER_MAX_HEIGHT).abs() < 0.0001);
        assert!((height - WORKSTATION_BANNER_MAX_HEIGHT).abs() < 0.0001);
        assert!(width <= 412.0);

        // A 12:1 banner is floored to the minimum height and letterboxes vertically.
        let wide_height = workstation_banner_header_height(136.0, 12.0);
        assert!((wide_height - WORKSTATION_BANNER_MIN_HEIGHT).abs() < f32::EPSILON);
        let (wide_width, wide_fitted) = banner_fit_size(136.0, wide_height, 12.0);
        assert!((wide_width - 136.0).abs() < 0.0001);
        assert!(wide_fitted < wide_height);
    }

    #[test]
    fn degenerate_banner_aspect_falls_back_to_the_bundled_shape() {
        for aspect_ratio in [0.0_f32, -2.0, f32::NAN, f32::INFINITY] {
            assert!(
                (sanitized_banner_aspect_ratio(aspect_ratio) - WORKSTATION_BANNER_ASPECT_RATIO)
                    .abs()
                    < f32::EPSILON
            );
            let height = workstation_banner_header_height(217.0, aspect_ratio);
            assert!((height - 217.0 / WORKSTATION_BANNER_ASPECT_RATIO).abs() < 0.0001);
        }
    }

    #[test]
    fn hidden_sidebar_gives_the_workspace_the_full_window_width() {
        let visible = sidebar_width_for_visibility(260.0, 1280.0, true);
        let hidden = sidebar_width_for_visibility(260.0, 1280.0, false);

        assert!((visible - 260.0).abs() < f32::EPSILON);
        assert!(hidden.abs() < f32::EPSILON);
        assert!((workspace_pixel_size(1280.0, 820.0, hidden).0 - 1280.0).abs() < f32::EPSILON);
    }

    #[test]
    fn widest_sidebar_still_leaves_two_constrained_split_panes_in_the_minimum_window() {
        let sidebar = constrained_sidebar_width(MAX_SIDEBAR_WIDTH, 720.0);
        let workspace = workspace_pixel_size(720.0, 460.0, sidebar);
        let ratio = effective_split_ratio(SplitAxis::Horizontal, workspace.0, workspace.1, 0.95);
        let (first_width, _, second_width, _) =
            split_child_dimensions(SplitAxis::Horizontal, workspace.0, workspace.1, ratio);

        assert!((sidebar - 400.0).abs() < 0.0001);
        assert!((workspace.0 - MIN_TERMINAL_AREA_WIDTH).abs() < 0.0001);
        assert!(first_width >= MIN_PANE_WIDTH);
        assert!(second_width >= MIN_PANE_WIDTH);
    }

    #[test]
    fn split_geometry_accounts_for_the_divider_and_each_panes_chrome() {
        let first = Pane {
            id: Uuid::from_u128(21),
            kind: hh_protocol::PaneKind::Terminal,
            title: "Terminal 1".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let second = Pane {
            id: Uuid::from_u128(22),
            kind: hh_protocol::PaneKind::Terminal,
            title: "Terminal 2".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf {
                pane: first.clone(),
            }),
            second: Box::new(PaneLayout::Leaf { pane: second }),
        };
        let metrics = typography::TerminalCellMetrics {
            font_size: 13.5,
            cell_width: 8.0,
            ascent: 10.0,
            descent: 3.0,
            baseline: 13.0,
            line_height: 19.0,
        };
        let workspace = workspace_pixel_size(1280.0, 820.0, DEFAULT_SIDEBAR_WIDTH);
        let mut sizes = Vec::new();
        collect_pane_sizes(
            &layout,
            workspace.0,
            workspace.1,
            &|_| metrics,
            &HashMap::new(),
            true,
            &mut sizes,
        );

        assert_eq!(
            sizes,
            vec![(first.id, 68, 37), (Uuid::from_u128(22), 68, 37)]
        );
        let used_pixel_width = 568.0 + SPLIT_DIVIDER_SIZE + 564.0;
        assert!((used_pixel_width - workspace.0).abs() < 0.0001);
    }

    #[test]
    fn pane_size_projection_uses_each_active_terminal_zoom() {
        let first = Pane {
            id: Uuid::from_u128(21),
            kind: hh_protocol::PaneKind::Terminal,
            title: "First".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let second = Pane {
            id: Uuid::from_u128(22),
            kind: hh_protocol::PaneKind::Terminal,
            title: "Second".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf {
                pane: first.clone(),
            }),
            second: Box::new(PaneLayout::Leaf {
                pane: second.clone(),
            }),
        };
        let default_metrics = typography::TerminalCellMetrics {
            font_size: 13.5,
            cell_width: 8.0,
            ascent: 10.0,
            descent: 3.0,
            baseline: 13.0,
            line_height: 19.0,
        };
        let zoomed_metrics = typography::TerminalCellMetrics {
            font_size: 27.0,
            cell_width: 16.0,
            ascent: 20.0,
            descent: 6.0,
            baseline: 26.0,
            line_height: 38.0,
        };
        let workspace = workspace_pixel_size(1280.0, 820.0, DEFAULT_SIDEBAR_WIDTH);
        let mut sizes = Vec::new();
        collect_pane_sizes(
            &layout,
            workspace.0,
            workspace.1,
            &|pane_id| {
                if pane_id == first.id {
                    zoomed_metrics
                } else {
                    default_metrics
                }
            },
            &HashMap::new(),
            true,
            &mut sizes,
        );

        assert_eq!(sizes[1], (second.id, 68, 37));
        assert!(sizes[0].1 < sizes[1].1);
        assert!(sizes[0].2 < sizes[1].2);
    }

    #[test]
    fn split_ratio_respects_practical_pane_constraints_at_each_window_size() {
        let narrow = effective_split_ratio(SplitAxis::Horizontal, 530.0, 422.0, 0.05);
        let wide = effective_split_ratio(SplitAxis::Horizontal, 1610.0, 962.0, 0.05);
        assert!((narrow - (MIN_PANE_WIDTH / 530.0)).abs() < 0.0001);
        assert!((wide - (MIN_PANE_WIDTH / 1610.0)).abs() < 0.0001);

        let too_short = effective_split_ratio(SplitAxis::Vertical, 530.0, 150.0, 0.9);
        assert!((too_short - 0.5).abs() < 0.0001);
    }
}
