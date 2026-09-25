//! The subset of the kitty graphics protocol Harness Harlot displays: PNG
//! images transmitted in-band and shown through Unicode placeholder cells
//! (`a=p,U=1` virtual placements). Cursor-positioned placements, file or
//! shared-memory transmission, raw pixel formats, and replies are not
//! supported; applications that send `q=2` (as omp does) expect no reply.

use std::collections::HashMap;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Largest decoded PNG accepted for one image.
pub const MAX_KITTY_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// Images kept per pane; the oldest is evicted first.
pub const MAX_KITTY_IMAGES: usize = 32;
/// Decoded bytes kept per pane across all images.
pub const MAX_KITTY_TOTAL_BYTES: usize = 128 * 1024 * 1024;
const MAX_PENDING_BASE64_BYTES: usize = MAX_KITTY_IMAGE_BYTES / 3 * 4 + 8;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Image data changes the session service persists for the desktop.
#[derive(Clone, Eq, PartialEq)]
pub enum KittyImageEvent {
    Stored {
        id: u32,
        generation: u64,
        png: Vec<u8>,
    },
    Removed {
        id: u32,
        generation: u64,
    },
}

impl fmt::Debug for KittyImageEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stored {
                id,
                generation,
                png,
            } => formatter
                .debug_struct("Stored")
                .field("id", id)
                .field("generation", generation)
                .field("bytes", &png.len())
                .finish(),
            Self::Removed { id, generation } => formatter
                .debug_struct("Removed")
                .field("id", id)
                .field("generation", generation)
                .finish(),
        }
    }
}

/// An image with a virtual placement, displayable through placeholder cells.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlacedKittyImage {
    pub id: u32,
    pub generation: u64,
    pub columns: u16,
    pub rows: u16,
}

#[derive(Debug)]
struct StoredImage {
    generation: u64,
    bytes: usize,
    placement: Option<(u16, u16)>,
}

/// A chunked transmission in progress (`m=1` until the final `m=0` chunk).
struct PendingTransmit {
    id: u32,
    placement: Option<(u16, u16)>,
    base64: Vec<u8>,
    /// Unsupported or oversized: keep consuming chunks, store nothing.
    discard: bool,
}

#[derive(Default)]
pub(crate) struct KittyGraphics {
    pending: Option<PendingTransmit>,
    images: HashMap<u32, StoredImage>,
    next_generation: u64,
    events: Vec<KittyImageEvent>,
}

