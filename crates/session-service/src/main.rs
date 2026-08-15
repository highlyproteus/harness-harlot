use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use hh_protocol::{ServiceResponse, legacy_socket_path, socket_path};
use hh_session_service::{SessionRegistry, serve_connection};
use rustix::fs::Mode;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

const MAX_CONCURRENT_CLIENTS: usize = 64;

#[derive(Debug)]
struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("failed to remove socket {}: {error}", self.0.display());
        }
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)
            .with_context(|| format!("create private runtime directory {}", path.display()))?,
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect runtime directory {}", path.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "runtime path must be a real directory: {}",
        path.display()
    );
    ensure!(
        metadata.uid() == rustix::process::geteuid().as_raw(),
        "runtime directory must be owned by the current user: {}",
        path.display()
    );
    ensure!(
        metadata.mode().trailing_zeros() >= 6,
        "runtime directory must not be accessible by group or other users: {} (run chmod 700)",
        path.display()
    );
    Ok(())
}

async fn prepare_socket(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    ensure!(
        metadata.file_type().is_socket(),
        "refusing to replace non-socket path {}",
        path.display()
    );
    if UnixStream::connect(path).await.is_ok() {
        bail!(
            "a Harness Harlot session service is already listening at {}",
            path.display()
        );
    }
    fs::remove_file(path).with_context(|| format!("remove stale socket {}", path.display()))?;
    Ok(())
}

fn reject_excess_client(stream: &UnixStream) {
    let response = ServiceResponse::Error {
        message: format!("session service connection limit of {MAX_CONCURRENT_CLIENTS} reached"),
    };
    if let Ok(frame) = hh_protocol::encode_frame(&response) {
        // Never create another waiting task after the client limit is reached.
        // A best-effort non-blocking error is enough; dropping the accepted
        // stream immediately is the resource boundary.
        let _ = stream.try_write(&frame);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Narrow the owner-only umask to runtime-directory and socket creation.
    // Restore it before spawning PTYs so child shells keep the user's umask.
    let inherited_umask = rustix::process::umask(Mode::from_bits_retain(0o077));
    let path = socket_path()?;
    if let Some(legacy_path) = legacy_socket_path().filter(|legacy| legacy != &path)
        && StdUnixStream::connect(&legacy_path).is_ok()
    {
        bail!(
            "an older session service is still listening at {}; refusing to start a second service and strand its live PTYs",
            legacy_path.display()
        );
    }
    let parent = path
        .parent()
        .context("session socket has no parent directory")?;
    ensure_private_directory(parent)?;
    prepare_socket(&path).await?;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind session socket {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict session socket {}", path.display()))?;
    rustix::process::umask(inherited_umask);
    let _guard = SocketGuard(path.clone());
    let sessions = SessionRegistry::load_default()?;
    let clients = Arc::new(Semaphore::new(MAX_CONCURRENT_CLIENTS));
    let mut persistence_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    persistence_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    println!(
        "Harness Harlot session service listening at {}",
        path.display()
    );
    loop {
        tokio::select! {
            connection = listener.accept() => {
                match connection {
                    Ok((stream, _address)) => {
                        let Ok(permit) = clients.clone().try_acquire_owned() else {
                            reject_excess_client(&stream);
                            continue;
                        };
                        let sessions = sessions.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(error) = serve_connection(stream, &sessions).await {
                                eprintln!("client connection ended with error: {error:#}");
                            }
                        });
                    }
                    Err(error) => eprintln!("failed to accept client: {error}"),
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal.context("install or await shutdown signal")?;
                let sessions = sessions.clone();
                tokio::task::spawn_blocking(move || sessions.persist())
                    .await
                    .context("join shutdown persistence task")?
                    .context("persist sessions before service shutdown")?;
                break;
            }
            _ = persistence_tick.tick() => {
                let sessions = sessions.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = sessions.persist() {
                        eprintln!("failed to persist session recovery state: {error:#}");
                    }
                })
                .await
                .context("join periodic persistence task")?;
            }
        }
    }
    Ok(())
}
