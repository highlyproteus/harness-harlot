//! Chrome shared by every place a tab or terminal appears (top bar, pane
//! headers, sidebar rows, window ring chips, bot threads): the state border
//! (working blue with task progress, needs-you magenta, done green) and an
//! always-visible close button.
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, BoxShadow, Context, ElementId, Hsla, InteractiveElement, IntoElement,
    MouseButton, ParentElement, StatefulInteractiveElement, Styled, canvas, div, point, px, rgb,
    rgba,
};
use hh_protocol::{Pane, PaneProgress, PaneStatus, Workspace, workstation_descendants};
use std::time::Instant;
use uuid::Uuid;

use crate::status_art::{
    BorderArt, BorderMotion, DONE_COLOR, FrameRate, NEEDS_YOU_COLOR, WORKING_COLOR, border_motion,
    paint_status_border,
};
use crate::view_models::TooltipView;
use crate::{HhApp, THEME};

const UNREAD_DOT_SIZE: f32 = 6.0;
const CLOSE_BUTTON_SIZE: f32 = 16.0;
/// Opacity of a plain working border (no task list) under its comet.
const WORKING_BASE_ALPHA: f32 = 0.5;

/// Completed and total tasks, for one pane or summed over several.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TaskCount {
    pub(crate) done: u32,
    pub(crate) total: u32,
}

impl TaskCount {
    pub(crate) const fn of(progress: &PaneProgress) -> Self {
        Self {
            done: progress.done,
            total: progress.total,
        }
    }

    /// Completed share in `0..=1`; an empty list counts as complete.
    pub(crate) fn fraction(self) -> f32 {
        PaneProgress {
            done: self.done,
            total: self.total,
            current: None,
            phase: None,
            source: hh_protocol::ProgressSource::Omp,
        }
        .fraction()
    }

    const fn plus(self, other: Self) -> Self {
        Self {
            done: self.done.saturating_add(other.done),
            total: self.total.saturating_add(other.total),
        }
    }
}

/// A tab's state, drawn as its border.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PaneIndicator {
    /// Idle, or a finished pane the user has opened since: no border.
    #[default]
    None,
    /// Finished (done or exited) and not opened since (`Pane.unseen`).
    Done,
    /// Working, with the agent's task counts when it reports them.
    Running(Option<TaskCount>),
    /// Needs input or approval, or rang a bell.
    NeedsYou,
}

impl PaneIndicator {
    /// Precedence when one border summarizes several tabs or panes: needs
    /// you, then finished unseen, then working.
    const fn rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Running(_) => 1,
            Self::Done => 2,
            Self::NeedsYou => 3,
        }
    }

    /// Border colour; `None` has no border.
    pub(crate) const fn color(self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Running(_) => Some(WORKING_COLOR),
            Self::Done => Some(DONE_COLOR),
            Self::NeedsYou => Some(NEEDS_YOU_COLOR),
        }
    }

    /// Hover text for a border that summarizes task counts.
    pub(crate) fn tooltip(self) -> Option<String> {
        match self {
            Self::Running(Some(count)) => Some(count_tooltip(count)),
            _ => None,
        }
    }
}

/// The shared Running rule for the indicator, the Notifications Running
/// section, and aggregation: the pane is working, or it is idle, seen, and
/// its reported task list is unfinished (Claude and Codex rarely report
/// `Working`, so their progress keeps the ring up between turns). Callers
/// rank needs-you above this and treat exited panes as not running.
pub(crate) fn shows_running(
    status: PaneStatus,
    unseen: bool,
    progress: Option<&PaneProgress>,
) -> bool {
    match status {
        PaneStatus::Working => true,
        PaneStatus::Idle => {
            !unseen && progress.is_some_and(|progress| progress.done < progress.total)
        }
        PaneStatus::NeedsApproval
        | PaneStatus::NeedsInput
        | PaneStatus::Attention
        | PaneStatus::Done => false,
    }
}

