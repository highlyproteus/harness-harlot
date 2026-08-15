use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use hh_protocol::{
    HistoryArchiveStatus, HistoryCleanupPolicy, HistoryClearScope, HistoryCursor,
    HistoryPageDirection, HistoryPageFlags, HistoryRetention, HistorySettings, HistoryWarning,
    TerminalHistoryPage,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const CONFIG_SCHEMA: u16 = 1;
const MANIFEST_SCHEMA: u16 = 1;
const CHUNK_MAGIC: &[u8; 8] = b"RMUXHST1";
const CHUNK_VERSION: u16 = 1;
const CHUNK_HEADER_BYTES: usize = 28;
const CHUNK_PAYLOAD_BYTES: usize = 128 * 1024;
const QUEUE_CAPACITY: usize = 256;
const MAX_LINE_CHARS: usize = 4096;
const MIN_QUOTA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_QUOTA_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;
const WARNING_PERCENT: u64 = 80;
const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_mins(1);

#[derive(Clone, Debug)]
pub(crate) struct HistoryArchive {
    sender: SyncSender<Command>,
    status: Arc<RwLock<HistoryArchiveStatus>>,
    dropped_bytes: Arc<AtomicU64>,
    enabled: Arc<AtomicBool>,
    _lifecycle: Arc<HistoryLifecycle>,
}

#[derive(Debug)]
struct HistoryLifecycle {
    sender: SyncSender<Command>,
    worker: std::sync::Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Debug)]
pub(crate) struct HistorySink {
    sender: SyncSender<Command>,
    session_id: Uuid,
    pending_gap: AtomicU64,
    dropped_bytes: Arc<AtomicU64>,
    enabled: Arc<AtomicBool>,
    ended: AtomicBool,
}

#[derive(Clone, Copy, Debug)]
struct SessionMeta {
    session_id: Uuid,
    pane_id: Uuid,
    workspace_id: Uuid,
    started_ms: u64,
}

