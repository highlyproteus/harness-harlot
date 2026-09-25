//! Kitty clipboard paste events (OSC 5522) served from bytes the desktop
//! explicitly handed over. The service never reads the system clipboard.
//!
//! Protocol: <https://sw.kovidgoyal.net/kitty/clipboard/> and
//! <https://rockorager.dev/misc/bracketed-paste-mime/>. A paste announces the
//! available MIME types with a one-time password; the application then reads
//! the types it wants with that password, once, within [`PENDING_PASTE_TTL`].

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hh_terminal_model::TerminalRequest;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::pty::PtySession;

/// Directory under `std::env::temp_dir()` where the desktop materializes
/// pasted clipboard images.
pub(crate) const PASTE_DIRECTORY_NAME: &str = "harness-harlot-paste";
pub(crate) const MAX_PASTE_IMAGE_BYTES: u64 = 25 * 1024 * 1024;
pub(crate) const MAX_PASTE_TEXT_BYTES: usize = 1024 * 1024;
/// Largest raw (pre-base64) payload of one `status=DATA` packet.
pub(crate) const MAX_DATA_CHUNK_BYTES: usize = 4096;
/// `S_IRWXG | S_IRWXO` as a literal: `mode_t` is 16 bits on macOS and 32 on
/// Linux, so the libc constants would need a platform-specific conversion.
const GROUP_OTHER_PERMISSIONS: u32 = 0o077;

const PENDING_PASTE_TTL: Duration = Duration::from_mins(2);
const MAX_QUEUED_REQUESTS: usize = 64;
const MAX_REPLY_ID_CHARS: usize = 64;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const PNG_MIME: &str = "image/png";
const TEXT_MIME: &str = "text/plain";
/// Kitty's "list the available MIME types" read target.
const LISTING_MIME: &str = ".";

pub(crate) fn paste_directory() -> PathBuf {
    std::env::temp_dir().join(PASTE_DIRECTORY_NAME)
}

