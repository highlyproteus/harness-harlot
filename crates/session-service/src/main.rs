use std::fs;
use std::io::{self, BufReader};
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use hh_protocol::{
    ClientRequest, PROTOCOL_VERSION, ServiceResponse, legacy_socket_path, read_message,
    socket_path, write_message,
};
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
    hh_protocol::ensure_private_directory(path)
        .with_context(|| format!("prepare private runtime directory {}", path.display()))
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

/// Shares the persist-then-exit path between all shutdown signals.
async fn persist_before_exit(sessions: &SessionRegistry) -> Result<()> {
    let sessions = sessions.clone();
    tokio::task::spawn_blocking(move || sessions.persist())
        .await
        .context("join shutdown persistence task")?
        .context("persist sessions before service shutdown")
}

fn connect_running_service(
    primary_path: &Path,
    legacy_path: Option<&Path>,
) -> Result<StdUnixStream> {
    match StdUnixStream::connect(primary_path) {
        Ok(stream) => Ok(stream),
        Err(primary_error) => {
            let Some(legacy_path) = legacy_path.filter(|legacy| *legacy != primary_path) else {
                return Err(primary_error).with_context(|| {
                    format!("connect to session service {}", primary_path.display())
                });
            };
            StdUnixStream::connect(legacy_path).with_context(|| {
                format!(
                    "connect to session service {} failed ({primary_error}); connect to legacy session service {}",
                    primary_path.display(),
                    legacy_path.display()
                )
            })
        }
    }
}

fn request_running_service_shutdown() -> Result<()> {
    let path = socket_path()?;
    let legacy_path = legacy_socket_path();
    let mut stream = connect_running_service(&path, legacy_path.as_deref())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .context("set shutdown response timeout")?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .context("set shutdown request timeout")?;
    let mut reader = BufReader::new(stream.try_clone().context("clone shutdown connection")?);
    write_message(
        &mut stream,
        &ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .context("write shutdown handshake")?;
    match read_message::<ServiceResponse>(&mut reader).context("read shutdown handshake")? {
        ServiceResponse::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => {}
        ServiceResponse::Hello { protocol_version } => bail!(
            "session service protocol version {protocol_version} does not match this session service version {PROTOCOL_VERSION}"
        ),
        ServiceResponse::Error { message } => bail!("{message}"),
        response => bail!("unexpected shutdown handshake response: {response:?}"),
    }
    write_message(&mut stream, &ClientRequest::ShutdownService)
        .context("request session service shutdown")?;
    match read_message::<ServiceResponse>(&mut reader)
        .context("read session service shutdown response")?
    {
        ServiceResponse::Ack => Ok(()),
        ServiceResponse::Error { message } => bail!("{message}"),
        response => bail!("unexpected shutdown response: {response:?}"),
    }
}

/// Installs a panic hook that appends a timestamped, symbolized entry to
/// `<state_dir>/panic.log`, or an owner-only temporary fallback, truncates it
/// past 1 MiB, and then delegates to the previous hook so stderr output is
/// preserved. Purely local diagnostics: nothing leaves the machine.
fn panic_log_directory() -> Option<PathBuf> {
    hh_protocol::state_directory()
        .filter(|directory| hh_protocol::ensure_private_directory(directory).is_ok())
        .or_else(|| {
            let directory = std::env::temp_dir().join(format!(
                "harness-harlot-{}-panic",
                rustix::process::geteuid().as_raw()
            ));
            hh_protocol::ensure_private_directory(&directory)
                .is_ok()
                .then_some(directory)
        })
}

fn install_panic_log() {
    let state_dir = panic_log_directory();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let log = state_dir
            .as_ref()
            .map(|directory| directory.join("panic.log"));
        let append_entry = |path: &std::path::Path| {
            use std::io::Write as _;
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

            static PANIC_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _guard = PANIC_LOG_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            if file.metadata()?.len() > 1024 * 1024 {
                file.set_len(0)?;
            }
            let thread = std::thread::current();
            let thread_name = thread.name().unwrap_or("<unnamed>");
            let payload = info.payload_as_str().unwrap_or("non-string panic payload");
            let backtrace = std::backtrace::Backtrace::force_capture();
            let timestamp = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unavailable".to_owned());
            writeln!(
                file,
                "{timestamp} version {} thread {thread_name}\npanic: {payload}\n{backtrace}\n",
                env!("CARGO_PKG_VERSION")
            )
        };
        if let Some(log) = log {
            let _ = append_entry(&log);
        }
        previous_hook(info);
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    install_panic_log();
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.as_slice() == ["--shutdown"] {
        return request_running_service_shutdown();
    }
    ensure!(arguments.is_empty(), "usage: hh-service [--shutdown]");
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
    let mut shutdown_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    shutdown_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut terminate_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("install SIGTERM shutdown signal")?;
    let mut hangup_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .context("install SIGHUP shutdown signal")?;

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
                persist_before_exit(&sessions).await?;
                break;
            }
            _ = terminate_signal.recv() => {
                persist_before_exit(&sessions).await?;
                break;
            }
            _ = hangup_signal.recv() => {
                persist_before_exit(&sessions).await?;
                break;
            }
            _ = shutdown_tick.tick() => {
                if sessions.shutdown_requested() {
                    persist_before_exit(&sessions).await?;
                    break;
                }
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

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener as StdUnixListener;

    use super::*;

    #[test]
    fn shutdown_connection_falls_back_to_the_legacy_socket() {
        let root =
            Path::new("/tmp").join(format!("hh-shutdown-legacy-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let primary = root.join("current.sock");
        let legacy = root.join("legacy.sock");
        let listener = StdUnixListener::bind(&legacy).unwrap();

        let stream = connect_running_service(&primary, Some(&legacy)).unwrap();
        let (accepted, _) = listener.accept().unwrap();

        drop(accepted);
        drop(stream);
        drop(listener);
        fs::remove_file(legacy).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