/// The one mapping from pane state to its indicator, by precedence: needs
/// you, then running, then finished-and-unseen. An exited pane is finished
/// whatever its last status said.
pub(crate) fn pane_indicator(
    status: PaneStatus,
    exited: bool,
    unseen: bool,
    progress: Option<&PaneProgress>,
) -> PaneIndicator {
    if exited {
        return if unseen {
            PaneIndicator::Done
        } else {
            PaneIndicator::None
        };
    }
    if matches!(
        status,
        PaneStatus::NeedsApproval | PaneStatus::NeedsInput | PaneStatus::Attention
    ) {
        return PaneIndicator::NeedsYou;
    }
    if shows_running(status, unseen, progress) {
        return PaneIndicator::Running(progress.map(TaskCount::of));
    }
    if unseen {
        PaneIndicator::Done
    } else {
        PaneIndicator::None
    }
}

/// What one state border draws this frame, or `None` for no border.
pub(crate) fn border_art(indicator: PaneIndicator, motion: BorderMotion) -> Option<BorderArt> {
    let color = indicator.color()?;
    let (base_alpha, fill) = match indicator {
        PaneIndicator::Running(None) => (WORKING_BASE_ALPHA, None),
        PaneIndicator::Running(Some(count)) => (1.0, Some(count.fraction())),
        PaneIndicator::None | PaneIndicator::Done | PaneIndicator::NeedsYou => (1.0, None),
    };
    Some(BorderArt {
        color,
        base_alpha,
        fill,
        motion,
    })
}

/// How often a visible border of this state asks for frames: the needs-you
/// comet at ~30 fps, working and done comets at ~12 fps, nothing when motion
/// is reduced or there is no border.
pub(crate) const fn border_frame_rate(indicator: PaneIndicator, reduced: bool) -> FrameRate {
    match (indicator, reduced) {
        (PaneIndicator::None, _) | (_, true) => FrameRate::Still,
        (PaneIndicator::NeedsYou, false) => FrameRate::Comet,
        (PaneIndicator::Running(_) | PaneIndicator::Done, false) => FrameRate::Calm,
    }
}

/// Several panes in one border: needs you beats finished-unseen beats
/// working; working progress sums the task counts of every running pane that
/// reports them, and stays indeterminate when none does.
pub(crate) fn aggregate_indicators(
    indicators: impl IntoIterator<Item = PaneIndicator>,
) -> PaneIndicator {
    let mut most_urgent = PaneIndicator::None;
    let mut tasks: Option<TaskCount> = None;
    for indicator in indicators {
        if let PaneIndicator::Running(Some(count)) = indicator {
            tasks = Some(tasks.map_or(count, |sum| sum.plus(count)));
        }
        if indicator.rank() > most_urgent.rank() {
            most_urgent = indicator;
        }
    }
    match most_urgent {
        PaneIndicator::Running(_) => PaneIndicator::Running(tasks),
        other => other,
    }
}

/// A collapsed workstation card's one status slot, aggregated across its own
/// panes and those of every workstation nested in it.
pub(crate) fn workstation_rollup_indicator(
    workspaces: &[Workspace],
    workstation_id: Uuid,
    indicator: impl Fn(&Pane) -> PaneIndicator,
) -> PaneIndicator {
    let nested = workstation_descendants(workspaces, workstation_id);
    aggregate_indicators(
        workspaces
            .iter()
            .filter(|workspace| workspace.id == workstation_id || nested.contains(&workspace.id))
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| {
                let mut panes = Vec::new();
                crate::helpers::collect_terminal_tabs(&tab.layout, &mut panes);
                panes
            })
            .map(indicator),
    )
}

/// "3 of 7 done · Phase — current task" for one pane's progress.
pub(crate) fn progress_tooltip(progress: &PaneProgress) -> String {
    let mut text = count_tooltip(TaskCount::of(progress));
    let phase = progress.phase.as_deref().filter(|text| !text.is_empty());
    let current = progress.current.as_deref().filter(|text| !text.is_empty());
    match (phase, current) {
        (Some(phase), Some(current)) => {
            text = format!("{text} · {phase} — {current}");
        }
        (Some(detail), None) | (None, Some(detail)) => {
            text = format!("{text} · {detail}");
        }
        (None, None) => {}
    }
    text
}

