//! Removes identifying metadata from images without re-encoding them.
//!
//! Photos carry EXIF, XMP and comment data that can include the location they were taken at.
//! Everything but the color profile and the orientation is dropped, so the image still displays
//! the same way it did before it was uploaded.

use image::metadata::Orientation;
use img_parts::{
    Bytes, ImageEXIF, ImageICC,
    jpeg::{Jpeg, JpegSegment, markers},
    png::Png,
    riff::{RiffChunk, RiffContent},
    webp::{CHUNK_VP8X, CHUNK_XMP, WebP},
};

/// Reasons an image's metadata couldn't be removed.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    /// A JPEG, PNG or WebP container could not be parsed.
    #[error(transparent)]
    Container(#[from] img_parts::Error),
    /// An AVIF container could not be parsed or holds something unsupported.
    #[error("{0}")]
    Avif(&'static str),
}

/// PNG chunks holding text, timestamps or EXIF data.
const PNG_METADATA_CHUNKS: [[u8; 4]; 5] = [*b"eXIf", *b"tEXt", *b"zTXt", *b"iTXt", *b"tIME"];

/// Bit of the first VP8X flags byte that marks a WebP as carrying XMP.
const WEBP_XMP_FLAG: u8 = 0b0000_0100;

/// Length of a JFIF APP0 segment up to its thumbnail dimensions.
const JFIF_HEADER_LEN: usize = 12;

/// Builds a big-endian EXIF block holding only the orientation tag, or `None` when the image
/// needs no transformation.
fn orientation_exif(exif: Option<&[u8]>) -> Option<Bytes> {
    let orientation = exif.and_then(Orientation::from_exif_chunk)?;
    if orientation == Orientation::NoTransforms {
        return None;
    }

    let mut block = b"MM\x00\x2a\x00\x00\x00\x08\x00\x01".to_vec();
    block.extend_from_slice(&0x0112_u16.to_be_bytes()); // orientation tag
    block.extend_from_slice(&3_u16.to_be_bytes()); // SHORT
    block.extend_from_slice(&1_u32.to_be_bytes()); // one value
    block.extend_from_slice(&u16::from(orientation.to_exif()).to_be_bytes());
    block.extend_from_slice(&[0; 2]); // value padding
    block.extend_from_slice(&[0; 4]); // no next IFD
    Some(Bytes::from(block))
}

/// Finds where a JPEG's EOI marker ends, so that anything appended after the image (such as
/// the secondary images of an HDR photo, with their own metadata) can be cut off.
fn jpeg_end(bytes: &[u8]) -> Option<usize> {
    let mut pos = 2;
    loop {
        // decoders skip stray bytes between segments, so this does too
        if *bytes.get(pos)? != 0xFF {
            pos += 1;
            continue;
        }
        let marker = *bytes.get(pos + 1)?;
        pos += 2;
        match marker {
            0xFF => pos -= 1,
            markers::EOI => return Some(pos),
            0x01 | markers::RST0..=markers::RST7 => {}
            _ => {
                let length = u16::from_be_bytes([*bytes.get(pos)?, *bytes.get(pos + 1)?]);
                pos += usize::from(length);
                if marker == markers::SOS {
                    // entropy-coded data runs until a marker other than a stuffed byte or a restart
                    loop {
                        if *bytes.get(pos)? == 0xFF
                            && !matches!(*bytes.get(pos + 1)?, 0x00 | markers::RST0..=markers::RST7)
                        {
                            break;
                        }
                        pos += 1;
                    }
                }
            }
        }
    }
}

/// Strips a JPEG down to the segments needed to display it.
///
/// A JPEG without an end marker is kept whole, since nothing can follow an image that never ends.
pub fn strip_jpeg(mut bytes: Vec<u8>) -> Result<Vec<u8>, MetadataError> {
    if let Some(end) = jpeg_end(&bytes) {
        bytes.truncate(end);
    }
    let mut jpeg = Jpeg::from_bytes(Bytes::from(bytes))?;

    let icc = jpeg.icc_profile();
    let exif = orientation_exif(jpeg.exif().as_deref());

    // JFIF (APP0) and Adobe color transform (APP14) segments affect decoding, every other APPn
    // segment, including JFXX thumbnails, is metadata
    jpeg.segments_mut().retain(|segment| match segment.marker() {
        markers::APP0 => segment.contents().starts_with(b"JFIF\0"),
        markers::APP14 => true,
        marker => marker != markers::COM && !(markers::APP1..=markers::APP15).contains(&marker),
    });
    // JFIF segments can embed a thumbnail of the original image after their header
    for segment in jpeg.segments_mut() {
        if segment.marker() == markers::APP0 && segment.contents().len() > JFIF_HEADER_LEN {
            let mut header = segment.contents()[..JFIF_HEADER_LEN].to_vec();
            header.extend_from_slice(&[0, 0]);
            *segment = JpegSegment::new_with_contents(markers::APP0, Bytes::from(header));
        }
    }
    jpeg.set_icc_profile(icc);
    jpeg.set_exif(exif);

    Ok(jpeg.encoder().bytes().into())
}

/// Strips text, timestamp and EXIF chunks from a PNG or APNG.
pub fn strip_png(bytes: Vec<u8>) -> Result<Vec<u8>, MetadataError> {
    let mut png = Png::from_bytes(Bytes::from(bytes))?;

    let exif = orientation_exif(png.exif().as_deref());
    png.chunks_mut()
        .retain(|chunk| !PNG_METADATA_CHUNKS.contains(&chunk.kind()));
    png.set_exif(exif);

    Ok(png.encoder().bytes().into())
}

/// Strips EXIF and XMP chunks from a WebP.
pub fn strip_webp(bytes: Vec<u8>) -> Result<Vec<u8>, MetadataError> {
    let mut webp = WebP::from_bytes(Bytes::from(bytes))?;

    let exif = orientation_exif(webp.exif().as_deref());
    webp.remove_chunks_by_id(CHUNK_XMP);
    webp.set_exif(exif);

    // `set_exif` keeps the EXIF and ICC flags in sync but leaves the XMP flag as it was
    if let Some(vp8x) = webp.chunks_mut().iter_mut().find(|chunk| chunk.id() == CHUNK_VP8X)
        && let Some(data) = vp8x.content().data()
    {
        let mut data = data.to_vec();
        if let Some(flags) = data.first_mut() {
            *flags &= !WEBP_XMP_FLAG;
        }
        *vp8x = RiffChunk::new(CHUNK_VP8X, RiffContent::Data(Bytes::from(data)));
    }

    Ok(webp.encoder().bytes().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_end_skips_stuffed_bytes_and_restarts() {
        // SOI, SOS with a 2 byte header, entropy holding FF00 and RST0, EOI, trailing data
        let bytes = [
            0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0x12, 0xFF, 0x00, 0xFF, 0xD0, 0x34, 0xFF, 0xD9, 0xAB,
        ];
        assert_eq!(jpeg_end(&bytes), Some(14));
    }

    #[test]
    fn jpeg_end_skips_padding_between_segments() {
        // SOI, padding, SOS with a 2 byte header, entropy, EOI, trailing data
        let bytes = [0xFF, 0xD8, 0x00, 0x00, 0xFF, 0xDA, 0x00, 0x02, 0x12, 0xFF, 0xD9, 0xAB];
        assert_eq!(jpeg_end(&bytes), Some(11));
    }

    #[test]
    fn jpeg_end_rejects_truncated_input() {
        assert_eq!(jpeg_end(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0x12]), None);
    }

    #[test]
    fn orientation_exif_drops_upright_images() {
        let upright = orientation_exif(Some(
            b"MM\x00\x2a\x00\x00\x00\x08\x00\x01\x01\x12\x00\x03\x00\x00\x00\x01\x00\x01\x00\x00\x00\x00\x00\x00",
        ));
        assert_eq!(upright, None);
        assert_eq!(orientation_exif(None), None);
    }
}
