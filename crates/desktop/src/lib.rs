use std::io::BufReader;
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, bail};
use nah_protocol::{
    ClientRequest, PROTOCOL_VERSION, ServiceResponse, WireError, read_message, socket_path,
    write_message,
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
}

impl SessionClient {
    /// Connects to the service and completes the protocol handshake once.
    ///
    /// # Errors
    ///
    /// Returns an error when the local socket is unavailable or the service
    /// rejects the protocol version.
    pub fn connect() -> Result<Self> {
        let path = socket_path();
        let mut stream =
            UnixStream::connect(&path).with_context(|| format!("connect to {}", path.display()))?;
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

        Ok(Self { stream, reader })
    }

    /// Sends one request and waits for its response.
    ///
    /// A transport failure reconnects and retries exactly once. Service errors
    /// are not retried.
    ///
    /// # Errors
    ///
    /// Returns an error after a second transport failure, a failed reconnect,
    /// or a service-side request rejection.
    pub fn call(&mut self, request: &ClientRequest) -> Result<ServiceResponse> {
        let response = if let Ok(response) = self.exchange(request) {
            response
        } else {
            *self = Self::connect()?;
            self.exchange(request)?
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
}

#[cfg(test)]
mod tests {
    use nah_protocol::MAX_FRAME_SIZE;

    #[test]
    fn protocol_keeps_individual_input_frames_bounded() {
        assert_eq!(MAX_FRAME_SIZE, 1024 * 1024);
    }
}
