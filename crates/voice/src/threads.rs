use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_THREAD_FILES: usize = 200;
pub const MAX_THREAD_TITLE_CHARS: usize = 60;

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

fn record_at_ms(record: &ThreadRecord) -> u64 {
    match record {
        ThreadRecord::Meta { at_ms, .. }
        | ThreadRecord::Turn { at_ms, .. }
        | ThreadRecord::Tool { at_ms, .. }
        | ThreadRecord::Title { at_ms, .. }
        | ThreadRecord::Summary { at_ms, .. } => *at_ms,
    }
}

fn append_record_in(dir: &Path, thread_id: Uuid, record: &ThreadRecord) -> Result<()> {
    hh_protocol::ensure_private_directory(dir)
        .with_context(|| format!("prepare assistant thread directory {}", dir.display()))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(thread_path(dir, thread_id))
        .with_context(|| format!("open assistant thread {thread_id}"))?;
    serde_json::to_writer(&mut file, record)
        .with_context(|| format!("serialize assistant thread {thread_id}"))?;
    file.write_all(b"\n")
        .with_context(|| format!("append assistant thread {thread_id}"))?;
    Ok(())
}

fn read_thread_in(dir: &Path, thread_id: Uuid) -> Option<Thread> {
    let file = File::open(thread_path(dir, thread_id)).ok()?;
    let mut workspace_id = None;
    let mut workspace_title = String::new();
    let mut title = None;
    let mut summary = None;
    let mut started_at_ms = None;
    let mut last_at_ms = 0;
    let mut entries = Vec::new();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(record) = serde_json::from_str::<ThreadRecord>(&line) else {
            continue;
        };
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

    Some(Thread {
        thread_id,
        workspace_id,
        workspace_title,
        title,
        summary,
        started_at_ms: started_at_ms?,
        last_at_ms,
        entries,
    })
}

fn list_threads_in(dir: &Path) -> Vec<ThreadSummary> {
    let Ok(files) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut threads = files
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                return None;
            }
            let thread_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| Uuid::parse_str(stem).ok())?;
            let thread = read_thread_in(dir, thread_id)?;
            let turns = thread
                .entries
                .iter()
                .filter(|record| matches!(record, ThreadRecord::Turn { .. }))
                .count();
            Some(ThreadSummary {
                thread_id,
                workspace_id: thread.workspace_id,
                workspace_title: thread.workspace_title,
                title: thread.title,
                started_at_ms: thread.started_at_ms,
                last_at_ms: thread.last_at_ms,
                turns,
            })
        })
        .collect::<Vec<_>>();
    threads.sort_by(|left, right| {
        right
            .last_at_ms
            .cmp(&left.last_at_ms)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    threads
}

fn adopt_thread_in(dir: &Path, old: Uuid, new: Uuid) -> bool {
    let old_path = thread_path(dir, old);
    let new_path = thread_path(dir, new);
    if !old_path.is_file() || new_path.exists() {
        return false;
    }
    fs::rename(old_path, new_path).is_ok()
}

fn prune_thread_files_in(dir: &Path) {
    let Ok(files) = fs::read_dir(dir) else {
        return;
    };
    let mut threads = files
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                return None;
            }
            let thread_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| Uuid::parse_str(stem).ok())?;
            let last_at_ms = read_thread_in(dir, thread_id).map_or(0, |thread| thread.last_at_ms);
            Some((last_at_ms, thread_id, path))
        })
        .collect::<Vec<_>>();
    threads.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    for (_, _, path) in threads.into_iter().skip(MAX_THREAD_FILES) {
        let _ = fs::remove_file(path);
    }
}

pub fn append_record(thread_id: Uuid, record: &ThreadRecord) {
    if let Some(dir) = thread_directory() {
        let _ = append_record_in(&dir, thread_id, record);
    }
}

pub fn read_thread(thread_id: Uuid) -> Option<Thread> {
    read_thread_in(&thread_directory()?, thread_id)
}

pub fn list_threads() -> Vec<ThreadSummary> {
    thread_directory().map_or_else(Vec::new, |dir| list_threads_in(&dir))
}

pub fn adopt_thread(old: Uuid, new: Uuid) -> bool {
    thread_directory().is_some_and(|dir| adopt_thread_in(&dir, old, new))
}

pub fn prune_thread_files() {
    if let Some(dir) = thread_directory() {
        prune_thread_files_in(&dir);
    }
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

        let thread = read_thread_in(&dir, thread_id).unwrap();
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

        let threads = list_threads_in(&dir);
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

        assert!(adopt_thread_in(&dir, old, new));
        assert!(read_thread_in(&dir, old).is_none());
        assert_eq!(read_thread_in(&dir, new).unwrap().thread_id, new);

        let other = Uuid::new_v4();
        append_record_in(&dir, old, &meta(old, 20)).unwrap();
        append_record_in(&dir, other, &meta(other, 30)).unwrap();
        assert!(!adopt_thread_in(&dir, old, other));
        assert!(read_thread_in(&dir, old).is_some());
        assert!(read_thread_in(&dir, other).is_some());
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
