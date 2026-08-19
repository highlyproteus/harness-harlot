use gpui::{Bounds, Pixels, Point};
use hh_protocol::{DropPlacement, Pane};
use std::time::Instant;
use uuid::Uuid;

use crate::PANE_HEADER_HEIGHT;

pub(crate) fn split_target_for_drag(source: Uuid, panes: &[Pane], active: Uuid) -> Option<Uuid> {
    let pane_ids = panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
    split_target_for_drag_ids(source, &pane_ids, active)
}

pub(crate) fn split_target_for_drag_ids(
    source: Uuid,
    pane_ids: &[Uuid],
    active: Uuid,
) -> Option<Uuid> {
    if source == active {
        pane_ids
            .iter()
            .copied()
            .find(|pane| *pane != source)
            .or_else(|| (pane_ids.len() == 1).then_some(active))
    } else {
        Some(active)
    }
}

pub(crate) fn split_placement_at(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
) -> Option<DropPlacement> {
    if !bounds.contains(&position) {
        return None;
    }
    let x = f32::from(position.x - bounds.origin.x);
    let y = f32::from(position.y - bounds.origin.y);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if y < PANE_HEADER_HEIGHT || width <= 0.0 || height <= PANE_HEADER_HEIGHT {
        return None;
    }
    if x <= width * 0.25 {
        Some(DropPlacement::Left)
    } else if x >= width * 0.75 {
        Some(DropPlacement::Right)
    } else if y - PANE_HEADER_HEIGHT <= (height - PANE_HEADER_HEIGHT) * 0.5 {
        Some(DropPlacement::Top)
    } else {
        Some(DropPlacement::Bottom)
    }
}

pub(crate) fn click_suppression_active(deadline: &mut Option<Instant>, now: Instant) -> bool {
    deadline.take().is_some_and(|deadline| now <= deadline)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeaderDropZone {
    Before,
    Into,
    After,
}

/// Middle half of a header row nests into it; top/bottom quarters reorder.
pub(crate) fn header_drop_zone(
    position_y: f32,
    bounds_top: f32,
    bounds_bottom: f32,
) -> HeaderDropZone {
    let height = (bounds_bottom - bounds_top).max(1.0);
    let fraction = ((position_y - bounds_top) / height).clamp(0.0, 1.0);
    if fraction < 0.25 {
        HeaderDropZone::Before
    } else if fraction > 0.75 {
        HeaderDropZone::After
    } else {
        HeaderDropZone::Into
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Bounds, DropPlacement, HeaderDropZone, Instant, click_suppression_active, header_drop_zone,
        split_placement_at,
    };
    use gpui::{point, px, size};
    use std::time::Duration;

    #[test]
    fn header_drop_zone_splits_quarter_half_quarter() {
        assert_eq!(header_drop_zone(10.0, 0.0, 100.0), HeaderDropZone::Before);
        assert_eq!(header_drop_zone(50.0, 0.0, 100.0), HeaderDropZone::Into);
        assert_eq!(header_drop_zone(90.0, 0.0, 100.0), HeaderDropZone::After);
        assert_eq!(header_drop_zone(25.0, 0.0, 100.0), HeaderDropZone::Into);
        assert_eq!(header_drop_zone(75.0, 0.0, 100.0), HeaderDropZone::Into);
    }

    #[test]
    fn expired_drag_suppression_never_eats_the_next_sidebar_click() {
        let now = Instant::now();
        let mut expired = Some(now.checked_sub(Duration::from_millis(1)).unwrap());
        assert!(!click_suppression_active(&mut expired, now));
        assert_eq!(expired, None);

        let mut immediate = Some(now + Duration::from_millis(1));
        assert!(click_suppression_active(&mut immediate, now));
        assert_eq!(immediate, None);
    }

    #[test]
    fn pointer_local_split_zones_exclude_the_tab_strip_and_cover_each_half() {
        let bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(100.0), px(100.0)),
        };

        assert_eq!(split_placement_at(point(px(50.0), px(10.0)), bounds), None);
        assert_eq!(
            split_placement_at(point(px(10.0), px(50.0)), bounds),
            Some(DropPlacement::Left)
        );
        assert_eq!(
            split_placement_at(point(px(90.0), px(50.0)), bounds),
            Some(DropPlacement::Right)
        );
        assert_eq!(
            split_placement_at(point(px(50.0), px(40.0)), bounds),
            Some(DropPlacement::Top)
        );
        assert_eq!(
            split_placement_at(point(px(50.0), px(90.0)), bounds),
            Some(DropPlacement::Bottom)
        );
        assert_eq!(split_placement_at(point(px(101.0), px(50.0)), bounds), None);
    }
}
