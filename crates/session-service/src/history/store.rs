use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use hh_protocol::{
    HistoryArchiveStatus, HistoryCleanupPolicy, HistoryClearScope, HistoryCursor,
    HistoryPageDirection, HistoryPageFlags, HistoryRetention, HistorySettings, HistoryWarning,
    TerminalHistoryPage,
};
use parking_lot::RwLock;
use serde::Serialize;
use uuid::Uuid;

use super::chunk::{
    CHUNK_HEADER_BYTES, CHUNK_PAYLOAD_BYTES, read_chunk, terminal_output_lines, write_chunk_atomic,
};
use super::{
    ARCHIVE_RECONCILE_INTERVAL, CONFIG_SCHEMA, Command, ConfigFile, Manifest,
    RETENTION_SWEEP_INTERVAL, SessionMeta, WARNING_PERCENT, cleanup_temporary_files,
    directory_size, ensure_private_directory, now_ms, read_json_private, remove_directory_if_real,
    validate_query, validate_settings, write_json_atomic,
};

#[derive(Debug)]
pub(super) struct ActiveSession {
    pub(super) manifest: Manifest,
    pub(super) buffer: Vec<u8>,
    pub(super) gap_before_buffer: bool,
}

#[derive(Debug)]
pub(super) struct Store {
    pub(super) root: PathBuf,
    pub(super) settings: HistorySettings,
    pub(super) active: HashMap<Uuid, ActiveSession>,
    pub(super) status: Arc<RwLock<HistoryArchiveStatus>>,
    pub(super) dropped_bytes: Arc<AtomicU64>,
    /// On-disk archive size, maintained incrementally at every write and
    /// removal so `refresh_status` and the quota loop never re-walk the
    /// tree. Reconciled against a full walk once per started session.
    pub(super) archived_bytes: u64,
    pub(super) corrupt_chunk_seen: bool,
    pub(super) capacity_paused: bool,
    pub(super) last_retention_sweep: Option<Instant>,
    pub(super) last_archive_reconcile: Option<Instant>,
}

pub(super) fn worker_loop(store: &mut Store, receiver: &Receiver<Command>) {
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Start(meta) => {
                let _ = store.start(meta).and_then(|()| {
                    if store.reconcile_archived_bytes_if_due()? {
                        store.refresh_status()
                    } else {
                        store.refresh_status_after_start(meta);
                        Ok(())
                    }
                });
            }
            Command::Append {
                session_id,
                bytes,
                gap_before,
            } => {
                if store
                    .append(session_id, &bytes, gap_before)
                    .is_ok_and(|status_changed| status_changed)
                {
                    let _ = store.refresh_status();
                }
            }
            Command::End { session_id } => {
                let _ = store
                    .end(session_id)
                    .and_then(|()| store.apply_retention_if_due())
                    .and_then(|()| store.refresh_status());
            }
            Command::UpdateSettings(settings, reply) => {
                let result = store
                    .flush_all()
                    .and_then(|()| store.update_settings(settings))
                    .and_then(|()| store.apply_retention())
                    .and_then(|()| store.refresh_status());
                let _ = reply.send(result);
            }
            Command::Clear(scope, reply) => {
                let result = store.clear(scope).and_then(|()| store.refresh_status());
                let _ = reply.send(result);
            }
            Command::Load {
                pane_id,
                cursor,
                direction,
                reply,
            } => {
                let result = store
                    .flush_all()
                    .and_then(|()| store.apply_retention_if_due())
                    .and_then(|()| store.load_page(pane_id, cursor, direction));
                let _ = reply.send(result);
            }
            Command::Search {
                pane_id,
                query,
                before,
                reply,
            } => {
                let result = store
                    .flush_all()
                    .and_then(|()| store.apply_retention_if_due())
                    .and_then(|()| store.search(pane_id, &query, before));
                let _ = reply.send(result);
            }
            Command::Shutdown(reply) => {
                let result = store.flush_all().and_then(|()| store.refresh_status());
                let _ = reply.send(result);
                break;
            }
        }
    }
}

impl Store {
    /// Writes JSON state and folds the file-size delta into the
    /// archived-bytes counter.
    fn write_json_tracked(&mut self, path: &Path, value: &impl Serialize) -> Result<()> {
        let before = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
        write_json_atomic(path, value)?;
        let after = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
        self.archived_bytes = self
            .archived_bytes
            .saturating_add(after)
            .saturating_sub(before);
        Ok(())
    }

