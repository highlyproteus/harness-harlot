use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_THREAD_FILES: usize = 200;
const MAX_THREAD_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_RECORDS: usize = 10_000;
const MAX_TEXT_CHARS: usize = 32 * 1024;
const GENERATION_FILE: &str = ".generation";
const AUTHORITY_FILE_SUFFIX: &str = "authority";
pub const MAX_THREAD_TITLE_CHARS: usize = 60;

static STORAGE_LOCK: LazyLock<parking_lot::Mutex<()>> =
    LazyLock::new(|| parking_lot::Mutex::new(()));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadGeneration {
    storage: u64,
    authority: Uuid,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadRecord {
    Meta {
        thread_id: Uuid,
        workspace_id: Option<Uuid>,
        workspace_title: String,
        at_ms: u64,
    },
    Turn {
        role: ThreadRole,
        text: String,
        at_ms: u64,
    },
    Tool {
        name: String,
        summary: String,
        at_ms: u64,
    },
    Title {
        text: String,
        at_ms: u64,
    },
    Summary {
        text: String,
        at_ms: u64,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub thread_id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub workspace_title: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub started_at_ms: u64,
    pub last_at_ms: u64,
    pub entries: Vec<ThreadRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadSummary {
    pub thread_id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub workspace_title: String,
    pub title: Option<String>,
    pub started_at_ms: u64,
    pub last_at_ms: u64,
    pub turns: usize,
}

fn thread_directory() -> Option<PathBuf> {
    hh_protocol::state_directory().map(|directory| directory.join("assistant-threads"))
}

fn thread_path(dir: &Path, thread_id: Uuid) -> PathBuf {
    dir.join(format!("{thread_id}.jsonl"))
}

fn generation_path(dir: &Path) -> PathBuf {
    dir.join(GENERATION_FILE)
}

fn authority_path(dir: &Path, thread_id: Uuid) -> PathBuf {
    dir.join(format!(".{thread_id}.{AUTHORITY_FILE_SUFFIX}"))
}

fn authority_thread_id(path: &Path) -> Option<Uuid> {
    let name = path.file_name()?.to_str()?;
    let thread_id = name
        .strip_prefix('.')?
        .strip_suffix(AUTHORITY_FILE_SUFFIX)?
        .strip_suffix('.')?;
    Uuid::parse_str(thread_id).ok()
}

fn current_storage_generation_in(dir: &Path) -> Result<u64> {
    let bytes = match hh_protocol::read_private_file(&generation_path(dir), 32) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(0);
        }
        Err(error) => return Err(error).context("read assistant thread generation"),
    };
    let text = std::str::from_utf8(&bytes).context("decode assistant thread generation")?;
    text.parse().context("parse assistant thread generation")
}

fn read_thread_authority_in(dir: &Path, thread_id: Uuid) -> Result<Option<Uuid>> {
    let bytes = match hh_protocol::read_private_file(&authority_path(dir, thread_id), 36) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read assistant thread authority"),
    };
    let text = std::str::from_utf8(&bytes).context("decode assistant thread authority")?;
    Ok(Some(
        Uuid::parse_str(text).context("parse assistant thread authority")?,
    ))
}

fn current_generation_in(dir: &Path, thread_id: Uuid) -> Result<ThreadGeneration> {
    hh_protocol::ensure_private_directory(dir)
        .with_context(|| format!("prepare assistant thread directory {}", dir.display()))?;
    let authority = if let Some(authority) = read_thread_authority_in(dir, thread_id)? {
        authority
    } else {
        let authority = Uuid::new_v4();
        hh_protocol::atomic_write_private(
            &authority_path(dir, thread_id),
            authority.to_string().as_bytes(),
        )
        .context("create assistant thread authority")?;
        authority
    };
    Ok(ThreadGeneration {
        storage: current_storage_generation_in(dir)?,
        authority,
    })
}

fn record_at_ms(record: &ThreadRecord) -> u64 {
    match record {
        ThreadRecord::Meta { at_ms, .. }
        | ThreadRecord::Turn { at_ms, .. }
        | ThreadRecord::Tool { at_ms, .. }
        | ThreadRecord::Title { at_ms, .. }
        | ThreadRecord::Summary { at_ms, .. } => *at_ms,
    }
}

