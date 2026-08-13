use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nah_protocol::socket_path;
use nah_session_service::{SessionRegistry, serve_connection};
use tokio::net::{UnixListener, UnixStream};

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

async fn prepare_socket(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if UnixStream::connect(path).await.is_ok() {
        bail!(
            "a Not a Harness session service is already listening at {}",
            path.display()
        );
    }
    fs::remove_file(path).with_context(|| format!("remove stale socket {}", path.display()))?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = socket_path();
    prepare_socket(&path).await?;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind session socket {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict session socket {}", path.display()))?;
    let _guard = SocketGuard(path.clone());
    let sessions = SessionRegistry::load_default()?;
    let mut persistence_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    persistence_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    println!(
        "Not a Harness session service listening at {}",
        path.display()
    );
    loop {
        tokio::select! {
            connection = listener.accept() => {
                match connection {
                    Ok((stream, _address)) => {
                        let sessions = sessions.clone();
                        tokio::spawn(async move {
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
