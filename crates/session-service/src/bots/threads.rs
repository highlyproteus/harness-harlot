//! Saved bot threads: the omp session files in a bot's HH-owned session
//! directory `<state>/bots/<bot id>/threads`.
//!
//! omp names each session `<timestamp>_<sessionId>.jsonl`. Current files start
//! with a fixed-width title slot line followed by the session header; legacy
//! files start with the header. Only a bounded prefix of each file is read.
use std::fs::{self, File};
use std::io::{ErrorKind, Read as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde_json::Value;
use uuid::Uuid;

/// Bytes read from the start of each session file.
const HEADER_PREFIX_BYTES: u64 = 4096;
/// Newest session files considered per listing.
pub(crate) const MAX_LISTED_THREADS: usize = 500;
/// Longest accepted agent session id.
const MAX_SESSION_ID_BYTES: usize = 128;
/// Longest thread title kept, in characters.
const MAX_TITLE_CHARS: usize = 200;
const THREADS_DIR: &str = "threads";
const SESSION_FILE_EXTENSION: &str = "jsonl";

/// A saved agent session of a bot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SavedThread {
    pub(crate) id: String,
    pub(crate) title: Option<String>,
    pub(crate) updated_ms: u64,
}

/// Whether `id` can be a session id: ASCII letters, digits, `-` and `_`.
pub(crate) fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The session directory of bot `bot_id`; it lives in the bot's default home
/// even when the bot uses a custom home, so deleting the bot deletes it.
pub(crate) fn threads_directory(bots_dir: &Path, bot_id: Uuid) -> PathBuf {
    bots_dir.join(bot_id.to_string()).join(THREADS_DIR)
}

/// Creates the owner-only session directory of bot `bot_id`.
pub(crate) fn prepare_threads_directory(bots_dir: &Path, bot_id: Uuid) -> Result<PathBuf> {
    let threads = threads_directory(bots_dir, bot_id);
    for directory in [bots_dir, threads.parent().unwrap_or(bots_dir), &threads] {
        hh_protocol::ensure_private_directory(directory)
            .with_context(|| format!("prepare bot directory {}", directory.display()))?;
    }
    Ok(threads)
}

/// The saved sessions in `directory`, newest first, at most
/// [`MAX_LISTED_THREADS`]. Unreadable or unparseable files are skipped; a
/// missing directory has none.
pub(crate) fn saved_threads(directory: &Path) -> Vec<SavedThread> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != ErrorKind::NotFound {
                eprintln!(
                    "could not list bot threads in {}: {error}",
                    directory.display()
                );
            }
            return Vec::new();
        }
    };
    let mut files = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            Path::new(&entry.file_name())
                .extension()
                .is_some_and(|extension| extension == SESSION_FILE_EXTENSION)
        })
        .filter_map(|entry| {
            // `DirEntry::metadata` does not follow symlinks.
            let metadata = entry.metadata().ok().filter(fs::Metadata::is_file)?;
            let modified = metadata.modified().ok()?;
            let updated_ms = modified.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
            Some((updated_ms, entry.path()))
        })
        .collect::<Vec<_>>();
    files.sort_unstable_by(|left, right| right.cmp(left));
    files.truncate(MAX_LISTED_THREADS);
    let mut threads: Vec<SavedThread> = Vec::with_capacity(files.len());
    for (updated_ms, path) in files {
        let Some((id, title)) = read_header(&path) else {
            continue;
        };
        if threads.iter().any(|thread| thread.id == id) {
            continue;
        }
        threads.push(SavedThread {
            id,
            title,
            updated_ms,
        });
    }
    threads
}

/// The session id and title from the start of a session file.
fn read_header(path: &Path) -> Option<(String, Option<String>)> {
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let mut prefix = Vec::new();
    file.take(HEADER_PREFIX_BYTES)
        .read_to_end(&mut prefix)
        .ok()?;
    parse_header(&prefix)
}

