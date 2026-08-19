//! Chunk file format: framed read/write, checksums, and terminal text extraction.
use super::MAX_LINE_CHARS;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub(crate) const CHUNK_MAGIC: &[u8; 8] = b"RMUXHST1";
pub(crate) const CHUNK_VERSION: u16 = 1;
pub(crate) const CHUNK_HEADER_BYTES: usize = 28;
pub(crate) const CHUNK_PAYLOAD_BYTES: usize = 128 * 1024;

pub(crate) fn read_chunk(path: &Path, expected_index: u32) -> Result<(Vec<u8>, bool)> {
    let max_bytes = u64::try_from(CHUNK_HEADER_BYTES + CHUNK_PAYLOAD_BYTES).unwrap_or(u64::MAX);
    let bytes = hh_protocol::read_private_file(path, max_bytes)
        .with_context(|| format!("read history chunk {}", path.display()))?;
    if bytes.len() < CHUNK_HEADER_BYTES {
        bail!("history chunk has an invalid file type or size");
    }
    if &bytes[..8] != CHUNK_MAGIC {
        bail!("history chunk magic does not match");
    }
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    let flags = u16::from_le_bytes([bytes[10], bytes[11]]);
    let index = u32::from_le_bytes(bytes[12..16].try_into().expect("four bytes"));
    let length = u32::from_le_bytes(bytes[16..20].try_into().expect("four bytes"));
    let expected_checksum = u64::from_le_bytes(bytes[20..28].try_into().expect("eight bytes"));
    if version != CHUNK_VERSION || index != expected_index {
        bail!("history chunk version or sequence does not match");
    }
    let length = usize::try_from(length).context("history chunk length exceeds usize")?;
    if length > CHUNK_PAYLOAD_BYTES || bytes.len() != CHUNK_HEADER_BYTES + length {
        bail!("history chunk payload length does not match the file");
    }
    let payload = bytes[CHUNK_HEADER_BYTES..].to_vec();
    if checksum(&payload) != expected_checksum {
        bail!("history chunk checksum does not match");
    }
    Ok((payload, flags & 1 != 0))
}

pub(crate) fn write_chunk_atomic(
    path: &Path,
    index: u32,
    gap_before: bool,
    payload: &[u8],
) -> Result<()> {
    if payload.len() > CHUNK_PAYLOAD_BYTES {
        bail!("history chunk payload exceeds {CHUNK_PAYLOAD_BYTES}-byte limit");
    }
    let mut bytes = Vec::with_capacity(CHUNK_HEADER_BYTES + payload.len());
    bytes.extend_from_slice(CHUNK_MAGIC);
    bytes.extend_from_slice(&CHUNK_VERSION.to_le_bytes());
    bytes.extend_from_slice(&u16::from(gap_before).to_le_bytes());
    bytes.extend_from_slice(&index.to_le_bytes());
    bytes.extend_from_slice(
        &u32::try_from(payload.len())
            .context("history chunk exceeds u32")?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&checksum(payload).to_le_bytes());
    bytes.extend_from_slice(payload);
    hh_protocol::atomic_write_private(path, &bytes)
        .with_context(|| format!("write history chunk {}", path.display()))
}

pub(crate) fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

pub(crate) fn terminal_output_lines(bytes: &[u8]) -> Vec<String> {
    let mut text = Vec::with_capacity(bytes.len());
    let mut escape = EscapeState::Text;
    for &byte in bytes {
        escape = match escape {
            EscapeState::Text if byte == 0x1b => EscapeState::Escape,
            EscapeState::Text => {
                text.push(byte);
                EscapeState::Text
            }
            EscapeState::Escape => match byte {
                b'[' => EscapeState::Csi,
                b']' => EscapeState::Osc,
                _ => EscapeState::Text,
            },
            EscapeState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    EscapeState::Text
                } else {
                    EscapeState::Csi
                }
            }
            EscapeState::Osc => {
                if byte == 0x07 {
                    EscapeState::Text
                } else if byte == 0x1b {
                    EscapeState::OscEscape
                } else {
                    EscapeState::Osc
                }
            }
            EscapeState::OscEscape => {
                if byte == b'\\' {
                    EscapeState::Text
                } else {
                    EscapeState::Osc
                }
            }
        };
    }
    let mut lines = vec![(Vec::<char>::new(), 0_usize)];
    for character in String::from_utf8_lossy(&text).chars() {
        match character {
            '\n' => {
                lines.push((Vec::new(), 0));
            }
            '\r' => {
                if let Some((_, cursor)) = lines.last_mut() {
                    *cursor = 0;
                }
            }
            '\u{8}' => {
                if let Some((_, cursor)) = lines.last_mut()
                    && *cursor > 0
                {
                    *cursor -= 1;
                }
            }
            character if !character.is_control() => {
                if let Some((line, cursor)) = lines.last_mut()
                    && *cursor < MAX_LINE_CHARS
                {
                    if *cursor < line.len() {
                        line[*cursor] = character;
                    } else {
                        line.push(character);
                    }
                    *cursor += 1;
                }
            }
            _ => {}
        }
    }
    lines
        .into_iter()
        .map(|(line, _)| line.into_iter().collect())
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum EscapeState {
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::history::ensure_private_directory;

    fn test_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hh-history-{label}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn chunk_round_trip_detects_corruption_and_sequence_mismatch() {
        let root = test_root("integrity");
        ensure_private_directory(&root).unwrap();
        let path = root.join("00000000.rmh");
        write_chunk_atomic(&path, 0, true, b"hello\nworld").unwrap();
        assert_eq!(
            read_chunk(&path, 0).unwrap(),
            (b"hello\nworld".to_vec(), true)
        );
        assert!(read_chunk(&path, 1).is_err());

        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(read_chunk(&path, 0).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backspace_moves_the_cursor_without_deleting_terminal_cells() {
        assert_eq!(terminal_output_lines(b"abc\rX\x08Y"), ["Ybc"]);
    }

    #[test]
    fn chunk_writer_rejects_oversized_payload_before_creating_a_file() {
        let root = test_root("oversized");
        ensure_private_directory(&root).unwrap();
        let path = root.join("00000000.rmh");
        let payload = vec![0; CHUNK_PAYLOAD_BYTES + 1];

        let error = write_chunk_atomic(&path, 0, false, &payload).unwrap_err();

        assert!(error.to_string().contains("payload exceeds"));
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