impl fmt::Debug for KittyGraphics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KittyGraphics")
            .field("images", &self.images.len())
            .field("pending", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

/// Parsed `key=value` control data of one graphics command.
#[derive(Default)]
struct Control {
    action: Option<u8>,
    format: Option<u32>,
    medium: Option<u8>,
    compression: Option<u8>,
    id: Option<u32>,
    more: bool,
    unicode_placeholder: bool,
    columns: Option<u16>,
    rows: Option<u16>,
    delete: Option<u8>,
}

impl Control {
    fn parse(control: &[u8]) -> Self {
        let mut parsed = Self::default();
        for pair in control.split(|byte| *byte == b',') {
            let Some((&key, value)) = pair.split_first() else {
                continue;
            };
            let Some(value) = value.strip_prefix(b"=") else {
                continue;
            };
            let number = || std::str::from_utf8(value).ok()?.parse::<u32>().ok();
            let letter = value.first().copied();
            match key {
                b'a' => parsed.action = letter,
                b'f' => parsed.format = number(),
                b't' => parsed.medium = letter,
                b'o' => parsed.compression = letter,
                b'i' => parsed.id = number(),
                b'm' => parsed.more = number() == Some(1),
                b'U' => parsed.unicode_placeholder = number() == Some(1),
                b'c' => parsed.columns = number().and_then(|value| u16::try_from(value).ok()),
                b'r' => parsed.rows = number().and_then(|value| u16::try_from(value).ok()),
                b'd' => parsed.delete = letter,
                _ => {}
            }
        }
        parsed
    }

    fn placement(&self) -> Option<(u16, u16)> {
        match (self.unicode_placeholder, self.columns, self.rows) {
            (true, Some(columns), Some(rows)) if columns > 0 && rows > 0 => Some((columns, rows)),
            _ => None,
        }
    }
}

impl KittyGraphics {
    /// Applies one graphics command: the APC body after `ESC _ G`.
    pub(crate) fn handle_command(&mut self, body: &[u8]) {
        let (control, payload) = match body.iter().position(|byte| *byte == b';') {
            Some(separator) => (&body[..separator], &body[separator + 1..]),
            None => (body, &[][..]),
        };
        let control = Control::parse(control);
        if self.pending.is_some() && control.action.is_none() && control.id.is_none() {
            self.continue_transmit(payload, control.more);
            return;
        }
        // A new command abandons any unfinished transmission, as in kitty.
        self.pending = None;
        match control.action.unwrap_or(b't') {
            b't' | b'T' => self.begin_transmit(&control, payload),
            b'p' => self.place(&control),
            b'd' => self.delete(&control),
            _ => {}
        }
    }

    pub(crate) fn take_events(&mut self) -> Vec<KittyImageEvent> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn placed_images(&self) -> Vec<PlacedKittyImage> {
        let mut placed = self
            .images
            .iter()
            .filter_map(|(&id, image)| {
                let (columns, rows) = image.placement?;
                Some(PlacedKittyImage {
                    id,
                    generation: image.generation,
                    columns,
                    rows,
                })
            })
            .collect::<Vec<_>>();
        placed.sort_unstable_by_key(|image| image.id);
        placed
    }

    fn begin_transmit(&mut self, control: &Control, payload: &[u8]) {
        let supported = control.id.is_some_and(|id| id != 0)
            && control.format == Some(100)
            && control.medium.is_none_or(|medium| medium == b'd')
            && control.compression.is_none();
        self.pending = Some(PendingTransmit {
            id: control.id.unwrap_or(0),
            placement: control.placement(),
            base64: Vec::new(),
            discard: !supported,
        });
        self.continue_transmit(payload, control.more);
    }

    fn continue_transmit(&mut self, payload: &[u8], more: bool) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        if !pending.discard {
            if pending.base64.len().saturating_add(payload.len()) > MAX_PENDING_BASE64_BYTES {
                pending.discard = true;
                pending.base64 = Vec::new();
            } else {
                pending.base64.extend_from_slice(payload);
            }
        }
        if more {
            return;
        }
        let Some(pending) = self.pending.take() else {
            return;
        };
        if pending.discard {
            return;
        }
        let Ok(png) = STANDARD.decode(&pending.base64) else {
            return;
        };
        if png.len() > MAX_KITTY_IMAGE_BYTES || !png.starts_with(PNG_SIGNATURE) {
            return;
        }
        self.store(pending.id, pending.placement, png);
    }

    fn store(&mut self, id: u32, placement: Option<(u16, u16)>, png: Vec<u8>) {
        self.next_generation += 1;
        let generation = self.next_generation;
        let previous = self.images.insert(
            id,
            StoredImage {
                generation,
                bytes: png.len(),
                placement,
            },
        );
        if let Some(previous) = previous {
            self.events.push(KittyImageEvent::Removed {
                id,
                generation: previous.generation,
            });
            // Re-transmitting an id keeps its placement unless one was given.
            if placement.is_none()
                && let Some(image) = self.images.get_mut(&id)
            {
                image.placement = previous.placement;
            }
        }
        self.events.push(KittyImageEvent::Stored {
            id,
            generation,
            png,
        });
        self.evict();
    }

    fn evict(&mut self) {
        loop {
            let total = self.images.values().map(|image| image.bytes).sum::<usize>();
            if self.images.len() <= MAX_KITTY_IMAGES && total <= MAX_KITTY_TOTAL_BYTES {
                return;
            }
            let Some((&oldest, _)) = self.images.iter().min_by_key(|(_, image)| image.generation)
            else {
                return;
            };
            self.remove(oldest);
        }
    }

    fn place(&mut self, control: &Control) {
        let (Some(id), Some(placement)) = (control.id, control.placement()) else {
            return;
        };
        if let Some(image) = self.images.get_mut(&id) {
            image.placement = Some(placement);
        }
    }

    fn delete(&mut self, control: &Control) {
        match control.delete.unwrap_or(b'a') {
            b'a' | b'A' => {
                let ids = self.images.keys().copied().collect::<Vec<_>>();
                for id in ids {
                    self.remove(id);
                }
            }
            // Only virtual placements exist, so deleting an image's placements
            // leaves nothing to show: free it either way.
            b'i' | b'I' => {
                if let Some(id) = control.id {
                    self.remove(id);
                }
            }
            _ => {}
        }
    }

    fn remove(&mut self, id: u32) {
        if let Some(image) = self.images.remove(&id) {
            self.events.push(KittyImageEvent::Removed {
                id,
                generation: image.generation,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot-really-a-png";

    fn command(graphics: &mut KittyGraphics, control: &str, payload: &[u8]) {
        let mut body = control.as_bytes().to_vec();
        body.push(b';');
        body.extend_from_slice(payload);
        graphics.handle_command(&body);
    }

    fn encoded(bytes: &[u8]) -> Vec<u8> {
        STANDARD.encode(bytes).into_bytes()
    }

    #[test]
    fn chunked_png_transmission_is_stored_once_complete() {
        let mut graphics = KittyGraphics::default();
        let data = encoded(PNG);
        let (first, rest) = data.split_at(8);
        command(&mut graphics, "a=t,f=100,q=2,i=7,m=1", first);
        assert!(graphics.take_events().is_empty());
        command(&mut graphics, "q=2,m=0", rest);
        assert_eq!(
            graphics.take_events(),
            vec![KittyImageEvent::Stored {
                id: 7,
                generation: 1,
                png: PNG.to_vec()
            }]
        );
    }

    #[test]
    fn only_images_with_a_virtual_placement_are_displayable() {
        let mut graphics = KittyGraphics::default();
        command(&mut graphics, "a=t,f=100,q=2,i=7", &encoded(PNG));
        assert!(graphics.placed_images().is_empty());
        command(&mut graphics, "a=p,q=2,C=1,i=7,c=40,r=10", b"");
        assert!(graphics.placed_images().is_empty(), "direct placement");
        command(&mut graphics, "a=p,U=1,q=2,i=7,p=7,c=40,r=10", b"");
        assert_eq!(
            graphics.placed_images(),
            vec![PlacedKittyImage {
                id: 7,
                generation: 1,
                columns: 40,
                rows: 10
            }]
        );
    }

    #[test]
    fn retransmitting_an_id_replaces_the_file_and_keeps_its_placement() {
        let mut graphics = KittyGraphics::default();
        command(&mut graphics, "a=t,f=100,i=3", &encoded(PNG));
        command(&mut graphics, "a=p,U=1,i=3,c=4,r=2", b"");
        graphics.take_events();
        command(&mut graphics, "a=t,f=100,i=3", &encoded(PNG));
        assert_eq!(
            graphics.take_events(),
            vec![
                KittyImageEvent::Removed {
                    id: 3,
                    generation: 1
                },
                KittyImageEvent::Stored {
                    id: 3,
                    generation: 2,
                    png: PNG.to_vec()
                },
            ]
        );
        assert_eq!(graphics.placed_images()[0].columns, 4);
    }

    #[test]
    fn unsupported_or_invalid_transmissions_store_nothing() {
        let mut graphics = KittyGraphics::default();
        for control in [
            "a=t,f=32,i=1",
            "a=t,f=100,t=f,i=1",
            "a=t,f=100,o=z,i=1",
            "a=t,f=100",
        ] {
            command(&mut graphics, control, &encoded(PNG));
        }
        command(&mut graphics, "a=t,f=100,i=2", &encoded(b"GIF89a"));
        command(&mut graphics, "a=t,f=100,i=3", b"!!not base64!!");
        assert!(graphics.take_events().is_empty());
    }

    #[test]
    fn discarded_transmissions_still_consume_their_continuation_chunks() {
        let mut graphics = KittyGraphics::default();
        command(&mut graphics, "a=t,f=32,i=1,m=1", b"AAAA");
        command(&mut graphics, "m=0", b"AAAA");
        command(&mut graphics, "m=0", &encoded(PNG));
        assert!(graphics.take_events().is_empty());
    }

    #[test]
    fn deleting_frees_one_image_or_all_of_them() {
        let mut graphics = KittyGraphics::default();
        for id in [1, 2, 3] {
            command(&mut graphics, &format!("a=t,f=100,i={id}"), &encoded(PNG));
        }
        graphics.take_events();
        command(&mut graphics, "a=d,d=I,i=2,q=2", b"");
        assert_eq!(
            graphics.take_events(),
            vec![KittyImageEvent::Removed {
                id: 2,
                generation: 2
            }]
        );
        command(&mut graphics, "a=d,d=A", b"");
        assert_eq!(graphics.take_events().len(), 2);
        assert!(graphics.images.is_empty());
    }

    #[test]
    fn the_oldest_image_is_evicted_past_the_per_pane_limit() {
        let mut graphics = KittyGraphics::default();
        for id in 1..=u32::try_from(MAX_KITTY_IMAGES).unwrap() + 1 {
            command(&mut graphics, &format!("a=t,f=100,i={id}"), &encoded(PNG));
        }
        assert!(!graphics.images.contains_key(&1));
        assert_eq!(graphics.images.len(), MAX_KITTY_IMAGES);
        assert!(graphics.take_events().contains(&KittyImageEvent::Removed {
            id: 1,
            generation: 1
        }));
    }
}
