//! Chrome shared by every place a tab or terminal appears (top bar, pane
//! headers, sidebar rows, window ring chips, bot threads): one status
//! indicator slot and an always-visible close button.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, AppContext, Context, ElementId, InteractiveElement, IntoElement, MouseButton,
    ParentElement, StatefulInteractiveElement, Styled, Transformation, div, percentage, px, rgb,
    rgba, svg,
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
const SPINNER_ICON: &str = "agent-icons/spinner.svg";
/// The spinner turns in coarse steps: a frame-driven animation redraws the
/// whole app every frame (measured ~44% CPU in a debug build) for as long as
/// any agent works, while stepping costs a few redraws per second.
pub(crate) const SPINNER_STEP: Duration = Duration::from_millis(250);
const SPINNER_STEPS: u128 = 8;
const CLOSE_BUTTON_SIZE: f32 = 16.0;

/// What a status slot shows. Ordered by urgency so aggregates over several
/// panes take the maximum.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PaneIndicator {
    /// Idle, or done/exited on a tab: the slot stays empty.
    #[default]
    None,
    /// Done or exited, shown only on Notifications rows.
    Done,
    Running,
    NeedsYou,
}

impl PaneIndicator {
    /// Symbol color: needs-you orange, running accent, done green, else dim.
    pub(crate) const fn color(self) -> u32 {
        match self {
            Self::None => THEME.dim,
            Self::Done => THEME.ansi[2],
            Self::Running => THEME.accent,
            Self::NeedsYou => THEME.warning,
        }
    }
}

/// The one mapping from pane activity to a tab's indicator. An exited pane is
/// finished whatever its last status said, and a finished tab shows nothing.
pub(crate) const fn pane_indicator(status: PaneStatus, exited: bool) -> PaneIndicator {
    match notification_indicator(status, exited) {
        PaneIndicator::Done => PaneIndicator::None,
        indicator => indicator,
    }
}

/// A Notifications row's indicator: like [`pane_indicator`], but a finished
/// pane shows the green Done circle.
pub(crate) const fn notification_indicator(status: PaneStatus, exited: bool) -> PaneIndicator {
    match activity_section(status, exited) {
        Some(ActivitySection::NeedsYou) => PaneIndicator::NeedsYou,
        Some(ActivitySection::Running) => PaneIndicator::Running,
        Some(ActivitySection::Done) => PaneIndicator::Done,
        None => PaneIndicator::None,
    }
}

/// The fixed-size status slot: a spinning ring while running, an orange dot
/// when the pane needs the user, a green dot when done (Notifications only),
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
            svg()
                .path(SPINNER_ICON)
                .w(px(INDICATOR_SIZE))
                .h(px(INDICATOR_SIZE))
                .text_color(rgb(indicator.color()))
                .with_transformation(Transformation::rotate(percentage(spinner_turn()))),
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

/// The spinner's current fraction of a turn, taken from the wall clock so
/// every running indicator agrees without per-view state.
fn spinner_turn() -> f32 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let step = (elapsed / SPINNER_STEP.as_millis()) % SPINNER_STEPS;
    // `step` < 8, so the conversion is exact.
    f32::from(u8::try_from(step).unwrap_or(0)) / 8.0
}

impl HhApp {
    /// Whether any live terminal is working, which is when the spinner ticker
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
        pane_indicator(pane.status, self.pane_exited(pane.id))
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
    use super::{PaneIndicator, notification_indicator, pane_indicator};
    use hh_protocol::PaneStatus;

    #[test]
    fn pane_indicator_maps_every_status_and_exit() {
        let expected = [
            (PaneStatus::Idle, PaneIndicator::None),
            (PaneStatus::Done, PaneIndicator::None),
            (PaneStatus::Working, PaneIndicator::Running),
            (PaneStatus::NeedsApproval, PaneIndicator::NeedsYou),
            (PaneStatus::NeedsInput, PaneIndicator::NeedsYou),
            (PaneStatus::Attention, PaneIndicator::NeedsYou),
        ];
        for (status, indicator) in expected {
            assert_eq!(pane_indicator(status, false), indicator, "{status:?}");
            // Nothing is going on in a pane whose process exited.
            assert_eq!(
                pane_indicator(status, true),
                PaneIndicator::None,
                "{status:?}"
            );
        }
    }

    #[test]
    fn notifications_show_done_where_tabs_keep_an_empty_slot() {
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
            pane_indicator(PaneStatus::Done, false),
            pane_indicator(PaneStatus::Working, false),
            pane_indicator(PaneStatus::NeedsInput, true),
        ];
        assert_eq!(tab.into_iter().max(), Some(PaneIndicator::Running));
        let tab = [
            pane_indicator(PaneStatus::Working, false),
            pane_indicator(PaneStatus::Attention, false),
        ];
        assert_eq!(tab.into_iter().max(), Some(PaneIndicator::NeedsYou));
    }
}