/// Reads and deletes a desktop-materialized PNG. The file must sit directly
/// in the private `directory`, be a regular file owned by this user (never
/// followed through a symlink), be at most [`MAX_PASTE_IMAGE_BYTES`], and
/// start with the PNG signature. Rejected files are left untouched.
pub(crate) fn take_paste_image(path: &Path, directory: &Path) -> Result<Vec<u8>> {
    ensure!(path.is_absolute(), "pasted image path must be absolute");
    ensure!(
        path.parent() == Some(directory)
            && matches!(path.components().next_back(), Some(Component::Normal(_))),
        "pasted image must be in {}",
        directory.display()
    );
    let uid = rustix::process::geteuid().as_raw();
    let directory_metadata = std::fs::symlink_metadata(directory)
        .with_context(|| format!("inspect paste directory {}", directory.display()))?;
    ensure!(
        directory_metadata.is_dir()
            && directory_metadata.uid() == uid
            && directory_metadata.mode() & GROUP_OTHER_PERMISSIONS == 0,
        "paste directory is not a private directory owned by this user"
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open pasted image {}", path.display()))?;
    let metadata = file.metadata().context("inspect pasted image")?;
    ensure!(metadata.is_file(), "pasted image is not a regular file");
    ensure!(
        metadata.uid() == uid,
        "pasted image is not owned by this user"
    );
    ensure!(
        metadata.len() <= MAX_PASTE_IMAGE_BYTES,
        "pasted image exceeds the 25 MiB limit"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    (&mut file)
        .take(MAX_PASTE_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read pasted image")?;
    ensure!(
        bytes.len() as u64 <= MAX_PASTE_IMAGE_BYTES,
        "pasted image exceeds the 25 MiB limit"
    );
    ensure!(
        bytes.starts_with(PNG_SIGNATURE),
        "pasted image is not a PNG"
    );
    std::fs::remove_file(path)
        .with_context(|| format!("remove pasted image {}", path.display()))?;
    Ok(bytes)
}

struct PendingPaste {
    password: String,
    items: Vec<(&'static str, Vec<u8>)>,
    created_at: Instant,
}

#[derive(Default)]
struct ReplyQueue {
    requests: VecDeque<TerminalRequest>,
    draining: bool,
}

/// Per-pane paste-event state: at most one pending paste, plus an ordered
/// queue of DECRQM / OSC 5522 requests answered off the output reader thread
/// (a tmux reader must never wait on its own tmux commands).
#[derive(Default)]
pub(crate) struct PasteEvents {
    session: OnceLock<Weak<PtySession>>,
    pending: Mutex<Option<PendingPaste>>,
    queue: Mutex<ReplyQueue>,
    /// Serializes every reply and announcement written to the pane.
    delivery: Mutex<()>,
}

impl std::fmt::Debug for PasteEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PasteEvents")
            .field("pending", &self.pending.lock().is_some())
            .finish_non_exhaustive()
    }
}

impl PasteEvents {
    pub(crate) fn bind(&self, session: &Arc<PtySession>) {
        let _ = self.session.set(Arc::downgrade(session));
    }

    /// Queues requests the pane wrote and answers them in order on a worker.
    pub(crate) fn enqueue(self: &Arc<Self>, requests: Vec<TerminalRequest>) {
        if requests.is_empty() {
            return;
        }
        let mut queue = self.queue.lock();
        for request in requests {
            if queue.requests.len() < MAX_QUEUED_REQUESTS {
                queue.requests.push_back(request);
            }
        }
        if queue.draining {
            return;
        }
        queue.draining = true;
        drop(queue);
        let events = Arc::clone(self);
        if let Err(error) = thread::Builder::new()
            .name("hh-paste-events".to_owned())
            .spawn(move || events.drain())
        {
            eprintln!("could not start the paste-event responder: {error}");
            let mut queue = self.queue.lock();
            queue.requests.clear();
            queue.draining = false;
        }
    }

    fn drain(&self) {
        loop {
            let request = {
                let mut queue = self.queue.lock();
                let Some(request) = queue.requests.pop_front() else {
                    queue.draining = false;
                    return;
                };
                request
            };
            let Some(session) = self.session.get().and_then(Weak::upgrade) else {
                let mut queue = self.queue.lock();
                queue.requests.clear();
                queue.draining = false;
                return;
            };
            let _delivery = self.delivery.lock();
            if let Some(reply) = self.reply(&request, Instant::now())
                && let Err(error) = session.write_reply(reply.into_bytes())
            {
                eprintln!("paste-event reply was not delivered: {error}");
            }
        }
    }

    /// Announces a paste to the application and keeps its bytes for one read.
    pub(crate) fn offer(
        &self,
        session: &PtySession,
        png: Vec<u8>,
        text: Option<String>,
    ) -> Result<()> {
        let _delivery = self.delivery.lock();
        let password = BASE64.encode(Uuid::new_v4().as_bytes());
        let announcement = self.announce(png, text, password, Instant::now());
        session
            .write_reply(announcement.into_bytes())
            .map_err(|error| {
                *self.pending.lock() = None;
                anyhow!("announce paste event: {error}")
            })
    }

    /// Stores the pending paste (replacing any earlier one) and returns the
    /// `status=OK` / per-type `status=DATA` / `status=DONE` announcement.
    fn announce(
        &self,
        png: Vec<u8>,
        text: Option<String>,
        password: String,
        now: Instant,
    ) -> String {
        let mut items = vec![(PNG_MIME, png)];
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            items.push((TEXT_MIME, text.into_bytes()));
        }
        let mut announcement = String::new();
        push_packet(
            &mut announcement,
            &format!("type=read:status=OK:pw={password}"),
            "",
        );
        for (mime, _) in &items {
            push_packet(
                &mut announcement,
                &format!("type=read:status=DATA:mime={}", BASE64.encode(mime)),
                "",
            );
        }
        push_packet(&mut announcement, "type=read:status=DONE", "");
        *self.pending.lock() = Some(PendingPaste {
            password,
            items,
            created_at: now,
        });
        announcement
    }

    fn reply(&self, request: &TerminalRequest, now: Instant) -> Option<String> {
        match request {
            TerminalRequest::ReportEnhancedPasteMode { enabled } => {
                Some(format!("\x1b[?5522;{}$y", if *enabled { 1 } else { 2 }))
            }
            TerminalRequest::ClipboardPacket(body) => self.reply_to_clipboard_packet(body, now),
        }
    }

    fn reply_to_clipboard_packet(&self, body: &str, now: Instant) -> Option<String> {
        let (metadata, payload) = body.split_once(';').unwrap_or((body, ""));
        let metadata = metadata
            .split(':')
            .filter_map(|part| part.split_once('='))
            .collect::<Vec<_>>();
        let get = |key: &str| {
            metadata
                .iter()
                .find(|(candidate, _)| *candidate == key)
                .map(|(_, value)| *value)
        };
        // Only application read requests get answers; `status` marks a
        // terminal-to-application packet echoed back.
        if get("type") != Some("read") || get("status").is_some() {
            return None;
        }
        let id = get("id").map(sanitize_reply_id).unwrap_or_default();
        let status = |code: &str| {
            let mut reply = String::new();
            push_packet(&mut reply, &format!("type=read:status={code}{id}"), "");
            reply
        };
        if get("loc") == Some("primary") {
            return Some(status("ENOSYS"));
        }
        let Some(requested) = requested_mimes(get("mime"), payload) else {
            return Some(status("EINVAL"));
        };
        let mut pending = self.pending.lock();
        if pending
            .as_ref()
            .is_some_and(|paste| now.duration_since(paste.created_at) > PENDING_PASTE_TTL)
        {
            *pending = None;
        }
        let Some(paste) = pending
            .as_ref()
            .filter(|paste| get("pw").is_some_and(|pw| pw == paste.password))
        else {
            return Some(status("EPERM"));
        };
        let mut reply = status("OK");
        if requested.iter().any(|mime| mime == LISTING_MIME) {
            let listing = paste
                .items
                .iter()
                .map(|(mime, _)| *mime)
                .collect::<Vec<_>>()
                .join(" ");
            push_packet(
                &mut reply,
                &format!(
                    "type=read:status=DATA:mime={}{id}",
                    BASE64.encode(LISTING_MIME)
                ),
                &BASE64.encode(listing),
            );
            push_packet(&mut reply, &format!("type=read:status=DONE{id}"), "");
            return Some(reply);
        }
        let mut served = Vec::new();
        for mime in &requested {
            if served.contains(&mime.as_str()) {
                continue;
            }
            let Some((mime, bytes)) = paste.items.iter().find(|(held, _)| held == mime) else {
                continue;
            };
            served.push(*mime);
            let metadata = format!("type=read:status=DATA:mime={}{id}", BASE64.encode(mime));
            for chunk in bytes.chunks(MAX_DATA_CHUNK_BYTES) {
                push_packet(&mut reply, &metadata, &BASE64.encode(chunk));
            }
        }
        if served.is_empty() {
            return Some(status("EPERM"));
        }
        push_packet(&mut reply, &format!("type=read:status=DONE{id}"), "");
        *pending = None;
        Some(reply)
    }
}

/// The MIME types a read asks for: the `mime` key, else the payload's
/// base64 space-separated list. `None` when either is malformed.
fn requested_mimes(mime_key: Option<&str>, payload: &str) -> Option<Vec<String>> {
    let decode = |value: &str| {
        BASE64
            .decode(value)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    };
    let mimes = match mime_key {
        Some(mime) => vec![decode(mime)?],
        None => decode(payload)?
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    };
    (!mimes.is_empty()).then_some(mimes)
}

/// `:id=<value>` restricted to `[A-Za-z0-9-_+.]`, or empty.
fn sanitize_reply_id(value: &str) -> String {
    let id = value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '+' | '.')
        })
        .take(MAX_REPLY_ID_CHARS)
        .collect::<String>();
    if id.is_empty() {
        id
    } else {
        format!(":id={id}")
    }
}

fn push_packet(out: &mut String, metadata: &str, payload: &str) {
    let _ = write!(out, "\x1b]5522;{metadata}");
    if !payload.is_empty() {
        out.push(';');
        out.push_str(payload);
    }
    out.push_str("\x1b\\");
}

#[cfg(test)]
#[path = "paste_events_tests.rs"]
mod tests;