/// Parses the title slot and session header lines of a session file prefix.
pub(crate) fn parse_header(prefix: &[u8]) -> Option<(String, Option<String>)> {
    let mut slot_title = None;
    let mut header = None;
    for line in prefix.split(|byte| *byte == b'\n').take(2) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            break;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("title") => slot_title = title_of(&value),
            Some("session") => {
                header = Some(value);
                break;
            }
            _ => break,
        }
    }
    let header = header?;
    let id = header.get("id").and_then(Value::as_str)?;
    if !valid_session_id(id) {
        return None;
    }
    Some((id.to_owned(), slot_title.or_else(|| title_of(&header))))
}

fn title_of(value: &Value) -> Option<String> {
    let title = value.get("title").and_then(Value::as_str)?.trim();
    (!title.is_empty()).then(|| title.chars().take(MAX_TITLE_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> PathBuf {
        let directory = std::env::temp_dir().join(format!("hh-threads-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn write_session(directory: &Path, name: &str, contents: &str, age_secs: u64) {
        let path = directory.join(name);
        fs::write(&path, contents).unwrap();
        let modified = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn titles_come_from_the_title_slot_else_the_header_and_garbage_is_ignored() {
        let directory = directory();
        write_session(
            &directory,
            "2026-01-01_aaa.jsonl",
            concat!(
                r#"{"type":"title","v":1,"title":"Fix login","source":"auto","updatedAt":"x","pad":"      "}"#,
                "\n",
                r#"{"type":"session","version":3,"id":"aaa","timestamp":"x","cwd":"/","title":"Old"}"#,
                "\n",
                r#"{"type":"message"}"#,
                "\n"
            ),
            30,
        );
        write_session(
            &directory,
            "2026-01-02_bbb.jsonl",
            concat!(
                r#"{"type":"session","version":2,"id":"bbb","timestamp":"x","cwd":"/","title":"Legacy header"}"#,
                "\n",
                r#"{"type":"message"}"#,
                "\n"
            ),
            20,
        );
        write_session(
            &directory,
            "2026-01-03_ccc.jsonl",
            concat!(
                r#"{"type":"title","v":1,"title":"   ","pad":" "}"#,
                "\n",
                r#"{"type":"session","version":3,"id":"ccc","timestamp":"x","cwd":"/"}"#,
                "\n"
            ),
            10,
        );
        write_session(&directory, "garbage.jsonl", "not json at all\n", 5);
        write_session(
            &directory,
            "bad-id.jsonl",
            r#"{"type":"session","id":"../escape"}"#,
            5,
        );
        write_session(
            &directory,
            "notes.txt",
            r#"{"type":"session","id":"txt"}"#,
            1,
        );
        let truncated = format!(
            "{{\"type\":\"title\",\"title\":\"{}\"}}\n{{\"type\":\"session\",\"id\":\"ddd\"}}\n",
            "x".repeat(5000)
        );
        write_session(&directory, "long.jsonl", &truncated, 1);

        let threads = saved_threads(&directory);
        let summary = threads
            .iter()
            .map(|thread| (thread.id.as_str(), thread.title.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("ccc", None),
                ("bbb", Some("Legacy header")),
                ("aaa", Some("Fix login")),
            ]
        );
        assert!(threads[0].updated_ms > threads[1].updated_ms);
        assert!(threads[1].updated_ms > threads[2].updated_ms);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_missing_directory_has_no_threads() {
        assert!(saved_threads(&std::env::temp_dir().join(Uuid::new_v4().to_string())).is_empty());
    }

    #[test]
    fn session_ids_are_bounded_plain_tokens() {
        assert!(valid_session_id("0193b7c2-6f1e-7a4b-9c1d-2e3f4a5b6c7d"));
        assert!(valid_session_id("abc_DEF-123"));
        for invalid in ["", "../x", "a b", "a/b", "a\n", &"a".repeat(129)] {
            assert!(!valid_session_id(invalid), "{invalid:?}");
        }
    }
}
