//! Bounded network fetches for the signed stable update channel.

use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
#[cfg(target_os = "linux")]
use std::process::Command;
use url::Url;

use crate::{
    CurrentRelease, EDGE_UPDATE_RELEASE_PREFIX, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES,
    ReleaseArtifact, UPDATE_MANIFEST_BASE, UpdateChannel, select_verified_update,
    update_manifest_name, verify_artifact_file, verify_manifest_with_trusted_keys_for_channel,
};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);
const GITHUB_RELEASE_ASSET_HOST: &str = "release-assets.githubusercontent.com";
static DOWNLOAD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// An owned description of one verified, newer update artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedUpdate {
    pub version: String,
    pub build: u64,
    pub artifact: ReleaseArtifact,
    pub requires_service_restart: bool,
}

/// Returns the stable update feed's platform spelling.
///
/// # Errors
///
/// Returns an error on operating systems without a packaged release channel.
pub fn runtime_platform() -> Result<&'static str> {
    match std::env::consts::OS {
        "macos" => Ok("macos"),
        "linux" => Ok("linux"),
        platform => anyhow::bail!("unsupported update platform: {platform}"),
    }
}

/// Maps the Rust target architecture to the release feed's spelling.
///
/// # Errors
///
/// Returns an error on architectures for which no release artifact is built.
pub fn runtime_architecture() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("x86_64"),
        architecture => anyhow::bail!("unsupported update architecture: {architecture}"),
    }
}

/// Fetches and verifies the signed stable manifest, returning only a newer
/// artifact for the requested architecture.
///
/// # Errors
///
/// Network, size-limit, signature, manifest-policy, and version errors are
/// returned to the caller. A missing release is not treated as "up to date".
pub fn fetch_available_update(current: &CurrentRelease<'_>) -> Result<Option<OwnedUpdate>> {
    fetch_available_update_for_channel(current, UpdateChannel::Stable)
}

/// Fetch and authenticate the newest release from the requested channel.
///
/// # Errors
///
/// Returns an error when network, redirect, size, signature, channel, platform,
/// architecture, host, or release-selection policy rejects the response.
pub fn fetch_available_update_for_channel(
    current: &CurrentRelease<'_>,
    channel: UpdateChannel,
) -> Result<Option<OwnedUpdate>> {
    let edge_base;
    let base = match channel {
        UpdateChannel::Stable => {
            UPDATE_MANIFEST_BASE.context("production update manifest URL is not configured")?
        }
        UpdateChannel::Edge => {
            let prefix =
                EDGE_UPDATE_RELEASE_PREFIX.context("edge update manifest URL is not configured")?;
            edge_base = format!("{prefix}-{}-{}", current.platform, current.architecture);
            &edge_base
        }
    };
    let manifest_url = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        update_manifest_name(current.platform, current.architecture)
    );
    let signature_url = format!("{manifest_url}.sig");
    let agent = ureq::AgentBuilder::new()
        .timeout(NETWORK_TIMEOUT)
        .https_only(true)
        .build();
    let manifest_bytes = fetch_capped_feed(&agent, &manifest_url, MAX_MANIFEST_BYTES)
        .context("fetch stable update manifest")?;
    let signature_bytes = fetch_capped_feed(&agent, &signature_url, MAX_SIGNATURE_BYTES)
        .context("fetch stable update signature")?;
    let signature =
        std::str::from_utf8(&signature_bytes).context("update manifest signature is not UTF-8")?;
    let manifest =
        verify_manifest_with_trusted_keys_for_channel(&manifest_bytes, signature, channel)?;
    let selected = select_verified_update(&manifest, current)?;
    #[cfg(target_os = "linux")]
    if current.platform == "linux" && selected.is_some() {
        ensure_minimum_glibc(
            manifest
                .minimum_glibc
                .as_deref()
                .context("Linux update manifest has no minimum glibc version")?,
        )?;
    }
    Ok(selected.map(|update| OwnedUpdate {
        version: update.manifest.version.clone(),
        build: update.manifest.build,
        artifact: update.artifact.clone(),
        requires_service_restart: update.requires_service_restart,
    }))
}

fn publish_verified_download(
    temporary: &Path,
    destination: &Path,
    artifact: &ReleaseArtifact,
) -> Result<()> {
    // A hard link publishes the already-fsynced inode atomically and never
    // replaces another file. A matching file left by an interrupted attempt is
    // safe to reuse; any other collision remains a fail-closed recovery error.
    match fs::hard_link(temporary, destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_artifact_file(artifact, destination).with_context(|| {
                format!(
                    "{} already exists but does not match the signed update; remove it and retry",
                    destination.display()
                )
            })?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "publish verified update {} as {} without replacement",
                    temporary.display(),
                    destination.display()
                )
            });
        }
    }
    if let Err(error) = fs::remove_file(temporary) {
        eprintln!(
            "verified update published but temporary {} could not be removed: {error}",
            temporary.display()
        );
    }
    Ok(())
}

