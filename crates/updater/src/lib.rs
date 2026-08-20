//! Offline verification primitives for the Harness Harlot stable update feed.
//!
//! Production verification pins the stable GitHub host and owner-held Ed25519
//! public key. Release infrastructure may use [`verify_manifest_with_key`] with
//! an explicit fixture policy.

#[cfg(feature = "fetch")]
pub mod fetch;

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;

pub const MANIFEST_SCHEMA: &str = "hh-update-manifest-v2";
pub const PRODUCT_NAME: &str = "Harness Harlot";
pub const STABLE_CHANNEL: &str = "stable";
pub const EDGE_CHANNEL: &str = "edge";
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
pub const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateChannel {
    Stable,
    Edge,
}

impl UpdateChannel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => STABLE_CHANNEL,
            Self::Edge => EDGE_CHANNEL,
        }
    }
}
/// Build sequence embedded by release packaging. Development builds use zero.
pub fn current_build() -> u64 {
    option_env!("HH_RELEASE_BUILD")
        .and_then(|build| build.parse().ok())
        .unwrap_or(0)
}

/// Returns the immutable manifest filename for this build's release channel.
///
/// Unnotarized community macOS builds use a distinct feed so a future
/// Developer ID build can never select an ad-hoc-signed artifact.
#[must_use]
pub fn update_manifest_name(platform: &str, architecture: &str) -> String {
    if cfg!(feature = "community-macos") && platform == "macos" {
        format!("manifest-{platform}-community-{architecture}.update.json")
    } else {
        format!("manifest-{platform}-{architecture}.update.json")
    }
}

/// Whether this build may replace an installed application in the background.
#[must_use]
pub const fn automatic_install_supported(platform: &str) -> bool {
    !(cfg!(feature = "community-macos") && matches!(platform.as_bytes(), b"macos"))
}

/// Whether an explicit user request may replace this packaged platform.
#[must_use]
pub const fn explicit_install_supported(_platform: &str) -> bool {
    true
}

/// One key compiled into a production client. Key IDs are part of the signed
/// manifest and select exactly one corresponding verifying key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrustedKey {
    pub key_id: &'static str,
    pub public_key_base64: &'static str,
}

/// Populate only after the owner selects the real offline release key.
/// hh-stable-2026 is the owner-held stable-channel key; its seed never
/// enters the repository or CI as a file.
pub const TRUSTED_UPDATE_KEYS: &[TrustedKey] = &[TrustedKey {
    key_id: "hh-stable-2026",
    public_key_base64: "Cy/alHdZ5R7fSJEeuvqu1UXH9j5O0f34hWv4Rv8TFwo=",
}];
/// The immutable production update host. Only artifact URLs on this host
/// are accepted by the verifier.
pub const UPDATE_HOST: Option<&str> = Some("github.com");
/// Stable manifest location; `releases/latest/download` resolves to the
/// newest non-prerelease GitHub release without knowing its tag.
pub const UPDATE_MANIFEST_BASE: Option<&str> =
    Some("https://github.com/highlyproteus/harness-harlot/releases/latest/download");
pub const EDGE_UPDATE_RELEASE_PREFIX: Option<&str> =
    Some("https://github.com/highlyproteus/harness-harlot/releases/download/edge");
/// Apple Developer Team ID, required by the in-app installer gate. Stays
/// fail-closed (None) until Apple Developer Program enrollment completes.
pub const TRUSTED_APPLE_TEAM_ID: Option<&str> = None;

