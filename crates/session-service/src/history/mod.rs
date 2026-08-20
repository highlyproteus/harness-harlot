use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use hh_protocol::{
    HistoryArchiveStatus, HistoryClearScope, HistoryCursor, HistoryPageDirection, HistoryRetention,
    HistorySettings, HistoryWarning, TerminalHistoryPage,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) mod chunk;
mod store;

use store::{Store, worker_loop};

#[cfg(test)]
use crate::history::chunk::{CHUNK_PAYLOAD_BYTES, read_chunk, terminal_output_lines};
#[cfg(test)]
use hh_protocol::{HistoryCleanupPolicy, HistoryPageFlags};

const CONFIG_SCHEMA: u16 = 1;

const MANIFEST_SCHEMA: u16 = 1;

const QUEUE_CAPACITY: usize = 256;

pub(crate) const MAX_LINE_CHARS: usize = 4096;

const MIN_QUOTA_BYTES: u64 = 16 * 1024 * 1024;

const MAX_QUOTA_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;

const WARNING_PERCENT: u64 = 80;

const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_mins(1);
const ARCHIVE_RECONCILE_INTERVAL: Duration = Duration::from_mins(1);

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
            archived_bytes: 0,
            corrupt_chunk_seen: false,
            capacity_paused: false,
            last_retention_sweep: None,
            last_archive_reconcile: None,
        };
        store.recover_interrupted_sessions()?;
        store.apply_retention()?;
        // One full walk seeds the incremental archived-bytes counter.
        store.reconcile_archived_bytes()?;
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
        if self.sender.send(Command::Start(meta)).is_err() && self.enabled.load(Ordering::Acquire) {
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
                    let _ = self.dropped_bytes.fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |dropped| Some(dropped.saturating_sub(gap_before)),
                    );
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
    hh_protocol::ensure_private_directory(path)
        .with_context(|| format!("prepare history directory {}", path.display()))
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
    let bytes = serde_json::to_vec(value).context("encode history metadata")?;
    hh_protocol::atomic_write_private(path, &bytes)
        .with_context(|| format!("write history metadata {}", path.display()))
}

