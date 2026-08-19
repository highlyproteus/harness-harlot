use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::Read as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _, symlink};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Output;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

#[cfg(feature = "fixture")]
use hh_updater::fetch::OwnedUpdate;
use hh_updater::fetch::{
    download_verified, fetch_available_update, runtime_architecture, runtime_platform,
};
use hh_updater::{
    CurrentRelease, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES, TRUSTED_APPLE_TEAM_ID, UpdateManifest,
    automatic_install_supported, current_build, verify_artifact_file,
    verify_manifest_with_trusted_keys,
};
#[cfg(feature = "fixture")]
use hh_updater::{
    ReleaseArtifact, public_key_from_base64, select_verified_update, verify_manifest_with_key,
};
use sysinfo::{Pid, ProcessesToUpdate, System};
#[cfg(feature = "fixture")]
use time::OffsetDateTime;

#[cfg(target_os = "macos")]
const BUNDLE_ID: &str = "com.harnessharlot.desktop";
#[cfg(target_os = "macos")]
const MACOS_APP_NAME: &str = "Harness Harlot.app";
#[cfg(target_os = "macos")]
const MACOS_BACKUP_NAME: &str = "Harness Harlot.previous.app";
const LINUX_APP_NAME: &str = "harness-harlot";
const LINUX_BACKUP_NAME: &str = "harness-harlot.previous";
const LINUX_ARCHIVE_ROOT: &str = "Harness-Harlot";
const MAX_LINUX_UNPACKED_BYTES: u64 = 1024 * 1024 * 1024;
const LINUX_INSTALL_MARKER: &str = "com.harnessharlot.desktop\n";
#[cfg(feature = "fixture")]
type FixtureUpdate = (Option<OwnedUpdate>, Option<(PathBuf, ReleaseArtifact)>);

fn usage() -> ! {
    eprintln!(
        "usage:\n  hh-update-tool verify-trusted --manifest FILE --signature FILE [--artifact FILE]\n  hh-update-tool install [--current-version VERSION] [--current-build BUILD] [--wait-pid PID --wait-start-time UNIX_SECONDS] [--prefix DIR]\n  hh-update-tool install-local --source DIR [--prefix DIR]"
    );
    #[cfg(feature = "community-macos")]
    eprintln!("community macOS:\n  hh-update-tool prepare-community-install");
    #[cfg(feature = "fixture")]
    eprintln!(
        "fixture-only:\n  hh-update-tool verify --key-id ID --public-key BASE64 --host HOST --manifest FILE --signature FILE [--artifact FILE] --fixture\n  hh-update-tool install --fixture --key-id ID --public-key BASE64 --host HOST --manifest FILE --signature FILE --artifact FILE [--platform macos|linux] [--architecture arm64|x86_64] [--team-id TEAM] [--current-version VERSION] [--current-build BUILD] [--wait-pid PID --wait-start-time UNIX_SECONDS] [--prefix DIR]"
    );
    std::process::exit(2);
}

fn option(arguments: &[String], name: &str) -> Result<PathBuf> {
    string_option(arguments, name).map(PathBuf::from)
}

fn optional_string_option(arguments: &[String], name: &str) -> Result<Option<String>> {
    let Some(index) = arguments.iter().position(|argument| argument == name) else {
        return Ok(None);
    };
    arguments
        .get(index + 1)
        .cloned()
        .map(Some)
        .with_context(|| format!("missing value for {name}"))
}

fn string_option(arguments: &[String], name: &str) -> Result<String> {
    optional_string_option(arguments, name)?.with_context(|| format!("missing {name}"))
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

fn run_verify_trusted(arguments: &[String]) -> Result<()> {
    let manifest_path = option(arguments, "--manifest")?;
    let signature_path = option(arguments, "--signature")?;
    let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
    let signature = String::from_utf8(read_bounded(&signature_path, MAX_SIGNATURE_BYTES)?)
        .context("update signature is not UTF-8")?;
    let manifest = verify_manifest_with_trusted_keys(&manifest_bytes, &signature)?;
    verify_optional_artifact(arguments, &manifest)?;
    println!(
        "trusted stable update {} build {}",
        manifest.version, manifest.build
    );
    Ok(())
}

#[cfg(feature = "fixture")]
fn run_verify_fixture(arguments: &[String]) -> Result<()> {
    ensure!(
        arguments.iter().any(|argument| argument == "--fixture"),
        "explicit update keys are accepted only with --fixture"
    );
    let key_id = string_option(arguments, "--key-id")?;
    let public_key = public_key_from_base64(&string_option(arguments, "--public-key")?)?;
    let host = string_option(arguments, "--host")?;
    let manifest_path = option(arguments, "--manifest")?;
    let signature_path = option(arguments, "--signature")?;
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
        true,
    )?;
    verify_optional_artifact(arguments, &manifest)?;
    println!(
        "trusted stable update {} build {}",
        manifest.version, manifest.build
    );
    Ok(())
}

