//! Chrome shared by every place a tab or terminal appears (top bar, pane
//! headers, sidebar rows, window ring chips, bot threads): one status
//! indicator slot, the needs-input border, and an always-visible close button.
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, BoxShadow, Context, Div, ElementId, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement, StatefulInteractiveElement, Styled, canvas, div,
    point, px, rgb, rgba,
};
use hh_protocol::{Pane, PaneLayout, PaneProgress, PaneStatus, Workspace, workstation_descendants};
use std::time::Instant;
use uuid::Uuid;

use crate::status_art::{
    BorderMotion, FrameRate, border_motion, paint_needs_input_border, paint_progress_ring,
    paint_spinner_ring, spinner_rotation,
};
use crate::view_models::TooltipView;
use crate::{HhApp, THEME};

/// Side of the indicator slot; reserved even when empty so labels never shift.
const INDICATOR_SIZE: f32 = 10.0;
const STATUS_DOT_SIZE: f32 = 7.0;
const UNREAD_DOT_SIZE: f32 = 6.0;
const CLOSE_BUTTON_SIZE: f32 = 16.0;

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

/// What a status slot shows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PaneIndicator {
    /// Idle, or a finished pane the user has opened since: the slot stays empty.
    #[default]
    None,
    /// Finished (done or exited) and not opened since (`Pane.unseen`).
    Done,
    /// Working, with the agent's task counts when it reports them.
    Running(Option<TaskCount>),
    NeedsYou,
}

impl PaneIndicator {
    /// Urgency, which decides what an aggregate over several panes shows.
    const fn rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Done => 1,
            Self::Running(_) => 2,
            Self::NeedsYou => 3,
        }
    }

    /// Symbol color: needs-you orange, running and done blue, else dim.
    pub(crate) const fn color(self) -> u32 {
        match self {
            Self::None => THEME.dim,
            Self::Done | Self::Running(_) => THEME.accent,
            Self::NeedsYou => THEME.warning,
        }
    }

    /// Hover text for a slot that summarizes task counts.
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

/// Whether a pane is blocked on an answer, which frames its tabs with the
/// travelling needs-input border. Bells (`Attention`) only get the dot.
pub(crate) const fn awaits_input(status: PaneStatus, exited: bool) -> bool {
    !exited && matches!(status, PaneStatus::NeedsInput | PaneStatus::NeedsApproval)
}

/// Several panes in one slot: the most urgent indicator wins; running
/// progress sums the task counts of every running pane that reports them,
/// and stays indeterminate when none does.
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

/// What the status slot draws this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum IndicatorArt {
    Empty,
    Dot(u32),
    /// A ring filled to this fraction.
    Progress(f32),
    /// The indeterminate running ring at this rotation; `None` is still.
    Spinner(Option<f32>),
}