    /// Removes one session directory, subtracting its measured size from
    /// the archived-bytes counter.
    fn remove_session_dir_tracked(&mut self, session_id: Uuid) -> Result<()> {
        let session_path = self.session_path(session_id);
        let removed = directory_size(&session_path).unwrap_or(0);
        remove_directory_if_real(&session_path)?;
        self.archived_bytes = self.archived_bytes.saturating_sub(removed);
        Ok(())
    }

    /// Re-derives the archived-bytes counter from one full walk of the
    /// archive so drift self-heals.
    pub(super) fn reconcile_archived_bytes(&mut self) -> Result<()> {
        self.archived_bytes = directory_size(&self.root)?;
        self.last_archive_reconcile = Some(Instant::now());
        Ok(())
    }

    fn reconcile_archived_bytes_if_due(&mut self) -> Result<bool> {
        if self
            .last_archive_reconcile
            .is_some_and(|last| last.elapsed() < ARCHIVE_RECONCILE_INTERVAL)
        {
            return Ok(false);
        }
        self.reconcile_archived_bytes()?;
        Ok(true)
    }

    pub(super) fn start(&mut self, meta: SessionMeta) -> Result<()> {
        let manifest = Manifest::from_meta(meta);
        if self.settings.enabled {
            self.apply_retention_if_due()?;
            ensure_private_directory(&self.session_path(meta.session_id))?;
            self.write_json_tracked(&self.manifest_path(meta.session_id), &manifest)?;
        }
        self.active.insert(
            meta.session_id,
            ActiveSession {
                manifest,
                buffer: Vec::with_capacity(CHUNK_PAYLOAD_BYTES),
                gap_before_buffer: false,
            },
        );
        Ok(())
    }

