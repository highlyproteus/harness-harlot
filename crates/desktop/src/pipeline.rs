//! Serialized async request pipelines.
//!
//! Every service request is enqueued here and executed on a background
//! executor by one task per lane (control and stream), preserving the two
//! latency classes of the old synchronous helpers while guaranteeing the UI
//! thread never blocks on socket IPC. A single consumer per lane also keeps
//! request ordering: keystrokes, pastes, and mutations apply in the order
//! the UI emitted them.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use futures::channel::mpsc::{Receiver, Sender};
use gpui::{AppContext as _, AsyncApp, Context, WeakEntity};
use hh_protocol::{ClientRequest, ServiceResponse};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::session::{session_call, session_notify};

/// Typed continuation applied with a response on the UI thread.
pub(crate) type ApplyFn = Box<dyn FnOnce(&mut HhApp, &mut Context<HhApp>, Result<ServiceResponse>)>;
use crate::{HhApp, SharedSessionClient};

pub(crate) const CONTROL_PIPELINE_CAPACITY: usize = 32;
pub(crate) const STREAM_PIPELINE_CAPACITY: usize = 8;
pub(crate) const TERMINAL_INPUT_CAPACITY_BYTES: usize = 256 * 1024;
const TERMINAL_INPUT_FRAME_BYTES: usize = 64 * 1024;

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
        matches!(request, ClientRequest::UpdateSelection { .. })
    }
}
/// The consumer side of one lane.
pub(crate) struct PipelineLane {
    rx: Receiver<PipelineJob>,
}

pub(crate) fn bounded_lane(capacity: usize) -> (Sender<PipelineJob>, PipelineLane) {
    assert!(capacity > 0, "pipeline capacity must be positive");
    // futures-mpsc reserves one slot per sender in addition to its buffer.
    // Each lane intentionally has exactly one sender, so subtract one to make
    // the declared capacity the actual maximum retained job count.
    let (tx, rx) = futures::channel::mpsc::channel(capacity - 1);
    (tx, PipelineLane { rx })
}

#[derive(Default)]
struct TerminalInputBuffer {
    queued: VecDeque<(Uuid, Vec<u8>)>,
    buffered_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct TerminalInputSender {
    state: Arc<Mutex<TerminalInputBuffer>>,
    wake: Arc<Mutex<Sender<()>>>,
    capacity_bytes: usize,
}

pub(crate) struct TerminalInputLane {
    state: Arc<Mutex<TerminalInputBuffer>>,
    wake_rx: Receiver<()>,
}

pub(crate) fn terminal_input_channel(
    capacity_bytes: usize,
) -> (TerminalInputSender, TerminalInputLane) {
    assert!(
        capacity_bytes > 0,
        "terminal input capacity must be positive"
    );
    let state = Arc::new(Mutex::new(TerminalInputBuffer::default()));
    let (wake, wake_rx) = futures::channel::mpsc::channel(1);
    (
        TerminalInputSender {
            state: Arc::clone(&state),
            wake: Arc::new(Mutex::new(wake)),
            capacity_bytes,
        },
        TerminalInputLane { state, wake_rx },
    )
}

impl TerminalInputSender {
    pub(crate) fn try_send(&self, pane_id: Uuid, bytes: &[u8]) -> Result<(), &'static str> {
        if bytes.len() > TERMINAL_INPUT_FRAME_BYTES {
            return Err("terminal input exceeds the 65536-byte frame limit");
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock();
        if state.buffered_bytes.saturating_add(bytes.len()) > self.capacity_bytes {
            return Err("terminal input buffer is full; input was rejected");
        }
        let previous_last_len = state.queued.back().map(|(_, bytes)| bytes.len());
        let coalesced = state.queued.back_mut().is_some_and(|(last_pane, pending)| {
            if *last_pane == pane_id
                && pending.len().saturating_add(bytes.len()) <= TERMINAL_INPUT_FRAME_BYTES
            {
                pending.extend_from_slice(bytes);
                true
            } else {
                false
            }
        });
        if !coalesced {
            state.queued.push_back((pane_id, bytes.to_vec()));
        }
        state.buffered_bytes += bytes.len();
        drop(state);

        let mut wake = self.wake.lock();
        if let Err(error) = wake.try_send(())
            && error.is_disconnected()
        {
            let mut state = self.state.lock();
            state.buffered_bytes -= bytes.len();
            if coalesced {
                state
                    .queued
                    .back_mut()
                    .expect("coalesced terminal input remains queued")
                    .1
                    .truncate(previous_last_len.expect("coalesced input had a prior frame"));
            } else {
                state.queued.pop_back();
            }
            return Err("terminal input worker is unavailable; input was rejected");
        }
        Ok(())
    }

    #[cfg(test)]
    fn drain_for_test(&self) -> Vec<(Uuid, Vec<u8>)> {
        self.state.lock().queued.drain(..).collect()
    }
}

fn pop_terminal_input(state: &Mutex<TerminalInputBuffer>) -> Option<(Uuid, Vec<u8>)> {
    let mut state = state.lock();
    let next = state.queued.pop_front()?;
    state.buffered_bytes -= next.1.len();
    Some(next)
}

pub(crate) fn spawn_terminal_input_lane(
    cx: &mut Context<HhApp>,
    lane: TerminalInputLane,
    client_of: fn(&HhApp) -> &SharedSessionClient,
) {
    cx.spawn(async move |this, cx: &mut AsyncApp| {
        let mut wake_rx = lane.wake_rx;
        while wake_rx.next().await.is_some() {
            while let Some((pane_id, bytes)) = pop_terminal_input(&lane.state) {
                let Ok(client) = this.update(cx, |this, _| Arc::clone(client_of(this))) else {
                    return;
                };
                let request = ClientRequest::WriteInput { pane_id, bytes };
                let result = cx
                    .background_spawn(async move { session_call(&client, &request) })
                    .await;
                let Ok(()) = this.update(cx, |this, _| match result {
                    Ok(ServiceResponse::Ack) => {}
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }) else {
                    return;
                };
            }
        }
    })
    .detach();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> PipelineJob {
        PipelineJob {
            request: ClientRequest::ShutdownService,
            one_way: false,
            followup_refresh: false,
            apply: None,
        }
    }

    #[test]
    fn bounded_lane_rejects_jobs_beyond_its_declared_capacity() {
        let (mut tx, _lane) = bounded_lane(2);
        tx.try_send(job()).unwrap();
        tx.try_send(job()).unwrap();

        assert!(tx.try_send(job()).is_err());
    }

    #[test]
    fn terminal_input_saturation_rejects_new_bytes_without_losing_accepted_bytes() {
        let (tx, _lane) = terminal_input_channel(4);
        let pane_id = uuid::Uuid::nil();

        assert!(tx.try_send(pane_id, b"ab").is_ok());
        assert!(tx.try_send(pane_id, b"cd").is_ok());
        assert!(tx.try_send(pane_id, b"e").is_err());

        let queued = tx.drain_for_test();
        assert_eq!(queued, vec![(pane_id, b"abcd".to_vec())]);
    }
}