pub(crate) fn indicator_art(indicator: PaneIndicator, reduced: bool, elapsed: f32) -> IndicatorArt {
    match indicator {
        PaneIndicator::None => IndicatorArt::Empty,
        PaneIndicator::Done | PaneIndicator::NeedsYou => IndicatorArt::Dot(indicator.color()),
        PaneIndicator::Running(Some(count)) => IndicatorArt::Progress(count.fraction()),
        PaneIndicator::Running(None) => IndicatorArt::Spinner(spinner_rotation(reduced, elapsed)),
    }
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

    pub(crate) fn pane_awaits_input(&self, pane: &Pane) -> bool {
        awaits_input(pane.status, self.pane_exited(pane.id))
    }

    /// Whether any terminal of a tab is blocked on an answer.
    pub(crate) fn layout_awaits_input(&self, layout: &PaneLayout) -> bool {
        let mut panes = Vec::new();
        crate::helpers::collect_terminal_tabs(layout, &mut panes);
        panes.iter().any(|pane| self.pane_awaits_input(pane))
    }

    fn indicator_slot(&self, indicator: PaneIndicator) -> Div {
        let slot = div()
            .flex_none()
            .w(px(INDICATOR_SIZE))
            .h(px(INDICATOR_SIZE))
            .flex()
            .items_center()
            .justify_center();
        let art = indicator_art(indicator, self.motion.reduced(), self.motion.elapsed_secs());
        match art {
            IndicatorArt::Empty => slot,
            IndicatorArt::Dot(color) => slot.child(
                div()
                    .w(px(STATUS_DOT_SIZE))
                    .h(px(STATUS_DOT_SIZE))
                    .rounded_full()
                    .bg(rgb(color)),
            ),
            IndicatorArt::Progress(fraction) => slot.child(
                canvas(
                    |_, _, _| {},
                    move |bounds, (), window, _| paint_progress_ring(bounds, window, fraction),
                )
                .size_full(),
            ),
            IndicatorArt::Spinner(rotation) => {
                if rotation.is_some() {
                    self.motion.request_frames(FrameRate::Spinner);
                }
                slot.child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, (), window, _| paint_spinner_ring(bounds, window, rotation),
                    )
                    .size_full(),
                )
            }
        }
    }

    /// The fixed-size status slot: an orange dot when the pane needs the
    /// user, a progress ring (or the indeterminate blue ring) while it runs,
    /// a blue dot when it finished unseen, otherwise empty space.
    pub(crate) fn render_pane_indicator(&self, indicator: PaneIndicator) -> AnyElement {
        self.indicator_slot(indicator).into_any_element()
    }

    /// [`Self::render_pane_indicator`] with hover text, e.g. task progress.
    pub(crate) fn render_pane_indicator_with_tooltip(
        &self,
        indicator: PaneIndicator,
        id: impl Into<ElementId>,
        tooltip: Option<String>,
    ) -> AnyElement {
        let slot = self.indicator_slot(indicator);
        match tooltip {
            Some(text) => slot
                .id(id)
                .tooltip(move |_, cx| cx.new(|_| TooltipView { text: text.clone() }).into())
                .into_any_element(),
            None => slot.into_any_element(),
        }
    }

    /// Frames `element` with the needs-input border when `awaits`: orange
    /// with a comet running clockwise, or a steady glow under reduced motion.
    /// `radius` is the element's corner radius.
    pub(crate) fn with_needs_input_border<E>(&self, element: E, awaits: bool, radius: f32) -> E
    where
        E: Styled + ParentElement + FluentBuilder,
    {
        if !awaits {
            return element;
        }
        let motion = border_motion(self.motion.reduced(), self.motion.elapsed_secs());
        if matches!(motion, BorderMotion::Comet { .. }) {
            self.motion.request_frames(FrameRate::Comet);
        }
        let mut glow = Hsla::from(rgb(THEME.warning));
        glow.a = 0.45;
        element
            .relative()
            .when(motion == BorderMotion::Glow, |element| {
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
                        paint_needs_input_border(bounds, window, radius, motion);
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
        IndicatorArt, PaneIndicator, TaskCount, aggregate_indicators, awaits_input, indicator_art,
        pane_indicator, progress_tooltip, workstation_rollup_indicator,
    };
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
    fn only_questions_and_approvals_get_the_needs_input_border() {
        assert!(awaits_input(PaneStatus::NeedsInput, false));
        assert!(awaits_input(PaneStatus::NeedsApproval, false));
        assert!(
            !awaits_input(PaneStatus::Attention, false),
            "bells keep the dot"
        );
        assert!(!awaits_input(PaneStatus::Working, false));
        assert!(!awaits_input(PaneStatus::NeedsInput, true), "exited");
    }

    #[test]
    fn aggregation_takes_the_most_urgent_and_sums_reported_progress() {
        assert_eq!(
            aggregate_indicators([
                PaneIndicator::Done,
                running(1, 3),
                PaneIndicator::Running(None),
                running(2, 5),
            ]),
            running(3, 8),
            "panes without progress do not dilute the sum"
        );
        assert_eq!(
            aggregate_indicators([PaneIndicator::Running(None), PaneIndicator::Done]),
            PaneIndicator::Running(None),
            "indeterminate when no running pane reports progress"
        );
        assert_eq!(
            aggregate_indicators([running(1, 2), PaneIndicator::NeedsYou]),
            PaneIndicator::NeedsYou
        );
        assert_eq!(
            aggregate_indicators([PaneIndicator::None, PaneIndicator::Done]),
            PaneIndicator::Done
        );
        assert_eq!(aggregate_indicators([]), PaneIndicator::None);
    }

    #[test]
    fn running_draws_a_ring_that_holds_still_under_reduced_motion() {
        assert_eq!(
            indicator_art(running(1, 4), false, 0.3),
            IndicatorArt::Progress(0.25)
        );
        assert_eq!(
            indicator_art(running(1, 4), true, 0.3),
            IndicatorArt::Progress(0.25)
        );
        assert_eq!(
            indicator_art(PaneIndicator::Running(None), true, 0.3),
            IndicatorArt::Spinner(None)
        );
        assert!(matches!(
            indicator_art(PaneIndicator::Running(None), false, 0.3),
            IndicatorArt::Spinner(Some(_))
        ));
        assert_eq!(
            indicator_art(PaneIndicator::NeedsYou, false, 0.0),
            IndicatorArt::Dot(PaneIndicator::NeedsYou.color())
        );
        assert_eq!(
            indicator_art(PaneIndicator::None, false, 0.0),
            IndicatorArt::Empty
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