/// A signed, immutable release description. The detached signature is over
/// the exact UTF-8 manifest bytes, not over a reserialized representation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateManifest {
    pub schema: String,
    pub product: String,
    pub channel: String,
    pub key_id: String,
    pub version: String,
    pub build: u64,
    pub published_at: String,
    pub valid_until: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_macos: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_glibc: Option<String>,
    pub session_service: SessionServicePolicy,
    pub artifacts: Vec<ReleaseArtifact>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionServicePolicy {
    pub protocol_version: u16,
    /// Replacing the bundle while a service owns PTYs can strand a client on a
    /// different protocol. The updater must wait for the user to end sessions.
    pub requires_quiescent_service: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub platform: String,
    pub architecture: String,
    pub format: String,
    pub file_name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentRelease<'a> {
    pub version: &'a str,
    pub build: u64,
    pub platform: &'a str,
    pub architecture: &'a str,
    pub protocol_version: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailableUpdate<'a> {
    pub manifest: &'a UpdateManifest,
    pub artifact: &'a ReleaseArtifact,
    /// A different local IPC protocol means the bundled service must be
    /// restarted only after the user has quiesced every terminal session.
    pub requires_service_restart: bool,
}

#[derive(Deserialize)]
struct KeySelector {
    key_id: String,
}

