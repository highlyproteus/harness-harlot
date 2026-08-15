use std::env;
use std::fs::File;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use hh_updater::{
    MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES, UpdateManifest, public_key_from_base64,
    verify_artifact_file, verify_manifest_with_key, verify_manifest_with_trusted_keys,
};
use time::OffsetDateTime;

fn usage() -> ! {
    eprintln!(
        "usage:\n  hh-update-tool verify-trusted --manifest FILE --signature FILE [--artifact FILE]\n  hh-update-tool verify --key-id ID --public-key BASE64 --host HOST --manifest FILE --signature FILE [--artifact FILE] --fixture"
    );
    std::process::exit(2);
}

fn option(arguments: &[String], name: &str) -> Result<PathBuf> {
    string_option(arguments, name).map(PathBuf::from)
}

fn string_option(arguments: &[String], name: &str) -> Result<String> {
    let index = arguments
        .iter()
        .position(|argument| argument == name)
        .with_context(|| format!("missing {name}"))?;
    arguments
        .get(index + 1)
        .cloned()
        .with_context(|| format!("missing value for {name}"))
}

fn verify_optional_artifact(arguments: &[String], manifest: &UpdateManifest) -> Result<()> {
    let Some(index) = arguments
        .iter()
        .position(|argument| argument == "--artifact")
    else {
        return Ok(());
    };
    let artifact_path = arguments
        .get(index + 1)
        .context("missing value for --artifact")?;
    let artifact_path = PathBuf::from(artifact_path);
    let file_name = artifact_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("artifact path has no UTF-8 filename")?;
    let artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.file_name == file_name)
        .context("artifact filename is absent from signed manifest")?;
    verify_artifact_file(artifact, &artifact_path)
}

fn read_bounded(path: &PathBuf, max_bytes: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        bail!("{} is not a bounded regular file", path.display());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        bail!("{} grew past its size limit", path.display());
    }
    Ok(bytes)
}

fn run() -> Result<()> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("verify-trusted") => {
            let manifest_path = option(&arguments, "--manifest")?;
            let signature_path = option(&arguments, "--signature")?;
            let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
            let signature = String::from_utf8(read_bounded(&signature_path, MAX_SIGNATURE_BYTES)?)
                .context("update signature is not UTF-8")?;
            let manifest = verify_manifest_with_trusted_keys(&manifest_bytes, &signature)?;
            verify_optional_artifact(&arguments, &manifest)?;
            println!(
                "trusted stable update {} build {}",
                manifest.version, manifest.build
            );
        }
        Some("verify") => {
            if !arguments.iter().any(|argument| argument == "--fixture") {
                bail!("explicit update keys are accepted only with --fixture");
            }
            let key_id = string_option(&arguments, "--key-id")?;
            let public_key = public_key_from_base64(&string_option(&arguments, "--public-key")?)?;
            let host = string_option(&arguments, "--host")?;
            let manifest_path = option(&arguments, "--manifest")?;
            let signature_path = option(&arguments, "--signature")?;
            let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
            let signature = String::from_utf8(read_bounded(&signature_path, MAX_SIGNATURE_BYTES)?)
                .context("update signature is not UTF-8")?;
            let manifest = verify_manifest_with_key(
                &manifest_bytes,
                &signature,
                &key_id,
                &public_key,
                &host,
                OffsetDateTime::now_utc(),
                arguments.iter().any(|argument| argument == "--fixture"),
            )?;
            verify_optional_artifact(&arguments, &manifest)?;
            println!(
                "trusted stable update {} build {}",
                manifest.version, manifest.build
            );
        }
        Some(command) => bail!("unknown command {command}"),
        None => usage(),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hh-update-tool: {error:#}");
            ExitCode::FAILURE
        }
    }
}
