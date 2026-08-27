use std::fs;
use std::io::BufReader;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, bail};
use hh_protocol::{
    ClientRequest, PROTOCOL_VERSION, ServiceResponse, WireError, legacy_socket_path, read_message,
    socket_path, write_message,
};

/// A persistent, serialized connection to the local session service.
///
/// Notification requests are written without waiting for a response, and the
/// service answers nothing at all for the one-way requests (terminal input and
/// selection updates) that `notify` carries. Typing therefore never blocks on a
/// round trip and the stream stays synchronized with one response per `call`.
#[derive(Debug)]
pub struct SessionClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    valid: bool,
}

fn validate_socket_path(path: &Path, allow_legacy_temp_parent: bool) -> Result<()> {
    let parent = path
        .parent()
        .context("session socket has no parent directory")?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspect session runtime directory {}", parent.display()))?;
    if !parent_metadata.file_type().is_dir() {
        bail!(
            "session runtime path is not a real directory: {}",
            parent.display()
        );
    }
    let private_parent = hh_protocol::validate_private_ownership(&parent_metadata);
    let accepted_legacy_parent = allow_legacy_temp_parent && parent == std::env::temp_dir();
    if !private_parent && !accepted_legacy_parent {
        bail!(
            "session runtime directory must be owned by the current user and mode 0700: {}",
            parent.display()
        );
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect session socket {}", path.display()))?;
    if !metadata.file_type().is_socket() {
        bail!("session path is not a Unix socket: {}", path.display());
    }
    if !hh_protocol::validate_private_ownership(&metadata) {
        bail!(
            "session socket must be owned by the current user and inaccessible to group/other: {}",
            path.display()
        );
    }
    Ok(())
}

impl SessionClient {
    /// Connects to the service and completes the protocol handshake once.
    ///
    /// # Errors
    ///
    /// Returns an error when the local socket is unavailable or the service
    /// rejects the protocol version.
    pub fn connect() -> Result<Self> {
        let path = socket_path()?;
        match Self::connect_path(&path, false) {
            Ok(client) => Ok(client),
            Err(primary_error) => {
                let Some(legacy_path) = legacy_socket_path().filter(|legacy| legacy != &path)
                else {
                    return Err(primary_error);
                };
                Self::connect_path(&legacy_path, true).with_context(|| {
                    format!(
                        "primary session socket failed ({primary_error:#}); legacy socket {} also failed",
                        legacy_path.display()
                    )
                })
            }
        }
    }

    fn connect_path(path: &Path, allow_legacy_temp_parent: bool) -> Result<Self> {
        validate_socket_path(path, allow_legacy_temp_parent)?;
        let mut stream =
            UnixStream::connect(path).with_context(|| format!("connect to {}", path.display()))?;
        // Deadlines bound every blocking operation on this connection: a
        // wedged service fails fast instead of freezing a caller forever.
        // Timeouts live on the shared file description, so the BufReader
        // clone below inherits them.
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .context("set session socket read timeout")?;
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .context("set session socket write timeout")?;
        let mut reader = BufReader::new(stream.try_clone().context("clone session socket")?);

        write_message(
            &mut stream,
            &ClientRequest::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )?;
        match read_message::<ServiceResponse>(&mut reader)? {
            ServiceResponse::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => {
            }
            ServiceResponse::Error { message } => bail!("service rejected handshake: {message}"),
            response => bail!("unexpected handshake response: {response:?}"),
        }

        Ok(Self {
            stream,
            reader,
            valid: true,
        })
    }

    /// Reports whether the previous socket location still has a listening
    /// service, even when its protocol is too old for this desktop.
    pub fn legacy_service_is_listening() -> bool {
        legacy_socket_path().is_some_and(|path| {
            if validate_socket_path(&path, true).is_err() {
                return false;
            }
            let Ok(mut stream) = UnixStream::connect(path) else {
                return false;
            };
            // Send a complete handshake so an older service never retains an
            // idle connection merely because this compatibility probe ran.
            write_message(
                &mut stream,
                &ClientRequest::Hello {
                    protocol_version: PROTOCOL_VERSION,
                },
            )
            .is_ok()
        })
    }

    /// Sends one request and waits for its response.
    ///
    /// A failed write reconnects and retries exactly once because an incomplete
    /// frame cannot be dispatched by the service. A read failure is not
    /// retried: the request may already have executed, so replaying it could
    /// duplicate a mutation.
    ///
    /// # Errors
    ///
    /// Returns an error after a failed response read, a failed reconnect or
    /// retry, or a service-side request rejection.
    pub fn call(&mut self, request: &ClientRequest) -> Result<ServiceResponse> {
        if !self.valid {
            *self = Self::connect()?;
        }
        let response = match write_message(&mut self.stream, request) {
            Ok(()) => match read_message(&mut self.reader) {
                Ok(response) => response,
                Err(error) => {
                    self.invalidate();
                    return Err(error.into());
                }
            },
            Err(WireError::Io(_)) => {
                *self = Self::connect()?;
                match self.exchange(request) {
                    Ok(response) => response,
                    Err(error) => {
                        self.invalidate();
                        return Err(error.into());
                    }
                }
            }
            Err(error) => return Err(error.into()),
        };
        match response {
            ServiceResponse::Error { message } => bail!("service request failed: {message}"),
            response => Ok(response),
        }
    }

    /// Writes one request without waiting for the service response.
    ///
    /// # Errors
    ///
    /// Returns an error when both the initial write and the single reconnect
    /// retry fail.
    pub fn notify(&mut self, request: &ClientRequest) -> Result<()> {
        if !self.valid {
            *self = Self::connect()?;
        }
        if write_message(&mut self.stream, request).is_err() {
            *self = Self::connect()?;
            write_message(&mut self.stream, request)?;
        }
        Ok(())
    }

    fn exchange(&mut self, request: &ClientRequest) -> Result<ServiceResponse, WireError> {
        write_message(&mut self.stream, request)?;
        read_message(&mut self.reader)
    }

    fn invalidate(&mut self) {
        self.valid = false;
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    use std::os::unix::net::UnixListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    use hh_protocol::MAX_FRAME_SIZE;

    #[test]
    fn protocol_keeps_individual_input_frames_bounded() {
        assert_eq!(MAX_FRAME_SIZE, 4 * 1024 * 1024);
    }

    #[test]
    fn response_timeout_invalidates_connection_before_any_later_request() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        client_stream
            .set_read_timeout(Some(std::time::Duration::from_millis(20)))
            .unwrap();
        let reader = BufReader::new(client_stream.try_clone().unwrap());
        let mut client = SessionClient {
            stream: client_stream,
            reader,
            valid: true,
        };
        let server = std::thread::spawn(move || {
            let mut reader = BufReader::new(server_stream.try_clone().unwrap());
            assert_eq!(
                read_message::<ClientRequest>(&mut reader).unwrap(),
                ClientRequest::GetSnapshot
            );
            std::thread::sleep(std::time::Duration::from_millis(80));
            let _ = write_message(&mut server_stream, &ServiceResponse::Ack);
        });

        assert!(client.call(&ClientRequest::GetSnapshot).is_err());
        assert!(!client.valid, "a timed-out stream must never be reused");
        server.join().unwrap();
    }

    #[test]
    fn stale_service_handshake_fails_cleanly_without_hanging() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let directory =
            Path::new("/tmp").join(format!("hhc-mismatch-{}-{suffix}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join("service.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            assert_eq!(
                read_message::<ClientRequest>(&mut reader).unwrap(),
                ClientRequest::Hello {
                    protocol_version: PROTOCOL_VERSION
                }
            );
            write_message(
                &mut stream,
                &ServiceResponse::Hello {
                    protocol_version: PROTOCOL_VERSION - 1,
                },
            )
            .unwrap();
        });

        let error = SessionClient::connect_path(&path, false).unwrap_err();
        assert!(error.to_string().contains("unexpected handshake response"));
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
