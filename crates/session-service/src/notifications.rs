//! On-disk ring of stored session notifications (`<state>/notifications.json`).
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hh_protocol::SessionNotification;
use serde::{Deserialize, Serialize};

use crate::persistence::quarantine_private_file;
use crate::registry::MAX_NOTIFICATIONS;

const FILE_NAME: &str = "notifications.json";
const FORMAT_VERSION: u16 = 1;
/// Generous bound for 200 notifications with long OSC messages.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// The ring as last written: its items, oldest first, and the id the next
/// notification receives, so ids keep increasing across service restarts.
#[derive(Debug, Default)]
pub(crate) struct StoredNotifications {
    pub(crate) items: VecDeque<SessionNotification>,
    pub(crate) next_id: u64,
}

#[derive(Serialize)]
struct EncodedRing<'a> {
    version: u16,
    next_id: u64,
    items: &'a VecDeque<SessionNotification>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodedRing {
    version: u16,
    next_id: u64,
    items: VecDeque<SessionNotification>,
}

#[derive(Clone, Debug)]
pub(crate) struct NotificationStore {
    path: PathBuf,
}

impl NotificationStore {
    pub(crate) fn in_state_directory(directory: &Path) -> Self {
        Self {
            path: directory.join(FILE_NAME),
        }
    }

    /// Loads the ring; a missing file starts empty and an invalid one is
    /// quarantined so the service starts empty instead of failing.
    pub(crate) fn load_or_quarantine(&self) -> StoredNotifications {
        match self.load() {
            Ok(Some(stored)) => stored,
            Ok(None) => StoredNotifications {
                items: VecDeque::new(),
                next_id: 1,
            },
            Err(error) => {
                match quarantine_private_file(&self.path, "notifications") {
                    Ok(quarantined) => eprintln!(
                        "quarantined invalid Harness Harlot notifications at {}: {error:#}",
                        quarantined.display()
                    ),
                    Err(quarantine_error) => eprintln!(
                        "ignoring invalid Harness Harlot notifications ({error:#}); quarantine failed: {quarantine_error:#}"
                    ),
                }
                StoredNotifications {
                    items: VecDeque::new(),
                    next_id: 1,
                }
            }
        }
    }

    fn load(&self) -> Result<Option<StoredNotifications>> {
        let bytes = match hh_protocol::read_private_file(&self.path, MAX_FILE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read notifications {}", self.path.display()));
            }
        };
        let decoded: DecodedRing =
            serde_json::from_slice(&bytes).context("decode stored notifications")?;
        if decoded.version != FORMAT_VERSION {
            bail!("unsupported notifications format {}", decoded.version);
        }
        if decoded.next_id == 0 {
            bail!("stored next notification id must be positive");
        }
        if decoded.items.len() > MAX_NOTIFICATIONS {
            bail!("stored notifications exceed {MAX_NOTIFICATIONS} items");
        }
        let mut previous = 0;
        for item in &decoded.items {
            if item.id <= previous || item.id >= decoded.next_id {
                bail!("stored notification ids are out of order");
            }
            previous = item.id;
        }
        Ok(Some(StoredNotifications {
            items: decoded.items,
            next_id: decoded.next_id,
        }))
    }

    /// Atomically replaces the file with an owner-only copy of the ring.
    pub(crate) fn write(&self, items: &VecDeque<SessionNotification>, next_id: u64) -> Result<()> {
        let bytes = serde_json::to_vec(&EncodedRing {
            version: FORMAT_VERSION,
            next_id,
            items,
        })
        .context("encode notifications")?;
        hh_protocol::atomic_write_private(&self.path, &bytes)
            .with_context(|| format!("write notifications {}", self.path.display()))
    }
}
