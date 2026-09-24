use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{Context, Result};

const MAX_EXTENSION_BYTES: u64 = 1024 * 1024;

pub(crate) fn ensure_extension_file() -> Result<PathBuf> {
    let source = include_bytes!("../../bundled/hh-orchestrator.ts");
    let state = hh_protocol::state_directory().context("state directory is unavailable")?;
    let directory = state.join("assistant");
    hh_protocol::ensure_private_directory(&directory)
        .with_context(|| format!("prepare assistant directory {}", directory.display()))?;
    let path = directory.join("hh-orchestrator.ts");
    let unchanged = match hh_protocol::read_private_file(&path, MAX_EXTENSION_BYTES) {
        Ok(existing) => existing == source,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read assistant extension {}", path.display()));
        }
    };
    if !unchanged {
        hh_protocol::atomic_write_private(&path, source)
            .with_context(|| format!("write assistant extension {}", path.display()))?;
    }
    Ok(path)
}