pub(crate) fn count_tooltip(count: TaskCount) -> String {
    format!("{} of {} done", count.done, count.total)
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

impl HhApp {
    /// Keeps frames coming while the window draws something moving: ~30 fps
    /// for a comet border, 10 fps for a spinning ring. Stops as soon as a
    /// frame draws nothing moving, or when the window stops drawing frames
    /// (hidden or minimised); the pending redraw restarts it on return.
    pub(crate) fn ensure_animation_tick(&mut self, cx: &mut Context<Self>) {
        if self.motion.tick_running {
            return;
        }
        let Some(first) = self.motion.wanted_interval() else {
            return;
        };
        self.motion.tick_running = true;
        cx.spawn(async move |this, cx| {
            let mut interval = first;
            loop {
                gpui::Timer::after(interval).await;
                let Ok(Some(next)) = this.update(cx, |this, cx| {
                    let next = this.motion.next_tick(Instant::now());
                    if next.is_some() {
                        cx.notify();
                    } else {
                        this.motion.tick_running = false;
                    }
                    next
                }) else {
                    break;
                };
                interval = next;
            }
        })
        .detach();
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
            pane.unseen,
            pane.progress.as_ref(),
        )
    }

    /// Hover text for one pane's slot: its task progress while running.
    pub(crate) fn pane_indicator_tooltip(&self, pane: &Pane) -> Option<String> {
        match self.pane_indicator(pane) {
            PaneIndicator::Running(Some(_)) => pane.progress.as_ref().map(progress_tooltip),
            _ => None,
        }
    }

    /// Frames `element` with its state border: the state colour (with the
    /// task-progress fill while working through a list) and a comet running
    /// clockwise, or a steady glow under reduced motion. `radius` is the
    /// element's corner radius. No border for `PaneIndicator::None`.
    pub(crate) fn with_status_border<E>(
        &self,
        element: E,
        indicator: PaneIndicator,
        radius: f32,
    ) -> E
    where
        E: Styled + ParentElement + FluentBuilder,
    {
        let reduced = self.motion.reduced();
        let Some(art) = border_art(
            indicator,
            border_motion(reduced, self.motion.elapsed_secs()),
        ) else {
            return element;
        };
        self.motion
            .request_frames(border_frame_rate(indicator, reduced));
        let mut glow = Hsla::from(rgb(art.color));
        glow.a = 0.45;
        element
            .relative()
            .when(art.motion == BorderMotion::Glow, |element| {
                element.shadow(vec![BoxShadow {
                    color: glow,
                    offset: point(px(0.0), px(0.0)),
                    blur_radius: px(6.0),
                    spread_radius: px(0.0),
                }])
            })
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, (), window, _| {
                        paint_status_border(bounds, window, radius, art);
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
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
        PaneIndicator, TaskCount, aggregate_indicators, border_art, border_frame_rate,
        pane_indicator, progress_tooltip, workstation_rollup_indicator,
    };
    use crate::status_art::{BorderMotion, DONE_COLOR, FrameRate, NEEDS_YOU_COLOR, WORKING_COLOR};
    use hh_protocol::{
        PaneLayout, PaneProgress, PaneStatus, ProgressSource, SessionSnapshot, Workspace,
    };
    use uuid::Uuid;

    fn progress(done: u32, total: u32) -> PaneProgress {
        PaneProgress {
            done,
            total,
            current: None,
            phase: None,
            source: ProgressSource::Claude,
        }
    }

    const fn running(done: u32, total: u32) -> PaneIndicator {
        PaneIndicator::Running(Some(TaskCount { done, total }))
    }

    #[test]
    fn a_collapsed_workstation_rolls_up_its_nested_workstations() {
        fn workstation(id: u128, parent: Option<u128>, status: PaneStatus) -> Workspace {
            let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
            workspace.id = Uuid::from_u128(id);
            workspace.home = false;
            workspace.parent_workstation = parent.map(Uuid::from_u128);
            let PaneLayout::Leaf { pane } = &mut workspace.tabs[0].layout else {
                unreachable!("seeded tabs are single panes");
            };
            pane.id = Uuid::from_u128(id * 10);
            pane.status = status;
            pane.unseen = true;
            pane.progress = Some(progress(1, 4));
            workspace
        }
        let workspaces = vec![
            workstation(1, None, PaneStatus::Done),
            workstation(11, Some(1), PaneStatus::Working),
            workstation(111, Some(11), PaneStatus::NeedsInput),
            workstation(2, None, PaneStatus::Working),
            workstation(21, Some(2), PaneStatus::Working),
        ];
        let rollup = |id: u128, workspaces: &[Workspace]| {
            workstation_rollup_indicator(workspaces, Uuid::from_u128(id), |pane| {
                pane_indicator(pane.status, false, pane.unseen, pane.progress.as_ref())
            })
        };

        assert_eq!(
            rollup(1, &workspaces),
            PaneIndicator::NeedsYou,
            "grandchild needs you"
        );
        assert_eq!(rollup(11, &workspaces), PaneIndicator::NeedsYou);
        assert_eq!(rollup(111, &workspaces), PaneIndicator::NeedsYou);
        assert_eq!(
            rollup(2, &workspaces),
            running(2, 8),
            "running progress sums over nested workstations, never siblings"
        );
        assert_eq!(rollup(1, &workspaces[..1]), PaneIndicator::Done);
    }

    #[test]
    fn needs_you_beats_running_beats_unseen_done() {
        let with = Some(progress(3, 7));
        for status in [
            PaneStatus::NeedsApproval,
            PaneStatus::NeedsInput,
            PaneStatus::Attention,
        ] {
            for unseen in [false, true] {
                assert_eq!(
                    pane_indicator(status, false, unseen, with.as_ref()),
                    PaneIndicator::NeedsYou,
                    "{status:?} unseen={unseen}"
                );
            }
        }
        for unseen in [false, true] {
            assert_eq!(
                pane_indicator(PaneStatus::Working, false, unseen, with.as_ref()),
                running(3, 7),
                "running ignores unseen"
            );
            assert_eq!(
                pane_indicator(PaneStatus::Working, false, unseen, None),
                PaneIndicator::Running(None)
            );
        }
        for status in [PaneStatus::Done, PaneStatus::Idle] {
            assert_eq!(
                pane_indicator(status, false, true, None),
                PaneIndicator::Done
            );
            assert_eq!(
                pane_indicator(status, false, false, None),
                PaneIndicator::None
            );
        }
    }

    #[test]
    fn an_idle_seen_pane_with_unfinished_tasks_shows_running() {
        let unfinished = Some(progress(2, 5));
        assert_eq!(
            pane_indicator(PaneStatus::Idle, false, false, unfinished.as_ref()),
            running(2, 5),
            "Claude and Codex stay Idle while they work through their list"
        );
        assert_eq!(
            pane_indicator(PaneStatus::Idle, false, true, unfinished.as_ref()),
            PaneIndicator::Done,
            "an unseen finish keeps its dot"
        );
        assert_eq!(
            pane_indicator(PaneStatus::Idle, false, false, Some(&progress(5, 5))),
            PaneIndicator::None,
            "a finished list is not running"
        );
        assert_eq!(
            pane_indicator(PaneStatus::Done, false, false, unfinished.as_ref()),
            PaneIndicator::None
        );
        assert_eq!(
            pane_indicator(PaneStatus::NeedsInput, false, false, unfinished.as_ref()),
            PaneIndicator::NeedsYou
        );
        assert_eq!(
            pane_indicator(PaneStatus::Idle, true, false, unfinished.as_ref()),
            PaneIndicator::None,
            "exited"
        );
        assert_eq!(
            aggregate_indicators([
                pane_indicator(PaneStatus::Idle, false, false, unfinished.as_ref()),
                pane_indicator(PaneStatus::Working, false, false, Some(&progress(1, 1))),
            ]),
            running(3, 6),
            "aggregation sums idle-running panes too"
        );
    }

    #[test]
    fn an_exited_pane_is_finished_whatever_its_last_status_said() {
        for status in [
            PaneStatus::Working,
            PaneStatus::NeedsInput,
            PaneStatus::Done,
        ] {
            assert_eq!(
                pane_indicator(status, true, true, Some(&progress(1, 2))),
                PaneIndicator::Done,
                "{status:?}"
            );
            assert_eq!(
                pane_indicator(status, true, false, None),
                PaneIndicator::None
            );
        }
    }

    #[test]
    fn aggregation_ranks_needs_you_then_done_then_working() {
        assert_eq!(
            aggregate_indicators([running(1, 3), PaneIndicator::Running(None), running(2, 5),]),
            running(3, 8),
            "panes without progress do not dilute the sum"
        );
        assert_eq!(
            aggregate_indicators([PaneIndicator::Running(None), PaneIndicator::None]),
            PaneIndicator::Running(None),
            "indeterminate when no running pane reports progress"
        );
        assert_eq!(
            aggregate_indicators([running(1, 3), PaneIndicator::Done]),
            PaneIndicator::Done,
            "a finished unseen tab outranks one still working"
        );
        assert_eq!(
            aggregate_indicators([PaneIndicator::Done, running(1, 2), PaneIndicator::NeedsYou]),
            PaneIndicator::NeedsYou
        );
        assert_eq!(
            aggregate_indicators([PaneIndicator::None, PaneIndicator::Done]),
            PaneIndicator::Done
        );
        assert_eq!(aggregate_indicators([]), PaneIndicator::None);
    }

    #[test]
    fn each_state_draws_its_own_border_colour_and_working_carries_its_progress() {
        let comet = BorderMotion::Comet { head: 0.3 };
        assert_eq!(border_art(PaneIndicator::None, comet), None, "no border");
        let needs = border_art(PaneIndicator::NeedsYou, comet).expect("border");
        let done = border_art(PaneIndicator::Done, comet).expect("border");
        let plain = border_art(PaneIndicator::Running(None), comet).expect("border");
        let list = border_art(running(1, 4), comet).expect("border");
        assert_eq!(needs.color, NEEDS_YOU_COLOR);
        assert_eq!(done.color, DONE_COLOR);
        assert_eq!(plain.color, WORKING_COLOR);
        assert_eq!(list.color, WORKING_COLOR);
        assert_eq!((needs.fill, done.fill, plain.fill), (None, None, None));
        assert_eq!(list.fill, Some(0.25), "the fill covers the done share");
        assert!(plain.base_alpha < 1.0, "a plain working border is quieter");
        assert_eq!(
            border_art(running(0, 0), comet).and_then(|art| art.fill),
            Some(1.0),
            "an empty list counts as complete"
        );
        assert_eq!(
            border_art(PaneIndicator::Done, BorderMotion::Glow).map(|art| art.motion),
            Some(BorderMotion::Glow),
            "reduced motion keeps the colour and drops the comet"
        );
    }

    #[test]
    fn only_needs_you_animates_fast_and_reduced_motion_stops_every_border() {
        assert_eq!(
            border_frame_rate(PaneIndicator::NeedsYou, false),
            FrameRate::Comet
        );
        for calm in [
            PaneIndicator::Done,
            PaneIndicator::Running(None),
            running(2, 3),
        ] {
            assert_eq!(border_frame_rate(calm, false), FrameRate::Calm, "{calm:?}");
        }
        for indicator in [
            PaneIndicator::None,
            PaneIndicator::NeedsYou,
            PaneIndicator::Done,
            running(2, 3),
        ] {
            assert_eq!(border_frame_rate(indicator, true), FrameRate::Still);
        }
        assert_eq!(
            border_frame_rate(PaneIndicator::None, false),
            FrameRate::Still
        );
    }

    #[test]
    fn progress_tooltip_names_the_phase_and_current_task() {
        let mut report = progress(3, 7);
        assert_eq!(progress_tooltip(&report), "3 of 7 done");
        report.current = Some("Write tests".to_owned());
        assert_eq!(progress_tooltip(&report), "3 of 7 done · Write tests");
        report.phase = Some("Build".to_owned());
        assert_eq!(
            progress_tooltip(&report),
            "3 of 7 done · Build — Write tests"
        );
    }
}