    pub(super) fn append(
        &mut self,
        session_id: Uuid,
        bytes: &[u8],
        gap_before: u64,
    ) -> Result<bool> {
        if !self.settings.enabled {
            return Ok(false);
        }
        if !self.active.contains_key(&session_id) {
            return Ok(false);
        }
        self.apply_retention_if_due()?;
        let mut status_changed = false;
        if gap_before > 0
            && let Some(active) = self.active.get_mut(&session_id)
        {
            active.manifest.dropped_bytes =
                active.manifest.dropped_bytes.saturating_add(gap_before);
            active.manifest.has_gap = true;
            active.gap_before_buffer = true;
            status_changed = true;
        }
        let incoming = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if !self.make_capacity(incoming)? {
            if let Some(active) = self.active.get_mut(&session_id) {
                active.manifest.dropped_bytes =
                    active.manifest.dropped_bytes.saturating_add(incoming);
                active.manifest.has_gap = true;
                active.gap_before_buffer = true;
            }
            self.capacity_paused = true;
            return Ok(true);
        }
        if std::mem::take(&mut self.capacity_paused) {
            status_changed = true;
        }
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let available =
                CHUNK_PAYLOAD_BYTES.saturating_sub(self.active[&session_id].buffer.len());
            let take = available.min(remaining.len());
            self.active
                .get_mut(&session_id)
                .context("active history session disappeared")?
                .buffer
                .extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            if self.active[&session_id].buffer.len() == CHUNK_PAYLOAD_BYTES {
                self.flush_session(session_id)?;
                status_changed = true;
            }
        }
        Ok(status_changed)
    }

    pub(super) fn make_capacity(&mut self, incoming: u64) -> Result<bool> {
        let buffered = self
            .active
            .values()
            .map(|active| u64::try_from(active.buffer.len()).unwrap_or(u64::MAX))
            .sum::<u64>();
        let required = buffered
            .saturating_add(incoming)
            .saturating_add(u64::try_from(CHUNK_HEADER_BYTES + 4_096).unwrap_or(u64::MAX));
        if self.archived_bytes.saturating_add(required) <= self.settings.quota_bytes {
            return Ok(true);
        }
        if self.settings.cleanup_policy == HistoryCleanupPolicy::DeleteOldest {
            self.delete_oldest_closed_until(required)?;
            return Ok(self.archived_bytes.saturating_add(required) <= self.settings.quota_bytes);
        }
        Ok(false)
    }

    pub(super) fn flush_session(&mut self, session_id: Uuid) -> Result<()> {
        let manifest_path = self.manifest_path(session_id);
        let Some(active) = self.active.get_mut(&session_id) else {
            return Ok(());
        };
        if !self.settings.enabled && !manifest_path.exists() {
            return Ok(());
        }
        if active.buffer.is_empty() {
            let manifest = active.manifest.clone();
            return self.write_json_tracked(&manifest_path, &manifest);
        }
        let index = active.manifest.chunk_count;
        let payload = std::mem::take(&mut active.buffer);
        let gap_before = std::mem::take(&mut active.gap_before_buffer);
        let chunk_path = manifest_path
            .parent()
            .context("history manifest has no parent")?
            .join(format!("{index:08}.rmh"));
        if let Err(error) = write_chunk_atomic(&chunk_path, index, gap_before, &payload) {
            if let Some(active) = self.active.get_mut(&session_id) {
                active.manifest.dropped_bytes = active
                    .manifest
                    .dropped_bytes
                    .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
                active.manifest.has_gap = true;
                active.gap_before_buffer = true;
            }
            let _ = self.refresh_status();
            return Err(error);
        }
        let active = self
            .active
            .get_mut(&session_id)
            .context("active history session disappeared")?;
        active.manifest.chunk_count = active.manifest.chunk_count.saturating_add(1);
        active.manifest.payload_bytes = active
            .manifest
            .payload_bytes
            .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        let manifest = active.manifest.clone();
        // A chunk file is exactly its header plus payload on disk.
        self.archived_bytes = self
            .archived_bytes
            .saturating_add(u64::try_from(CHUNK_HEADER_BYTES + payload.len()).unwrap_or(u64::MAX));
        self.write_json_tracked(&manifest_path, &manifest)
    }

    pub(super) fn flush_all(&mut self) -> Result<()> {
        let ids = self.active.keys().copied().collect::<Vec<_>>();
        for session_id in ids {
            self.flush_session(session_id)?;
        }
        Ok(())
    }

    pub(super) fn end(&mut self, session_id: Uuid) -> Result<()> {
        self.flush_session(session_id)?;
        let Some(mut active) = self.active.remove(&session_id) else {
            return Ok(());
        };
        let manifest_path = self.manifest_path(session_id);
        if !manifest_path.exists() {
            return Ok(());
        }
        active.manifest.ended_ms = Some(now_ms());
        self.write_json_tracked(&manifest_path, &active.manifest)
    }

    pub(super) fn update_settings(&mut self, settings: HistorySettings) -> Result<()> {
        validate_settings(&settings)?;
        self.write_json_tracked(
            &self.root.join("config.json"),
            &ConfigFile {
                schema_version: CONFIG_SCHEMA,
                settings: settings.clone(),
            },
        )?;
        let was_enabled = self.settings.enabled;
        self.settings = settings;
        self.capacity_paused = false;
        if was_enabled != self.settings.enabled {
            let ids = self.active.keys().copied().collect::<Vec<_>>();
            for session_id in ids {
                if let Some(active) = self.active.get_mut(&session_id) {
                    active.manifest.has_gap = true;
                    active.gap_before_buffer = true;
                }
                if self.settings.enabled {
                    ensure_private_directory(&self.session_path(session_id))?;
                    let manifest = self.active[&session_id].manifest.clone();
                    self.write_json_tracked(&self.manifest_path(session_id), &manifest)?;
                } else if self.manifest_path(session_id).exists() {
                    let manifest = self.active[&session_id].manifest.clone();
                    self.write_json_tracked(&self.manifest_path(session_id), &manifest)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn clear(&mut self, scope: HistoryClearScope) -> Result<()> {
        self.flush_all()?;
        let active_meta = self
            .active
            .values()
            .map(|active| SessionMeta {
                session_id: active.manifest.session_id,
                pane_id: active.manifest.pane_id,
                workspace_id: active.manifest.workspace_id,
                started_ms: now_ms(),
            })
            .collect::<Vec<_>>();
        let active_ids = active_meta
            .iter()
            .filter(|meta| scope_matches(scope, meta.pane_id, meta.workspace_id))
            .map(|meta| meta.session_id)
            .collect::<Vec<_>>();
        for session_id in &active_ids {
            self.active.remove(session_id);
        }
        for manifest in self.manifests()? {
            if scope_matches(scope, manifest.pane_id, manifest.workspace_id) {
                self.remove_session_dir_tracked(manifest.session_id)?;
            }
        }
        for meta in active_meta
            .into_iter()
            .filter(|meta| active_ids.contains(&meta.session_id))
        {
            self.start(meta)?;
        }
        self.corrupt_chunk_seen = false;
        self.capacity_paused = false;
        self.dropped_bytes.store(0, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn load_page(
        &mut self,
        pane_id: Uuid,
        cursor: Option<HistoryCursor>,
        direction: HistoryPageDirection,
    ) -> Result<Option<TerminalHistoryPage>> {
        let manifests = self.manifests_for_pane(pane_id)?;
        self.load_page_from_manifests(pane_id, &manifests, cursor, direction)
    }

    fn load_page_from_manifests(
        &mut self,
        pane_id: Uuid,
        manifests: &[Manifest],
        cursor: Option<HistoryCursor>,
        direction: HistoryPageDirection,
    ) -> Result<Option<TerminalHistoryPage>> {
        let Some((manifest, chunk_index)) = select_chunk(manifests, cursor, direction)? else {
            return Ok(None);
        };
        let path = self.chunk_path(manifest.session_id, chunk_index);
        let (payload, gap_before, corrupt) = match read_chunk(&path, chunk_index) {
            Ok((payload, gap_before)) => (payload, gap_before, false),
            Err(error) => {
                self.corrupt_chunk_seen = true;
                eprintln!(
                    "Harness Harlot history chunk {} is corrupt: {error:#}",
                    path.display()
                );
                (Vec::new(), true, true)
            }
        };
        let lines = terminal_output_lines(&payload);
        let cursor = HistoryCursor {
            session_id: manifest.session_id,
            chunk_index,
        };
        let mut flags = 0;
        for (present, flag) in [
            (
                previous_chunk(manifests, cursor).is_some(),
                HistoryPageFlags::HAS_OLDER,
            ),
            (
                next_chunk(manifests, cursor).is_some(),
                HistoryPageFlags::HAS_NEWER,
            ),
            (gap_before, HistoryPageFlags::GAP_BEFORE),
            (
                manifest.has_gap && chunk_index + 1 == manifest.chunk_count,
                HistoryPageFlags::GAP_AFTER,
            ),
            (corrupt, HistoryPageFlags::CORRUPT),
        ] {
            if present {
                flags |= flag;
            }
        }
        Ok(Some(TerminalHistoryPage {
            pane_id,
            cursor,
            started_ms: manifest.started_ms,
            lines,
            flags: HistoryPageFlags::new(flags),
        }))
    }

    pub(super) fn search(
        &mut self,
        pane_id: Uuid,
        query: &str,
        before: Option<HistoryCursor>,
    ) -> Result<Option<TerminalHistoryPage>> {
        validate_query(query)?;
        let manifests = self.manifests_for_pane(pane_id)?;
        let mut cursor = before;
        loop {
            let Some(page) = self.load_page_from_manifests(
                pane_id,
                &manifests,
                cursor,
                HistoryPageDirection::Older,
            )?
            else {
                return Ok(None);
            };
            if page.lines.iter().any(|line| line.contains(query)) {
                return Ok(Some(page));
            }
            if !page.flags.contains(HistoryPageFlags::HAS_OLDER) {
                return Ok(None);
            }
            cursor = Some(page.cursor);
        }
    }

    pub(super) fn recover_interrupted_sessions(&mut self) -> Result<()> {
        for mut manifest in self.manifests()? {
            if manifest.ended_ms.is_none() {
                manifest.ended_ms = Some(now_ms());
                manifest.has_gap = true;
                self.write_json_tracked(&self.manifest_path(manifest.session_id), &manifest)?;
            }
        }
        cleanup_temporary_files(&self.root)?;
        Ok(())
    }

    pub(super) fn apply_retention(&mut self) -> Result<()> {
        self.last_retention_sweep = Some(Instant::now());
        if !self.settings.enabled {
            return Ok(());
        }
        let HistoryRetention::Days { days } = self.settings.retention else {
            return Ok(());
        };
        let cutoff = now_ms().saturating_sub(u64::from(days) * 24 * 60 * 60 * 1_000);
        for manifest in self.manifests()? {
            if manifest.ended_ms.is_some_and(|ended| ended < cutoff) {
                self.remove_session_dir_tracked(manifest.session_id)?;
            }
        }
        Ok(())
    }

    pub(super) fn apply_retention_if_due(&mut self) -> Result<()> {
        if !matches!(self.settings.retention, HistoryRetention::Days { .. }) {
            return Ok(());
        }
        if self
            .last_retention_sweep
            .is_some_and(|last| last.elapsed() < RETENTION_SWEEP_INTERVAL)
        {
            return Ok(());
        }
        self.apply_retention()
    }

    pub(super) fn delete_oldest_closed_until(&mut self, required: u64) -> Result<()> {
        let mut manifests = self
            .manifests()?
            .into_iter()
            .filter(|manifest| manifest.ended_ms.is_some())
            .collect::<Vec<_>>();
        manifests.sort_by_key(|manifest| manifest.started_ms);
        for manifest in manifests {
            if self.archived_bytes.saturating_add(required) <= self.settings.quota_bytes {
                break;
            }
            self.remove_session_dir_tracked(manifest.session_id)?;
        }
        Ok(())
    }

    fn current_warning(&self) -> Option<HistoryWarning> {
        if self.corrupt_chunk_seen {
            Some(HistoryWarning::CorruptChunk)
        } else if self.settings.enabled
            && (self.capacity_paused || self.archived_bytes >= self.settings.quota_bytes)
        {
            Some(HistoryWarning::PausedAtCapacity)
        } else if self.settings.enabled
            && self.archived_bytes.saturating_mul(100)
                >= self.settings.quota_bytes.saturating_mul(WARNING_PERCENT)
        {
            Some(HistoryWarning::ApproachingCapacity)
        } else {
            None
        }
    }

    fn refresh_status_after_start(&self, meta: SessionMeta) {
        let mut status = self.status.write();
        status.archived_bytes = self.archived_bytes;
        status.warning = self.current_warning();
        if self.settings.enabled {
            status.retained_sessions = status.retained_sessions.saturating_add(1);
            status.oldest_started_ms = Some(
                status
                    .oldest_started_ms
                    .map_or(meta.started_ms, |oldest| oldest.min(meta.started_ms)),
            );
        }
    }

    pub(super) fn refresh_status(&self) -> Result<()> {
        let mut manifests = self
            .manifests()?
            .into_iter()
            .map(|manifest| (manifest.session_id, manifest))
            .collect::<HashMap<_, _>>();
        for active in self.active.values() {
            if self.settings.enabled
                && (self.session_path(active.manifest.session_id).exists()
                    || active.manifest.dropped_bytes > 0)
            {
                manifests.insert(active.manifest.session_id, active.manifest.clone());
            }
        }
        let manifests = manifests.into_values().collect::<Vec<_>>();
        let archived_bytes = self.archived_bytes;
        let dropped = manifests
            .iter()
            .map(|manifest| manifest.dropped_bytes)
            .sum::<u64>();
        let warning = self.current_warning();
        let status = HistoryArchiveStatus {
            settings: self.settings.clone(),
            live_scrollback_lines: 2_000,
            archived_bytes,
            retained_sessions: u32::try_from(manifests.len()).unwrap_or(u32::MAX),
            oldest_started_ms: manifests.iter().map(|manifest| manifest.started_ms).min(),
            dropped_bytes: dropped,
            warning,
        };
        *self.status.write() = status;
        Ok(())
    }

    pub(super) fn manifests(&self) -> Result<Vec<Manifest>> {
        let mut manifests = Vec::new();
        for entry in fs::read_dir(&self.root).context("scan history sessions")? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            let manifest_path = path.join("manifest.json");
            if !manifest_path.exists() {
                continue;
            }
            match read_json_private::<Manifest>(&manifest_path).and_then(|manifest| {
                manifest.validate()?;
                Ok(manifest)
            }) {
                Ok(manifest) => manifests.push(manifest),
                Err(error) => eprintln!(
                    "ignoring invalid Harness Harlot history manifest {}: {error:#}",
                    manifest_path.display()
                ),
            }
        }
        manifests.sort_by_key(|manifest| manifest.started_ms);
        Ok(manifests)
    }

    pub(super) fn manifests_for_pane(&self, pane_id: Uuid) -> Result<Vec<Manifest>> {
        Ok(self
            .manifests()?
            .into_iter()
            .filter(|manifest| manifest.pane_id == pane_id && manifest.chunk_count > 0)
            .collect())
    }

    pub(super) fn session_path(&self, session_id: Uuid) -> PathBuf {
        self.root.join(session_id.to_string())
    }

    pub(super) fn manifest_path(&self, session_id: Uuid) -> PathBuf {
        self.session_path(session_id).join("manifest.json")
    }

    pub(super) fn chunk_path(&self, session_id: Uuid, index: u32) -> PathBuf {
        self.session_path(session_id)
            .join(format!("{index:08}.rmh"))
    }
}

fn select_chunk(
    manifests: &[Manifest],
    cursor: Option<HistoryCursor>,
    direction: HistoryPageDirection,
) -> Result<Option<(Manifest, u32)>> {
    if let Some(cursor) = cursor {
        let current = manifests
            .iter()
            .find(|manifest| manifest.session_id == cursor.session_id)
            .context("invalid history cursor session")?;
        if cursor.chunk_index >= current.chunk_count {
            bail!("invalid history cursor chunk index");
        }
        let selected = match direction {
            HistoryPageDirection::Older => previous_chunk(manifests, cursor),
            HistoryPageDirection::Newer => next_chunk(manifests, cursor),
        };
        let Some(selected) = selected else {
            return Ok(None);
        };
        let manifest = manifests
            .iter()
            .find(|manifest| manifest.session_id == selected.session_id)
            .context("selected history cursor session disappeared")?;
        return Ok(Some((manifest.clone(), selected.chunk_index)));
    }
    let selected = match direction {
        HistoryPageDirection::Older => {
            let Some(manifest) = manifests.last() else {
                return Ok(None);
            };
            manifest
                .chunk_count
                .checked_sub(1)
                .map(|index| (manifest.clone(), index))
        }
        HistoryPageDirection::Newer => manifests
            .first()
            .filter(|manifest| manifest.chunk_count > 0)
            .map(|manifest| (manifest.clone(), 0)),
    };
    Ok(selected)
}

fn previous_chunk(manifests: &[Manifest], cursor: HistoryCursor) -> Option<HistoryCursor> {
    let position = manifests
        .iter()
        .position(|manifest| manifest.session_id == cursor.session_id)?;
    let current = &manifests[position];
    if cursor.chunk_index >= current.chunk_count {
        return None;
    }
    if cursor.chunk_index > 0 {
        return Some(HistoryCursor {
            session_id: cursor.session_id,
            chunk_index: cursor.chunk_index - 1,
        });
    }
    let manifest = manifests.get(position.checked_sub(1)?)?;
    Some(HistoryCursor {
        session_id: manifest.session_id,
        chunk_index: manifest.chunk_count.checked_sub(1)?,
    })
}

fn next_chunk(manifests: &[Manifest], cursor: HistoryCursor) -> Option<HistoryCursor> {
    let position = manifests
        .iter()
        .position(|manifest| manifest.session_id == cursor.session_id)?;
    let manifest = &manifests[position];
    if cursor.chunk_index >= manifest.chunk_count {
        return None;
    }
    let next_index = cursor.chunk_index.checked_add(1)?;
    if next_index < manifest.chunk_count {
        return Some(HistoryCursor {
            session_id: cursor.session_id,
            chunk_index: next_index,
        });
    }
    let manifest = manifests.get(position.checked_add(1)?)?;
    (manifest.chunk_count > 0).then_some(HistoryCursor {
        session_id: manifest.session_id,
        chunk_index: 0,
    })
}

fn scope_matches(scope: HistoryClearScope, pane_id: Uuid, workspace_id: Uuid) -> bool {
    match scope {
        HistoryClearScope::Terminal { pane_id: target } => pane_id == target,
        HistoryClearScope::Workspace {
            workspace_id: target,
        } => workspace_id == target,
        HistoryClearScope::All => true,
    }
}
