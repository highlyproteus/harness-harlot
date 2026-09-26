//! Chrome shared by every place a tab or terminal appears (top bar, pane
//! headers, sidebar rows, window ring chips, bot threads): one status
//! indicator slot and an always-visible close button.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, AppContext, Context, ElementId, InteractiveElement, IntoElement, MouseButton,
    ParentElement, StatefulInteractiveElement, Styled, div, px, rgb, rgba,
};
use hh_protocol::{Pane, PaneStatus};
use uuid::Uuid;

use crate::notifications::{ActivitySection, activity_section};
use crate::view_models::TooltipView;
use crate::{HhApp, THEME};

/// Side of the indicator slot; reserved even when empty so labels never shift.
const INDICATOR_SIZE: f32 = 10.0;
const STATUS_DOT_SIZE: f32 = 7.0;
const UNREAD_DOT_SIZE: f32 = 6.0;
/// The running dot fades in coarse steps: a frame-driven animation redraws the
/// whole app every frame (measured ~44% CPU in a debug build) for as long as
/// any agent works, while stepping costs a few redraws per second.
pub(crate) const PULSE_STEP: Duration = Duration::from_millis(250);
/// Steps in one fade out and back in: a slow two-second breath.
const PULSE_STEPS: u128 = 8;
/// The dimmest point of the fade, so a running dot never vanishes.
const PULSE_MIN_OPACITY: f32 = 0.25;
const CLOSE_BUTTON_SIZE: f32 = 16.0;

/// What a status slot shows. Ordered by urgency so aggregates over several
/// panes take the maximum.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PaneIndicator {
    /// Idle, or a finished pane already viewed: the slot stays empty.
    #[default]
    None,
    /// Finished (done or exited); on a tab only until the user views it.
    Done,
    Running,
    NeedsYou,
}

impl PaneIndicator {
    /// Symbol color: needs-you orange, running and done blue, else dim.
    pub(crate) const fn color(self) -> u32 {
        match self {
            Self::None => THEME.dim,
            Self::Done | Self::Running => THEME.accent,
            Self::NeedsYou => THEME.warning,
        }
    }
}

/// The one mapping from pane activity to a tab's indicator. An exited pane is
/// finished whatever its last status said; a finished pane keeps its blue dot
/// until the user views it (`unread`), then shows nothing.
pub(crate) const fn pane_indicator(
    status: PaneStatus,
    exited: bool,
    unread: bool,
) -> PaneIndicator {
    match notification_indicator(status, exited) {
        PaneIndicator::Done if !unread => PaneIndicator::None,
        indicator => indicator,
    }
}

/// A Notifications row's indicator: like [`pane_indicator`], but a finished
/// pane keeps its Done dot, since the row itself carries the unread mark.
pub(crate) const fn notification_indicator(status: PaneStatus, exited: bool) -> PaneIndicator {
    match activity_section(status, exited) {
        Some(ActivitySection::NeedsYou) => PaneIndicator::NeedsYou,
        Some(ActivitySection::Running) => PaneIndicator::Running,
        Some(ActivitySection::Done) => PaneIndicator::Done,
        None => PaneIndicator::None,
    }
}

/// The fixed-size status slot: a slowly pulsing blue dot while running, a
/// solid blue dot when done, an orange dot when the pane needs the user,
/// otherwise empty space.
pub(crate) fn render_pane_indicator(indicator: PaneIndicator) -> AnyElement {
    let slot = div()
        .flex_none()
        .w(px(INDICATOR_SIZE))
        .h(px(INDICATOR_SIZE))
        .flex()
        .items_center()
        .justify_center();
    match indicator {
        PaneIndicator::None => slot,
        PaneIndicator::Running => slot.child(
            div()
                .w(px(STATUS_DOT_SIZE))
                .h(px(STATUS_DOT_SIZE))
                .rounded_full()
                .bg(rgb(indicator.color()))
                .opacity(pulse_opacity()),
        ),
        PaneIndicator::NeedsYou | PaneIndicator::Done => slot.child(
            div()
                .w(px(STATUS_DOT_SIZE))
                .h(px(STATUS_DOT_SIZE))
                .rounded_full()
                .bg(rgb(indicator.color())),
        ),
    }
    .into_any_element()
}

/// The blue unread dot beside a Notifications row's status symbol; an empty
/// slot of the same size when read, so the symbols stay aligned.
pub(crate) fn render_unread_dot(unread: bool) -> AnyElement {
    let slot = div()
        .flex_none()
        .w(px(UNREAD_DOT_SIZE))
        .h(px(UNREAD_DOT_SIZE))
        .rounded_full();
    if unread {
        slot.bg(rgb(THEME.accent)).into_any_element()
    } else {
        slot.into_any_element()
    }
}

/// The running dot's opacity now, taken from the wall clock so every running
/// indicator pulses together without per-view state.
fn pulse_opacity() -> f32 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    pulse_opacity_at_step(elapsed / PULSE_STEP.as_millis())
}

/// Fades linearly from full to [`PULSE_MIN_OPACITY`] and back over
/// [`PULSE_STEPS`] steps.
fn pulse_opacity_at_step(step: u128) -> f32 {
    // Both values are below 8, so the conversions are exact.
    let phase = f32::from(u8::try_from(step % PULSE_STEPS).unwrap_or(0));
    let half = f32::from(u8::try_from(PULSE_STEPS / 2).unwrap_or(1));
    let brightness = (phase - half).abs() / half;
    PULSE_MIN_OPACITY + (1.0 - PULSE_MIN_OPACITY) * brightness
}