fn run_install(arguments: &[String]) -> Result<()> {
    ensure!(
        rustix::process::geteuid().as_raw() != 0,
        "refusing to install as root"
    );
    #[cfg(feature = "fixture")]
    let fixture = arguments.iter().any(|argument| argument == "--fixture");
    #[cfg(not(feature = "fixture"))]
    let fixture = {
        ensure!(
            !arguments.iter().any(|argument| argument == "--fixture"),
            "fixture support is not compiled into this updater"
        );
        false
    };
    let native_platform = runtime_platform()?;
    let platform = optional_string_option(arguments, "--platform")?
        .unwrap_or_else(|| native_platform.to_owned());
    ensure!(
        fixture || platform == native_platform,
        "production update platform must match the running system"
    );
    ensure!(
        fixture || automatic_install_supported(&platform),
        "automatic update installation is unavailable for unnotarized community macOS builds; use install-community-macos.sh"
    );
    let native_architecture = runtime_architecture()?;
    let architecture = optional_string_option(arguments, "--architecture")?
        .unwrap_or_else(|| native_architecture.to_owned());
    ensure!(
        fixture || architecture == native_architecture,
        "production update architecture must match the running system"
    );
    let current_version = optional_string_option(arguments, "--current-version")?
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
    let installed_build = optional_string_option(arguments, "--current-build")?.map_or_else(
        || Ok(current_build()),
        |value| value.parse().context("parse --current-build"),
    )?;
    let current = CurrentRelease {
        version: &current_version,
        build: installed_build,
        platform: &platform,
        architecture: &architecture,
        protocol_version: hh_protocol::PROTOCOL_VERSION,
    };
    #[cfg(feature = "fixture")]
    let (update, fixture_artifact) = if fixture {
        load_fixture_update(arguments, &current)?
    } else {
        (fetch_available_update(&current)?, None)
    };
    #[cfg(not(feature = "fixture"))]
    let (update, fixture_artifact): (
        Option<hh_updater::fetch::OwnedUpdate>,
        Option<(PathBuf, hh_updater::ReleaseArtifact)>,
    ) = (fetch_available_update(&current)?, None);
    let Some(update) = update else {
        println!("up to date");
        return Ok(());
    };

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let default_prefix = match platform.as_str() {
        "macos" => home.join("Applications"),
        "linux" => home.join(".local/lib"),
        _ => bail!("unsupported install platform {platform}"),
    };
    let prefix =
        optional_string_option(arguments, "--prefix")?.map_or(default_prefix, PathBuf::from);
    ensure!(
        path_is_confined(&prefix, &home)?,
        "install prefix must be an absolute normalized path inside HOME"
    );

    let work = TemporaryDirectory::new()?;
    let package = if let Some((source, artifact)) = fixture_artifact {
        let destination = work.path.join(
            source
                .file_name()
                .context("fixture artifact has no filename")?,
        );
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "copy fixture {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        verify_artifact_file(&artifact, &destination)?;
        destination
    } else {
        download_verified(&update, &work.path)?
    };
    match (
        optional_string_option(arguments, "--wait-pid")?,
        optional_string_option(arguments, "--wait-start-time")?,
    ) {
        (Some(pid), Some(start_time)) => wait_for_process_exit(
            pid.parse().context("parse --wait-pid")?,
            start_time.parse().context("parse --wait-start-time")?,
        )?,
        (None, None) => {}
        (Some(_), None) => bail!("--wait-start-time is required with --wait-pid"),
        (None, Some(_)) => bail!("--wait-pid is required with --wait-start-time"),
    }

    match platform.as_str() {
        "macos" => {
            let team_id = if fixture {
                string_option(arguments, "--team-id")?
            } else {
                TRUSTED_APPLE_TEAM_ID
                    .context("update install is not release-configured")?
                    .to_owned()
            };
            ensure!(
                !team_id.is_empty()
                    && team_id
                        .bytes()
                        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()),
                "invalid expected Apple Team ID"
            );
            #[cfg(target_os = "macos")]
            {
                install_dmg(&package, &prefix, &home, &team_id)
            }
            #[cfg(not(target_os = "macos"))]
            {
                bail!("macOS update installation requires macOS")
            }
        }

        "linux" => install_linux_archive(&package, &prefix, &home),
        _ => bail!("unsupported install platform {platform}"),
    }
}
#[cfg(feature = "community-macos")]
fn run_prepare_community_install(arguments: &[String]) -> Result<()> {
    ensure!(
        arguments == ["prepare-community-install"],
        "prepare-community-install accepts no arguments"
    );
    let executable = env::current_exe().context("resolve community updater executable")?;
    let service = executable
        .parent()
        .context("community updater has no parent directory")?
        .join("hh-service");
    if service.is_file() {
        return stop_managed_service(&service);
    }
    let socket = hh_protocol::socket_path()?;
    ensure!(
        StdUnixStream::connect(&socket).is_err(),
        "a session service is running outside the managed community app; close every terminal and stop it before installing"
    );
    Ok(())
}

