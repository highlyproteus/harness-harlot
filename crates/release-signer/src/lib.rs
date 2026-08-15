//! Offline release-signing primitives.
//!
//! This crate is release infrastructure. Its binary is never copied into the
//! application bundle.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer as _, SigningKey};
use zeroize::Zeroizing;

/// Signs exact manifest bytes with a base64-encoded Ed25519 seed.
///
/// # Errors
///
/// Returns an error if the key is a symlink, is not a private regular file, or
/// does not contain exactly one base64-encoded 32-byte Ed25519 seed.
pub fn sign_manifest_from_private_key_file(
    manifest_bytes: &[u8],
    key_file: &Path,
) -> Result<String> {
    let signing_key = signing_key_from_private_file(key_file)?;
    let signature = signing_key.sign(manifest_bytes);
    Ok(STANDARD.encode(signature.to_bytes()))
}

/// Derives the base64 public key corresponding to an offline signing seed.
///
/// # Errors
///
/// Returns the same key-file validation errors as manifest signing.
pub fn public_key_from_private_key_file(key_file: &Path) -> Result<String> {
    let signing_key = signing_key_from_private_file(key_file)?;
    Ok(STANDARD.encode(signing_key.verifying_key().as_bytes()))
}

fn signing_key_from_private_file(key_file: &Path) -> Result<SigningKey> {
    let metadata = fs::symlink_metadata(key_file)
        .with_context(|| format!("inspect update signing key {}", key_file.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "update signing key must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        ensure!(
            metadata.mode().trailing_zeros() >= 6,
            "update signing key must not be accessible by group or other users"
        );
    }
    let encoded = Zeroizing::new(
        fs::read_to_string(key_file)
            .with_context(|| format!("read update signing key {}", key_file.display()))?,
    );
    let decoded = Zeroizing::new(
        STANDARD
            .decode(encoded.trim())
            .context("decode base64 update signing key")?,
    );
    ensure!(
        decoded.len() == 32,
        "an Ed25519 update signing key must be a 32-byte seed"
    );
    let mut seed = Zeroizing::new([0_u8; 32]);
    seed.copy_from_slice(&decoded);
    // SigningKey implements ZeroizeOnDrop when ed25519-dalek's zeroize
    // feature is enabled; the source buffers above are also zeroized.
    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn signs_private_regular_key_and_rejects_public_mode() {
        let directory =
            std::env::temp_dir().join(format!("hh-release-signer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let key = directory.join("key");
        fs::write(&key, STANDARD.encode([7_u8; 32])).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            !sign_manifest_from_private_key_file(b"manifest", &key)
                .unwrap()
                .is_empty()
        );
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(sign_manifest_from_private_key_file(b"manifest", &key).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
