use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use hh_release_signer::{public_key_from_private_key_file, sign_manifest_from_private_key_file};

fn usage() -> ! {
    eprintln!(
        "usage:\n  hh-release-sign sign --manifest FILE --signature FILE --private-key FILE\n  hh-release-sign public-key --private-key FILE"
    );
    std::process::exit(2);
}

fn option(arguments: &[String], name: &str) -> Result<PathBuf> {
    let index = arguments
        .iter()
        .position(|argument| argument == name)
        .with_context(|| format!("missing {name}"))?;
    arguments
        .get(index + 1)
        .map(PathBuf::from)
        .with_context(|| format!("missing value for {name}"))
}

fn run() -> Result<()> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("sign") => {
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
        Some("public-key") => {
            let key_path = option(&arguments, "--private-key")?;
            println!("{}", public_key_from_private_key_file(&key_path)?);
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
            eprintln!("hh-release-sign: {error:#}");
            ExitCode::FAILURE
        }
    }
}