fn wait_for_process_exit(process_id: u32, start_time: u64) -> Result<()> {
    ensure!(
        process_id != std::process::id(),
        "installer cannot wait for itself"
    );
    let pid = Pid::from_u32(process_id);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut system = System::new();
    loop {
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]));
        if !process_matches_start_time(&system, pid, start_time) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "desktop process {process_id} did not exit before update"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn process_matches_start_time(system: &System, pid: Pid, start_time: u64) -> bool {
    system
        .process(pid)
        .is_some_and(|process| process.start_time() == start_time)
}

fn run_install_local(arguments: &[String]) -> Result<()> {
    ensure!(
        runtime_platform()? == "linux",
        "local Linux installation requires Linux"
    );
    ensure!(
        rustix::process::geteuid().as_raw() != 0,
        "refusing to install as root"
    );
    let source = option(arguments, "--source")?;
    validate_linux_install(&source)?;
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let prefix = optional_string_option(arguments, "--prefix")?
        .map_or_else(|| home.join(".local/lib"), PathBuf::from);
    ensure!(
        path_is_confined(&prefix, &home)?,
        "install prefix must be an absolute normalized path inside HOME"
    );

    let archive_directory = TemporaryDirectory::new()?;
    let archive_path = archive_directory.path.join("local-install.tar.gz");
    let archive_file = File::create(&archive_path)
        .with_context(|| format!("create local install archive {}", archive_path.display()))?;
    let encoder = flate2::write::GzEncoder::new(archive_file, flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    archive.follow_symlinks(false);
    archive
        .append_dir_all(LINUX_ARCHIVE_ROOT, &source)
        .context("stage local Linux installation")?;
    let encoder = archive.into_inner().context("finish local Linux archive")?;
    encoder.finish().context("finish local Linux compression")?;
    install_linux_archive(&archive_path, &prefix, &home)
}

#[cfg(feature = "fixture")]
fn load_fixture_update(
    arguments: &[String],
    current: &CurrentRelease<'_>,
) -> Result<FixtureUpdate> {
    let key_id = string_option(arguments, "--key-id")?;
    let public_key = public_key_from_base64(&string_option(arguments, "--public-key")?)?;
    let host = string_option(arguments, "--host")?;
    let manifest_path = option(arguments, "--manifest")?;
    let signature_path = option(arguments, "--signature")?;
    let artifact_path = option(arguments, "--artifact")?;
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
        true,
    )?;
    let Some(selected) = select_verified_update(&manifest, current)? else {
        return Ok((None, None));
    };
    verify_artifact_file(selected.artifact, &artifact_path)?;
    Ok((
        Some(OwnedUpdate {
            version: selected.manifest.version.clone(),
            artifact: selected.artifact.clone(),
            requires_quiescent_service: selected
                .manifest
                .session_service
                .requires_quiescent_service,
        }),
        Some((artifact_path, selected.artifact.clone())),
    ))
}