fn validate_record(record: &ThreadRecord) -> Result<()> {
    let strings: &[&str] = match record {
        ThreadRecord::Meta {
            workspace_title, ..
        } => &[workspace_title],
        ThreadRecord::Turn { text, .. }
        | ThreadRecord::Title { text, .. }
        | ThreadRecord::Summary { text, .. } => &[text],
        ThreadRecord::Tool { name, summary, .. } => &[name, summary],
    };
    if strings
        .iter()
        .any(|value| value.chars().count() > MAX_TEXT_CHARS)
    {
        anyhow::bail!("assistant thread text exceeds {MAX_TEXT_CHARS} characters");
    }
    Ok(())
}

fn append_record_in(dir: &Path, thread_id: Uuid, record: &ThreadRecord) -> Result<()> {
    hh_protocol::ensure_private_directory(dir)
        .with_context(|| format!("prepare assistant thread directory {}", dir.display()))?;
    validate_record(record)?;
    let mut encoded = serde_json::to_vec(record)
        .with_context(|| format!("serialize assistant thread {thread_id}"))?;
    if encoded.len() > MAX_RECORD_BYTES {
        anyhow::bail!("assistant thread record exceeds {MAX_RECORD_BYTES} bytes");
    }
    encoded.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(thread_path(dir, thread_id))
        .with_context(|| format!("open assistant thread {thread_id}"))?;
    let metadata = file
        .metadata()
        .context("inspect assistant thread descriptor")?;
    if !metadata.is_file() || !hh_protocol::validate_private_ownership(&metadata) {
        anyhow::bail!("assistant thread must be an owner-only regular file");
    }
    if metadata.len().saturating_add(encoded.len() as u64) > MAX_THREAD_FILE_BYTES {
        anyhow::bail!("assistant thread exceeds {MAX_THREAD_FILE_BYTES} bytes");
    }
    file.write_all(&encoded)
        .with_context(|| format!("append assistant thread {thread_id}"))?;
    file.sync_data()
        .with_context(|| format!("sync assistant thread {thread_id}"))?;
    Ok(())
}

fn append_record_for_generation_in(
    dir: &Path,
    generation: ThreadGeneration,
    thread_id: Uuid,
    record: &ThreadRecord,
) -> Result<bool> {
    if current_storage_generation_in(dir)? != generation.storage
        || read_thread_authority_in(dir, thread_id)? != Some(generation.authority)
    {
        return Ok(false);
    }
    append_record_in(dir, thread_id, record)?;
    Ok(true)
}

fn read_thread_in(dir: &Path, thread_id: Uuid) -> Result<Option<Thread>> {
    let path = thread_path(dir, thread_id);
    let bytes = match hh_protocol::read_private_file(&path, MAX_THREAD_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read assistant thread {thread_id}"));
        }
    };
    let mut workspace_id = None;
    let mut workspace_title = String::new();
    let mut title = None;
    let mut summary = None;
    let mut started_at_ms = None;
    let mut last_at_ms = 0;
    let mut entries = Vec::new();

    // Ignore only an incomplete final record left by a process crash. Every
    // newline-terminated malformed record is a surfaced corruption error.
    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    for (index, line) in bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if index >= MAX_RECORDS {
            anyhow::bail!("assistant thread exceeds {MAX_RECORDS} records");
        }
        if line.len() > MAX_RECORD_BYTES {
            anyhow::bail!("assistant thread line exceeds {MAX_RECORD_BYTES} bytes");
        }
        let record = serde_json::from_slice::<ThreadRecord>(line)
            .with_context(|| format!("decode assistant thread {thread_id} record {}", index + 1))?;
        validate_record(&record)?;
        let at_ms = record_at_ms(&record);
        started_at_ms.get_or_insert(at_ms);
        last_at_ms = at_ms;
        match &record {
            ThreadRecord::Meta {
                workspace_id: record_workspace_id,
                workspace_title: record_workspace_title,
                ..
            } => {
                workspace_id = *record_workspace_id;
                workspace_title.clone_from(record_workspace_title);
            }
            ThreadRecord::Title { text, .. } => title = Some(text.clone()),
            ThreadRecord::Summary { text, .. } => summary = Some(text.clone()),
            ThreadRecord::Turn { .. } | ThreadRecord::Tool { .. } => entries.push(record),
        }
    }

    let Some(started_at_ms) = started_at_ms else {
        return Ok(None);
    };
    Ok(Some(Thread {
        thread_id,
        workspace_id,
        workspace_title,
        title,
        summary,
        started_at_ms,
        last_at_ms,
        entries,
    }))
}

