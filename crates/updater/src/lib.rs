//! Offline verification primitives for the Not a Harness stable update feed.
//!
//! The desktop does not download or install updates yet. This crate is the
//! deliberately small, testable seam that a future macOS UI integration must
//! use before showing an update or handing an artifact to the installer.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST_SCHEMA: &str = "nah-update-manifest-v1";
pub const PRODUCT_NAME: &str = "Not a Harness";
pub const STABLE_CHANNEL: &str = "stable";
pub const TEST_KEY_ID: &str = "test-only-v1";
const TEST_SIGNING_SEED: [u8; 32] = [42; 32];

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
    pub minimum_macos: String,
    pub session_service: SessionServicePolicy,
    pub artifacts: Vec<ReleaseArtifact>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionServicePolicy {
    pub protocol_version: u32,
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
    pub architecture: &'a str,
    pub protocol_version: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailableUpdate<'a> {
    pub manifest: &'a UpdateManifest,
    pub artifact: &'a ReleaseArtifact,
    /// A different local IPC protocol means the bundled service must be
    /// restarted only after the user has quiesced every terminal session.
    pub requires_service_restart: bool,
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

/// Verifies the detached signature before parsing and validating the manifest.
///
/// # Errors
///
/// Returns an error when the signature is invalid, the bytes are not a valid
/// manifest, or the signed manifest violates the stable-channel policy.
pub fn verify_manifest(
    manifest_bytes: &[u8],
    signature_base64: &str,
    public_key: &VerifyingKey,
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
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Checks that a signed manifest remains within the single stable-channel
/// policy and contains only safe, immutable macOS DMG references.
///
/// # Errors
///
/// Returns an error for unsupported schema fields, an unsafe artifact
/// reference, or a manifest that is outside the stable release policy.
pub fn validate_manifest(manifest: &UpdateManifest) -> Result<()> {
    ensure!(
        manifest.schema == MANIFEST_SCHEMA,
        "unsupported update manifest schema"
    );
    ensure!(
        manifest.product == PRODUCT_NAME,
        "unexpected update product"
    );
    ensure!(
        manifest.channel == STABLE_CHANNEL,
        "only the stable channel is supported"
    );
    ensure!(
        !manifest.key_id.trim().is_empty(),
        "update manifest key ID is empty"
    );
    Version::parse(&manifest.version).context("parse update version as semantic version")?;
    ensure!(manifest.build > 0, "update build must be greater than zero");
    validate_macos_version(&manifest.minimum_macos)?;
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
        validate_artifact(artifact)?;
    }
    Ok(())
}

fn validate_macos_version(version: &str) -> Result<()> {
    let mut components = version.split('.');
    let major = components
        .next()
        .context("minimum macOS version is empty")?;
    ensure!(
        major.parse::<u16>().is_ok(),
        "minimum macOS major version is invalid"
    );
    for component in components {
        ensure!(
            component.parse::<u16>().is_ok(),
            "minimum macOS version is invalid"
        );
    }
    Ok(())
}

fn validate_artifact(artifact: &ReleaseArtifact) -> Result<()> {
    ensure!(artifact.platform == "macos", "unsupported update platform");
    ensure!(
        matches!(artifact.architecture.as_str(), "arm64" | "x86_64"),
        "unsupported macOS architecture"
    );
    ensure!(
        artifact.format == "dmg",
        "only macOS DMG update artifacts are accepted"
    );
    ensure!(
        !artifact.file_name.is_empty()
            && !artifact.file_name.contains('/')
            && !artifact.file_name.contains('\\')
            && Path::new(&artifact.file_name)
                .extension()
                .is_some_and(|extension| extension == "dmg"),
        "update artifact name must be a plain DMG filename"
    );
    ensure!(
        artifact.url.starts_with("https://")
            && artifact.url.ends_with(&artifact.file_name)
            && !artifact.url.contains('?')
            && !artifact.url.contains('#'),
        "update artifact URL must be an immutable HTTPS filename URL"
    );
    ensure!(
        artifact.size > 0,
        "update artifact size must be greater than zero"
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
/// the running CPU architecture. A mismatched service protocol is not a reason
/// to restart the service; callers must defer until it is quiescent.
///
/// # Errors
///
/// Returns an error when the release/current version is invalid, the manifest
/// fails policy validation, or no artifact exists for the current CPU.
pub fn select_update<'a>(
    manifest: &'a UpdateManifest,
    current: &CurrentRelease<'_>,
) -> Result<Option<AvailableUpdate<'a>>> {
    validate_manifest(manifest)?;
    let incoming = Version::parse(&manifest.version).context("parse incoming update version")?;
    let installed = Version::parse(current.version).context("parse installed update version")?;
    if incoming < installed || (incoming == installed && manifest.build <= current.build) {
        return Ok(None);
    }
    let artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.architecture == current.architecture)
        .context("signed update has no artifact for this macOS architecture")?;
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

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

pub fn sign_manifest_for_test(manifest_bytes: &[u8]) -> String {
    let signature = SigningKey::from_bytes(&TEST_SIGNING_SEED).sign(manifest_bytes);
    STANDARD.encode(signature.to_bytes())
}

pub fn test_public_key_base64() -> String {
    STANDARD.encode(
        SigningKey::from_bytes(&TEST_SIGNING_SEED)
            .verifying_key()
            .as_bytes(),
    )
}

/// Signs exact manifest bytes with an Ed25519 seed kept outside the repository.
///
/// # Errors
///
/// Returns an error when the key file cannot be read or does not contain one
/// base64-encoded 32-byte Ed25519 seed.
pub fn sign_manifest_from_private_key_file(
    manifest_bytes: &[u8],
    key_file: &std::path::Path,
) -> Result<String> {
    let encoded = std::fs::read_to_string(key_file)
        .with_context(|| format!("read update signing key {}", key_file.display()))?;
    let bytes = STANDARD
        .decode(encoded.trim())
        .context("decode base64 update signing key")?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("an Ed25519 update signing key must be a 32-byte seed"))?;
    let signature = SigningKey::from_bytes(&seed).sign(manifest_bytes);
    Ok(STANDARD.encode(signature.to_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_for(bytes: &[u8]) -> UpdateManifest {
        UpdateManifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            product: PRODUCT_NAME.to_owned(),
            channel: STABLE_CHANNEL.to_owned(),
            key_id: TEST_KEY_ID.to_owned(),
            version: "0.2.0".to_owned(),
            build: 2,
            minimum_macos: "13.0".to_owned(),
            session_service: SessionServicePolicy {
                protocol_version: 11,
                requires_quiescent_service: true,
            },
            artifacts: vec![ReleaseArtifact {
                platform: "macos".to_owned(),
                architecture: "arm64".to_owned(),
                format: "dmg".to_owned(),
                file_name: "Not-a-Harness-0.2.0-macos-arm64.dmg".to_owned(),
                url: "https://updates.example.invalid/Not-a-Harness-0.2.0-macos-arm64.dmg"
                    .to_owned(),
                sha256: sha256_hex(bytes),
                size: u64::try_from(bytes.len()).unwrap(),
            }],
        }
    }

    #[test]
    fn verifies_test_signed_manifest_and_download() {
        let artifact = b"immutable test artifact";
        let body = serde_json::to_vec_pretty(&manifest_for(artifact)).unwrap();
        let signature = sign_manifest_for_test(&body);
        let key = public_key_from_base64(&test_public_key_base64()).unwrap();
        let manifest = verify_manifest(&body, &signature, &key).unwrap();
        verify_artifact_bytes(&manifest.artifacts[0], artifact).unwrap();
    }

    #[test]
    fn rejects_tampered_metadata_and_artifacts() {
        let artifact = b"immutable test artifact";
        let body = serde_json::to_vec(&manifest_for(artifact)).unwrap();
        let signature = sign_manifest_for_test(&body);
        let key = public_key_from_base64(&test_public_key_base64()).unwrap();
        let mut tampered = body.clone();
        tampered[0] ^= 1;
        assert!(verify_manifest(&tampered, &signature, &key).is_err());
        let manifest = verify_manifest(&body, &signature, &key).unwrap();
        assert!(verify_artifact_bytes(&manifest.artifacts[0], b"wrong bytes").is_err());
    }

    #[test]
    fn only_selects_newer_stable_compatible_updates() {
        let manifest = manifest_for(b"artifact");
        let current = CurrentRelease {
            version: "0.1.0",
            build: 1,
            architecture: "arm64",
            protocol_version: 11,
        };
        let update = select_update(&manifest, &current).unwrap().unwrap();
        assert!(!update.requires_service_restart);
        let installed = CurrentRelease {
            version: "0.2.0",
            build: 2,
            ..current
        };
        assert!(select_update(&manifest, &installed).unwrap().is_none());
    }
}