fn install_linux_archive(package: &Path, prefix: &Path, home: &Path) -> Result<()> {
    fs::create_dir_all(prefix)
        .with_context(|| format!("create install prefix {}", prefix.display()))?;
    let bin_directory = home.join(".local/bin");
    let applications_directory = home.join(".local/share/applications");
    let icons_directory = home.join(".local/share/icons/hicolor/512x512/apps");
    for directory in [&bin_directory, &applications_directory, &icons_directory] {
        fs::create_dir_all(directory)
            .with_context(|| format!("create integration directory {}", directory.display()))?;
    }

    let app = prefix.join(LINUX_APP_NAME);
    let backup = prefix.join(LINUX_BACKUP_NAME);
    let staging = prefix.join(format!(".{LINUX_APP_NAME}.new.{}", std::process::id()));
    let mut links = vec![
        (
            bin_directory.join("hh"),
            app.join("bin/hh"),
            bin_directory.join(format!(".hh.update.{}", std::process::id())),
            false,
        ),
        (
            applications_directory.join("com.harnessharlot.desktop.desktop"),
            app.join("share/applications/com.harnessharlot.desktop.desktop"),
            applications_directory.join(format!(".hh-desktop.update.{}", std::process::id())),
            false,
        ),
        (
            icons_directory.join("com.harnessharlot.desktop.png"),
            app.join("share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png"),
            icons_directory.join(format!(".hh-icon.update.{}", std::process::id())),
            false,
        ),
    ];
    ensure_absent(&staging, "staging application")?;
    for (link, target, temporary, had_link) in &mut links {
        ensure_absent(temporary, "staging integration link")?;
        *had_link = validate_linux_managed_link(link, target)?;
    }
    let current_installed = path_exists(&app)?;
    if current_installed {
        validate_linux_install(&app)?;
    }
    if path_exists(&backup)? {
        validate_linux_install(&backup)?;
        fs::remove_dir_all(&backup)
            .with_context(|| format!("remove previous update backup {}", backup.display()))?;
    }

    let extraction = TemporaryDirectory::new_in(prefix, ".hh-update-extract")?;
    extract_linux_archive(package, &extraction.path)?;
    let extracted = extraction.path.join(LINUX_ARCHIVE_ROOT);
    validate_linux_install(&extracted)?;
    if current_installed {
        stop_managed_service(&app.join("bin/hh-service"))?;
    }
    let mut old_moved = false;
    let mut new_installed = false;
    let install_result = (|| -> Result<()> {
        fs::rename(&extracted, &staging).with_context(|| {
            format!(
                "stage extracted application {} at {}",
                extracted.display(),
                staging.display()
            )
        })?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("set application permissions on {}", staging.display()))?;
        for (_, target, temporary, _) in &links {
            symlink(target, temporary).with_context(|| {
                format!("create staging integration link {}", temporary.display())
            })?;
        }
        if path_exists(&app)? {
            fs::rename(&app, &backup).with_context(|| {
                format!(
                    "move current application {} to {}",
                    app.display(),
                    backup.display()
                )
            })?;
            old_moved = true;
        }
        fs::rename(&staging, &app)
            .with_context(|| format!("install application {}", app.display()))?;
        new_installed = true;
        for (link, _, temporary, _) in &links {
            fs::rename(temporary, link)
                .with_context(|| format!("install integration link {}", link.display()))?;
        }
        validate_linux_install(&app)?;
        for (link, target, _, _) in &links {
            ensure!(
                validate_linux_managed_link(link, target)?,
                "installed integration link is missing: {}",
                link.display()
            );
        }
        let mut desktop = Command::new(app.join("bin/hh"))
            .spawn()
            .context("launch updated Harness Harlot")?;
        thread::sleep(Duration::from_millis(250));
        if let Some(status) = desktop
            .try_wait()
            .context("probe updated Harness Harlot launch")?
        {
            ensure!(
                status.success(),
                "updated Harness Harlot exited during launch: {status}"
            );
        }
        Ok(())
    })();
    if let Err(error) = install_result {
        if new_installed {
            let _ = fs::remove_dir_all(&app);
        }
        let _ = fs::remove_dir_all(&staging);
        for (link, _, temporary, had_link) in &links {
            if !*had_link {
                let _ = fs::remove_file(link);
            }
            let _ = fs::remove_file(temporary);
        }
        if old_moved
            && let Err(restore_error) = fs::rename(&backup, &app)
                .with_context(|| format!("restore previous application {}", app.display()))
        {
            return Err(error).context(format!(
                "install Linux update failed and previous application restoration also failed: {restore_error:#}"
            ));
        }
        if old_moved {
            let _ = Command::new(app.join("bin/hh")).spawn();
        }
        let rollback = if old_moved {
            "previous application restored"
        } else {
            "partial installation removed"
        };
        return Err(error).context(format!("install Linux update; {rollback}"));
    }
    println!("updated Harness Harlot");
    Ok(())
}