impl HhApp {
    /// Whether any live terminal is working, which is when the pulse ticker
    /// has to redraw.
    pub(crate) fn any_pane_running(&self) -> bool {
        self.session.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .any(|tab| {
                    let mut panes = Vec::new();
                    crate::helpers::collect_terminal_tabs(&tab.layout, &mut panes);
                    panes
                        .iter()
                        .any(|pane| self.pane_indicator(pane) == PaneIndicator::Running)
                })
        })
    }

    pub(crate) fn pane_exited(&self, pane_id: Uuid) -> bool {
        self.session
            .pane_states
            .get(&pane_id)
            .is_some_and(|state| state.exited)
    }

    pub(crate) fn pane_indicator(&self, pane: &Pane) -> PaneIndicator {
        pane_indicator(
            pane.status,
            self.pane_exited(pane.id),
            self.session.pane_views.is_unread(pane),
        )
    }

    /// The always-visible ×: dim until hovered. It swallows its own mouse
    /// downs so pressing it never selects, drags, or opens a menu on the row.
    pub(crate) fn render_close_button(
        &self,
        id: impl Into<ElementId>,
        color: u32,
        tooltip: String,
        on_close: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .flex_none()
            .w(px(CLOSE_BUTTON_SIZE))
            .h(px(CLOSE_BUTTON_SIZE))
            .rounded(px(3.0))
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(color))
            .opacity(0.55)
            .hover(|element| element.opacity(1.0).bg(rgba(0xffffff24)))
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: tooltip.clone(),
                })
                .into()
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                on_close(this, cx);
                cx.stop_propagation();
            }))
            .child("×")
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PULSE_MIN_OPACITY, PULSE_STEPS, PaneIndicator, notification_indicator, pane_indicator,
        pulse_opacity_at_step,
    };
    use hh_protocol::PaneStatus;

    #[test]
    fn pane_indicator_maps_every_status_and_exit() {
        let expected = [
            (PaneStatus::Idle, PaneIndicator::None),
            (PaneStatus::Working, PaneIndicator::Running),
            (PaneStatus::NeedsApproval, PaneIndicator::NeedsYou),
            (PaneStatus::NeedsInput, PaneIndicator::NeedsYou),
            (PaneStatus::Attention, PaneIndicator::NeedsYou),
        ];
        for (status, indicator) in expected {
            for unread in [false, true] {
                assert_eq!(
                    pane_indicator(status, false, unread),
                    indicator,
                    "{status:?}"
                );
            }
        }
    }

    #[test]
    fn a_finished_tab_stays_blue_until_viewed() {
        // A finished turn, or a pane whose process exited whatever its last
        // status said, shows the Done dot while unread and nothing once seen.
        for (status, exited) in [
            (PaneStatus::Done, false),
            (PaneStatus::Working, true),
            (PaneStatus::NeedsInput, true),
        ] {
            assert_eq!(
                pane_indicator(status, exited, true),
                PaneIndicator::Done,
                "{status:?} exited={exited}"
            );
            assert_eq!(
                pane_indicator(status, exited, false),
                PaneIndicator::None,
                "{status:?} exited={exited}"
            );
        }
        assert_eq!(PaneIndicator::Done.color(), PaneIndicator::Running.color());
        assert_ne!(PaneIndicator::Done.color(), PaneIndicator::NeedsYou.color());
    }

    #[test]
    fn notifications_keep_done_rows_after_they_are_read() {
        let expected = [
            (PaneStatus::Idle, false, PaneIndicator::None),
            (PaneStatus::Done, false, PaneIndicator::Done),
            (PaneStatus::Working, false, PaneIndicator::Running),
            (PaneStatus::NeedsApproval, false, PaneIndicator::NeedsYou),
            (PaneStatus::NeedsInput, false, PaneIndicator::NeedsYou),
            (PaneStatus::Attention, false, PaneIndicator::NeedsYou),
            (PaneStatus::Working, true, PaneIndicator::Done),
            (PaneStatus::NeedsInput, true, PaneIndicator::Done),
            (PaneStatus::Idle, true, PaneIndicator::Done),
        ];
        for (status, exited, indicator) in expected {
            assert_eq!(
                notification_indicator(status, exited),
                indicator,
                "{status:?} exited={exited}"
            );
        }
    }

    #[test]
    fn aggregates_surface_the_most_urgent_indicator() {
        let tab = [
            pane_indicator(PaneStatus::Done, false, true),
            pane_indicator(PaneStatus::Working, false, false),
            pane_indicator(PaneStatus::NeedsInput, true, false),
        ];
        assert_eq!(tab.into_iter().max(), Some(PaneIndicator::Running));
        let tab = [
            pane_indicator(PaneStatus::Done, false, true),
            pane_indicator(PaneStatus::Idle, false, false),
        ];
        assert_eq!(tab.into_iter().max(), Some(PaneIndicator::Done));
        let tab = [
            pane_indicator(PaneStatus::Working, false, false),
            pane_indicator(PaneStatus::Attention, false, false),
        ];
        assert_eq!(tab.into_iter().max(), Some(PaneIndicator::NeedsYou));
    }

    #[test]
    fn the_running_dot_fades_out_and_back_without_vanishing() {
        let cycle = (0..PULSE_STEPS)
            .map(pulse_opacity_at_step)
            .collect::<Vec<_>>();
        assert!((cycle[0] - 1.0).abs() < f32::EPSILON);
        assert!((cycle[4] - PULSE_MIN_OPACITY).abs() < f32::EPSILON);
        assert!(cycle[..=4].windows(2).all(|pair| pair[1] < pair[0]));
        assert!(cycle[4..].windows(2).all(|pair| pair[1] > pair[0]));
        assert!((pulse_opacity_at_step(PULSE_STEPS) - cycle[0]).abs() < f32::EPSILON);
    }
}
