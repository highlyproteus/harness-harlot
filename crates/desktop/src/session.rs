//! Shared session client, request plumbing, and PTY synchronization.

use gpui::AppContext;
use gpui::{Context, Window};
use hh_desktop::SessionClient;
use hh_protocol::{
    ClientRequest, PaneLayout, PaneRevisionCursor, PaneStreamState, ServiceResponse,
    SessionSnapshot, Workspace,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::helpers::{
    collect_pane_sizes, find_pane, inactive_stack_contains, paced_subscriptions,
    sidebar_width_for_visibility, terminal_poll_wake_requested, visible_panes,
    workspace_layout_for_focused_pane, workspace_pixel_size, workspace_tab_standalone_pane,
    zoom_projection,
};
use crate::pipeline::{ApplyFn, PipelineJob};
use crate::reconcile::{UpdatePayload, reconcile_updates};
use crate::{HhApp, PTY_RESIZE_DEBOUNCE_MS, SECONDARY_PANE_INTERVAL, SharedSessionClient};
use uuid::Uuid;

pub(crate) fn session_call(
    client: &SharedSessionClient,
    request: &ClientRequest,
) -> anyhow::Result<ServiceResponse> {
    with_session_client(client, |client| client.call(request))
}

pub(crate) fn session_notify(
    client: &SharedSessionClient,
    request: &ClientRequest,
) -> anyhow::Result<()> {
    with_session_client(client, |client| client.notify(request))
}

/// Runs one request against the shared lazily-connected session client,
/// resetting the connection after a failure so the next call reconnects.
pub(crate) fn with_session_client<T>(
    client: &SharedSessionClient,
    request: impl FnOnce(&mut SessionClient) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut client = client.lock();
    if client.is_none() {
        *client = Some(SessionClient::connect()?);
    }
    let result = request(client.as_mut().expect("session client initialized"));
    if result.is_err() {
        *client = None;
    }
    result
}

impl HhApp {
    /// One synchronous startup fetch. All later refreshes flow through the
    /// async pipelines and the shared poll cycle.
    pub(crate) fn initial_state_fetch(&mut self, cx: &mut Context<Self>) -> bool {
        self.apply_update_result(self.stream_call(&self.pane_update_request()), cx)
    }

    /// Enqueues a control-lane request; one poll cycle follows so the
    /// visible state refreshes without waiting for the next poll tick.
    pub(crate) fn dispatch(&mut self, request: ClientRequest) {
        let one_way = PipelineJob::is_one_way(&request);
        let _ = self.session.control_tx.unbounded_send(PipelineJob {
            request,
            one_way,
            followup_refresh: true,
            apply: None,
        });
    }

    /// Enqueues a control-lane request with no follow-up refresh. Terminal
    /// input and selection updates are written one-way.
    pub(crate) fn dispatch_control(&mut self, request: ClientRequest) {
        let wake_poll = terminal_poll_wake_requested(&request);
        let one_way = PipelineJob::is_one_way(&request);
        if self
            .session
            .control_tx
            .unbounded_send(PipelineJob {
                request,
                one_way,
                followup_refresh: false,
                apply: None,
            })
            .is_ok()
            && wake_poll
        {
            let _ = self.session.poll_wake_tx.try_send(());
        }
    }

    /// Enqueues a control-lane request with a typed continuation applied
    /// with the response on the UI thread, followed by one poll cycle.
    pub(crate) fn dispatch_with(&mut self, request: ClientRequest, apply: ApplyFn) {
        let one_way = PipelineJob::is_one_way(&request);
        let _ = self.session.control_tx.unbounded_send(PipelineJob {
            request,
            one_way,
            followup_refresh: true,
            apply: Some(apply),
        });
    }

    /// Enqueues a stream-lane request (screen traffic) with a typed
    /// continuation; no follow-up refresh.
    pub(crate) fn dispatch_stream_with(&mut self, request: ClientRequest, apply: ApplyFn) {
        let _ = self.session.stream_tx.unbounded_send(PipelineJob {
            request,
            one_way: false,
            followup_refresh: false,
            apply: Some(apply),
        });
    }

    pub(crate) fn pane_update_request(&self) -> ClientRequest {
        let now = Instant::now();
        let pane_revisions = self
            .session
            .screens
            .values()
            .map(|screen| PaneRevisionCursor {
                pane_id: screen.pane_id,
                revision: screen.revision,
            })
            .collect();
        let subscribed_panes = paced_subscriptions(
            now,
            &self.on_screen_panes(),
            self.layout.focused_pane,
            &self.session.last_delivery,
            SECONDARY_PANE_INTERVAL,
        );
        ClientRequest::GetUpdates {
            snapshot_revision: self
                .session
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.revision),
            pane_revisions,
            subscribed_panes,
            notifications_after: self.session.notifications_latest_id,
        }
    }
    pub(crate) fn apply_update_result(
        &mut self,
        result: anyhow::Result<ServiceResponse>,
        cx: &mut Context<Self>,
    ) -> bool {
        let state_changed = match result {
            Ok(ServiceResponse::Updates {
                session_revision,
                snapshot,
                screens,
                pane_states,
                notifications: notification_deltas,
                diagnostics,
            }) => {
                let apply_started = Instant::now();
                let outcome = reconcile_updates(
                    &mut self.session,
                    &mut self.sidebar,
                    &mut self.layout,
                    &mut self.terminal_zoom_levels,
                    UpdatePayload {
                        session_revision,
                        snapshot,
                        screens,
                        pane_states,
                        notification_deltas,
                    },
                    Instant::now(),
                );
                if let Some(pane_id) = outcome.reassert_tab {
                    self.dispatch_control(ClientRequest::ActivateTab { pane_id });
                }
                if outcome.notifications_need_refresh {
                    self.refresh_notifications();
                }
                if outcome.auto_read_or_badge {
                    if self.session.window_active
                        && let Some(pane_id) = self.layout.focused_pane
                    {
                        self.auto_read_pane_notifications(pane_id, cx);
                    } else {
                        self.sync_dock_badge();
                    }
                }
                let mut state_changed = outcome.state_changed;
                if let Some(pane_id) = outcome.focus_resync {
                    state_changed |= self.focus_pane_with_snapshot(pane_id, cx);
                }
                self.session.stream_diagnostics = diagnostics;
                self.session.stream_diagnostics.desktop_apply_micros =
                    u64::try_from(apply_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                state_changed
            }
            Ok(response) => {
                let previous = self.session.connection_error.clone();
                self.report_unexpected(&response);
                self.session.connection_error != previous
            }
            Err(error) => {
                let previous = self.session.connection_error.clone();
                self.report(&error);
                self.session.connection_error != previous
            }
        };
        state_changed | self.sync_browser_callback_state()
    }

    pub(crate) fn focus_pane_with_snapshot(
        &mut self,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) -> bool {
        let needs_activation = self.session.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.workspaces.iter().any(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .any(|tab| inactive_stack_contains(&tab.layout, pane_id))
            })
        });
        if needs_activation {
            self.dispatch(ClientRequest::ActivateTab { pane_id });
        }
        let notifications_changed = self.auto_read_pane_notifications(pane_id, cx);
        if self
            .pane_metadata(pane_id)
            .is_some_and(|pane| pane.kind.is_browser())
        {
            let changed = self.layout.focused_pane != Some(pane_id);
            self.layout.focused_pane = Some(pane_id);
            self.session.connection_error = None;
            return changed || notifications_changed;
        }
        if self.layout.focused_pane == Some(pane_id) {
            return notifications_changed;
        }
        self.dispatch_stream_with(
            ClientRequest::GetPaneSnapshot { pane_id },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::PaneSnapshot {
                        screen,
                        diagnostics,
                    }) => {
                        let delivered_at = Instant::now();
                        let changed = this.layout.focused_pane != Some(pane_id)
                            || this
                                .session
                                .screens
                                .get(&pane_id)
                                .is_none_or(|current| current.revision != screen.revision);
                        this.session.pane_states.insert(
                            pane_id,
                            PaneStreamState {
                                pane_id,
                                revision: screen.revision,
                                subscribed: true,
                                dirty: false,
                                // A focus snapshot says nothing about liveness; keep
                                // whatever the last update round reported.
                                exited: this
                                    .session
                                    .pane_states
                                    .get(&pane_id)
                                    .is_some_and(|state| state.exited),
                            },
                        );
                        this.session.screens.insert(pane_id, screen);
                        this.layout.focused_pane = Some(pane_id);
                        this.session.last_delivery.insert(pane_id, delivered_at);
                        this.session.stream_diagnostics = diagnostics;
                        this.session.connection_error = None;
                        if changed || notifications_changed {
                            cx.notify();
                        }
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
            }),
        );
        notifications_changed
    }
    pub(crate) fn active_workspace_in<'a>(
        &self,
        snapshot: &'a SessionSnapshot,
    ) -> Option<&'a Workspace> {
        let active = self.sidebar.active_workspace?;
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == active)
    }

    /// The layout the viewport is actually rendering: the tab holding the
    /// focused pane, not blindly the workstation's first tab. Sizing, zoom,
    /// and split geometry must agree with what is on screen.
    pub(crate) fn active_layout<'a>(
        &self,
        snapshot: &'a SessionSnapshot,
    ) -> Option<&'a PaneLayout> {
        self.active_workspace_in(snapshot).and_then(|workspace| {
            workspace_layout_for_focused_pane(workspace, self.layout.focused_pane)
        })
    }

    /// Panes rendered right now: the tab holding the focused pane, zoom
    /// applied. This is the same projection `sync_pty_sizes` resizes, so what
    /// is sized is exactly what streams.
    pub(crate) fn on_screen_panes(&self) -> Vec<Uuid> {
        let Some(snapshot) = self.session.snapshot.as_ref() else {
            return Vec::new();
        };
        let Some(layout) = self.active_layout(snapshot) else {
            return Vec::new();
        };
        let projected = self
            .layout
            .zoomed_pane
            .and_then(|pane_id| zoom_projection(layout, pane_id));
        visible_panes(projected.as_ref().unwrap_or(layout))
    }

    pub(crate) fn refresh_history_status(&mut self) {
        self.dispatch_stream_with(
            ClientRequest::GetHistoryStatus,
            Box::new(|this, cx, result| {
                if this.apply_history_status_result(result) {
                    cx.notify();
                }
            }),
        );
    }

    pub(crate) fn apply_history_status_result(
        &mut self,
        response: anyhow::Result<ServiceResponse>,
    ) -> bool {
        let previous = self.session.history_status.clone();
        match response {
            Ok(ServiceResponse::HistoryStatus { status }) => {
                self.session.history_status = Some(status);
                self.session.connection_error = None;
            }
            Ok(response) => {
                self.report_unexpected(&response);
            }
            Err(error) => self.report(&error),
        }
        self.session.history_status != previous
    }

    pub(crate) fn update_window_geometry(&mut self, window: &Window) -> bool {
        let window_width = f32::from(window.bounds().size.width);
        let sidebar_pixels = sidebar_width_for_visibility(
            self.sidebar.preferred_sidebar_width,
            window_width,
            self.sidebar.sidebar_visible,
        );
        let next = workspace_pixel_size(
            window_width,
            f32::from(window.bounds().size.height),
            sidebar_pixels,
        );
        if self.layout.workspace_pixels == next
            && (self.sidebar.sidebar_pixels - sidebar_pixels).abs() < f32::EPSILON
        {
            return false;
        }
        self.sidebar.sidebar_pixels = sidebar_pixels;
        self.layout.workspace_pixels = next;
        true
    }

    pub(crate) fn sync_pty_sizes(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.session.snapshot.as_ref() else {
            return;
        };
        let Some(layout) = self.active_layout(snapshot) else {
            return;
        };
        let show_root_header = self
            .active_workspace_in(snapshot)
            .and_then(|workspace| {
                self.layout
                    .focused_pane
                    .and_then(|pane_id| {
                        workspace
                            .tabs
                            .iter()
                            .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                    })
                    .or_else(|| workspace.tabs.first())
            })
            .is_none_or(|tab| workspace_tab_standalone_pane(tab).is_none());
        let mut sizes = Vec::new();
        let projected = self
            .layout
            .zoomed_pane
            .and_then(|pane_id| zoom_projection(layout, pane_id));
        collect_pane_sizes(
            projected.as_ref().unwrap_or(layout),
            self.layout.workspace_pixels.0,
            self.layout.workspace_pixels.1,
            &|pane_id| self.terminal_metrics(pane_id),
            &self.layout.split_ratios,
            show_root_header,
            &mut sizes,
        );
        let changed = sizes.len() != self.layout.last_sizes.len()
            || sizes.iter().any(|(pane_id, columns, rows)| {
                self.layout.last_sizes.get(pane_id) != Some(&(*columns, *rows))
            });
        if !changed {
            return;
        }
        self.layout.last_sizes.clear();
        self.layout.last_sizes.extend(
            sizes
                .iter()
                .map(|(pane_id, columns, rows)| (*pane_id, (*columns, *rows))),
        );
        self.layout.resize_generation = self.layout.resize_generation.wrapping_add(1);
        let generation = self.layout.resize_generation;
        let client = Arc::clone(&self.session.control_client);
        cx.spawn(async move |this, cx| {
            gpui::Timer::after(Duration::from_millis(PTY_RESIZE_DEBOUNCE_MS)).await;
            let Ok(true) = this.update(cx, |this, _| this.layout.resize_generation == generation)
            else {
                return;
            };
            let result = cx
                .background_spawn(async move {
                    let mut first_error = None;
                    for (pane_id, columns, rows) in sizes {
                        let failure = match session_call(
                            &client,
                            &ClientRequest::ResizePane {
                                pane_id,
                                columns,
                                rows,
                            },
                        ) {
                            Ok(ServiceResponse::Ack) => None,
                            Ok(response) => Some(anyhow::anyhow!(
                                "unexpected resize response for {pane_id}: {response:?}"
                            )),
                            Err(error) => Some(error.context(format!("resize pane {pane_id}"))),
                        };
                        if first_error.is_none() {
                            first_error = failure;
                        }
                    }
                    if let Some(error) = first_error {
                        Err(error)
                    } else {
                        Ok(())
                    }
                })
                .await;
            let _ = this.update(cx, |this, _| {
                if let Err(error) = result
                    && this.layout.resize_generation == generation
                {
                    this.layout.last_sizes.clear();
                    this.report(&error);
                }
            });
        })
        .detach();
    }
}
