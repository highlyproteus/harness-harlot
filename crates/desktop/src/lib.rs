use std::io::BufReader;
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, bail};
use nah_protocol::{
    ClientRequest, PROTOCOL_VERSION, ServiceResponse, read_message, socket_path, write_message,
};

/// Sends one bounded request over a fresh local connection. PTY ownership and
/// output draining stay in the service when this client disconnects.
///
/// # Errors
///
/// Returns an error when the local socket is unavailable, the protocol
/// handshake fails, or the daemon rejects the request.
#[allow(clippy::needless_pass_by_value)]
pub fn request(request: ClientRequest) -> Result<ServiceResponse> {
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
        ServiceResponse::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => {}
        ServiceResponse::Error { message } => bail!("service rejected handshake: {message}"),
        response => bail!("unexpected handshake response: {response:?}"),
    }

    write_message(&mut stream, &request)?;
    match read_message(&mut reader)? {
        ServiceResponse::Error { message } => bail!("service request failed: {message}"),
        response => Ok(response),
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