/// Decodes a base64-encoded Ed25519 public key.
///
/// # Errors
///
/// Returns an error when the value is not base64 or is not exactly one Ed25519
/// public key.
pub fn public_key_from_base64(encoded: &str) -> Result<VerifyingKey> {
    let bytes = STANDARD
        .decode(encoded)
        .context("decode base64 update public key")?;
    let key_bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("an Ed25519 update public key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&key_bytes).context("parse Ed25519 update public key")
}

/// Verifies a manifest against the compiled production trust roots.
///
/// # Errors
///
/// Fails closed when the production host or key ring has not been configured,
/// or when signature, key binding, expiry, or artifact policy validation fails.
pub fn verify_manifest_with_trusted_keys(
    manifest_bytes: &[u8],
    signature_base64: &str,
) -> Result<UpdateManifest> {
    verify_manifest_with_trusted_keys_for_channel(
        manifest_bytes,
        signature_base64,
        UpdateChannel::Stable,
    )
}

/// Verify a channel-specific manifest against the compiled production keys.
///
/// # Errors
///
/// Returns an error for an unknown key, invalid signature, malformed manifest,
/// wrong channel, expired release, or any production policy violation.
pub fn verify_manifest_with_trusted_keys_for_channel(
    manifest_bytes: &[u8],
    signature_base64: &str,
    channel: UpdateChannel,
) -> Result<UpdateManifest> {
    let host = UPDATE_HOST.context("production update host is not configured")?;
    ensure!(
        !TRUSTED_UPDATE_KEYS.is_empty(),
        "production update trust keys are not configured"
    );
    let selector: KeySelector =
        serde_json::from_slice(manifest_bytes).context("read update manifest key selector")?;
    let trusted = TRUSTED_UPDATE_KEYS
        .iter()
        .find(|key| key.key_id == selector.key_id)
        .context("update manifest key ID is not trusted")?;
    let public_key = public_key_from_base64(trusted.public_key_base64)?;
    verify_manifest_with_key_for_channel(
        manifest_bytes,
        signature_base64,
        trusted.key_id,
        &public_key,
        host,
        OffsetDateTime::now_utc(),
        false,
        channel,
    )
}

/// Verifies exact manifest bytes with one explicitly selected key.
///
/// `allow_test_key` exists only for local release fixtures. Production callers
/// must pass `false`.
///
/// # Errors
///
/// Returns an error when the signature is invalid, `key_id` does not bind to
/// this key, the manifest is expired, or an artifact escapes `update_host`.
pub fn verify_manifest_with_key(
    manifest_bytes: &[u8],
    signature_base64: &str,
    expected_key_id: &str,
    public_key: &VerifyingKey,
    update_host: &str,
    now: OffsetDateTime,
    allow_test_key: bool,
) -> Result<UpdateManifest> {
    verify_manifest_with_key_for_channel(
        manifest_bytes,
        signature_base64,
        expected_key_id,
        public_key,
        update_host,
        now,
        allow_test_key,
        UpdateChannel::Stable,
    )
}

#[allow(clippy::too_many_arguments)]
/// Verify a channel-specific manifest with an explicitly supplied key.
///
/// # Errors
///
/// Returns an error when signature verification, parsing, channel matching, or
/// manifest policy validation fails.
pub fn verify_manifest_with_key_for_channel(
    manifest_bytes: &[u8],
    signature_base64: &str,
    expected_key_id: &str,
    public_key: &VerifyingKey,
    update_host: &str,
    now: OffsetDateTime,
    allow_test_key: bool,
    channel: UpdateChannel,
) -> Result<UpdateManifest> {
    let signature_bytes = STANDARD
        .decode(signature_base64.trim())
        .context("decode base64 update manifest signature")?;
    let signature = Signature::from_slice(&signature_bytes)
        .context("an Ed25519 update signature must be 64 bytes")?;
    public_key
        .verify(manifest_bytes, &signature)
        .context("update manifest signature is not trusted")?;

    let manifest: UpdateManifest =
        serde_json::from_slice(manifest_bytes).context("parse signed update manifest JSON")?;
    ensure!(
        manifest.key_id == expected_key_id,
        "update manifest key ID does not match its verifying key"
    );
    validate_manifest_for_host_and_channel(&manifest, update_host, now, allow_test_key, channel)?;
    Ok(manifest)
}

/// Checks stable-channel, expiry, and immutable artifact-host policy.
///
/// # Errors
///
/// Returns an error for unsupported fields, unsafe artifact references, test
/// trust material in production, or an expired manifest.
pub fn validate_manifest_for_host(
    manifest: &UpdateManifest,
    update_host: &str,
    now: OffsetDateTime,
    allow_test_key: bool,
) -> Result<()> {
    validate_manifest_for_host_and_channel(
        manifest,
        update_host,
        now,
        allow_test_key,
        UpdateChannel::Stable,
    )
}

/// Validate manifest policy for the expected host and requested channel.
///
/// # Errors
///
/// Returns an error when any schema, product, channel, time, platform,
/// architecture, artifact, host, or test-key policy is violated.
pub fn validate_manifest_for_host_and_channel(
    manifest: &UpdateManifest,
    update_host: &str,
    now: OffsetDateTime,
    allow_test_key: bool,
    channel: UpdateChannel,
) -> Result<()> {
    ensure!(
        manifest.schema == MANIFEST_SCHEMA,
        "unsupported update manifest schema"
    );
    ensure!(
        manifest.product == PRODUCT_NAME,
        "unexpected update product"
    );
    ensure!(
        manifest.channel == channel.as_str(),
        "update manifest channel does not match the requested channel"
    );
    ensure!(
        !manifest.key_id.trim().is_empty(),
        "update manifest key ID is empty"
    );
    if !allow_test_key {
        ensure!(
            manifest.key_id != "test-only-v1",
            "test update key is forbidden in production"
        );
        ensure!(
            !update_host.ends_with(".invalid"),
            "invalid fixture host is forbidden in production"
        );
    }
    ensure!(
        !update_host.is_empty()
            && !update_host.contains('/')
            && !update_host.contains('@')
            && !update_host.contains(':'),
        "update host must be a bare HTTPS hostname"
    );
    Version::parse(&manifest.version).context("parse update version as semantic version")?;
    ensure!(manifest.build > 0, "update build must be greater than zero");

    let published_at = OffsetDateTime::parse(&manifest.published_at, &Rfc3339)
        .context("parse manifest published_at as RFC 3339")?;
    let valid_until = OffsetDateTime::parse(&manifest.valid_until, &Rfc3339)
        .context("parse manifest valid_until as RFC 3339")?;
    ensure!(
        valid_until > published_at,
        "manifest expiry must follow publication"
    );
    ensure!(
        published_at <= now,
        "manifest publication time is in the future"
    );
    ensure!(now <= valid_until, "update manifest has expired");

    match manifest.platform.as_str() {
        "macos" => {
            ensure!(
                manifest.minimum_glibc.is_none(),
                "macOS manifest must not declare minimum glibc"
            );
            validate_dotted_version(
                manifest
                    .minimum_macos
                    .as_deref()
                    .context("macOS manifest has no minimum macOS version")?,
                "macOS",
            )?;
        }
        "linux" => {
            ensure!(
                manifest.minimum_macos.is_none(),
                "Linux manifest must not declare minimum macOS"
            );
            validate_dotted_version(
                manifest
                    .minimum_glibc
                    .as_deref()
                    .context("Linux manifest has no minimum glibc version")?,
                "glibc",
            )?;
        }
        _ => bail!("unsupported update platform"),
    }
    ensure!(
        manifest.session_service.protocol_version > 0,
        "invalid session service protocol"
    );
    ensure!(
        manifest.session_service.requires_quiescent_service,
        "updates must require a quiescent session service"
    );
    ensure!(
        !manifest.artifacts.is_empty(),
        "update manifest has no artifacts"
    );
    for artifact in &manifest.artifacts {
        ensure!(
            artifact.platform == manifest.platform,
            "update artifact platform does not match manifest"
        );
        validate_artifact(artifact, update_host)?;
    }
    Ok(())
}

fn validate_dotted_version(version: &str, label: &str) -> Result<()> {
    ensure!(!version.is_empty(), "minimum {label} version is empty");
    for component in version.split('.') {
        ensure!(
            !component.is_empty() && component.parse::<u16>().is_ok(),
            "minimum {label} version is invalid"
        );
    }
    Ok(())
}

fn validate_artifact(artifact: &ReleaseArtifact, update_host: &str) -> Result<()> {
    ensure!(
        matches!(artifact.architecture.as_str(), "arm64" | "x86_64"),
        "unsupported update architecture"
    );
    let valid_format = matches!(
        (artifact.platform.as_str(), artifact.format.as_str()),
        ("macos", "dmg") | ("linux", "tar.gz")
    );
    ensure!(valid_format, "unsupported update artifact format");
    if artifact.platform == "macos" {
        let community_artifact = artifact.file_name.ends_with("-community.dmg");
        ensure!(
            community_artifact == cfg!(feature = "community-macos"),
            "macOS update artifact does not match this build's release channel"
        );
    }
    let artifact_path = Path::new(&artifact.file_name);
    let valid_suffix = match artifact.format.as_str() {
        "dmg" => artifact_path.extension() == Some(std::ffi::OsStr::new("dmg")),
        "tar.gz" => {
            artifact_path.extension() == Some(std::ffi::OsStr::new("gz"))
                && artifact_path
                    .file_stem()
                    .and_then(|stem| Path::new(stem).extension())
                    == Some(std::ffi::OsStr::new("tar"))
        }
        _ => false,
    };
    ensure!(
        !artifact.file_name.is_empty()
            && !artifact.file_name.starts_with('.')
            && !artifact.file_name.contains('/')
            && !artifact.file_name.contains('\\')
            && valid_suffix,
        "update artifact name is not a plain platform package filename"
    );
    let url = Url::parse(&artifact.url).context("parse update artifact URL")?;
    ensure!(
        url.scheme() == "https",
        "update artifact URL must use HTTPS"
    );
    ensure!(
        url.host_str() == Some(update_host),
        "update artifact URL host is not trusted"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "update artifact URL must not contain credentials"
    );
    ensure!(
        url.port().is_none(),
        "update artifact URL must not contain a port"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "update artifact URL must not contain a query or fragment"
    );
    let last_segment = url
        .path_segments()
        .and_then(Iterator::last)
        .context("update artifact URL has no filename")?;
    ensure!(
        last_segment == artifact.file_name,
        "update artifact URL filename does not exactly match file_name"
    );
    ensure!(
        artifact.size > 0 && artifact.size <= MAX_ARTIFACT_BYTES,
        "update artifact size must be between 1 and {MAX_ARTIFACT_BYTES} bytes"
    );
    ensure!(
        artifact.sha256.len() == 64
            && artifact
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "update artifact SHA-256 must be lower-case hexadecimal"
    );
    Ok(())
}

/// Returns an update only when it is strictly newer and ships an artifact for
/// the running CPU architecture.
///
/// # Errors
///
/// Returns an error when production trust configuration is absent, the
/// release/current version is invalid, or no artifact exists for the CPU.
pub fn select_update<'a>(
    manifest: &'a UpdateManifest,
    current: &CurrentRelease<'_>,
) -> Result<Option<AvailableUpdate<'a>>> {
    let host = UPDATE_HOST.context("production update host is not configured")?;
    validate_manifest_for_host(manifest, host, OffsetDateTime::now_utc(), false)?;
    select_verified_update(manifest, current)
}

