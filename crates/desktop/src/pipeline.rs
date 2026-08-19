//! Serialized async request pipelines.
//!
//! Every service request is enqueued here and executed on a background
//! executor by one task per lane (control and stream), preserving the two
//! latency classes of the old synchronous helpers while guaranteeing the UI
//! thread never blocks on socket IPC. A single consumer per lane also keeps
//! request ordering: keystrokes, pastes, and mutations apply in the order
//! the UI emitted them.

use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{AppContext as _, AsyncApp, Context, WeakEntity};
use hh_protocol::{ClientRequest, ServiceResponse};

use crate::session::{session_call, session_notify};

/// Typed continuation applied with a response on the UI thread.
pub(crate) type ApplyFn = Box<dyn FnOnce(&mut HhApp, &mut Context<HhApp>, Result<ServiceResponse>)>;
use crate::{HhApp, SharedSessionClient};

/// One queued service request plus how its response lands back on the UI.
pub(crate) struct PipelineJob {
    pub request: ClientRequest,
    /// One-way requests (terminal input, selection updates) are written
    /// without waiting for a response read.
    pub one_way: bool,
    /// Run one `GetUpdates` poll cycle after applying, so mutations refresh
    /// the visible snapshot without waiting for the next poll tick.
    pub followup_refresh: bool,
    /// Typed continuation applied on the UI thread with the response.
    pub apply: Option<ApplyFn>,
}

impl PipelineJob {
    /// Whether the request is answered on the wire at all.
    pub(crate) fn is_one_way(request: &ClientRequest) -> bool {
        matches!(
            request,
            ClientRequest::WriteInput { .. } | ClientRequest::UpdateSelection { .. }
        )
    }
}
/// The consumer side of one lane.
pub(crate) struct PipelineLane {
    rx: UnboundedReceiver<PipelineJob>,
}

impl PipelineLane {
    pub fn from_receiver(rx: UnboundedReceiver<PipelineJob>) -> Self {
        Self { rx }
    }
}
/// lane's shared client under a UI update so the Arc stays current.
pub(crate) fn spawn_lane(
    cx: &mut Context<HhApp>,
    lane: PipelineLane,
    client_of: fn(&HhApp) -> &SharedSessionClient,
) {
    cx.spawn(async move |this, cx: &mut AsyncApp| {
        let mut receiver = lane.rx;
        while let Some(job) = receiver.next().await {
            let Ok(client) = this.update(cx, |this, _| Arc::clone(client_of(this))) else {
                break;
            };
            let request = job.request;
            let result = if job.one_way {
                cx.background_spawn(async move {
                    session_notify(&client, &request).map(|()| ServiceResponse::Ack)
                })
                .await
            } else {
                cx.background_spawn(async move { session_call(&client, &request) })
                    .await
            };
            let followup_refresh = job.followup_refresh;
            let apply = job.apply;
            let Ok(()) = this.update(cx, |this, cx| match apply {
                Some(apply) => apply(this, cx, result),
                None => match &result {
                    Ok(ServiceResponse::Ack) => {}
                    Ok(response) => this.report_unexpected(response),
                    Err(error) => this.report(error),
                },
            }) else {
                break;
            };
            if followup_refresh {
                let _ = poll_once(&this, cx).await;
            }
        }
    })
    .detach();
}

/// One shared `GetUpdates` cycle: request built on the UI thread, executed on
/// a background thread, applied with resize/browser flushes and repaint.
/// Returns whether session state changed, or `None` when the app is gone.
pub(crate) async fn poll_once(this: &WeakEntity<HhApp>, cx: &mut AsyncApp) -> Option<bool> {
    let Ok((update_request, client)) = this.update(cx, |this, _| {
        (
            this.pane_update_request(),
            Arc::clone(&this.session.stream_client),
        )
    }) else {
        return None;
    };
    let response = cx
        .background_spawn(async move { session_call(&client, &update_request) })
        .await;
    let Ok(state_changed) = this.update(cx, |this, cx| {
        let state_changed = this.apply_update_result(response, cx);
        if state_changed {
            this.session.last_activity = std::time::Instant::now();
        }
        this.sync_pty_sizes(cx);
        this.flush_browser_state_updates(cx);
        if state_changed {
            cx.notify();
        }
        state_changed
    }) else {
        return None;
    };
    Some(state_changed)
}