fn read_summary_in(dir: &Path, thread_id: Uuid) -> Result<Option<String>> {
    Ok(read_thread_in(dir, thread_id)?.and_then(|thread| thread.summary))
}

fn list_threads_in(dir: &Path) -> Result<Vec<ThreadSummary>> {
    let files = match fs::read_dir(dir) {
        Ok(files) => files,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("read assistant thread directory"),
    };
    let mut threads = Vec::new();
    for entry in files {
        let entry = entry.context("read assistant thread directory entry")?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(thread_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        let Some(thread) = read_thread_in(dir, thread_id)? else {
            continue;
        };
        let turns = thread
            .entries
            .iter()
            .filter(|record| matches!(record, ThreadRecord::Turn { .. }))
            .count();
        threads.push(ThreadSummary {
            thread_id,
            workspace_id: thread.workspace_id,
            workspace_title: thread.workspace_title,
            title: thread.title,
            started_at_ms: thread.started_at_ms,
            last_at_ms: thread.last_at_ms,
            turns,
        });
    }
    threads.sort_by(|left, right| {
        right
            .last_at_ms
            .cmp(&left.last_at_ms)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    Ok(threads)
}

fn adopt_thread_in(dir: &Path, old: Uuid, new: Uuid) -> Result<bool> {
    if read_thread_in(dir, old)?.is_none() {
        return Ok(false);
    }
    let new_path = thread_path(dir, new);
    let new_authority_path = authority_path(dir, new);
    if fs::symlink_metadata(&new_path).is_ok() || fs::symlink_metadata(&new_authority_path).is_ok()
    {
        return Ok(false);
    }
    let old_authority_path = authority_path(dir, old);
    let moved_authority = read_thread_authority_in(dir, old)?.is_some();
    if moved_authority {
        fs::rename(&old_authority_path, &new_authority_path)
            .context("adopt assistant thread authority")?;
    }
    if let Err(error) = fs::rename(thread_path(dir, old), new_path) {
        if moved_authority {
            let _ = fs::rename(new_authority_path, old_authority_path);
        }
        return Err(error).context("adopt assistant thread");
    }
    Ok(true)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadRetention {
    pub max_count: usize,
    pub max_age_ms: u64,
    pub max_total_bytes: u64,
}

impl Default for ThreadRetention {
    fn default() -> Self {
        Self {
            max_count: MAX_THREAD_FILES,
            max_age_ms: 90 * 24 * 60 * 60 * 1_000,
            max_total_bytes: 64 * 1024 * 1024,
        }
    }
}

fn prune_thread_files_in(dir: &Path, policy: ThreadRetention, current_ms: u64) -> Result<usize> {
    let threads = list_threads_in(dir)?;
    let cutoff = current_ms.saturating_sub(policy.max_age_ms);
    let mut kept_bytes = 0_u64;
    let mut removed = 0;
    for (index, thread) in threads.into_iter().enumerate() {
        let path = thread_path(dir, thread.thread_id);
        let size = hh_protocol::read_private_file(&path, MAX_THREAD_FILE_BYTES)?.len() as u64;
        let exceeds = index >= policy.max_count
            || thread.last_at_ms < cutoff
            || kept_bytes.saturating_add(size) > policy.max_total_bytes;
        if exceeds {
            removed += usize::from(delete_thread_in(dir, thread.thread_id).with_context(|| {
                format!("delete retained assistant thread {}", thread.thread_id)
            })?);
        } else {
            kept_bytes = kept_bytes.saturating_add(size);
        }
    }
    Ok(removed)
}

fn prepare_writer_in(
    dir: &Path,
    thread_id: Uuid,
    policy: ThreadRetention,
    current_ms: u64,
) -> Result<ThreadGeneration> {
    prune_thread_files_in(dir, policy, current_ms)?;
    current_generation_in(dir, thread_id)
}

fn required_thread_directory() -> Result<PathBuf> {
    thread_directory().context("HOME is not set and HH_STATE_DIR is not configured")
}

/// Captures the generation that authorizes a newly started thread writer.
///
/// # Errors
/// Returns an error when the generation file is unsafe or malformed.
pub fn current_generation(thread_id: Uuid) -> Result<ThreadGeneration> {
    let _guard = STORAGE_LOCK.lock();
    current_generation_in(&required_thread_directory()?, thread_id)
}

/// Applies retention and captures authority for a newly started thread writer atomically.
///
/// # Errors
/// Returns an error when retention or writer authority cannot be updated safely.
pub fn prepare_writer(thread_id: Uuid) -> Result<ThreadGeneration> {
    let _guard = STORAGE_LOCK.lock();
    prepare_writer_in(
        &required_thread_directory()?,
        thread_id,
        ThreadRetention::default(),
        now_ms(),
    )
}

/// Appends a record only while the writer's captured storage generation is current.
///
/// Returns `false` after Clear all has revoked the writer's generation.
///
/// # Errors
/// Returns an error when generation state or the thread file cannot be read or written safely.
pub fn append_record(
    generation: ThreadGeneration,
    thread_id: Uuid,
    record: &ThreadRecord,
) -> Result<bool> {
    let _guard = STORAGE_LOCK.lock();
    append_record_for_generation_in(&required_thread_directory()?, generation, thread_id, record)
}

/// Loads a thread from its bounded private record file.
///
/// # Errors
/// Returns an error when the file is unsafe, malformed, oversized, or cannot
/// be read.
pub fn read_thread(thread_id: Uuid) -> Result<Option<Thread>> {
    read_thread_in(&required_thread_directory()?, thread_id)
}

/// Loads the latest summary from the retained thread record.
///
/// # Errors
/// Returns an error when the thread file is unsafe, malformed, or unavailable.
pub fn read_summary(thread_id: Uuid) -> Result<Option<String>> {
    read_summary_in(&required_thread_directory()?, thread_id)
}

/// Lists saved threads after validating their private files.
///
/// # Errors
/// Returns an error when the thread directory or any completed thread record
/// cannot be read safely.
pub fn list_threads() -> Result<Vec<ThreadSummary>> {
    list_threads_in(&required_thread_directory()?)
}

/// Renames a saved thread to a new identifier.
///
/// # Errors
/// Returns an error when either path is unsafe or the rename cannot be
/// completed.
pub fn adopt_thread(old: Uuid, new: Uuid) -> Result<bool> {
    let _guard = STORAGE_LOCK.lock();
    adopt_thread_in(&required_thread_directory()?, old, new)
}

fn delete_thread_in(dir: &Path, thread_id: Uuid) -> Result<bool> {
    let path = thread_path(dir, thread_id);
    match fs::remove_file(authority_path(dir, thread_id)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("revoke assistant thread writer"),
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            anyhow::bail!("assistant thread delete target is not a regular file")
        }
        Ok(_) => {
            // Descriptor validation prevents deleting a foreign-owned/open file.
            let _ = hh_protocol::read_private_file(&path, MAX_THREAD_FILE_BYTES)?;
            fs::remove_file(path).context("delete assistant thread")?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect assistant thread delete target"),
    }
}

fn clear_all_threads_in(dir: &Path) -> Result<usize> {
    hh_protocol::ensure_private_directory(dir)
        .with_context(|| format!("prepare assistant thread directory {}", dir.display()))?;
    let next_generation = current_storage_generation_in(dir)?
        .checked_add(1)
        .context("assistant thread generation exhausted")?;
    hh_protocol::atomic_write_private(
        &generation_path(dir),
        next_generation.to_string().as_bytes(),
    )
    .context("advance assistant thread generation")?;
    let entries = fs::read_dir(dir).context("read assistant thread directory")?;
    let mut deleted = 0;
    for entry in entries {
        let path = entry
            .context("read assistant thread directory entry")?
            .path();
        if authority_thread_id(&path).is_some() {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("remove assistant thread authority"),
            }
            continue;
        }
        let Some(thread_id) = path
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| *extension == "jsonl")
            .and_then(|_| path.file_stem())
            .and_then(|stem| stem.to_str())
            .and_then(|stem| Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        deleted += usize::from(delete_thread_in(dir, thread_id)?);
    }
    Ok(deleted)
}

/// Deletes one validated private thread file.
///
/// # Errors
/// Returns an error when the target is unsafe, foreign-owned, or cannot be
/// removed.
pub fn delete_thread(thread_id: Uuid) -> Result<bool> {
    let _guard = STORAGE_LOCK.lock();
    delete_thread_in(&required_thread_directory()?, thread_id)
}

/// Deletes all validated saved thread files.
///
/// # Errors
/// Returns an error when the directory cannot be enumerated or a thread file
/// cannot be safely removed.
pub fn clear_all_threads() -> Result<usize> {
    let _guard = STORAGE_LOCK.lock();
    clear_all_threads_in(&required_thread_directory()?)
}

/// Applies age, count, and byte retention limits to saved threads.
///
/// # Errors
/// Returns an error when thread metadata cannot be validated or an expired
/// thread cannot be removed safely.
pub fn prune_thread_files(policy: ThreadRetention) -> Result<usize> {
    let _guard = STORAGE_LOCK.lock();
    prune_thread_files_in(&required_thread_directory()?, policy, now_ms())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

pub fn thread_title(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_THREAD_TITLE_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!("hh-threads-{}", Uuid::new_v4()))
    }

    fn meta(thread_id: Uuid, at_ms: u64) -> ThreadRecord {
        ThreadRecord::Meta {
            thread_id,
            workspace_id: Some(Uuid::from_u128(7)),
            workspace_title: "Research".to_owned(),
            at_ms,
        }
    }

    #[test]
    fn append_rejects_symlink_without_touching_target() {
        let dir = test_dir();
        hh_protocol::ensure_private_directory(&dir).unwrap();
        let thread_id = Uuid::new_v4();
        let target = dir.join("target");
        fs::write(&target, b"sentinel").unwrap();
        std::os::unix::fs::symlink(&target, thread_path(&dir, thread_id)).unwrap();

        assert!(append_record_in(&dir, thread_id, &meta(thread_id, 10)).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"sentinel");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_thread_records_surface_an_error() {
        let dir = test_dir();
        hh_protocol::ensure_private_directory(&dir).unwrap();
        let thread_id = Uuid::new_v4();
        hh_protocol::atomic_write_private(&thread_path(&dir, thread_id), b"{not json}\n").unwrap();

        let error = read_thread_in(&dir, thread_id).unwrap_err();
        assert!(error.to_string().contains("decode assistant thread"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn records_fold_into_thread() {
        let dir = test_dir();
        let thread_id = Uuid::new_v4();
        append_record_in(&dir, thread_id, &meta(thread_id, 10)).unwrap();
        append_record_in(
            &dir,
            thread_id,
            &ThreadRecord::Turn {
                role: ThreadRole::User,
                text: "question".to_owned(),
                at_ms: 20,
            },
        )
        .unwrap();
        append_record_in(
            &dir,
            thread_id,
            &ThreadRecord::Turn {
                role: ThreadRole::Assistant,
                text: "answer".to_owned(),
                at_ms: 30,
            },
        )
        .unwrap();
        append_record_in(
            &dir,
            thread_id,
            &ThreadRecord::Title {
                text: "A useful thread".to_owned(),
                at_ms: 40,
            },
        )
        .unwrap();
        append_record_in(
            &dir,
            thread_id,
            &ThreadRecord::Summary {
                text: "question answered".to_owned(),
                at_ms: 50,
            },
        )
        .unwrap();

        let thread = read_thread_in(&dir, thread_id).unwrap().unwrap();
        assert_eq!(thread.title.as_deref(), Some("A useful thread"));
        assert_eq!(thread.summary.as_deref(), Some("question answered"));
        assert_eq!(thread.entries.len(), 2);
        assert_eq!(thread.started_at_ms, 10);
        assert_eq!(thread.last_at_ms, 50);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn list_orders_by_last_activity() {
        let dir = test_dir();
        let older = Uuid::new_v4();
        let newer = Uuid::new_v4();
        append_record_in(&dir, older, &meta(older, 10)).unwrap();
        append_record_in(&dir, newer, &meta(newer, 20)).unwrap();

        let threads = list_threads_in(&dir).unwrap();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].thread_id, newer);
        assert_eq!(threads[1].thread_id, older);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn adopt_renames_thread_file() {
        let dir = test_dir();
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        append_record_in(&dir, old, &meta(old, 10)).unwrap();

        assert!(adopt_thread_in(&dir, old, new).unwrap());
        assert!(read_thread_in(&dir, old).unwrap().is_none());
        assert_eq!(read_thread_in(&dir, new).unwrap().unwrap().thread_id, new);

        let other = Uuid::new_v4();
        append_record_in(&dir, old, &meta(old, 20)).unwrap();
        append_record_in(&dir, other, &meta(other, 30)).unwrap();
        assert!(!adopt_thread_in(&dir, old, other).unwrap());
        assert!(read_thread_in(&dir, old).unwrap().is_some());
        assert!(read_thread_in(&dir, other).unwrap().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn adopt_moves_writer_authority_away_from_old_thread_id() {
        let dir = test_dir();
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        let old_generation = current_generation_in(&dir, old).unwrap();
        assert!(
            append_record_for_generation_in(&dir, old_generation, old, &meta(old, 10)).unwrap()
        );

        assert!(adopt_thread_in(&dir, old, new).unwrap());
        assert!(
            !append_record_for_generation_in(
                &dir,
                old_generation,
                old,
                &ThreadRecord::Summary {
                    text: "delayed old summary".to_owned(),
                    at_ms: 20,
                },
            )
            .unwrap()
        );
        let new_generation = current_generation_in(&dir, new).unwrap();
        assert_eq!(new_generation.authority, old_generation.authority);
        assert!(
            append_record_for_generation_in(
                &dir,
                new_generation,
                new,
                &ThreadRecord::Turn {
                    role: ThreadRole::User,
                    text: "continued thread".to_owned(),
                    at_ms: 30,
                },
            )
            .unwrap()
        );
        assert!(read_thread_in(&dir, old).unwrap().is_none());
        assert!(read_thread_in(&dir, new).unwrap().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retention_enforces_age_count_and_total_bytes() {
        let dir = test_dir();
        let old = Uuid::new_v4();
        let newest = Uuid::new_v4();
        let over_count = Uuid::new_v4();
        append_record_in(&dir, old, &meta(old, 10)).unwrap();
        append_record_in(&dir, over_count, &meta(over_count, 90)).unwrap();
        append_record_in(&dir, newest, &meta(newest, 100)).unwrap();
        let newest_size = fs::metadata(thread_path(&dir, newest)).unwrap().len();

        let removed = prune_thread_files_in(
            &dir,
            ThreadRetention {
                max_count: 2,
                max_age_ms: 50,
                max_total_bytes: newest_size,
            },
            100,
        )
        .unwrap();

        assert_eq!(removed, 2);
        assert!(read_thread_in(&dir, newest).unwrap().is_some());
        assert!(read_thread_in(&dir, over_count).unwrap().is_none());
        assert!(read_thread_in(&dir, old).unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retention_revokes_active_writer_for_pruned_thread() {
        let dir = test_dir();
        let thread_id = Uuid::new_v4();
        let active_generation = current_generation_in(&dir, thread_id).unwrap();
        assert!(
            append_record_for_generation_in(
                &dir,
                active_generation,
                thread_id,
                &meta(thread_id, 10),
            )
            .unwrap()
        );

        assert_eq!(
            prune_thread_files_in(
                &dir,
                ThreadRetention {
                    max_count: 0,
                    max_age_ms: u64::MAX,
                    max_total_bytes: u64::MAX,
                },
                10,
            )
            .unwrap(),
            1
        );
        assert!(
            !append_record_for_generation_in(
                &dir,
                active_generation,
                thread_id,
                &ThreadRecord::Summary {
                    text: "delayed retention summary".to_owned(),
                    at_ms: 20,
                },
            )
            .unwrap()
        );
        assert!(read_thread_in(&dir, thread_id).unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preparing_writer_after_retention_returns_fresh_authority() {
        let dir = test_dir();
        let thread_id = Uuid::new_v4();
        let stale_generation = current_generation_in(&dir, thread_id).unwrap();
        assert!(
            append_record_for_generation_in(
                &dir,
                stale_generation,
                thread_id,
                &meta(thread_id, 10),
            )
            .unwrap()
        );

        let fresh_generation = prepare_writer_in(
            &dir,
            thread_id,
            ThreadRetention {
                max_count: 0,
                max_age_ms: u64::MAX,
                max_total_bytes: u64::MAX,
            },
            10,
        )
        .unwrap();
        assert_ne!(fresh_generation.authority, stale_generation.authority);
        assert!(
            append_record_for_generation_in(
                &dir,
                fresh_generation,
                thread_id,
                &meta(thread_id, 20),
            )
            .unwrap()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn summary_lifecycle_follows_clear_and_retention() {
        let dir = test_dir();
        let old = Uuid::new_v4();
        let current = Uuid::new_v4();
        append_record_in(&dir, old, &meta(old, 10)).unwrap();
        append_record_in(
            &dir,
            old,
            &ThreadRecord::Summary {
                text: "old context".to_owned(),
                at_ms: 10,
            },
        )
        .unwrap();
        append_record_in(&dir, current, &meta(current, 100)).unwrap();
        append_record_in(
            &dir,
            current,
            &ThreadRecord::Summary {
                text: "current context".to_owned(),
                at_ms: 100,
            },
        )
        .unwrap();

        assert_eq!(
            read_summary_in(&dir, old).unwrap().as_deref(),
            Some("old context")
        );
        assert_eq!(
            prune_thread_files_in(
                &dir,
                ThreadRetention {
                    max_count: 10,
                    max_age_ms: 50,
                    max_total_bytes: u64::MAX,
                },
                100,
            )
            .unwrap(),
            1
        );
        assert_eq!(read_summary_in(&dir, old).unwrap(), None);
        assert_eq!(
            read_summary_in(&dir, current).unwrap().as_deref(),
            Some("current context")
        );
        assert_eq!(clear_all_threads_in(&dir).unwrap(), 1);
        assert_eq!(read_summary_in(&dir, current).unwrap(), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clear_all_revokes_active_generation_and_allows_new_writers() {
        let dir = test_dir();
        let stale_thread = Uuid::new_v4();
        let fresh_thread = Uuid::new_v4();
        let active_generation = current_generation_in(&dir, stale_thread).unwrap();
        assert!(
            append_record_for_generation_in(
                &dir,
                active_generation,
                stale_thread,
                &meta(stale_thread, 10),
            )
            .unwrap()
        );

        assert_eq!(clear_all_threads_in(&dir).unwrap(), 1);
        assert!(list_threads_in(&dir).unwrap().is_empty());
        assert!(
            !append_record_for_generation_in(
                &dir,
                active_generation,
                stale_thread,
                &ThreadRecord::Turn {
                    role: ThreadRole::Assistant,
                    text: "delayed reply".to_owned(),
                    at_ms: 20,
                },
            )
            .unwrap()
        );
        assert!(
            !append_record_for_generation_in(
                &dir,
                active_generation,
                stale_thread,
                &ThreadRecord::Summary {
                    text: "shutdown summary".to_owned(),
                    at_ms: 30,
                },
            )
            .unwrap()
        );
        assert!(list_threads_in(&dir).unwrap().is_empty());

        let fresh_generation = current_generation_in(&dir, fresh_thread).unwrap();
        assert_ne!(fresh_generation.storage, active_generation.storage);
        assert!(
            append_record_for_generation_in(
                &dir,
                fresh_generation,
                fresh_thread,
                &meta(fresh_thread, 40),
            )
            .unwrap()
        );
        assert_eq!(list_threads_in(&dir).unwrap()[0].thread_id, fresh_thread);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn single_delete_revokes_active_writer_and_allows_new_writer() {
        let dir = test_dir();
        let deleted_thread = Uuid::new_v4();
        let active_generation = current_generation_in(&dir, deleted_thread).unwrap();
        assert!(
            append_record_for_generation_in(
                &dir,
                active_generation,
                deleted_thread,
                &meta(deleted_thread, 10),
            )
            .unwrap()
        );

        assert!(delete_thread_in(&dir, deleted_thread).unwrap());
        for delayed_record in [
            ThreadRecord::Turn {
                role: ThreadRole::Assistant,
                text: "delayed reply".to_owned(),
                at_ms: 20,
            },
            ThreadRecord::Tool {
                name: "delayed.tool".to_owned(),
                summary: "delayed result".to_owned(),
                at_ms: 30,
            },
            ThreadRecord::Title {
                text: "Delayed title".to_owned(),
                at_ms: 40,
            },
            ThreadRecord::Summary {
                text: "shutdown summary".to_owned(),
                at_ms: 50,
            },
            meta(deleted_thread, 60),
        ] {
            assert!(
                !append_record_for_generation_in(
                    &dir,
                    active_generation,
                    deleted_thread,
                    &delayed_record,
                )
                .unwrap()
            );
        }
        assert!(read_thread_in(&dir, deleted_thread).unwrap().is_none());

        let fresh_generation = current_generation_in(&dir, deleted_thread).unwrap();
        assert_ne!(fresh_generation.authority, active_generation.authority);
        assert!(
            append_record_for_generation_in(
                &dir,
                fresh_generation,
                deleted_thread,
                &meta(deleted_thread, 70),
            )
            .unwrap()
        );
        assert_eq!(list_threads_in(&dir).unwrap()[0].thread_id, deleted_thread);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delete_revokes_writer_even_before_thread_file_exists() {
        let dir = test_dir();
        let thread_id = Uuid::new_v4();
        let active_generation = current_generation_in(&dir, thread_id).unwrap();

        assert!(!delete_thread_in(&dir, thread_id).unwrap());
        assert!(
            !append_record_for_generation_in(
                &dir,
                active_generation,
                thread_id,
                &meta(thread_id, 10),
            )
            .unwrap()
        );
        assert!(read_thread_in(&dir, thread_id).unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clear_all_removes_pre_file_writer_authorities_without_blocking_new_writers() {
        let dir = test_dir();
        let thread_id = Uuid::new_v4();
        let stale_generation = current_generation_in(&dir, thread_id).unwrap();
        let authority = authority_path(&dir, thread_id);
        assert!(authority.is_file());

        assert_eq!(clear_all_threads_in(&dir).unwrap(), 0);
        assert!(!authority.exists());
        assert!(
            !append_record_for_generation_in(
                &dir,
                stale_generation,
                thread_id,
                &meta(thread_id, 10),
            )
            .unwrap()
        );

        let fresh_generation = current_generation_in(&dir, thread_id).unwrap();
        assert_ne!(fresh_generation, stale_generation);
        assert!(
            append_record_for_generation_in(
                &dir,
                fresh_generation,
                thread_id,
                &meta(thread_id, 20),
            )
            .unwrap()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delete_thread_is_idempotent_and_clear_all_removes_every_thread() {
        let dir = test_dir();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        append_record_in(&dir, first, &meta(first, 10)).unwrap();
        append_record_in(&dir, second, &meta(second, 20)).unwrap();

        assert!(delete_thread_in(&dir, first).unwrap());
        assert!(!delete_thread_in(&dir, first).unwrap());
        assert_eq!(clear_all_threads_in(&dir).unwrap(), 1);
        assert!(list_threads_in(&dir).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn thread_title_truncates_and_flattens() {
        let input = format!("first line\n{}", "word ".repeat(30));
        let title = thread_title(&input);
        assert!(!title.contains('\n'));
        assert!(title.chars().count() <= MAX_THREAD_TITLE_CHARS);
        assert_eq!(title, title.trim());
    }
}
