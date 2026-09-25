use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use uuid::Uuid;

pub(crate) const MAX_GALLERY_IMAGE_BYTES: u64 = 25 * 1024 * 1024;

/// Returns the normalized extension for accepted PNG, JPEG, GIF, and WebP bytes.
pub(crate) fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Builds `<unix_ms>-<stem>.<ext>` with a filesystem-safe, bounded stem.
pub(crate) fn gallery_file_name(source: &Path, ext: &str, now_ms: u64) -> String {
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(40)
        .collect::<String>();
    let stem = if stem.is_empty() { "image" } else { &stem };
    format!("{now_ms}-{stem}.{ext}")
}

/// Copies one accepted image into the workspace's owner-only gallery directory.
pub(crate) fn import_gallery_image(workspace_id: Uuid, source: &Path) -> Result<PathBuf> {
    if !source.is_absolute() {
        bail!("gallery image path must be absolute");
    }
    let canonical = source
        .canonicalize()
        .with_context(|| format!("resolve gallery image {}", source.display()))?;
    let metadata = canonical
        .metadata()
        .with_context(|| format!("read gallery image metadata {}", canonical.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", canonical.display());
    }
    if metadata.len() > MAX_GALLERY_IMAGE_BYTES {
        bail!(
            "{} exceeds the {}-byte gallery image limit",
            canonical.display(),
            MAX_GALLERY_IMAGE_BYTES
        );
    }
    let bytes = fs::read(&canonical)
        .with_context(|| format!("read gallery image {}", canonical.display()))?;
    let Some(extension) = image_extension(&bytes) else {
        bail!(
            "{} is not a PNG, JPEG, GIF, or WebP image",
            canonical.display()
        );
    };
    let directory = hh_protocol::gallery_directory(workspace_id)
        .context("Harness Harlot state directory is unavailable")?;
    hh_protocol::ensure_private_directory(&directory)
        .with_context(|| format!("create gallery directory {}", directory.display()))?;
    let now_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let destination = directory.join(gallery_file_name(&canonical, extension, now_ms));
    hh_protocol::atomic_write_private(&destination, &bytes)
        .with_context(|| format!("write gallery image {}", destination.display()))?;
    Ok(destination)
}