fn read_json_private<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = hh_protocol::read_private_file(path, 1024 * 1024)
        .with_context(|| format!("read local history metadata {}", path.display()))?;
    serde_json::from_slice(&bytes).context("decode history metadata")
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

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use std::fmt::Write as FmtWrite;
    use std::fs::File;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hh-history-{label}-{}", Uuid::new_v4()))
    }

    fn open_store(label: &str) -> (PathBuf, Store) {
        let root = test_root(label);
        ensure_private_directory(&root).unwrap();
        // Store-level tests exercise an active archive unless a test opts out
        // explicitly. Product defaults are covered in hh-protocol.
        let settings = HistorySettings {
            enabled: true,
            ..HistorySettings::default()
        };
        let status = Arc::new(RwLock::new(empty_status(settings.clone())));
        let store = Store {
            root: root.clone(),
            settings,
            active: HashMap::new(),
            status,
            dropped_bytes: Arc::new(AtomicU64::new(0)),
            archived_bytes: 0,
            corrupt_chunk_seen: false,
            capacity_paused: false,
            last_retention_sweep: None,
            last_archive_reconcile: None,
        };
        (root, store)
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
    fn failed_chunk_write_marks_payload_lost_without_advancing_manifest() {
        let (root, mut store) = open_store("chunk-write-failure");
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(meta).unwrap();
        store.append(meta.session_id, b"unsaved output", 0).unwrap();
        let session_path = store.session_path(meta.session_id);
        fs::remove_dir_all(&session_path).unwrap();
        fs::write(&session_path, b"not a directory").unwrap();

        assert!(store.flush_session(meta.session_id).is_err());
        let active = &store.active[&meta.session_id];
        assert_eq!(active.manifest.chunk_count, 0);
        assert_eq!(active.manifest.payload_bytes, 0);
        assert_eq!(active.manifest.dropped_bytes, 14);
        assert!(active.manifest.has_gap);
        assert!(active.gap_before_buffer);
        assert_eq!(store.status.read().dropped_bytes, 14);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]

    fn accepted_gap_cannot_underflow_cleared_drop_counter() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let dropped_bytes = Arc::new(AtomicU64::new(0));
        let sink = HistorySink {
            sender,
            session_id: Uuid::new_v4(),
            pending_gap: AtomicU64::new(12),
            dropped_bytes: Arc::clone(&dropped_bytes),
            enabled: Arc::new(AtomicBool::new(true)),
            ended: AtomicBool::new(false),
        };

        sink.record(b"x");

        assert_eq!(dropped_bytes.load(Ordering::Relaxed), 0);
        let _ = receiver.recv().unwrap();
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
            archived_bytes: 0,
            corrupt_chunk_seen: false,
            capacity_paused: false,
            last_retention_sweep: None,
            last_archive_reconcile: None,
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
        store.reconcile_archived_bytes().unwrap();
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
        fs::remove_file(&filler).unwrap();
        store.reconcile_archived_bytes().unwrap();
        assert!(
            store
                .append(meta.session_id, b"archiving resumed", 0)
                .unwrap()
        );
        store.refresh_status().unwrap();
        assert_eq!(store.status.read().warning, None);
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
    fn capacity_cleanup_obeys_the_selected_pause_policy() {
        let (root, mut store) = open_store("oldest-first");
        store.settings.cleanup_policy = HistoryCleanupPolicy::PauseWhenFull;
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
        store.reconcile_archived_bytes().unwrap();
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
    fn oldest_first_capacity_cleanup_accounts_for_buffered_headroom() {
        let (root, mut store) = open_store("oldest-first-buffered");
        let mut closed_ids = Vec::new();
        for started_ms in [1, 2] {
            let meta = SessionMeta {
                session_id: Uuid::new_v4(),
                pane_id: Uuid::new_v4(),
                workspace_id: Uuid::new_v4(),
                started_ms,
            };
            store.start(meta).unwrap();
            store.append(meta.session_id, b"closed session", 0).unwrap();
            store.end(meta.session_id).unwrap();
            File::create(store.session_path(meta.session_id).join("filler"))
                .unwrap()
                .set_len(1024 * 1024)
                .unwrap();
            closed_ids.push(meta.session_id);
        }
        let active = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: 3,
        };
        store.start(active).unwrap();
        store
            .active
            .get_mut(&active.session_id)
            .unwrap()
            .buffer
            .resize(2 * 1024 * 1024, b'x');
        File::create(root.join("external-pressure"))
            .unwrap()
            .set_len(MIN_QUOTA_BYTES - 4 * 1024 * 1024)
            .unwrap();
        store.settings.quota_bytes = MIN_QUOTA_BYTES;
        store.settings.cleanup_policy = HistoryCleanupPolicy::DeleteOldest;
        store.reconcile_archived_bytes().unwrap();

        assert!(store.make_capacity(1).unwrap());
        assert!(!store.session_path(closed_ids[0]).exists());
        assert!(store.session_path(closed_ids[1]).exists());
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
    fn terminal_started_and_closed_while_disabled_leaves_no_archive_session() {
        let (root, mut store) = open_store("disabled-no-empty-session");
        store.settings.enabled = false;
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };

        store.start(meta).unwrap();
        store.end(meta.session_id).unwrap();

        assert!(!store.session_path(meta.session_id).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabled_finite_retention_does_not_delete_existing_archive() {
        let (root, mut store) = open_store("disabled-retention-preserve");
        let meta = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: 1,
        };
        store.start(meta).unwrap();
        store.append(meta.session_id, b"old output\n", 0).unwrap();
        store.end(meta.session_id).unwrap();
        let mut manifest: Manifest =
            read_json_private(&store.manifest_path(meta.session_id)).unwrap();
        manifest.ended_ms = Some(1);
        write_json_atomic(&store.manifest_path(meta.session_id), &manifest).unwrap();
        store.settings.enabled = false;
        store.settings.retention = HistoryRetention::Days { days: 1 };

        store.apply_retention().unwrap();

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

    #[test]
    fn archived_bytes_counter_matches_disk_after_appends_and_deletion() {
        let (root, mut store) = open_store("accounting");
        store.settings.cleanup_policy = HistoryCleanupPolicy::DeleteOldest;
        store.settings.quota_bytes = MIN_QUOTA_BYTES;

        let oldest = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: 1,
        };
        store.start(oldest).unwrap();
        store
            .append(oldest.session_id, b"oldest session\n", 0)
            .unwrap();
        store.end(oldest.session_id).unwrap();
        let mut manifest: Manifest =
            read_json_private(&store.manifest_path(oldest.session_id)).unwrap();
        manifest.ended_ms = Some(1);
        write_json_atomic(&store.manifest_path(oldest.session_id), &manifest).unwrap();

        let fresh = SessionMeta {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            started_ms: now_ms(),
        };
        store.start(fresh).unwrap();
        let bulk = vec![b'x'; 300_000];
        store.append(fresh.session_id, &bulk, 0).unwrap();
        store.flush_all().unwrap();

        // External pressure beyond the quota, folded in by reconciliation,
        // then an append that triggers oldest-first deletion.
        let filler = root.join("filler");
        let file = File::create(&filler).unwrap();
        file.set_len(MIN_QUOTA_BYTES).unwrap();
        store.reconcile_archived_bytes().unwrap();
        let _ = store.append(fresh.session_id, b"pressure", 0);
        store.refresh_status().unwrap();
        assert_eq!(
            store.status.read().archived_bytes,
            directory_size(&root).unwrap(),
            "incremental counter must match a full disk walk"
        );
        assert!(!store.session_path(oldest.session_id).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