/// Selects a newer architecture-matched artifact from an already verified
/// manifest.
///
/// # Errors
///
/// Returns an error when either version is invalid or no artifact exists for
/// the requested architecture.
pub fn select_verified_update<'a>(
    manifest: &'a UpdateManifest,
    current: &CurrentRelease<'_>,
) -> Result<Option<AvailableUpdate<'a>>> {
    ensure!(
        manifest.platform == current.platform,
        "signed update targets a different platform"
    );
    let incoming = Version::parse(&manifest.version).context("parse incoming update version")?;
    let installed = Version::parse(current.version).context("parse installed update version")?;
    if incoming < installed || manifest.build <= current.build {
        return Ok(None);
    }
    let artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| {
            artifact.platform == current.platform && artifact.architecture == current.architecture
        })
        .context("signed update has no artifact for this platform and architecture")?;
    Ok(Some(AvailableUpdate {
        manifest,
        artifact,
        requires_service_restart: manifest.session_service.protocol_version
            != current.protocol_version,
    }))
}

/// Hashes the completed download and checks its exact size. This must run
/// before a DMG is mounted or passed to the installer.
///
/// # Errors
///
/// Returns an error when the byte count or SHA-256 differs from the signed
/// artifact description.
pub fn verify_artifact_bytes(artifact: &ReleaseArtifact, bytes: &[u8]) -> Result<()> {
    let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    ensure!(
        actual_size == artifact.size,
        "update artifact size mismatch: expected {}, got {actual_size}",
        artifact.size
    );
    let actual_sha256 = sha256_hex(bytes);
    ensure!(
        actual_sha256 == artifact.sha256,
        "update artifact SHA-256 mismatch"
    );
    Ok(())
}

