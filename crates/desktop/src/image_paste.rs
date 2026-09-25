//! PNG normalization for pasted images and delivery as kitty OSC 5522 paste
//! events to applications that enabled them.

use anyhow::{Context as _, Result, bail, ensure};
use hh_protocol::{ClientRequest, ServiceResponse};
use std::fs;
use std::io::Read as _;
use std::path::Path;
use uuid::Uuid;

use crate::SharedSessionClient;
use crate::image_transfer::write_paste_png;
use crate::session::session_call;
use crate::ui_state::{CanonicalPngLimits, decode_canonical_png};

pub(crate) const MAX_PASTE_IMAGE_BYTES: usize = 25 * 1024 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const PASTE_IMAGE_LIMITS: CanonicalPngLimits = CanonicalPngLimits {
    what: "pasted image",
    max_dimension: 16_384,
    max_pixels: 64 * 1024 * 1024,
    max_alloc: 512 * 1024 * 1024,
};
/// Extensions of dropped files that are delivered as an image paste event
/// when the application accepts them.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "tif", "tiff", "bmp"];

/// Returns the image as PNG bytes: PNG input passes through unchanged, and
/// any other decodable raster format (macOS screenshots are TIFF) is
/// re-encoded.
pub(crate) fn png_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(!bytes.is_empty(), "pasted image is empty");
    ensure!(
        bytes.len() <= MAX_PASTE_IMAGE_BYTES,
        "pasted image exceeds the 25 MiB limit"
    );
    let png = if bytes.starts_with(PNG_SIGNATURE) {
        bytes.to_vec()
    } else {
        decode_canonical_png(bytes, &PASTE_IMAGE_LIMITS)
            .context("convert pasted image to PNG")?
            .png
    };
    ensure!(
        png.len() <= MAX_PASTE_IMAGE_BYTES,
        "pasted image exceeds the 25 MiB limit as PNG"
    );
    Ok(png)
}

pub(crate) fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

/// Reads a dropped image file (bounded) and returns it as PNG bytes.
pub(crate) fn image_file_png(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    ensure!(
        file.metadata()
            .with_context(|| format!("inspect {}", path.display()))?
            .is_file(),
        "dropped item is not a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_PASTE_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    png_bytes(&bytes)
}

/// Hands `png` to the service as a paste event for `pane_id`. The service
/// consumes the private file on success; on failure it is removed here so
/// the caller can fall back to typing a path.
pub(crate) fn offer_png_paste(
    client: &SharedSessionClient,
    pane_id: Uuid,
    png: &[u8],
    text: Option<String>,
) -> Result<()> {
    let path = write_paste_png(png)?;
    let result = (|| {
        let image_path = path
            .to_str()
            .context("paste image path is not UTF-8")?
            .to_owned();
        let response = session_call(
            client,
            &ClientRequest::PasteImage {
                pane_id,
                image_path,
                text,
            },
        )?;
        match response {
            ServiceResponse::Ack => Ok(()),
            ServiceResponse::Error { message } => bail!("{message}"),
            response => bail!("unexpected PasteImage response: {response:?}"),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{PNG_SIGNATURE, is_image_path, png_bytes};
    use std::io::Cursor;
    use std::path::Path;

    fn encoded(format: image::ImageFormat) -> Vec<u8> {
        let mut image = image::RgbaImage::new(3, 2);
        image.put_pixel(2, 1, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, format)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn tiff_screenshots_are_converted_to_equivalent_pngs() {
        let tiff = encoded(image::ImageFormat::Tiff);
        assert!(!tiff.starts_with(PNG_SIGNATURE));

        let png = png_bytes(&tiff).unwrap();
        assert!(png.starts_with(PNG_SIGNATURE));
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (3, 2));
        assert_eq!(decoded.get_pixel(2, 1).0, [10, 20, 30, 255]);
    }

    #[test]
    fn pngs_pass_through_and_undecodable_or_oversized_input_is_rejected() {
        let png = encoded(image::ImageFormat::Png);
        assert_eq!(png_bytes(&png).unwrap(), png);
        assert!(png_bytes(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_err());
        assert!(png_bytes(&[]).is_err());
        assert!(png_bytes(&vec![0; super::MAX_PASTE_IMAGE_BYTES + 1]).is_err());
    }

    #[test]
    fn only_raster_image_extensions_count_as_image_drops() {
        assert!(is_image_path(Path::new("/tmp/Screen Shot.TIFF")));
        assert!(is_image_path(Path::new("/tmp/photo.jpeg")));
        assert!(!is_image_path(Path::new("/tmp/vector.svg")));
        assert!(!is_image_path(Path::new("/tmp/notes.txt")));
        assert!(!is_image_path(Path::new("/tmp/png")));
    }
}
