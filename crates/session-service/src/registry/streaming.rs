//! Pane streaming, notifications, and diagnostics sampling.
use super::{PaneUpdateBatch, SessionRegistry, serialized_len, snapshot_with_runtime_transports};
use crate::registry::identity::refresh_runtime_metadata;
use anyhow::{Result, bail};
use hh_protocol::{
    MAX_PANES, PaneRevisionCursor, PaneStreamState, SessionNotification, SessionSnapshot,
    StreamDiagnostics, TerminalScreen,
};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use uuid::Uuid;

pub(crate) const DIAGNOSTICS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Byte budget for terminal screens in one `GetUpdates` response. A single
/// wire frame is capped at 4 MiB, and 32 max-size dirty screens can exceed
/// it, which would turn every response into an encode error and a permanent
/// reconnect loop; this budget leaves headroom for the snapshot and framing.
const RESPONSE_SCREEN_BUDGET_BYTES: u64 = 3 * 1024 * 1024;

/// Trims collected screens to [`RESPONSE_SCREEN_BUDGET_BYTES`], returning the
/// survivors and the pane IDs withheld. An individually oversized screen is
/// withheld rather than creating a frame the protocol must reject.
fn screens_within_budget(screens: Vec<TerminalScreen>) -> (Vec<TerminalScreen>, Vec<Uuid>) {
    let mut included = Vec::with_capacity(screens.len());
    let mut total = 0_u64;
    let mut withheld = Vec::new();
    for screen in screens {
        let size = serialized_len(&screen).unwrap_or(u64::MAX);
        if total.saturating_add(size) > RESPONSE_SCREEN_BUDGET_BYTES {
            withheld.push(screen.pane_id);
            continue;
        }
        total = total.saturating_add(size);
        included.push(screen);
    }
    (included, withheld)
}

fn preserve_withheld_cursors(
    pane_states: &mut [PaneStreamState],
    withheld: &[Uuid],
    known_revisions: &HashMap<Uuid, u64>,
) {
    for pane in pane_states {
        if withheld.contains(&pane.pane_id) {
            pane.revision = known_revisions.get(&pane.pane_id).copied().unwrap_or(0);
            pane.dirty = true;
        }
    }
}

#[derive(Debug)]
pub(crate) struct DiagnosticsSampler {
    system: System,
    last_refresh: Option<Instant>,
    cpu_milli_percent: u32,
    memory_bytes: u64,
}

impl Default for DiagnosticsSampler {
    fn default() -> Self {
        Self {
            system: System::new(),
            last_refresh: None,
            cpu_milli_percent: 0,
            memory_bytes: 0,
        }
    }
}

// Sampling diagnostics rounds a saturated percentage into a bounded u32.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub(crate) fn cpu_milli_percent(percent: f32) -> u32 {
    (percent.max(0.0) * 1_000.0).round().min(u32::MAX as f32) as u32
}