fn extract_linux_archive(package: &Path, destination: &Path) -> Result<()> {
    let file = File::open(package)
        .with_context(|| format!("open Linux update package {}", package.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let expected = linux_install_files();
    let expected_directories = linux_install_directories();
    let mut extracted = HashSet::new();
    let mut seen_paths = HashSet::new();
    let mut unpacked_bytes = 0_u64;
    for entry in archive.entries().context("read Linux update archive")? {
        let mut entry = entry.context("read Linux update archive entry")?;
        let path = entry
            .path()
            .context("read Linux update archive path")?
            .into_owned();
        ensure!(
            path.components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
            "Linux update archive contains an unsafe path"
        );
        let relative = path
            .strip_prefix(LINUX_ARCHIVE_ROOT)
            .context("Linux update archive has an unexpected root")?;
        ensure!(
            seen_paths.insert(relative.to_owned()),
            "Linux update archive contains a duplicate path"
        );
        let kind = entry.header().entry_type();
        if relative.as_os_str().is_empty() {
            ensure!(
                kind.is_dir(),
                "Linux update archive root is not a directory"
            );
        } else if kind.is_dir() {
            let relative_text = relative
                .to_str()
                .context("Linux update archive path is not UTF-8")?;
            ensure!(
                expected_directories.contains(relative_text),
                "Linux update archive contains an unexpected directory"
            );
        } else {
            ensure!(
                kind.is_file(),
                "Linux update archive contains a link or special file"
            );
            let relative_text = relative
                .to_str()
                .context("Linux update archive path is not UTF-8")?;
            ensure!(
                expected.contains(relative_text),
                "Linux update archive contains unexpected file {relative_text}"
            );
            unpacked_bytes = unpacked_bytes
                .checked_add(entry.size())
                .context("Linux update archive unpacked size overflow")?;
            ensure!(
                unpacked_bytes <= MAX_LINUX_UNPACKED_BYTES,
                "Linux update archive exceeds unpacked size limit"
            );
            extracted.insert(relative_text.to_owned());
        }
        ensure!(
            entry
                .unpack_in(destination)
                .context("extract Linux update archive entry")?,
            "Linux update archive entry escapes the staging directory"
        );
    }
    ensure!(
        extracted.len() == expected.len(),
        "Linux update archive is missing required files"
    );
    Ok(())
}

fn linux_install_directories() -> HashSet<&'static str> {
    [
        "bin",
        "share",
        "share/applications",
        "share/icons",
        "share/icons/hicolor",
        "share/icons/hicolor/512x512",
        "share/icons/hicolor/512x512/apps",
        "share/licenses",
        "share/licenses/harness-harlot",
        "share/harness-harlot",
    ]
    .into_iter()
    .collect()
}

fn linux_install_files() -> HashSet<&'static str> {
    [
        "install.sh",
        "bin/hh",
        "bin/hh-service",
        "bin/hh-update-tool",
        "share/applications/com.harnessharlot.desktop.desktop",
        "share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png",
        "share/licenses/harness-harlot/LICENSE",
        "share/licenses/harness-harlot/THIRD_PARTY_NOTICES.md",
        "share/licenses/harness-harlot/ASSET_NOTICES.md",
        "share/harness-harlot/install-id",
    ]
    .into_iter()
    .collect()
}

fn validate_linux_install(app: &Path) -> Result<()> {
    ensure!(
        app.is_dir(),
        "{} is not an application directory",
        app.display()
    );
    let expected = linux_install_files();
    let expected_directories = linux_install_directories();
    let mut discovered = HashSet::new();
    let mut pending = vec![app.to_owned()];
    while let Some(directory) = pending.pop() {
        let metadata = fs::symlink_metadata(&directory)
            .with_context(|| format!("inspect {}", directory.display()))?;
        ensure!(
            metadata.is_dir() && metadata.uid() == rustix::process::getuid().as_raw(),
            "application directory is not owned by the current user"
        );
        ensure!(
            metadata.mode() & 0o022 == 0,
            "application directory is group- or world-writable"
        );
        for item in
            fs::read_dir(&directory).with_context(|| format!("read {}", directory.display()))?
        {
            let item = item.context("read application directory entry")?;
            let path = item.path();
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("inspect {}", path.display()))?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "application contains a symbolic link"
            );
            if metadata.is_dir() {
                let relative = path
                    .strip_prefix(app)
                    .context("application directory escaped its root")?
                    .to_str()
                    .context("application directory path is not UTF-8")?;
                ensure!(
                    expected_directories.contains(relative),
                    "application contains unexpected directory {relative}"
                );
                pending.push(path);
                continue;
            }
            ensure!(metadata.is_file(), "application contains a special file");
            let relative = path
                .strip_prefix(app)
                .context("application file escaped its root")?
                .to_str()
                .context("application path is not UTF-8")?
                .to_owned();
            ensure!(
                expected.contains(relative.as_str()),
                "application contains unexpected file {relative}"
            );
            ensure!(
                metadata.uid() == rustix::process::getuid().as_raw()
                    && metadata.mode() & 0o6022 == 0,
                "application file has unsafe ownership or permissions"
            );
            if !matches!(
                relative.as_str(),
                "install.sh" | "bin/hh" | "bin/hh-service" | "bin/hh-update-tool"
            ) {
                ensure!(
                    metadata.mode() & 0o111 == 0,
                    "application data file is unexpectedly executable"
                );
            }
            ensure!(
                discovered.insert(relative),
                "application contains a duplicate file"
            );
        }
    }
    ensure!(
        discovered.len() == expected.len(),
        "application is missing required files"
    );
    for executable in [
        "install.sh",
        "bin/hh",
        "bin/hh-service",
        "bin/hh-update-tool",
    ] {
        let metadata = fs::metadata(app.join(executable))
            .with_context(|| format!("inspect application executable {executable}"))?;
        ensure!(
            metadata.mode() & 0o111 != 0,
            "application executable {executable} is not executable"
        );
    }
    let marker = fs::read_to_string(app.join("share/harness-harlot/install-id"))
        .context("read application install marker")?;
    ensure!(
        marker == LINUX_INSTALL_MARKER,
        "application install marker is invalid"
    );
    Ok(())
}

