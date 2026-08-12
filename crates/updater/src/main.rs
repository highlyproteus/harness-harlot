use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use nah_updater::{
    public_key_from_base64, sign_manifest_for_test, sign_manifest_from_private_key_file,
    test_public_key_base64, verify_artifact_bytes, verify_manifest,
};

fn usage() -> ! {
    eprintln!(
        "usage:\n  nah-update-tool verify --public-key BASE64 --manifest FILE --signature FILE [--artifact FILE]\n  nah-update-tool sign --manifest FILE --signature FILE --private-key FILE\n  nah-update-tool test-public-key\n  nah-update-tool test-sign --manifest FILE --signature FILE"
    );
    std::process::exit(2);
}

fn option(arguments: &[String], name: &str) -> Result<PathBuf> {
    let index = arguments
        .iter()
        .position(|argument| argument == name)
        .with_context(|| format!("missing {name}"))?;
    let value = arguments
        .get(index + 1)
        .with_context(|| format!("missing value for {name}"))?;
    Ok(PathBuf::from(value))
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

fn run() -> Result<()> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    let Some(command) = arguments.first().map(String::as_str) else {
        usage();
    };
    match command {
        "verify" => {
            let public_key = public_key_from_base64(&string_option(&arguments, "--public-key")?)?;
            let manifest_path = option(&arguments, "--manifest")?;
            let signature_path = option(&arguments, "--signature")?;
            let manifest_bytes = fs::read(&manifest_path)
                .with_context(|| format!("read {}", manifest_path.display()))?;
            let signature = fs::read_to_string(&signature_path)
                .with_context(|| format!("read {}", signature_path.display()))?;
            let manifest = verify_manifest(&manifest_bytes, &signature, &public_key)?;
            if let Some(index) = arguments
                .iter()
                .position(|argument| argument == "--artifact")
            {
                let artifact_path = arguments
                    .get(index + 1)
                    .context("missing value for --artifact")?;
                let bytes =
                    fs::read(artifact_path).with_context(|| format!("read {artifact_path}"))?;
                let artifact = manifest
                    .artifacts
                    .iter()
                    .find(|artifact| {
                        artifact.file_name
                            == PathBuf::from(artifact_path)
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or_default()
                    })
                    .context("artifact filename is absent from signed manifest")?;
                verify_artifact_bytes(artifact, &bytes)?;
            }
            println!(
                "trusted stable update {} build {}",
                manifest.version, manifest.build
            );
        }
        "sign" => {
            let manifest_path = option(&arguments, "--manifest")?;
            let signature_path = option(&arguments, "--signature")?;
            let key_path = option(&arguments, "--private-key")?;
            let body = fs::read(&manifest_path)
                .with_context(|| format!("read {}", manifest_path.display()))?;
            fs::write(
                &signature_path,
                sign_manifest_from_private_key_file(&body, &key_path)? + "\n",
            )
            .with_context(|| format!("write {}", signature_path.display()))?;
        }
        "test-public-key" => println!("{}", test_public_key_base64()),
        "test-sign" => {
            let manifest_path = option(&arguments, "--manifest")?;
            let signature_path = option(&arguments, "--signature")?;
            let body = fs::read(&manifest_path)
                .with_context(|| format!("read {}", manifest_path.display()))?;
            fs::write(&signature_path, sign_manifest_for_test(&body) + "\n")
                .with_context(|| format!("write {}", signature_path.display()))?;
        }
        _ => bail!("unknown command {command}"),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nah-update-tool: {error:#}");
            ExitCode::FAILURE
        }
    }
}