impl SessionRegistry {
    pub fn state(&self) -> Result<(SessionSnapshot, Vec<TerminalScreen>)> {
        let state = self.state.read();
        let snapshot = snapshot_with_runtime_transports(&state);
        let screens = state
            .panes
            .iter()
            .filter_map(|(pane_id, runtime)| {
                runtime
                    .terminal()
                    .map(|terminal| terminal.session.screen(*pane_id))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((snapshot, screens))
    }

    pub(crate) fn refresh_pending_pane_events(&self) {
        let (has_pending_events, has_finished_reader) = {
            let state = self.state.read();
            let has_pending_events = state.panes.values().any(|runtime| {
                runtime
                    .terminal()
                    .is_some_and(|terminal| terminal.session.has_pending_events())
            });
            let has_finished_reader = state.panes.values().any(|runtime| {
                runtime.terminal().is_some_and(|terminal| {
                    terminal.exit_status.is_none() && terminal.session.reader_is_finished()
                })
            });
            (has_pending_events, has_finished_reader)
        };
        if has_pending_events || has_finished_reader {
            let mut state = self.state.write();
            if has_finished_reader {
                refresh_runtime_metadata(&mut state);
            }
            state.drain_pane_events();
        }
    }

    /// Builds one coalesced receiver update without serializing unchanged or
    /// unsubscribed terminal screens. PTY reader threads continue advancing
    /// terminal models independently of this method.
    ///
    /// `measure_bytes` opts into the `snapshot_bytes`/`screen_bytes`
    /// diagnostics, each of which costs a full extra serialization of the
    /// payload. The socket path leaves it off; tests that assert on payload
    /// size turn it on.
    pub fn pane_updates(
        &self,
        snapshot_revision: Option<u64>,
        pane_revisions: &[PaneRevisionCursor],
        subscribed_panes: &[Uuid],
        measure_bytes: bool,
        notifications_after: u64,
    ) -> Result<PaneUpdateBatch> {
        if pane_revisions.len() > MAX_PANES || subscribed_panes.len() > MAX_PANES {
            bail!("pane update request exceeds the {MAX_PANES}-pane limit");
        }
        let started = Instant::now();
        let known_revisions = pane_revisions
            .iter()
            .map(|cursor| (cursor.pane_id, cursor.revision))
            .collect::<HashMap<_, _>>();
        let subscribed = subscribed_panes.iter().copied().collect::<HashSet<_>>();
        self.refresh_pending_pane_events();
        let state = self.state.read();

        let session_revision = state.snapshot.revision;
        let snapshot = (snapshot_revision != Some(session_revision))
            .then(|| snapshot_with_runtime_transports(&state));
        let mut screens = Vec::new();
        let mut pane_states = Vec::with_capacity(state.panes.len());
        let mut coalesced_revisions = 0_u64;
        for (pane_id, runtime) in &state.panes {
            let Some(runtime) = runtime.terminal() else {
                pane_states.push(PaneStreamState {
                    pane_id: *pane_id,
                    revision: 0,
                    subscribed: false,
                    dirty: false,
                    exited: false,
                });
                continue;
            };
            let subscribed = subscribed.contains(pane_id);
            let known_revision = known_revisions.get(pane_id).copied();
            let observed_revision = runtime.session.current_revision();
            let changed = known_revision != Some(observed_revision);
            let delivered = subscribed && changed;
            let revision = if delivered {
                let screen = runtime.session.screen(*pane_id)?;
                let revision = screen.revision;
                if let Some(known) = known_revision {
                    coalesced_revisions = coalesced_revisions
                        .saturating_add(revision.saturating_sub(known).saturating_sub(1));
                }
                screens.push(screen);
                revision
            } else {
                observed_revision
            };
            pane_states.push(PaneStreamState {
                pane_id: *pane_id,
                revision,
                subscribed,
                dirty: !delivered && known_revision != Some(revision),
                exited: runtime.exit_status.is_some(),
            });
        }
        let notifications = state
            .notifications
            .iter()
            .filter(|notification| notification.id > notifications_after)
            .cloned()
            .collect();
        drop(state);

        pane_states.sort_unstable_by_key(|pane| pane.pane_id);
        screens.sort_unstable_by_key(|screen| screen.pane_id);
        let (screens, withheld) = screens_within_budget(screens);
        preserve_withheld_cursors(&mut pane_states, &withheld, &known_revisions);
        let screens_queued = screens.len().saturating_add(withheld.len());
        let snapshot_bytes = if measure_bytes {
            snapshot
                .as_ref()
                .map(serialized_len)
                .transpose()?
                .unwrap_or(0)
        } else {
            0
        };
        let screen_bytes = if measure_bytes {
            screens.iter().try_fold(0_u64, |total, screen| {
                Ok::<_, anyhow::Error>(total.saturating_add(serialized_len(screen)?))
            })?
        } else {
            0
        };
        let (service_cpu_milli_percent, service_memory_bytes) = self.service_metrics();
        let diagnostics = StreamDiagnostics {
            panes_considered: u32::try_from(pane_states.len()).unwrap_or(u32::MAX),
            panes_subscribed: u32::try_from(
                pane_states.iter().filter(|pane| pane.subscribed).count(),
            )
            .unwrap_or(u32::MAX),
            screens_queued: u32::try_from(screens_queued).unwrap_or(u32::MAX),
            screens_delivered: u32::try_from(screens.len()).unwrap_or(u32::MAX),
            coalesced_revisions,
            snapshot_bytes,
            screen_bytes,
            preparation_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            desktop_apply_micros: 0,
            service_cpu_milli_percent,
            service_memory_bytes,
        };
        Ok(PaneUpdateBatch {
            session_revision,
            snapshot,
            screens,
            pane_states,
            notifications,
            diagnostics,
        })
    }
    pub fn notifications(&self) -> Result<Vec<SessionNotification>> {
        let mut state = self.state.write();
        state.drain_pane_events();
        Ok(state.notifications.iter().cloned().collect())
    }

    pub fn mark_notifications_read(&self, ids: &[u64]) {
        let ids = ids.iter().copied().collect::<HashSet<_>>();
        let mut state = self.state.write();
        for notification in &mut state.notifications {
            if ids.contains(&notification.id) {
                notification.read = true;
            }
        }
    }

    pub fn clear_notifications(&self) {
        let mut state = self.state.write();
        state.drain_pane_events();
        state.notifications.clear();
    }

    /// Returns one current screen for deterministic focus/reconnect resync.
    pub fn pane_snapshot(&self, pane_id: Uuid) -> Result<(TerminalScreen, StreamDiagnostics)> {
        let started = Instant::now();
        let screen = self.pane(pane_id)?.screen(pane_id)?;
        let screen_bytes = serialized_len(&screen)?;
        let (service_cpu_milli_percent, service_memory_bytes) = self.service_metrics();
        Ok((
            screen,
            StreamDiagnostics {
                panes_considered: 1,
                panes_subscribed: 1,
                screens_queued: 1,
                screens_delivered: 1,
                screen_bytes,
                preparation_micros: u64::try_from(started.elapsed().as_micros())
                    .unwrap_or(u64::MAX),
                service_cpu_milli_percent,
                service_memory_bytes,
                ..StreamDiagnostics::default()
            },
        ))
    }

    pub(crate) fn service_metrics(&self) -> (u32, u64) {
        let pid = Pid::from_u32(std::process::id());
        let mut sampler = self.diagnostics_sampler.lock();
        let now = Instant::now();
        let should_refresh = sampler
            .last_refresh
            .is_none_or(|last| now.saturating_duration_since(last) >= DIAGNOSTICS_SAMPLE_INTERVAL);
        if should_refresh {
            sampler.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                ProcessRefreshKind::new().with_cpu().with_memory(),
            );
            if let Some((cpu_milli_percent, memory_bytes)) = sampler
                .system
                .process(pid)
                .map(|process| (cpu_milli_percent(process.cpu_usage()), process.memory()))
            {
                sampler.cpu_milli_percent = cpu_milli_percent;
                sampler.memory_bytes = memory_bytes;
            }
            sampler.last_refresh = Some(now);
        }
        (sampler.cpu_milli_percent, sampler.memory_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::SessionRegistry;
    use hh_protocol::{TerminalAttributes, TerminalColor, TerminalLine, TerminalRun};
    use uuid::Uuid;

    #[test]
    fn pane_update_vectors_are_bounded_independently() {
        let registry = SessionRegistry::new().unwrap();
        let subscriptions = vec![Uuid::nil(); MAX_PANES + 1];
        assert!(
            registry
                .pane_updates(None, &[], &subscriptions, false, 0)
                .is_err()
        );
        let revisions = vec![
            PaneRevisionCursor {
                pane_id: Uuid::nil(),
                revision: 0,
            };
            MAX_PANES + 1
        ];
        assert!(
            registry
                .pane_updates(None, &revisions, &[], false, 0)
                .is_err()
        );
    }

    fn fabricated_screen(pane_id: Uuid, text_bytes: usize) -> TerminalScreen {
        TerminalScreen {
            pane_id,
            revision: 1,
            columns: 100,
            rows: 30,
            lines: vec![TerminalLine {
                runs: vec![TerminalRun {
                    text: "a".repeat(text_bytes),
                    columns: 1,
                    foreground: TerminalColor::DefaultForeground,
                    background: TerminalColor::DefaultBackground,
                    attributes: TerminalAttributes::new(0),
                }],
            }],
            cursor: None,
            selection: None,
            display_offset: 0,
            history_size: 0,
            modes: hh_protocol::TerminalModes::new(0),
        }
    }

    #[test]
    fn screens_beyond_the_response_budget_are_withheld_without_advancing_cursors() {
        // Three ~1.4 MiB screens: the first two fit within the 3 MiB
        // budget, the third is withheld.
        let first = Uuid::nil();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        let screens = vec![
            fabricated_screen(first, 1_400_000),
            fabricated_screen(second, 1_400_000),
            fabricated_screen(third, 1_400_000),
        ];
        let (included, withheld) = screens_within_budget(screens);
        assert_eq!(included.len(), 2);
        assert_eq!(withheld, vec![third]);
        let mut pane_states = vec![PaneStreamState {
            pane_id: third,
            revision: 9,
            subscribed: true,
            dirty: false,
            exited: false,
        }];
        preserve_withheld_cursors(&mut pane_states, &withheld, &HashMap::from([(third, 4)]));
        assert_eq!(pane_states[0].revision, 4);
        assert!(pane_states[0].dirty);

        // An individually oversized screen is withheld rather than producing
        // a frame that exceeds the protocol ceiling.
        let screens = vec![fabricated_screen(first, 4_000_000)];
        let (included, withheld) = screens_within_budget(screens);
        assert!(included.is_empty());
        assert_eq!(withheld, vec![first]);
    }
}