/// Streams an artifact from disk while checking its exact signed size and
/// SHA-256. This avoids allocating a complete DMG in the verifier process.
///
/// # Errors
///
/// Returns an error for non-regular files, oversized artifacts, read failures,
/// or a signed size/hash mismatch.
pub fn verify_artifact_file(artifact: &ReleaseArtifact, path: &Path) -> Result<()> {
    ensure!(
        artifact.size > 0 && artifact.size <= MAX_ARTIFACT_BYTES,
        "update artifact size is outside the supported range"
    );
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?;
    ensure!(metadata.is_file(), "update artifact must be a regular file");
    ensure!(
        metadata.len() == artifact.size,
        "update artifact size mismatch: expected {}, got {}",
        artifact.size,
        metadata.len()
    );
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        ensure!(total <= artifact.size, "update artifact grew while hashing");
        digest.update(&buffer[..read]);
    }
    ensure!(
        total == artifact.size,
        "update artifact changed while hashing"
    );
    let actual_sha256 = format!("{:x}", digest.finalize());
    ensure!(
        actual_sha256 == artifact.sha256,
        "update artifact SHA-256 mismatch"
    );
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ed25519_dalek::{Signer as _, SigningKey};
    use time::macros::datetime;

    use super::*;

    const KEY_ID: &str = "fixture-v1";
    const HOST: &str = "updates.test.invalid";
    const NOW: OffsetDateTime = datetime!(2026-08-14 12:00 UTC);

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn manifest_for(bytes: &[u8]) -> UpdateManifest {
        let file_name = if cfg!(feature = "community-macos") {
            "Harness-Harlot-0.2.0-macos-arm64-community.dmg"
        } else {
            "Harness-Harlot-0.2.0-macos-arm64.dmg"
        };
        UpdateManifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            product: PRODUCT_NAME.to_owned(),
            channel: STABLE_CHANNEL.to_owned(),
            key_id: KEY_ID.to_owned(),
            version: "0.2.0".to_owned(),
            build: 2,
            published_at: "2026-08-14T00:00:00Z".to_owned(),
            valid_until: "2026-08-21T00:00:00Z".to_owned(),
            platform: "macos".to_owned(),
            minimum_macos: Some("13.0".to_owned()),
            minimum_glibc: None,
            session_service: SessionServicePolicy {
                protocol_version: 11,
                requires_quiescent_service: true,
            },
            artifacts: vec![ReleaseArtifact {
                platform: "macos".to_owned(),
                architecture: "arm64".to_owned(),
                format: "dmg".to_owned(),
                file_name: file_name.to_owned(),
                url: format!("https://{HOST}/stable/{file_name}"),
                sha256: sha256_hex(bytes),
                size: u64::try_from(bytes.len()).unwrap(),
            }],
        }
    }
    fn linux_manifest_for(bytes: &[u8]) -> UpdateManifest {
        let mut manifest = manifest_for(bytes);
        manifest.platform = "linux".to_owned();
        manifest.minimum_macos = None;
        manifest.minimum_glibc = Some("2.35".to_owned());
        let artifact = &mut manifest.artifacts[0];
        artifact.platform = "linux".to_owned();
        artifact.format = "tar.gz".to_owned();
        artifact.file_name = "Harness-Harlot-0.2.0-linux-arm64.tar.gz".to_owned();
        artifact.url = format!("https://{HOST}/stable/{}", artifact.file_name);
        manifest
    }

    fn signed(manifest: &UpdateManifest, key: &SigningKey) -> (Vec<u8>, String) {
        let body = serde_json::to_vec_pretty(manifest).unwrap();
        let signature = STANDARD.encode(key.sign(&body).to_bytes());
        (body, signature)
    }

    fn verify_fixture(body: &[u8], signature: &str, key: &SigningKey) -> Result<UpdateManifest> {
        verify_manifest_with_key(
            body,
            signature,
            KEY_ID,
            &key.verifying_key(),
            HOST,
            NOW,
            true,
        )
    }

    #[test]
    fn verifies_trusted_manifest_and_download() {
        let artifact = b"immutable test artifact";
        let key = signing_key(42);
        let (body, signature) = signed(&manifest_for(artifact), &key);
        let manifest = verify_fixture(&body, &signature, &key).unwrap();
        verify_artifact_bytes(&manifest.artifacts[0], artifact).unwrap();
    }

    #[test]
    fn edge_manifests_are_accepted_only_by_the_edge_channel() {
        let key = signing_key(42);
        let mut manifest = manifest_for(b"edge artifact");
        manifest.channel = "edge".to_owned();
        let (body, signature) = signed(&manifest, &key);
        assert!(
            verify_manifest_with_key_for_channel(
                &body,
                &signature,
                KEY_ID,
                &key.verifying_key(),
                HOST,
                NOW,
                true,
                UpdateChannel::Edge,
            )
            .is_ok()
        );
        assert!(verify_fixture(&body, &signature, &key).is_err());
    }
    #[test]
    fn validates_and_selects_linux_artifact_only_for_linux() {
        let manifest = linux_manifest_for(b"linux artifact");
        validate_manifest_for_host(&manifest, HOST, NOW, true).unwrap();
        let current = CurrentRelease {
            version: "0.1.0",
            build: 1,
            platform: "linux",
            architecture: "arm64",
            protocol_version: 11,
        };
        let update = select_verified_update(&manifest, &current)
            .unwrap()
            .expect("newer Linux release");
        assert_eq!(update.artifact.format, "tar.gz");
        assert!(!update.requires_service_restart);

        let mut protocol_manifest = manifest.clone();
        protocol_manifest.session_service.protocol_version += 1;
        let protocol_update = select_verified_update(&protocol_manifest, &current)
            .unwrap()
            .expect("newer Linux protocol release");
        assert!(protocol_update.requires_service_restart);

        let current = CurrentRelease {
            platform: "macos",
            ..current
        };
        assert!(select_verified_update(&manifest, &current).is_err());
    }

    #[test]
    fn rejects_cross_platform_manifest_fields_and_artifacts() {
        let mut manifest = linux_manifest_for(b"artifact");
        manifest.minimum_macos = Some("13.0".to_owned());
        assert!(validate_manifest_for_host(&manifest, HOST, NOW, true).is_err());

        let mut manifest = linux_manifest_for(b"artifact");
        manifest.artifacts[0].platform = "macos".to_owned();
        assert!(validate_manifest_for_host(&manifest, HOST, NOW, true).is_err());

        let mut manifest = linux_manifest_for(b"artifact");
        manifest.minimum_glibc = Some("2.bad".to_owned());
        assert!(validate_manifest_for_host(&manifest, HOST, NOW, true).is_err());
    }

    #[test]
    fn rejects_manifest_signed_by_untrusted_key() {
        let trusted = signing_key(42);
        let untrusted = signing_key(7);
        let (body, signature) = signed(&manifest_for(b"artifact"), &untrusted);
        assert!(verify_fixture(&body, &signature, &trusted).is_err());
    }

    #[test]
    fn rejects_manifest_whose_key_id_does_not_match_signing_key() {
        let key = signing_key(42);
        let mut manifest = manifest_for(b"artifact");
        manifest.key_id = "other-v1".to_owned();
        let (body, signature) = signed(&manifest, &key);
        assert!(verify_fixture(&body, &signature, &key).is_err());
    }

    #[test]
    fn rejects_macos_artifact_from_the_other_release_channel() {
        let mut manifest = manifest_for(b"artifact");
        let wrong_name = if cfg!(feature = "community-macos") {
            "Harness-Harlot-0.2.0-macos-arm64.dmg"
        } else {
            "Harness-Harlot-0.2.0-macos-arm64-community.dmg"
        };
        manifest.artifacts[0].file_name = wrong_name.to_owned();
        manifest.artifacts[0].url = format!("https://{HOST}/stable/{wrong_name}");
        assert!(validate_manifest_for_host(&manifest, HOST, NOW, true).is_err());
    }

    #[test]
    fn rejects_expired_manifest() {
        let key = signing_key(42);
        let mut manifest = manifest_for(b"artifact");
        manifest.valid_until = "2026-08-13T00:00:00Z".to_owned();
        let (body, signature) = signed(&manifest, &key);
        assert!(verify_fixture(&body, &signature, &key).is_err());
    }

    #[test]
    fn rejects_artifact_url_on_unexpected_host() {
        let mut manifest = manifest_for(b"artifact");
        manifest.artifacts[0].url =
            format!("https://evil.example/{}", manifest.artifacts[0].file_name);
        assert!(validate_manifest_for_host(&manifest, HOST, NOW, true).is_err());
    }

    #[test]
    fn rejects_url_whose_last_segment_differs_from_file_name() {
        let mut manifest = manifest_for(b"artifact");
        manifest.artifacts[0].url = format!("https://{HOST}/x{}", manifest.artifacts[0].file_name);
        assert!(validate_manifest_for_host(&manifest, HOST, NOW, true).is_err());
    }

    #[test]
    fn artifact_policy_rejects_unsafe_variants() {
        let cases = [
            (
                "../release.dmg",
                format!("https://{HOST}/../release.dmg"),
                "traversal",
            ),
            (
                "release.zip",
                format!("https://{HOST}/release.zip"),
                "non-DMG",
            ),
            ("release.dmg", format!("http://{HOST}/release.dmg"), "HTTP"),
            (
                "release.dmg",
                format!("https://{HOST}/release.dmg?x=1"),
                "query",
            ),
            (
                "release.dmg",
                format!("https://{HOST}/release.dmg#x"),
                "fragment",
            ),
        ];
        for (file_name, url, label) in cases {
            let mut manifest = manifest_for(b"artifact");
            manifest.artifacts[0].file_name = file_name.to_owned();
            manifest.artifacts[0].url = url;
            assert!(
                validate_manifest_for_host(&manifest, HOST, NOW, true).is_err(),
                "accepted {label}"
            );
        }
        let mut manifest = manifest_for(b"artifact");
        manifest.artifacts[0].sha256 = "A".repeat(64);
        assert!(validate_manifest_for_host(&manifest, HOST, NOW, true).is_err());
    }

    #[test]
    fn unknown_key_id_and_foreign_host_are_rejected_by_compiled_trust() {
        let mut manifest = manifest_for(b"artifact");
        manifest.key_id = "not-a-known-key".to_owned();
        assert!(validate_manifest_for_host(&manifest, HOST, NOW, false).is_err());
        manifest.key_id = KEY_ID.to_owned();
        assert!(
            validate_manifest_for_host(&manifest, "updates.example.invalid", NOW, false).is_err()
        );
        assert_eq!(UPDATE_HOST, Some("github.com"));
        assert!(
            !TRUSTED_UPDATE_KEYS
                .iter()
                .any(|key| key.key_id == "not-a-known-key")
        );
    }

    #[test]
    fn garbage_manifests_fail_closed_against_compiled_trust() {
        assert!(verify_manifest_with_trusted_keys(b"{}", "bad").is_err());
    }

    #[test]
    fn only_selects_newer_fixture_updates() {
        let manifest = manifest_for(b"artifact");
        validate_manifest_for_host(&manifest, HOST, NOW, true).unwrap();
        let older_build = CurrentRelease {
            version: &manifest.version,
            build: manifest.build - 1,
            platform: "macos",
            architecture: "arm64",
            protocol_version: 1,
        };
        assert!(
            select_verified_update(&manifest, &older_build)
                .unwrap()
                .is_some()
        );

        let newer_build_with_older_version = CurrentRelease {
            version: "0.1.0",
            build: manifest.build + 1,
            platform: "macos",
            architecture: "arm64",
            protocol_version: 1,
        };
        assert!(
            select_verified_update(&manifest, &newer_build_with_older_version)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn community_macos_feed_is_isolated_and_explicit_install_is_supported() {
        assert_eq!(
            update_manifest_name("linux", "arm64"),
            "manifest-linux-arm64.update.json"
        );
        assert!(automatic_install_supported("linux"));
        assert!(explicit_install_supported("linux"));
        if cfg!(feature = "community-macos") {
            assert_eq!(
                update_manifest_name("macos", "arm64"),
                "manifest-macos-community-arm64.update.json"
            );
            assert!(!automatic_install_supported("macos"));
            assert!(explicit_install_supported("macos"));
        } else {
            assert_eq!(
                update_manifest_name("macos", "arm64"),
                "manifest-macos-arm64.update.json"
            );
            assert!(automatic_install_supported("macos"));
            assert!(explicit_install_supported("macos"));
        }
    }

    #[test]
    fn public_key_decoder_accepts_exact_ed25519_key() {
        let encoded = STANDARD.encode(signing_key(42).verifying_key().as_bytes());
        assert!(public_key_from_base64(&encoded).is_ok());
        assert!(public_key_from_base64("not-base64").is_err());
    }
}