#[derive(Debug)]
enum Command {
    Start(SessionMeta),
    Append {
        session_id: Uuid,
        bytes: Vec<u8>,
        gap_before: u64,
    },
    End {
        session_id: Uuid,
    },
    UpdateSettings(HistorySettings, mpsc::Sender<Result<()>>),
    Clear(HistoryClearScope, mpsc::Sender<Result<()>>),
    Load {
        pane_id: Uuid,
        cursor: Option<HistoryCursor>,
        direction: HistoryPageDirection,
        reply: mpsc::Sender<Result<Option<TerminalHistoryPage>>>,
    },
    Search {
        pane_id: Uuid,
        query: String,
        before: Option<HistoryCursor>,
        reply: mpsc::Sender<Result<Option<TerminalHistoryPage>>>,
    },
    Shutdown(mpsc::Sender<Result<()>>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    schema_version: u16,
    settings: HistorySettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u16,
    session_id: Uuid,
    pane_id: Uuid,
    workspace_id: Uuid,
    started_ms: u64,
    ended_ms: Option<u64>,
    chunk_count: u32,
    payload_bytes: u64,
    dropped_bytes: u64,
    has_gap: bool,
}

impl Manifest {
    fn from_meta(meta: SessionMeta) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA,
            session_id: meta.session_id,
            pane_id: meta.pane_id,
            workspace_id: meta.workspace_id,
            started_ms: meta.started_ms,
            ended_ms: None,
            chunk_count: 0,
            payload_bytes: 0,
            dropped_bytes: 0,
            has_gap: false,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != MANIFEST_SCHEMA {
            bail!(
                "unsupported history manifest schema {}",
                self.schema_version
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ActiveSession {
    manifest: Manifest,
    buffer: Vec<u8>,
    gap_before_buffer: bool,
}

#[derive(Debug)]
struct Store {
    root: PathBuf,
    settings: HistorySettings,
    active: HashMap<Uuid, ActiveSession>,
    status: Arc<RwLock<HistoryArchiveStatus>>,
    dropped_bytes: Arc<AtomicU64>,
    corrupt_chunk_seen: bool,
    capacity_paused: bool,
    last_retention_sweep: Option<Instant>,
}

impl HistoryArchive {
    pub(crate) fn disabled() -> Self {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let status = Arc::new(RwLock::new(HistoryArchiveStatus {
            settings: HistorySettings {
                enabled: false,
                ..HistorySettings::default()
            },
            live_scrollback_lines: 2_000,
            archived_bytes: 0,
            retained_sessions: 0,
            oldest_started_ms: None,
            dropped_bytes: 0,
            warning: None,
        }));
        let dropped_bytes = Arc::new(AtomicU64::new(0));
        let enabled = Arc::new(AtomicBool::new(false));
        let lifecycle = Arc::new(HistoryLifecycle {
            sender: sender.clone(),
            worker: std::sync::Mutex::new(None),
        });
        drop(receiver);
        Self {
            sender,
            status,
            dropped_bytes,
            enabled,
            _lifecycle: lifecycle,
        }
    }

    pub(crate) fn open(root: PathBuf) -> Result<Self> {
        if !root.is_absolute() {
            bail!("history directory must be absolute");
        }
        ensure_private_directory(&root)?;
        let settings = load_settings(&root)?.unwrap_or_default();
        validate_settings(&settings)?;
        let status = Arc::new(RwLock::new(empty_status(settings.clone())));
        let dropped_bytes = Arc::new(AtomicU64::new(0));
        let enabled = Arc::new(AtomicBool::new(settings.enabled));
        let mut store = Store {
            root,
            settings,
            active: HashMap::new(),
            status: Arc::clone(&status),
            dropped_bytes: Arc::clone(&dropped_bytes),
            corrupt_chunk_seen: false,
            capacity_paused: false,
            last_retention_sweep: None,
        };
        store.recover_interrupted_sessions()?;
        store.apply_retention()?;
        store.refresh_status()?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let worker = thread::Builder::new()
            .name("rmux-history-writer".to_owned())
            .spawn(move || worker_loop(&mut store, &receiver))
            .context("spawn local history writer")?;
        let lifecycle = Arc::new(HistoryLifecycle {
            sender: sender.clone(),
            worker: std::sync::Mutex::new(Some(worker)),
        });
        Ok(Self {
            sender,
            status,
            dropped_bytes,
            enabled,
            _lifecycle: lifecycle,
        })
    }

    pub(crate) fn start_session(&self, pane_id: Uuid, workspace_id: Uuid) -> HistorySink {
        let session_id = Uuid::new_v4();
        let meta = SessionMeta {
            session_id,
            pane_id,
            workspace_id,
            started_ms: now_ms(),
        };
        if self.sender.send(Command::Start(meta)).is_err() {
            self.dropped_bytes.fetch_add(1, Ordering::Relaxed);
        }
        HistorySink {
            sender: self.sender.clone(),
            session_id,
            pending_gap: AtomicU64::new(0),
            dropped_bytes: Arc::clone(&self.dropped_bytes),
            enabled: Arc::clone(&self.enabled),
            ended: AtomicBool::new(false),
        }
    }

    pub(crate) fn status(&self) -> HistoryArchiveStatus {
        let mut status = self.status.read().clone();
        status.dropped_bytes = status
            .dropped_bytes
            .saturating_add(self.dropped_bytes.load(Ordering::Relaxed));
        if status.dropped_bytes > 0 && status.warning.is_none() {
            status.warning = Some(HistoryWarning::QueueOverflow);
        }
        status
    }

    pub(crate) fn update_settings(&self, settings: HistorySettings) -> Result<()> {
        validate_settings(&settings)?;
        let enabled = settings.enabled;
        let (reply, receive) = mpsc::channel();
        self.sender
            .send(Command::UpdateSettings(settings, reply))
            .map_err(|_| anyhow!("history writer stopped"))?;
        let result = receive
            .recv()
            .map_err(|_| anyhow!("history writer stopped"))?;
        if result.is_ok() {
            self.enabled.store(enabled, Ordering::Release);
        }
        result
    }

    pub(crate) fn clear(&self, scope: HistoryClearScope) -> Result<()> {
        let (reply, receive) = mpsc::channel();
        self.sender
            .send(Command::Clear(scope, reply))
            .map_err(|_| anyhow!("history writer stopped"))?;
        receive
            .recv()
            .map_err(|_| anyhow!("history writer stopped"))?
    }

    pub(crate) fn load_page(
        &self,
        pane_id: Uuid,
        cursor: Option<HistoryCursor>,
        direction: HistoryPageDirection,
    ) -> Result<Option<TerminalHistoryPage>> {
        let (reply, receive) = mpsc::channel();
        self.sender
            .send(Command::Load {
                pane_id,
                cursor,
                direction,
                reply,
            })
            .map_err(|_| anyhow!("history writer stopped"))?;
        receive
            .recv()
            .map_err(|_| anyhow!("history writer stopped"))?
    }

    pub(crate) fn search(
        &self,
        pane_id: Uuid,
        query: &str,
        before: Option<HistoryCursor>,
    ) -> Result<Option<TerminalHistoryPage>> {
        validate_query(query)?;
        let (reply, receive) = mpsc::channel();
        self.sender
            .send(Command::Search {
                pane_id,
                query: query.to_owned(),
                before,
                reply,
            })
            .map_err(|_| anyhow!("history writer stopped"))?;
        receive
            .recv()
            .map_err(|_| anyhow!("history writer stopped"))?
    }
}

impl Drop for HistoryLifecycle {
    fn drop(&mut self) {
        let (reply, receive) = mpsc::channel();
        if self.sender.send(Command::Shutdown(reply)).is_ok() {
            let _ = receive.recv();
        }
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl HistorySink {
    pub(crate) fn record(&self, bytes: &[u8]) {
        if bytes.is_empty()
            || self.ended.load(Ordering::Relaxed)
            || !self.enabled.load(Ordering::Acquire)
        {
            return;
        }
        let gap_before = self.pending_gap.swap(0, Ordering::AcqRel);
        let command = Command::Append {
            session_id: self.session_id,
            bytes: bytes.to_vec(),
            gap_before,
        };
        match self.sender.try_send(command) {
            Ok(()) => {
                if gap_before > 0 {
                    self.dropped_bytes.fetch_sub(gap_before, Ordering::Relaxed);
                }
            }
            Err(
                TrySendError::Full(Command::Append {
                    bytes, gap_before, ..
                })
                | TrySendError::Disconnected(Command::Append {
                    bytes, gap_before, ..
                }),
            ) => {
                let dropped = u64::try_from(bytes.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(gap_before);
                self.pending_gap.fetch_add(dropped, Ordering::Relaxed);
                self.dropped_bytes.fetch_add(dropped, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => unreachable!(),
        }
    }

    pub(crate) fn finish(&self) {
        if !self.ended.swap(true, Ordering::AcqRel) {
            let _ = self.sender.send(Command::End {
                session_id: self.session_id,
            });
        }
    }
}

impl Drop for HistorySink {
    fn drop(&mut self) {
        self.finish();
    }
}

fn worker_loop(store: &mut Store, receiver: &Receiver<Command>) {
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Start(meta) => {
                let _ = store.start(meta).and_then(|()| store.refresh_status());
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
                    .and_then(|()| store.apply_retention())
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
                    .and_then(|()| store.apply_retention())
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
                    .and_then(|()| store.apply_retention())
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
    let _ = store.flush_all();
}

impl Store {
    fn start(&mut self, meta: SessionMeta) -> Result<()> {
        let manifest = Manifest::from_meta(meta);
        if self.settings.enabled {
            self.apply_retention()?;
            ensure_private_directory(&self.session_path(meta.session_id))?;
            write_json_atomic(&self.manifest_path(meta.session_id), &manifest)?;
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

    fn append(&mut self, session_id: Uuid, bytes: &[u8], gap_before: u64) -> Result<bool> {
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

    fn make_capacity(&mut self, incoming: u64) -> Result<bool> {
        let archived = self.status.read().archived_bytes;
        let buffered = self
            .active
            .values()
            .map(|active| u64::try_from(active.buffer.len()).unwrap_or(u64::MAX))
            .sum::<u64>();
        let projected = archived
            .saturating_add(buffered)
            .saturating_add(incoming)
            .saturating_add(u64::try_from(CHUNK_HEADER_BYTES + 4_096).unwrap_or(u64::MAX));
        if projected <= self.settings.quota_bytes {
            return Ok(true);
        }
        if self.settings.cleanup_policy == HistoryCleanupPolicy::DeleteOldest {
            self.delete_oldest_closed_until(incoming)?;
            let archived = directory_size(&self.root)?;
            return Ok(archived
                .saturating_add(buffered)
                .saturating_add(incoming)
                .saturating_add(u64::try_from(CHUNK_HEADER_BYTES + 4_096).unwrap_or(u64::MAX))
                <= self.settings.quota_bytes);
        }
        Ok(false)
    }

    fn flush_session(&mut self, session_id: Uuid) -> Result<()> {
        let manifest_path = self.manifest_path(session_id);
        let Some(active) = self.active.get_mut(&session_id) else {
            return Ok(());
        };
        if active.buffer.is_empty() {
            write_json_atomic(&manifest_path, &active.manifest)?;
            return Ok(());
        }
        let index = active.manifest.chunk_count;
        let chunk_path = manifest_path
            .parent()
            .context("history manifest has no parent")?
            .join(format!("{index:08}.rmh"));
        let payload = std::mem::take(&mut active.buffer);
        let gap_before = std::mem::take(&mut active.gap_before_buffer);
        write_chunk_atomic(&chunk_path, index, gap_before, &payload)?;
        active.manifest.chunk_count = active.manifest.chunk_count.saturating_add(1);
        active.manifest.payload_bytes = active
            .manifest
            .payload_bytes
            .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        write_json_atomic(&manifest_path, &active.manifest)
    }

    fn flush_all(&mut self) -> Result<()> {
        let ids = self.active.keys().copied().collect::<Vec<_>>();
        for session_id in ids {
            self.flush_session(session_id)?;
        }
        Ok(())
    }

    fn end(&mut self, session_id: Uuid) -> Result<()> {
        self.flush_session(session_id)?;
        let Some(mut active) = self.active.remove(&session_id) else {
            return Ok(());
        };
        active.manifest.ended_ms = Some(now_ms());
        write_json_atomic(&self.manifest_path(session_id), &active.manifest)
    }

    fn update_settings(&mut self, settings: HistorySettings) -> Result<()> {
        validate_settings(&settings)?;
        write_json_atomic(
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
                    let manifest = &self.active[&session_id].manifest;
                    write_json_atomic(&self.manifest_path(session_id), manifest)?;
                } else if self.manifest_path(session_id).exists() {
                    let manifest = &self.active[&session_id].manifest;
                    write_json_atomic(&self.manifest_path(session_id), manifest)?;
                }
            }
        }
        Ok(())
    }

    fn clear(&mut self, scope: HistoryClearScope) -> Result<()> {
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
                remove_directory_if_real(&self.session_path(manifest.session_id))?;
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

    fn load_page(
        &mut self,
        pane_id: Uuid,
        cursor: Option<HistoryCursor>,
        direction: HistoryPageDirection,
    ) -> Result<Option<TerminalHistoryPage>> {
        let manifests = self.manifests_for_pane(pane_id)?;
        let Some((manifest, chunk_index)) = select_chunk(&manifests, cursor, direction)? else {
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
                previous_chunk(&manifests, cursor).is_some(),
                HistoryPageFlags::HAS_OLDER,
            ),
            (
                next_chunk(&manifests, cursor).is_some(),
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

    fn search(
        &mut self,
        pane_id: Uuid,
        query: &str,
        before: Option<HistoryCursor>,
    ) -> Result<Option<TerminalHistoryPage>> {
        validate_query(query)?;
        let manifests = self.manifests_for_pane(pane_id)?;
        let mut cursor = before;
        loop {
            let Some(page) = self.load_page(pane_id, cursor, HistoryPageDirection::Older)? else {
                return Ok(None);
            };
            if page.lines.iter().any(|line| line.contains(query)) {
                return Ok(Some(page));
            }
            if !page.flags.contains(HistoryPageFlags::HAS_OLDER) {
                return Ok(None);
            }
            cursor = Some(page.cursor);
            if previous_chunk(&manifests, page.cursor).is_none() {
                return Ok(None);
            }
        }
    }

    fn recover_interrupted_sessions(&mut self) -> Result<()> {
        for mut manifest in self.manifests()? {
            if manifest.ended_ms.is_none() {
                manifest.ended_ms = Some(now_ms());
                manifest.has_gap = true;
                write_json_atomic(&self.manifest_path(manifest.session_id), &manifest)?;
            }
        }
        cleanup_temporary_files(&self.root)?;
        Ok(())
    }

    fn apply_retention(&mut self) -> Result<()> {
        self.last_retention_sweep = Some(Instant::now());
        let HistoryRetention::Days { days } = self.settings.retention else {
            return Ok(());
        };
        let cutoff = now_ms().saturating_sub(u64::from(days) * 24 * 60 * 60 * 1_000);
        for manifest in self.manifests()? {
            if manifest.ended_ms.is_some_and(|ended| ended < cutoff) {
                remove_directory_if_real(&self.session_path(manifest.session_id))?;
            }
        }
        Ok(())
    }

    fn apply_retention_if_due(&mut self) -> Result<()> {
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

    fn delete_oldest_closed_until(&mut self, incoming: u64) -> Result<()> {
        let mut manifests = self
            .manifests()?
            .into_iter()
            .filter(|manifest| manifest.ended_ms.is_some())
            .collect::<Vec<_>>();
        manifests.sort_by_key(|manifest| manifest.started_ms);
        for manifest in manifests {
            if directory_size(&self.root)?.saturating_add(incoming) <= self.settings.quota_bytes {
                break;
            }
            remove_directory_if_real(&self.session_path(manifest.session_id))?;
        }
        Ok(())
    }

    fn refresh_status(&self) -> Result<()> {
        let mut manifests = self
            .manifests()?
            .into_iter()
            .map(|manifest| (manifest.session_id, manifest))
            .collect::<HashMap<_, _>>();
        for active in self.active.values() {
            if self.session_path(active.manifest.session_id).exists() {
                manifests.insert(active.manifest.session_id, active.manifest.clone());
            }
        }
        let manifests = manifests.into_values().collect::<Vec<_>>();
        let archived_bytes = directory_size(&self.root)?;
        let dropped = manifests
            .iter()
            .map(|manifest| manifest.dropped_bytes)
            .sum::<u64>();
        let warning = if self.corrupt_chunk_seen {
            Some(HistoryWarning::CorruptChunk)
        } else if self.settings.enabled
            && (self.capacity_paused || archived_bytes >= self.settings.quota_bytes)
        {
            Some(HistoryWarning::PausedAtCapacity)
        } else if self.settings.enabled
            && archived_bytes.saturating_mul(100)
                >= self.settings.quota_bytes.saturating_mul(WARNING_PERCENT)
        {
            Some(HistoryWarning::ApproachingCapacity)
        } else {
            None
        };
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

    fn manifests(&self) -> Result<Vec<Manifest>> {
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

    fn manifests_for_pane(&self, pane_id: Uuid) -> Result<Vec<Manifest>> {
        Ok(self
            .manifests()?
            .into_iter()
            .filter(|manifest| manifest.pane_id == pane_id && manifest.chunk_count > 0)
            .collect())
    }

    fn session_path(&self, session_id: Uuid) -> PathBuf {
        self.root.join(session_id.to_string())
    }

    fn manifest_path(&self, session_id: Uuid) -> PathBuf {
        self.session_path(session_id).join("manifest.json")
    }

    fn chunk_path(&self, session_id: Uuid, index: u32) -> PathBuf {
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

fn empty_status(settings: HistorySettings) -> HistoryArchiveStatus {
    HistoryArchiveStatus {
        settings,
        live_scrollback_lines: 2_000,
        archived_bytes: 0,
        retained_sessions: 0,
        oldest_started_ms: None,
        dropped_bytes: 0,
        warning: None,
    }
}

fn validate_settings(settings: &HistorySettings) -> Result<()> {
    if !(MIN_QUOTA_BYTES..=MAX_QUOTA_BYTES).contains(&settings.quota_bytes) {
        bail!("history quota must be between {MIN_QUOTA_BYTES} and {MAX_QUOTA_BYTES} bytes");
    }
    if let HistoryRetention::Days { days } = settings.retention
        && !(1..=3_650).contains(&days)
    {
        bail!("history retention must be between 1 and 3650 days");
    }
    Ok(())
}

fn validate_query(query: &str) -> Result<()> {
    if query.is_empty() || query.chars().count() > 256 || query.chars().any(char::is_control) {
        bail!("history search must contain 1 to 256 visible characters");
    }
    Ok(())
}
fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => bail!("history path must be a real directory"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(path)
                .with_context(|| format!("create history directory {}", path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect history directory {}", path.display()));
        }
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect history directory {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!("history path must be a real directory");
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restrict history directory {}", path.display()))
}
fn load_settings(root: &Path) -> Result<Option<HistorySettings>> {
    let path = root.join("config.json");
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect history settings"),
    }
    let config: ConfigFile = read_json_private(&path)?;
    if config.schema_version != CONFIG_SCHEMA {
        bail!(
            "unsupported history config schema {}",
            config.schema_version
        );
    }
    Ok(Some(config.settings))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("history file has no parent")?;
    ensure_private_directory(parent)?;
    let bytes = serde_json::to_vec(value).context("encode history metadata")?;
    let temporary = parent.join(format!(".{}.{}.tmp", file_label(path), Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("create temporary history file {}", temporary.display()))?;
        file.write_all(&bytes).context("write history metadata")?;
        file.sync_all().context("sync history metadata")?;
        fs::rename(&temporary, path)
            .with_context(|| format!("replace history metadata {} atomically", path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .context("sync history metadata directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn read_json_private<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = hh_protocol::read_private_file(path, 1024 * 1024)
        .with_context(|| format!("read local history metadata {}", path.display()))?;
    serde_json::from_slice(&bytes).context("decode history metadata")
}

fn write_chunk_atomic(path: &Path, index: u32, gap_before: bool, payload: &[u8]) -> Result<()> {
    let parent = path.parent().context("history chunk has no parent")?;
    ensure_private_directory(parent)?;
    let temporary = parent.join(format!(".chunk-{index:08}-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .context("create temporary history chunk")?;
        file.write_all(CHUNK_MAGIC)?;
        file.write_all(&CHUNK_VERSION.to_le_bytes())?;
        file.write_all(&u16::from(gap_before).to_le_bytes())?;
        file.write_all(&index.to_le_bytes())?;
        file.write_all(
            &u32::try_from(payload.len())
                .context("history chunk exceeds u32")?
                .to_le_bytes(),
        )?;
        file.write_all(&checksum(payload).to_le_bytes())?;
        file.write_all(payload)?;
        file.sync_all().context("sync history chunk")?;
        fs::rename(&temporary, path).context("atomically publish history chunk")?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .context("sync history chunk directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn read_chunk(path: &Path, expected_index: u32) -> Result<(Vec<u8>, bool)> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open history chunk {}", path.display()))?;
    let metadata = file.metadata().context("inspect history chunk")?;
    if !metadata.is_file()
        || metadata.len()
            > u64::try_from(CHUNK_HEADER_BYTES + CHUNK_PAYLOAD_BYTES).unwrap_or(u64::MAX)
    {
        bail!("history chunk has an invalid file type or size");
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .context("restrict history chunk")?;
    let mut header = [0_u8; CHUNK_HEADER_BYTES];
    file.read_exact(&mut header)
        .context("read history chunk header")?;
    if &header[..8] != CHUNK_MAGIC {
        bail!("history chunk magic does not match");
    }
    let version = u16::from_le_bytes([header[8], header[9]]);
    let flags = u16::from_le_bytes([header[10], header[11]]);
    let index = u32::from_le_bytes(header[12..16].try_into().expect("four bytes"));
    let length = u32::from_le_bytes(header[16..20].try_into().expect("four bytes"));
    let expected_checksum = u64::from_le_bytes(header[20..28].try_into().expect("eight bytes"));
    if version != CHUNK_VERSION || index != expected_index {
        bail!("history chunk version or sequence does not match");
    }
    let length = usize::try_from(length).context("history chunk length exceeds usize")?;
    if length > CHUNK_PAYLOAD_BYTES || metadata.len() != (CHUNK_HEADER_BYTES + length) as u64 {
        bail!("history chunk payload length does not match the file");
    }
    let mut payload = vec![0_u8; length];
    file.read_exact(&mut payload)
        .context("read history chunk payload")?;
    if checksum(&payload) != expected_checksum {
        bail!("history chunk checksum does not match");
    }
    Ok((payload, flags & 1 != 0))
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn terminal_output_lines(bytes: &[u8]) -> Vec<String> {
    let mut text = Vec::with_capacity(bytes.len());
    let mut escape = EscapeState::Text;
    for &byte in bytes {
        escape = match escape {
            EscapeState::Text if byte == 0x1b => EscapeState::Escape,
            EscapeState::Text => {
                text.push(byte);
                EscapeState::Text
            }
            EscapeState::Escape => match byte {
                b'[' => EscapeState::Csi,
                b']' => EscapeState::Osc,
                _ => EscapeState::Text,
            },
            EscapeState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    EscapeState::Text
                } else {
                    EscapeState::Csi
                }
            }
            EscapeState::Osc => {
                if byte == 0x07 {
                    EscapeState::Text
                } else if byte == 0x1b {
                    EscapeState::OscEscape
                } else {
                    EscapeState::Osc
                }
            }
            EscapeState::OscEscape => {
                if byte == b'\\' {
                    EscapeState::Text
                } else {
                    EscapeState::Osc
                }
            }
        };
    }
    let mut lines = vec![(Vec::<char>::new(), 0_usize)];
    for character in String::from_utf8_lossy(&text).chars() {
        match character {
            '\n' => {
                lines.push((Vec::new(), 0));
            }
            '\r' => {
                if let Some((_, cursor)) = lines.last_mut() {
                    *cursor = 0;
                }
            }
            '\u{8}' => {
                if let Some((line, cursor)) = lines.last_mut()
                    && *cursor > 0
                {
                    *cursor -= 1;
                    if *cursor < line.len() {
                        line.remove(*cursor);
                    }
                }
            }
            character if !character.is_control() => {
                if let Some((line, cursor)) = lines.last_mut()
                    && *cursor < MAX_LINE_CHARS
                {
                    if *cursor < line.len() {
                        line[*cursor] = character;
                    } else {
                        line.push(character);
                    }
                    *cursor += 1;
                }
            }
            _ => {}
        }
    }
    lines
        .into_iter()
        .map(|(line, _)| line.into_iter().collect())
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum EscapeState {
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

fn directory_size(path: &Path) -> Result<u64> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path).with_context(|| format!("scan {}", path.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if file_type.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else if file_type.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn cleanup_temporary_files(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("scan {}", path.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let entry_path = entry.path();
        if file_type.is_dir() {
            cleanup_temporary_files(&entry_path)?;
        } else if file_type.is_file()
            && entry_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with('.')
                        && Path::new(name)
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
                })
        {
            fs::remove_file(&entry_path)
                .with_context(|| format!("remove stale history temp {}", entry_path.display()))?;
        }
    }
    Ok(())
}

fn remove_directory_if_real(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect history deletion target {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("refusing to clear a non-directory history target");
    }
    fs::remove_dir_all(path)
        .with_context(|| format!("clear local history directory {}", path.display()))
}

fn file_label(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("history")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as FmtWrite;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hh-history-{label}-{}", Uuid::new_v4()))
    }

    fn open_store(label: &str) -> (PathBuf, Store) {
        let root = test_root(label);
        ensure_private_directory(&root).unwrap();
        let settings = HistorySettings::default();
        let status = Arc::new(RwLock::new(empty_status(settings.clone())));
        let store = Store {
            root: root.clone(),
            settings,
            active: HashMap::new(),
            status,
            dropped_bytes: Arc::new(AtomicU64::new(0)),
            corrupt_chunk_seen: false,
            capacity_paused: false,
            last_retention_sweep: None,
        };
        (root, store)
    }

    #[test]
    fn chunk_round_trip_detects_corruption_and_sequence_mismatch() {
        let root = test_root("integrity");
        ensure_private_directory(&root).unwrap();
        let path = root.join("00000000.rmh");
        write_chunk_atomic(&path, 0, true, b"hello\nworld").unwrap();
        assert_eq!(
            read_chunk(&path, 0).unwrap(),
            (b"hello\nworld".to_vec(), true)
        );
        assert!(read_chunk(&path, 1).is_err());

        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(read_chunk(&path, 0).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_files_and_directories_are_owner_only() {
        let (root, mut store) = open_store("permissions");
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(meta).unwrap();
        store.append(meta.session_id, b"private output", 0).unwrap();
        store.flush_all().unwrap();
        store.update_settings(HistorySettings::default()).unwrap();

        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.session_path(meta.session_id))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.chunk_path(meta.session_id, 0))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(root.join("config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restart_marks_unfinished_sessions_and_preserves_chunks() {
        let (root, mut store) = open_store("restart");
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(meta).unwrap();
        store.append(meta.session_id, b"before crash\n", 0).unwrap();
        store.flush_all().unwrap();
        drop(store);

        let settings = HistorySettings::default();
        let status = Arc::new(RwLock::new(empty_status(settings.clone())));
        let mut reopened = Store {
            root: root.clone(),
            settings,
            active: HashMap::new(),
            status,
            dropped_bytes: Arc::new(AtomicU64::new(0)),
            corrupt_chunk_seen: false,
            capacity_paused: false,
            last_retention_sweep: None,
        };
        reopened.recover_interrupted_sessions().unwrap();
        let manifest = reopened.manifests().unwrap().pop().unwrap();
        assert!(manifest.ended_ms.is_some());
        assert!(manifest.has_gap);
        assert_eq!(
            read_chunk(&reopened.chunk_path(meta.session_id, 0), 0)
                .unwrap()
                .0,
            b"before crash\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quota_pauses_without_deleting_and_records_a_gap() {
        let (root, mut store) = open_store("quota");
        store.settings.quota_bytes = MIN_QUOTA_BYTES;
        let filler = root.join("filler");
        let file = File::create(&filler).unwrap();
        file.set_len(MIN_QUOTA_BYTES).unwrap();
        store.refresh_status().unwrap();
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(meta).unwrap();
        store.append(meta.session_id, b"not archived", 0).unwrap();
        assert!(store.active[&meta.session_id].manifest.has_gap);
        assert_eq!(store.active[&meta.session_id].manifest.dropped_bytes, 12);
        assert!(
            filler.exists(),
            "pause policy must not delete retained data"
        );
        store.refresh_status().unwrap();
        assert_eq!(
            store.status.read().warning,
            Some(HistoryWarning::PausedAtCapacity)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bounded_queue_reports_and_carries_an_honest_gap() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(Command::Start(SessionMeta {
                session_id: Uuid::new_v4(),
                pane_id: Uuid::new_v4(),
                workspace_id: Uuid::new_v4(),
                started_ms: now_ms(),
            }))
            .unwrap();
        let dropped = Arc::new(AtomicU64::new(0));
        let sink = HistorySink {
            sender: sender.clone(),
            session_id: Uuid::new_v4(),
            pending_gap: AtomicU64::new(0),
            dropped_bytes: Arc::clone(&dropped),
            enabled: Arc::new(AtomicBool::new(true)),
            ended: AtomicBool::new(false),
        };

        sink.record(b"lost");
        assert_eq!(dropped.load(Ordering::Relaxed), 4);
        let _ = receiver.recv().unwrap();
        sink.record(b"kept");
        let Command::Append {
            bytes, gap_before, ..
        } = receiver.recv().unwrap()
        else {
            panic!("expected queued append");
        };
        assert_eq!(bytes, b"kept");
        assert_eq!(gap_before, 4);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn finite_retention_removes_only_closed_expired_sessions() {
        let (root, mut store) = open_store("retention");
        let old = now_ms().saturating_sub(10 * 24 * 60 * 60 * 1_000);
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: old,
        };
        store.start(meta).unwrap();
        store.append(meta.session_id, b"old", 0).unwrap();
        store.end(meta.session_id).unwrap();
        let mut manifest: Manifest =
            read_json_private(&store.manifest_path(meta.session_id)).unwrap();
        manifest.ended_ms = Some(old);
        write_json_atomic(&store.manifest_path(meta.session_id), &manifest).unwrap();
        store.settings.retention = HistoryRetention::Days { days: 7 };
        store.apply_retention().unwrap();
        assert!(!store.session_path(meta.session_id).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oldest_first_capacity_cleanup_requires_the_opt_in_policy() {
        let (root, mut store) = open_store("oldest-first");
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(meta).unwrap();
        store.append(meta.session_id, b"closed session", 0).unwrap();
        store.end(meta.session_id).unwrap();
        let filler = root.join("filler");
        let file = File::create(&filler).unwrap();
        file.set_len(MIN_QUOTA_BYTES).unwrap();
        store.settings.quota_bytes = MIN_QUOTA_BYTES;
        store.refresh_status().unwrap();

        assert!(!store.make_capacity(1).unwrap());
        assert!(store.session_path(meta.session_id).exists());

        store.settings.cleanup_policy = HistoryCleanupPolicy::DeleteOldest;
        assert!(!store.make_capacity(1).unwrap());
        assert!(!store.session_path(meta.session_id).exists());
        assert!(filler.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabling_history_preserves_archived_bytes_until_explicit_clear() {
        let (root, mut store) = open_store("disable-preserve");
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(meta).unwrap();
        store
            .append(meta.session_id, b"secret output\n", 0)
            .unwrap();
        store.flush_all().unwrap();
        assert!(store.session_path(meta.session_id).exists());

        let mut disabled = store.settings.clone();
        disabled.enabled = false;
        store.update_settings(disabled).unwrap();

        assert!(store.session_path(meta.session_id).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_runs_during_append_for_long_lived_sessions() {
        let (root, mut store) = open_store("append-retention");
        store.settings.retention = HistoryRetention::Days { days: 1 };
        let expired = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: 1,
        };
        store.start(expired).unwrap();
        store.end(expired.session_id).unwrap();
        let mut manifest: Manifest =
            read_json_private(&store.manifest_path(expired.session_id)).unwrap();
        manifest.ended_ms = Some(1);
        write_json_atomic(&store.manifest_path(expired.session_id), &manifest).unwrap();

        let active = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(active).unwrap();
        store.last_retention_sweep = None;
        store
            .append(active.session_id, b"still running\n", 0)
            .unwrap();

        assert!(!store.session_path(expired.session_id).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lazy_pages_walk_chunks_without_loading_the_whole_session() {
        let (root, mut store) = open_store("lazy");
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(meta).unwrap();
        store
            .append(meta.session_id, &vec![b'a'; CHUNK_PAYLOAD_BYTES], 0)
            .unwrap();
        store.append(meta.session_id, b"newest\n", 0).unwrap();
        store.flush_all().unwrap();

        let newest = store
            .load_page(meta.pane_id, None, HistoryPageDirection::Older)
            .unwrap()
            .unwrap();
        assert!(newest.lines.iter().any(|line| line.contains("newest")));
        assert!(newest.flags.contains(HistoryPageFlags::HAS_OLDER));
        let older = store
            .load_page(
                meta.pane_id,
                Some(newest.cursor),
                HistoryPageDirection::Older,
            )
            .unwrap()
            .unwrap();
        assert_eq!(older.cursor.chunk_index, 0);
        assert!(!older.flags.contains(HistoryPageFlags::HAS_OLDER));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_cursor_never_marks_archive_corrupt_or_overflows() {
        let (root, mut store) = open_store("invalid-cursor");
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(meta).unwrap();
        store.append(meta.session_id, b"history\n", 0).unwrap();
        store.flush_all().unwrap();

        let result = store.load_page(
            meta.pane_id,
            Some(HistoryCursor {
                session_id: meta.session_id,
                chunk_index: u32::MAX,
            }),
            HistoryPageDirection::Newer,
        );
        assert!(result.is_err());
        assert!(!store.corrupt_chunk_seen);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lazy_load_turns_corruption_into_a_visible_gap() {
        let (root, mut store) = open_store("lazy-corruption");
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(meta).unwrap();
        store.append(meta.session_id, b"important\n", 0).unwrap();
        store.flush_all().unwrap();
        let path = store.chunk_path(meta.session_id, 0);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        fs::write(path, bytes).unwrap();

        let page = store
            .load_page(meta.pane_id, None, HistoryPageDirection::Older)
            .unwrap()
            .unwrap();
        assert!(page.flags.contains(HistoryPageFlags::CORRUPT));
        assert!(page.flags.contains(HistoryPageFlags::GAP_BEFORE));
        assert!(page.lines.iter().all(String::is_empty));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restart_removes_unpublished_atomic_temporary_files() {
        let (root, mut store) = open_store("stale-temp");
        let nested = root.join(Uuid::new_v4().to_string());
        ensure_private_directory(&nested).unwrap();
        let temporary = nested.join(".chunk-00000000-stale.tmp");
        fs::write(&temporary, b"partial").unwrap();

        store.recover_interrupted_sessions().unwrap();

        assert!(!temporary.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn output_sanitizer_removes_escape_sequences_and_bounds_lines() {
        let lines = terminal_output_lines(b"\x1b[31mred\x1b[0m\nnext\rreplace");
        assert_eq!(lines, vec!["red", "replace"]);
    }

    #[test]
    fn one_lazy_chunk_keeps_its_first_line_available() {
        let mut output = String::new();
        for line in 0..2_500 {
            writeln!(&mut output, "line-{line}").unwrap();
        }
        let lines = terminal_output_lines(output.as_bytes());

        assert_eq!(lines.first().map(String::as_str), Some("line-0"));
        assert_eq!(lines.get(2_499).map(String::as_str), Some("line-2499"));
    }
}
