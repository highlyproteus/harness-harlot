//! Bounded, length-prefixed JSON framing for the Unix-domain socket.

use std::io::{BufRead, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

pub const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum WireError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("peer closed the connection")]
    Closed,
    #[error("frame is too large: {0} bytes")]
    FrameTooLarge(usize),
}

/// Encodes a JSON message as one bounded, big-endian length-prefixed frame.
///
/// # Errors
///
/// Returns [`WireError::Json`] when serialization fails and
/// [`WireError::FrameTooLarge`] when the encoded payload exceeds
/// [`MAX_FRAME_SIZE`].
pub fn encode_frame<T: Serialize + ?Sized>(message: &T) -> Result<Vec<u8>, WireError> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge(payload.len()));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| WireError::FrameTooLarge(payload.len()))?
        .to_be_bytes();
    let mut frame = Vec::with_capacity(length.len() + payload.len());
    frame.extend_from_slice(&length);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decodes one bounded JSON frame payload.
///
/// # Errors
///
/// Returns [`WireError::FrameTooLarge`] when `payload` exceeds
/// [`MAX_FRAME_SIZE`] and [`WireError::Json`] when it is not a valid message
/// of the requested type.
pub fn decode_frame<T: DeserializeOwned>(payload: &[u8]) -> Result<T, WireError> {
    if payload.len() > MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge(payload.len()));
    }
    Ok(serde_json::from_slice(payload)?)
}

/// Writes one length-prefixed JSON message and flushes it to the peer.
///
/// # Errors
///
/// Returns [`WireError::Json`] when serialization fails and [`WireError::Io`]
/// when the encoded message cannot be written or flushed.
pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> Result<(), WireError> {
    let frame = encode_frame(message)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// Reads and decodes one length-prefixed JSON message.
///
/// # Errors
///
/// Returns [`WireError::Closed`] when the peer closes before another message,
/// [`WireError::Io`] when reading fails, and [`WireError::Json`] when the
/// length-prefixed message is not valid JSON for the requested type.
pub fn read_message<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<T, WireError> {
    let mut length = [0_u8; 4];
    if let Err(error) = reader.read_exact(&mut length) {
        return if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Err(WireError::Closed)
        } else {
            Err(WireError::Io(error))
        };
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge(length));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    decode_frame(&payload)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::{ClientRequest, PROTOCOL_VERSION};

    #[test]
    fn messages_round_trip_as_length_prefixed_json() {
        let request = ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        };
        let mut bytes = Vec::new();

        write_message(&mut bytes, &request).unwrap();
        let decoded: ClientRequest = read_message(&mut Cursor::new(bytes)).unwrap();

        assert_eq!(decoded, request);
    }

    #[test]
    fn stale_protocol_versions_remain_detectable_before_dispatch() {
        let stale_version = PROTOCOL_VERSION - 1;
        let request: ClientRequest = serde_json::from_value(serde_json::json!({
            "type": "hello",
            "protocol_version": stale_version,
        }))
        .unwrap();

        let ClientRequest::Hello { protocol_version } = request else {
            panic!("expected hello request");
        };
        assert_ne!(protocol_version, PROTOCOL_VERSION);
    }
}