fn validate_linux_managed_link(link: &Path, expected_target: &Path) -> Result<bool> {
    let Ok(metadata) = fs::symlink_metadata(link) else {
        return Ok(false);
    };
    ensure!(
        metadata.file_type().is_symlink(),
        "{} exists and is not a managed symlink",
        link.display()
    );
    let target =
        fs::read_link(link).with_context(|| format!("read integration link {}", link.display()))?;
    ensure!(
        target == expected_target,
        "{} is not managed by Harness Harlot",
        link.display()
    );
    Ok(true)
}

#[cfg(target_os = "macos")]
fn install_dmg(dmg: &Path, prefix: &Path, home: &Path, team_id: &str) -> Result<()> {
    fs::create_dir_all(prefix)
        .with_context(|| format!("create install prefix {}", prefix.display()))?;
    let bin_directory = home.join(".local/bin");
    fs::create_dir_all(&bin_directory)
        .with_context(|| format!("create command directory {}", bin_directory.display()))?;

    let mut mount = MountedDmg::attach(dmg, TemporaryDirectory::new()?)?;
    let mounted_app = mount.path().join(MACOS_APP_NAME);
    validate_managed_app(&mounted_app, team_id)?;

    let app = prefix.join(MACOS_APP_NAME);
    let backup = prefix.join(MACOS_BACKUP_NAME);
    let link = bin_directory.join("hh");
    let staging = prefix.join(format!(".{MACOS_APP_NAME}.new.{}", std::process::id()));
    ensure_absent(&staging, "staging app")?;
    run_status(
        "ditto",
        [mounted_app.as_os_str(), staging.as_os_str()],
        "stage mounted update app",
    )?;
    validate_managed_app(&staging, team_id)?;
    validate_managed_link(&link, &app)?;
    if path_exists(&app)? {
        validate_managed_app(&app, team_id)?;
        stop_managed_service(&app.join("Contents/MacOS/hh-service"))?;
    }
    if path_exists(&backup)? {
        validate_managed_app(&backup, team_id)?;
        fs::remove_dir_all(&backup)
            .with_context(|| format!("remove previous backup {}", backup.display()))?;
    }

    let had_app = path_exists(&app)?;
    let had_link = path_exists(&link)?;
    let mut old_app_moved = false;
    let mut new_app_installed = false;
    let result = (|| -> Result<()> {
        if had_app {
            fs::rename(&app, &backup).with_context(|| {
                format!("move current app {} to {}", app.display(), backup.display())
            })?;
            old_app_moved = true;
        }
        fs::rename(&staging, &app).with_context(|| {
            format!(
                "install staged app {} as {}",
                staging.display(),
                app.display()
            )
        })?;
        new_app_installed = true;
        if had_link {
            fs::remove_file(&link)
                .with_context(|| format!("remove command link {}", link.display()))?;
        }
        symlink(app.join("Contents/MacOS/hh"), &link)
            .with_context(|| format!("create command link {}", link.display()))?;
        validate_managed_app(&app, team_id)?;
        ensure!(
            path_exists(&link)?,
            "updated command link is missing: {}",
            link.display()
        );
        validate_managed_link(&link, &app)?;
        mount.detach()?;
        run_status("open", [app.as_os_str()], "launch updated app")?;
        Ok(())
    })();

    if let Err(error) = result {
        let rollback = (|| -> Result<()> {
            if new_app_installed && path_exists(&app)? {
                fs::remove_dir_all(&app)
                    .with_context(|| format!("remove failed update {}", app.display()))?;
            }
            if old_app_moved {
                fs::rename(&backup, &app).with_context(|| {
                    format!(
                        "restore previous app {} as {}",
                        backup.display(),
                        app.display()
                    )
                })?;
            }
            if path_exists(&link)? {
                fs::remove_file(&link)
                    .with_context(|| format!("remove command link {}", link.display()))?;
            }
            if had_app {
                symlink(app.join("Contents/MacOS/hh"), &link)
                    .with_context(|| format!("restore command link {}", link.display()))?;
            }
            if path_exists(&staging)? {
                fs::remove_dir_all(&staging)
                    .with_context(|| format!("remove staging app {}", staging.display()))?;
            }
            if had_app {
                validate_managed_app(&app, team_id)?;
                ensure!(
                    path_exists(&link)?,
                    "restored command link is missing: {}",
                    link.display()
                );
                validate_managed_link(&link, &app)?;
            } else {
                ensure!(
                    !path_exists(&app)? && !path_exists(&link)?,
                    "partial app or command link remains after rollback"
                );
            }
            Ok(())
        })();
        return match rollback {
            Ok(()) => {
                Err(error).context("install macOS update; previous app and command link restored")
            }
            Err(rollback_error) => Err(error).context(format!(
                "install macOS update; rollback also failed: {rollback_error:#}"
            )),
        };
    }
    println!("installed {}", app.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn validate_managed_app(candidate: &Path, team_id: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(candidate)
        .with_context(|| format!("inspect app {}", candidate.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "refusing non-directory or symlink app path: {}",
        candidate.display()
    );
    let plist = candidate.join("Contents/Info.plist");
    ensure!(
        plist.is_file(),
        "app has no Info.plist: {}",
        candidate.display()
    );
    let bundle_id = run_output(
        "plutil",
        [
            "-extract".as_ref(),
            "CFBundleIdentifier".as_ref(),
            "raw".as_ref(),
            "-o".as_ref(),
            "-".as_ref(),
            plist.as_os_str(),
        ],
        "read app bundle identifier",
    )?;
    ensure!(
        String::from_utf8(bundle_id.stdout)?.trim() == BUNDLE_ID,
        "refusing app with a different bundle identifier: {}",
        candidate.display()
    );
    ensure!(
        candidate.join("Contents/MacOS/hh").is_file(),
        "app has no hh executable: {}",
        candidate.display()
    );
    let requirement =
        format!("=anchor apple generic and certificate leaf[subject.OU] = \"{team_id}\"");
    run_status(
        "codesign",
        [
            "--verify".as_ref(),
            "--deep".as_ref(),
            "--strict".as_ref(),
            "-R".as_ref(),
            requirement.as_ref(),
            candidate.as_os_str(),
        ],
        "verify app signature and Apple Team ID",
    )
}

#[cfg(target_os = "macos")]
fn validate_managed_link(link: &Path, app: &Path) -> Result<()> {
    if !path_exists(link)? {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(link)
        .with_context(|| format!("inspect command link {}", link.display()))?;
    ensure!(
        metadata.file_type().is_symlink(),
        "refusing to overwrite non-symlink command: {}",
        link.display()
    );
    ensure!(
        fs::read_link(link)? == app.join("Contents/MacOS/hh"),
        "refusing to overwrite command symlink not owned by this install: {}",
        link.display()
    );
    Ok(())
}

fn stop_managed_service(service: &Path) -> Result<()> {
    let socket = hh_protocol::socket_path()?;
    if StdUnixStream::connect(&socket).is_err() {
        return Ok(());
    }
    let status = Command::new(service)
        .arg("--shutdown")
        .status()
        .with_context(|| format!("request shutdown through {}", service.display()))?;
    ensure!(
        status.success(),
        "session service refused to shut down; close every terminal before updating"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while StdUnixStream::connect(&socket).is_ok() {
        ensure!(
            Instant::now() < deadline,
            "session service did not stop before update"
        );
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn path_is_confined(path: &Path, root: &Path) -> Result<bool> {
    let normalized = |candidate: &Path| {
        candidate.is_absolute()
            && candidate.components().all(|component| {
                matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
    };
    if !normalized(root) || !normalized(path) {
        return Ok(false);
    }

    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("resolve {}", root.display()))?;
    let mut existing_ancestor = path;
    while !path_exists(existing_ancestor)? {
        existing_ancestor = existing_ancestor
            .parent()
            .context("confined path has no existing ancestor")?;
    }
    let canonical_ancestor = fs::canonicalize(existing_ancestor)
        .with_context(|| format!("resolve {}", existing_ancestor.display()))?;
    Ok(canonical_ancestor.starts_with(canonical_root))
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn ensure_absent(path: &Path, label: &str) -> Result<()> {
    ensure!(
        !path_exists(path)?,
        "{label} already exists: {}",
        path.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_status<I, S>(program: &str, arguments: I, operation: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let status = Command::new(program)
        .args(arguments)
        .status()
        .with_context(|| format!("{operation}: run {program}"))?;
    ensure!(status.success(), "{operation} failed with {status}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_output<I, S>(program: &str, arguments: I, operation: &str) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new(program)
        .args(arguments)
        .output()
        .with_context(|| format!("{operation}: run {program}"))?;
    ensure!(
        output.status.success(),
        "{operation} failed with {}",
        output.status
    );
    Ok(output)
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new() -> Result<Self> {
        let parent = env::var_os("TMPDIR").map_or_else(env::temp_dir, PathBuf::from);
        Self::new_in(&parent, "hh-update")
    }

    fn new_in(parent: &Path, stem: &str) -> Result<Self> {
        for sequence in 0_u16..=u16::MAX {
            let path = parent.join(format!("{stem}.{}.{sequence}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("create temporary directory {}", path.display()));
                }
            }
        }
        bail!("could not allocate a temporary update directory")
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(target_os = "macos")]
struct MountedDmg {
    directory: TemporaryDirectory,
    attached: bool,
}

#[cfg(target_os = "macos")]
impl MountedDmg {
    fn attach(dmg: &Path, directory: TemporaryDirectory) -> Result<Self> {
        run_status(
            "hdiutil",
            [
                "attach".as_ref(),
                "-nobrowse".as_ref(),
                "-readonly".as_ref(),
                "-mountpoint".as_ref(),
                directory.path.as_os_str(),
                dmg.as_os_str(),
            ],
            "attach verified update DMG",
        )?;
        Ok(Self {
            directory,
            attached: true,
        })
    }

    fn path(&self) -> &Path {
        &self.directory.path
    }

    fn detach(&mut self) -> Result<()> {
        if self.attached {
            run_status(
                "hdiutil",
                ["detach".as_ref(), self.directory.path.as_os_str()],
                "detach update DMG",
            )?;
            self.attached = false;
            fs::remove_dir_all(&self.directory.path).with_context(|| {
                format!(
                    "remove update mount directory {}",
                    self.directory.path.display()
                )
            })?;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for MountedDmg {
    fn drop(&mut self) {
        if self.attached {
            let _ = Command::new("hdiutil")
                .args(["detach".as_ref(), self.directory.path.as_os_str()])
                .status();
        }
    }
}

fn run() -> Result<()> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("verify-trusted") => run_verify_trusted(&arguments),
        #[cfg(feature = "fixture")]
        Some("verify") => run_verify_fixture(&arguments),
        Some("install") => run_install(&arguments),
        Some("install-local") => run_install_local(&arguments),
        #[cfg(feature = "community-macos")]
        Some("prepare-community-install") => run_prepare_community_install(&arguments),
        Some(command) => bail!("unknown command {command}"),
        None => usage(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confined_paths_resolve_existing_symlink_ancestors() {
        let temporary = TemporaryDirectory::new().unwrap();
        let home = temporary.path.join("home");
        let outside = temporary.path.join("outside");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&outside).unwrap();

        assert!(path_is_confined(&home.join(".local/lib/new"), &home).unwrap());
        symlink(&outside, home.join("escape")).unwrap();
        assert!(!path_is_confined(&home.join("escape/new"), &home).unwrap());
        assert!(!path_is_confined(&home.join("../outside"), &home).unwrap());
    }

    #[test]
    fn linux_install_rejects_unlisted_empty_directories() {
        let temporary = TemporaryDirectory::new().unwrap();
        let app = temporary.path.join("app");
        fs::create_dir(&app).unwrap();
        fs::create_dir(app.join("unexpected")).unwrap();

        let error = validate_linux_install(&app).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("application contains unexpected directory unexpected")
        );
    }

    #[test]
    fn process_identity_includes_start_time() {
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]));
        let start_time = system.process(pid).unwrap().start_time();

        assert!(process_matches_start_time(&system, pid, start_time));
        assert!(!process_matches_start_time(
            &system,
            pid,
            start_time.wrapping_add(1)
        ));
    }
}
