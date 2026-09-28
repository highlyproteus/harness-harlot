//! Settings → Bots → Agent task progress: the opt-in hooks that let omp,
//! Claude Code, and Codex report their task lists as pane progress rings.
//! Nothing is written until the user clicks Install.
use std::path::PathBuf;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px, rgb,
};

use crate::agent_progress::{self, InstallState, ProgressAgent};
use crate::{HhApp, THEME};

/// Codex asks the user to trust a newly added hook before running it.
const CODEX_TRUST_NOTE: &str = "Codex asks you to trust the new hook the next time it starts; progress appears once you accept.";

/// What the card knows about one agent's integration.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentProgressRow {
    state: Option<InstallState>,
    path: Option<PathBuf>,
    /// An install or removal is running.
    busy: bool,
    /// What the last install reported the user must know.
    note: Option<&'static str>,
    error: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct AgentProgressUi {
    rows: [AgentProgressRow; 3],
}

fn row_index(agent: ProgressAgent) -> usize {
    ProgressAgent::ALL
        .iter()
        .position(|candidate| *candidate == agent)
        .unwrap_or_default()
}

const fn state_label(state: InstallState) -> &'static str {
    match state {
        InstallState::NotInstalled => "Not installed",
        InstallState::Installed => "Installed",
        InstallState::Modified => "Modified",
        InstallState::Outdated => "Update available",
    }
}

/// The row's button, or `None` when no safe action exists (a hand-edited
/// omp extension is never overwritten or removed).
const fn action_for(state: InstallState) -> Option<(&'static str, bool)> {
    match state {
        InstallState::NotInstalled => Some(("Install", true)),
        InstallState::Outdated => Some(("Update", true)),
        InstallState::Installed => Some(("Remove", false)),
        InstallState::Modified => None,
    }
}

/// Reads one agent's install state and target path off the UI thread.
fn probe(agent: ProgressAgent) -> (Result<InstallState, String>, Option<PathBuf>) {
    (
        agent_progress::status(agent).map_err(|error| format!("{error:#}")),
        agent_progress::target_path(agent).ok(),
    )
}

impl HhApp {
    /// Re-reads every agent's install state in the background.
    pub(crate) fn refresh_agent_progress(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let probes = cx
                .background_executor()
                .spawn(async move { ProgressAgent::ALL.map(|agent| (agent, probe(agent))) })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (agent, (state, path)) in probes {
                    let row = &mut this.editor.agent_progress.rows[row_index(agent)];
                    if row.busy {
                        continue;
                    }
                    row.path = path;
                    match state {
                        Ok(state) => row.state = Some(state),
                        Err(error) => row.error = Some(error),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Installs (or updates) or removes one integration in the background,
    /// then shows its new state, note, or error in its row.
    fn run_agent_progress_action(
        &mut self,
        agent: ProgressAgent,
        install: bool,
        cx: &mut Context<Self>,
    ) {
        let row = &mut self.editor.agent_progress.rows[row_index(agent)];
        if row.busy {
            return;
        }
        row.busy = true;
        row.error = None;
        row.note = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (outcome, (state, path)) = cx
                .background_executor()
                .spawn(async move {
                    let outcome = if install {
                        agent_progress::install(agent).map(|report| report.note)
                    } else {
                        agent_progress::uninstall(agent).map(|()| None)
                    }
                    .map_err(|error| format!("{error:#}"));
                    (outcome, probe(agent))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let row = &mut this.editor.agent_progress.rows[row_index(agent)];
                row.busy = false;
                row.path = path;
                match outcome {
                    Ok(note) => row.note = note,
                    Err(error) => row.error = Some(error),
                }
                match state {
                    Ok(state) => row.state = Some(state),
                    Err(error) => {
                        row.error.get_or_insert(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn render_agent_progress_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let intro = div()
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(THEME.dim))
            .child(
                "Show an agent's task list as a progress ring on its tab. Install adds a small hook to that agent's own settings.",
            )
            .into_any_element();
        std::iter::once(intro)
            .chain(
                ProgressAgent::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, agent)| {
                        let row = &self.editor.agent_progress.rows[index];
                        self.render_agent_progress_row(index, agent, row, cx)
                    }),
            )
            .collect()
    }

    fn render_agent_progress_row(
        &self,
        index: usize,
        agent: ProgressAgent,
        row: &AgentProgressRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = if row.busy {
            "Working…"
        } else {
            row.state.map_or("Checking…", state_label)
        };
        let action = row.state.filter(|_| !row.busy).and_then(action_for);
        let line = |text: String, color: u32| {
            div()
                .font_family(".SystemUIFont")
                .text_xs()
                .text_color(rgb(color))
                .child(text)
        };
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .child(agent.display_name()),
                    )
                    .child(line(
                        status.to_owned(),
                        if row.state == Some(InstallState::Installed) {
                            THEME.accent
                        } else {
                            THEME.muted
                        },
                    ))
                    .when_some(action, |element, (label, install)| {
                        element.child(
                            div()
                                .id(("agent-progress-action", index))
                                .flex_none()
                                .cursor_pointer()
                                .px(px(10.0))
                                .py(px(5.0))
                                .rounded(px(5.0))
                                .border_1()
                                .border_color(rgb(THEME.border))
                                .text_xs()
                                .text_color(rgb(if install { THEME.accent } else { THEME.danger }))
                                .hover(|element| element.bg(rgb(THEME.accent_soft)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.run_agent_progress_action(agent, install, cx);
                                    cx.stop_propagation();
                                }))
                                .child(label),
                        )
                    }),
            )
            .when_some(row.path.as_ref(), |element, path| {
                element.child(
                    div()
                        .font_family("SF Mono")
                        .text_xs()
                        .text_color(rgb(THEME.dim))
                        .child(path.display().to_string()),
                )
            })
            .when(row.state == Some(InstallState::Modified), |element| {
                element.child(line(
                    "This file was edited by hand, so Harness Harlot leaves it alone.".to_owned(),
                    THEME.dim,
                ))
            })
            .when_some(row.note, |element, note| {
                element.child(line(note.to_owned(), THEME.dim))
            })
            .when(agent == ProgressAgent::Codex, |element| {
                element.child(line(CODEX_TRUST_NOTE.to_owned(), THEME.dim))
            })
            .when_some(row.error.clone(), |element, error| {
                element.child(line(error, THEME.danger))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::action_for;
    use crate::agent_progress::InstallState;

    #[test]
    fn a_hand_edited_integration_offers_no_action() {
        let installs = |state| action_for(state).map(|(_, install)| install);
        assert_eq!(installs(InstallState::NotInstalled), Some(true));
        assert_eq!(
            installs(InstallState::Outdated),
            Some(true),
            "update reinstalls"
        );
        assert_eq!(installs(InstallState::Installed), Some(false), "remove");
        assert_eq!(installs(InstallState::Modified), None);
    }
}