/// Downloads one previously verified update description and verifies the
/// completed regular file against its signed size and SHA-256.
///
/// # Errors
///
/// Returns an error for unsafe filenames, network or filesystem failures,
/// oversized responses, or artifact verification failures.
pub fn download_verified(update: &OwnedUpdate, dest_dir: &Path) -> Result<PathBuf> {
    let url = Url::parse(&update.artifact.url).context("parse verified update artifact URL")?;
    let file_name = url
        .path_segments()
        .and_then(Iterator::last)
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .context("verified update artifact URL has no safe filename")?;
    ensure!(
        file_name == update.artifact.file_name,
        "verified update URL filename differs from signed artifact"
    );
    ensure!(
        !file_name.contains('/') && !file_name.contains('\\'),
        "verified update artifact filename contains a path separator"
    );

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dest_dir)
        .with_context(|| format!("create update download directory {}", dest_dir.display()))?;
    let sequence = DOWNLOAD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = dest_dir.join(format!(
        ".hh-update-download-{}-{sequence}",
        std::process::id()
    ));
    let destination = dest_dir.join(file_name);

    let result = (|| -> Result<()> {
        let response = ureq::AgentBuilder::new()
            .timeout_connect(NETWORK_TIMEOUT)
            .timeout_read(NETWORK_TIMEOUT)
            .https_only(true)
            .build()
            .get(&update.artifact.url)
            .call()
            .context("download verified update artifact")?;
        // `ureq` follows redirects; trust the final destination before reading
        // any response bytes, not only the signed URL from the manifest.
        ensure_trusted_response_url(response.get_url())?;
        let mut input = response
            .into_reader()
            .take(update.artifact.size.saturating_add(1));
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        std::io::copy(&mut input, &mut output)
            .with_context(|| format!("write {}", temporary.display()))?;
        output.flush()?;
        output.sync_all()?;
        drop(output);

        verify_artifact_file(&update.artifact, &temporary)?;
        publish_verified_download(&temporary, &destination, &update.artifact)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(destination)
}
#[cfg(target_os = "linux")]
fn ensure_minimum_glibc(required: &str) -> Result<()> {
    // The updater must not execute an attacker-controlled `getconf` from PATH.
    // Supported glibc release hosts provide the coreutils binary here.
    let output = Command::new("/usr/bin/getconf")
        .arg("GNU_LIBC_VERSION")
        .output()
        .context("query GNU libc version")?;
    ensure!(
        output.status.success(),
        "getconf could not query GNU libc version"
    );
    let reported = String::from_utf8(output.stdout).context("GNU libc version is not UTF-8")?;
    let installed = reported
        .trim()
        .strip_prefix("glibc ")
        .context("system C library is not GNU libc")?;
    ensure!(
        dotted_version_at_least(installed, required)?,
        "update requires glibc {required} or newer; system has {installed}"
    );
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn dotted_version_at_least(installed: &str, required: &str) -> Result<bool> {
    fn components(version: &str) -> Result<Vec<u64>> {
        version
            .split('.')
            .map(|component| {
                let numeric_len = component.bytes().take_while(u8::is_ascii_digit).count();
                let numeric = &component[..numeric_len];
                ensure!(
                    !numeric.is_empty(),
                    "version component in {version} has no numeric prefix"
                );
                numeric
                    .parse()
                    .with_context(|| format!("parse version component in {version}"))
            })
            .collect()
    }
    let mut installed = components(installed)?;
    let mut required = components(required)?;
    let width = installed.len().max(required.len());
    installed.resize(width, 0);
    required.resize(width, 0);
    Ok(installed >= required)
}

fn ensure_trusted_response_url(response_url: &str) -> Result<()> {
    let url = Url::parse(response_url).context("parse final update response URL")?;
    ensure!(url.scheme() == "https", "update redirect must use HTTPS");
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.port().is_none(),
        "update redirect must not contain credentials or a custom port"
    );
    ensure!(
        matches!(
            url.host_str(),
            Some("github.com" | GITHUB_RELEASE_ASSET_HOST)
        ),
        "update redirect host is not trusted"
    );
    Ok(())
}

fn ensure_trusted_feed_response_url(response_url: &str, requested_url: &str) -> Result<()> {
    let requested = Url::parse(requested_url).context("parse requested update feed URL")?;
    ensure!(
        requested.scheme() == "https"
            && requested.host_str() == Some("harnessharlot.com")
            && requested.username().is_empty()
            && requested.password().is_none()
            && requested.port().is_none()
            && requested.query().is_none()
            && requested.fragment().is_none()
            && requested.path().starts_with("/releases/stable-v2/"),
        "requested update feed URL is not the canonical stable-v2 origin"
    );
    ensure!(
        response_url == requested_url,
        "update feed must not redirect away from its exact requested URL"
    );
    Ok(())
}

fn fetch_capped_feed(agent: &ureq::Agent, url: &str, maximum: u64) -> Result<Vec<u8>> {
    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    ensure_trusted_feed_response_url(response.get_url(), url)?;
    let mut reader = response.into_reader().take(maximum.saturating_add(1));
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {url}"))?;
    ensure!(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= maximum,
        "update response exceeds {maximum} bytes"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::dotted_version_at_least;
    use super::{
        DOWNLOAD_SEQUENCE, ensure_trusted_feed_response_url, ensure_trusted_response_url,
        publish_verified_download,
    };
    use crate::{ReleaseArtifact, sha256_hex};

    #[test]
    fn stable_feed_accepts_only_the_exact_website_alias_origin() {
        let manifest =
            "https://harnessharlot.com/releases/stable-v2/manifest-linux-x86_64-v2.update.json";
        assert!(ensure_trusted_feed_response_url(manifest, manifest).is_ok());
        for response in [
            "http://harnessharlot.com/releases/stable-v2/manifest-linux-x86_64-v2.update.json",
            "https://www.harnessharlot.com/releases/stable-v2/manifest-linux-x86_64-v2.update.json",
            "https://harnessharlot.com/releases/stable-v2/other.json",
            "https://harnessharlot.com:443/releases/stable-v2/manifest-linux-x86_64-v2.update.json",
        ] {
            assert!(
                ensure_trusted_feed_response_url(response, manifest).is_err(),
                "accepted {response}"
            );
        }
    }
    #[test]
    fn accepts_only_expected_https_redirect_hosts() {
        assert!(ensure_trusted_response_url("https://github.com/release").is_ok());
        assert!(
            ensure_trusted_response_url(
                "https://release-assets.githubusercontent.com/release?token=opaque"
            )
            .is_ok()
        );
        for url in [
            "http://github.com/release",
            "https://github.com:8443/release",
            "https://github.com.evil.invalid/release",
            "https://objects.githubusercontent.com/release",
            "https://user@github.com/release",
        ] {
            assert!(ensure_trusted_response_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn verified_download_publication_never_replaces_an_existing_file() {
        let sequence = DOWNLOAD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "hh-update-publish-test-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let temporary = root.join("temporary");
        let destination = root.join("artifact");
        let verified = b"verified";
        let artifact = ReleaseArtifact {
            platform: "macos".to_owned(),
            architecture: "arm64".to_owned(),
            format: "dmg".to_owned(),
            file_name: "artifact".to_owned(),
            url: "https://github.com/example/artifact".to_owned(),
            sha256: sha256_hex(verified),
            size: u64::try_from(verified.len()).unwrap(),
        };
        std::fs::write(&temporary, verified).unwrap();
        std::fs::write(&destination, b"existing").unwrap();

        assert!(publish_verified_download(&temporary, &destination, &artifact).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing");
        assert_eq!(std::fs::read(&temporary).unwrap(), verified);

        std::fs::write(&destination, verified).unwrap();
        publish_verified_download(&temporary, &destination, &artifact).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), verified);
        assert!(!temporary.exists());

        std::fs::write(&temporary, verified).unwrap();
        std::fs::remove_file(&destination).unwrap();
        publish_verified_download(&temporary, &destination, &artifact).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), verified);
        assert!(!temporary.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compares_glibc_versions_numerically() {
        assert!(dotted_version_at_least("2.35", "2.35").unwrap());
        assert!(dotted_version_at_least("2.39", "2.35").unwrap());
        assert!(!dotted_version_at_least("2.9", "2.35").unwrap());

        assert!(dotted_version_at_least("2.35-9", "2.35").unwrap());
        assert!(dotted_version_at_least("2.35p1", "2.35").unwrap());
        assert!(dotted_version_at_least("2.p1", "2.35").is_err());
        assert!(dotted_version_at_least("2.35.1", "2.35").unwrap());
    }
}
