//! Corkboard image processing library.

use image::{
    Frame, ImageFormat, ImageReader, ImageReaderOptions,
    codecs::{png::PngDecoder, webp::WebPDecoder},
    error::UnsupportedError,
};
use num_integer::Integer;

pub use png::EncodingError as PngEncodingError;

pub use image::ImageError as ImageDecodingError;

pub use metadata::MetadataError;

mod heif;
mod metadata;

/// Largest number of pixels in a stored image, enough for the 48MP photos of recent phones.
pub const MAX_PIXELS: u64 = 50_000_000;

/// Largest AVIF image returned as its own thumbnail, about the size of a PNG thumbnail.
pub const MAX_AVIF_THUMBNAIL_BYTES: usize = 1024 * 1024;

/// An image ready to be stored and served as is.
pub struct StoredImage {
    /// The encoded image.
    pub bytes: Vec<u8>,
    /// The MIME type of `bytes`.
    pub content_type: &'static str,
}

/// Reasons an uploaded image can't be stored.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The image format is unsupported or the image is corrupted.
    #[error("failed to decode image, please use a supported format: {0}")]
    Decoding(#[from] ImageDecodingError),
    /// The image's container could not be parsed to remove its metadata.
    #[error("failed to read image metadata: {0}")]
    Metadata(#[from] MetadataError),
    /// The image is too large to be shown to players.
    #[error("image is {width}x{height}, which is more than {MAX_PIXELS} pixels")]
    TooManyPixels {
        /// Width of the image.
        width: u32,
        /// Height of the image.
        height: u32,
    },
    /// Converting the image to PNG failed.
    #[error("failed to encode image: {0}")]
    Encoding(#[from] PngEncodingError),
    /// The image is in a format browsers other than Safari can't display, which can't be converted.
    #[error("{0} images aren't supported, please convert them to JPEG or PNG")]
    UnsupportedFormat(&'static str),
}

impl StorageError {
    /// The HTTP status code to answer an upload that failed with this error.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Decoding(_) | Self::Metadata(_) => 400,
            Self::TooManyPixels { .. } => 413,
            Self::UnsupportedFormat(_) => 415,
            Self::Encoding(_) => 500,
        }
    }
}

/// Rejects dimensions with more than [`MAX_PIXELS`] pixels.
fn check_pixels((width, height): (u32, u32)) -> Result<(), StorageError> {
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(StorageError::TooManyPixels { width, height });
    }
    Ok(())
}

/// Tells whether `bytes` start like a JPEG XL codestream or container.
fn is_jpeg_xl(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0x0A]) || bytes.starts_with(b"\0\0\0\x0CJXL \r\n\x87\n")
}

/// Builds an error describing an image whose frames could not be read.
fn unsupported_image() -> ImageDecodingError {
    ImageDecodingError::Unsupported(UnsupportedError::from_format_and_kind(
        image::error::ImageFormatHint::Unknown,
        image::error::UnsupportedErrorKind::Format(image::error::ImageFormatHint::Unknown),
    ))
}

/// Reads an image from bytes and returns its frames.
///
/// # Errors
///
/// Returns an [`ImageDecodingError`] if the image format is unsupported, decoding fails, or the
/// image contains no frames.
pub fn read_image_as_frames(bytes: &[u8]) -> Result<Vec<Frame>, ImageDecodingError> {
    let options = ImageReaderOptions::new(std::io::Cursor::new(bytes)).with_guessed_format()?;

    let frames = match options.format().ok_or_else(unsupported_image)? {
        ImageFormat::Png => {
            let mut decoder = PngDecoder::new(options.into_inner());
            if decoder.is_apng()? {
                ImageReader::from_decoder(Box::new(decoder.apng()?))
                    .into_frames()
                    .collect_frames()?
            } else {
                let image = ImageReader::from_decoder(Box::new(decoder)).decode()?.0;
                vec![Frame::new(image.into_rgba8())]
            }
        }
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(options.into_inner())?;
            let is_animated = decoder.has_animation();
            let mut reader = ImageReader::from_decoder(Box::new(decoder));
            if is_animated {
                reader.into_frames().collect_frames()?
            } else {
                vec![Frame::new(reader.decode()?.0.into_rgba8())]
            }
        }
        ImageFormat::Gif => options.into_reader()?.into_frames().collect_frames()?,
        _ => {
            let image = options.into_reader()?.decode()?.0;
            vec![Frame::new(image.into_rgba8())]
        }
    };

    if frames.is_empty() {
        return Err(unsupported_image());
    }

    Ok(frames)
}

/// Encodes a sequence of frames as an animated PNG.
///
/// # Errors
///
/// Returns a [`PngEncodingError`] if encoding fails.
pub fn encode_frames_as_png(frames: Vec<Frame>) -> Result<Vec<u8>, PngEncodingError> {
    let mut output: Vec<u8> = Vec::new();

    let (width, height) = frames[0].buffer().dimensions();

    let mut encoder = png::Encoder::new(&mut output, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // ten times faster than the default for files about 8% larger
    encoder.set_compression(png::Compression::Fast);
    encoder.set_animated(frames.len() as u32, 0)?; // 0 = loop forever

    let mut writer = encoder.write_header()?;

    for frame in frames {
        let buf = frame.buffer();

        let (num_ms, denom_ms) = frame.delay().numer_denom_ms();
        let (num_sec, denom_sec) = (num_ms, denom_ms * 1000);
        let gcd = num_sec.gcd(&denom_sec);
        let num_sec_simple = num_sec / gcd;
        let denom_sec_simple = denom_sec / gcd;

        writer.set_frame_delay(num_sec_simple as u16, denom_sec_simple as u16)?;
        writer.set_frame_position(frame.left(), frame.top())?;

        writer.write_image_data(buf.as_raw())?;
    }

    writer.finish()?;

    Ok(output)
}

/// Prepares an uploaded image for storage.
///
/// Formats every browser displays (JPEG, PNG, GIF, WebP and still AVIF) are kept as they are,
/// minus any metadata other than the color profile and orientation. HEIC and JPEG XL are
/// rejected, since only Safari displays them. Other formats are converted to PNG. Only the header
/// is read for kept formats, so their image data isn't validated.
///
/// # Errors
///
/// Returns a [`StorageError`] if the image format is unsupported, the image is corrupted, or it
/// has more than [`MAX_PIXELS`] pixels.
pub fn prepare_for_storage(bytes: Vec<u8>) -> Result<StoredImage, StorageError> {
    match heif::kind(&bytes) {
        Some(heif::HeifKind::Avif) => {
            let (bytes, dimensions) = heif::strip_avif(bytes)?;
            check_pixels(dimensions)?;
            return Ok(StoredImage {
                bytes,
                content_type: "image/avif",
            });
        }
        Some(heif::HeifKind::AvifSequence) => return Err(StorageError::UnsupportedFormat("animated AVIF")),
        Some(heif::HeifKind::Heic) => return Err(StorageError::UnsupportedFormat("HEIC")),
        None if is_jpeg_xl(&bytes) => return Err(StorageError::UnsupportedFormat("JPEG XL")),
        None => {}
    }

    let format = {
        let options = ImageReaderOptions::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(ImageDecodingError::IoError)?;
        let format = options.format().ok_or_else(unsupported_image)?;
        check_pixels(options.into_dimensions()?)?;
        format
    };

    let (bytes, content_type) = match format {
        ImageFormat::Jpeg => (metadata::strip_jpeg(bytes)?, "image/jpeg"),
        ImageFormat::Png => (metadata::strip_png(bytes)?, "image/png"),
        ImageFormat::WebP => (metadata::strip_webp(bytes)?, "image/webp"),
        ImageFormat::Gif => (bytes, "image/gif"),
        _ => (encode_frames_as_png(read_image_as_frames(&bytes)?)?, "image/png"),
    };

    Ok(StoredImage { bytes, content_type })
}

/// Computes a PNG thumbnail of an image from bytes.
///
/// Still AVIF images can't be decoded, so a small one is returned as is, without its metadata.
///
/// # Errors
///
/// Returns a [`StorageError`] if the image format is unsupported, decoding fails, or an AVIF image
/// is larger than [`MAX_AVIF_THUMBNAIL_BYTES`].
pub fn generate_thumbnail(data: &[u8]) -> Result<StoredImage, StorageError> {
    if heif::kind(data) == Some(heif::HeifKind::Avif) {
        if data.len() > MAX_AVIF_THUMBNAIL_BYTES {
            return Err(StorageError::UnsupportedFormat("large AVIF"));
        }
        return Ok(StoredImage {
            bytes: heif::strip_avif(data.to_vec())?.0,
            content_type: "image/avif",
        });
    }

    let mut decoded_image = image::ImageReaderOptions::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(ImageDecodingError::IoError)?
        .into_reader()?
        .decode()?
        .0;

    decoded_image.resize(400, 400, image::imageops::FilterType::Nearest);

    let mut thumbnail_bytes: Vec<u8> = Vec::new();
    decoded_image.write_to(&mut std::io::Cursor::new(&mut thumbnail_bytes), image::ImageFormat::Png)?;

    Ok(StoredImage {
        bytes: thumbnail_bytes,
        content_type: "image/png",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, RgbaImage};
    use img_parts::{
        Bytes, ImageEXIF, ImageICC,
        jpeg::{Jpeg, JpegSegment, markers},
        png::{Png, PngChunk},
        riff::{RiffChunk, RiffContent},
        webp::{CHUNK_VP8X, CHUNK_XMP, WebP},
    };
    use std::time::Duration;

    /// Bit of the first VP8X flags byte that marks a WebP as carrying XMP.
    const XMP_FLAG: u8 = 0b0000_0100;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(width, height, image::Rgba(color))
    }

    fn frame(color: [u8; 4], millis: u64) -> Frame {
        Frame::from_parts(
            solid(16, 16, color),
            0,
            0,
            Delay::from_saturating_duration(Duration::from_millis(millis)),
        )
    }

    fn encode_static(image: &RgbaImage, format: ImageFormat) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut std::io::Cursor::new(&mut bytes), format)
            .unwrap();
        bytes
    }

    fn encode_gif(frames: Vec<Frame>) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::gif::GifEncoder::new(std::io::Cursor::new(&mut bytes));
        encoder.encode_frames(frames).unwrap();
        drop(encoder);
        bytes
    }

    #[test]
    fn static_png_reads_as_one_frame() {
        let bytes = encode_static(&solid(16, 16, [255, 0, 0, 255]), ImageFormat::Png);
        let frames = read_image_as_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].buffer().dimensions(), (16, 16));
        assert_eq!(frames[0].buffer().get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn static_jpeg_reads_as_one_frame() {
        let bytes = encode_static(&solid(16, 16, [0, 0, 255, 255]), ImageFormat::Jpeg);
        let frames = read_image_as_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].buffer().dimensions(), (16, 16));
    }

    #[test]
    fn static_webp_reads_as_one_frame() {
        let bytes = encode_static(&solid(16, 16, [0, 255, 255, 255]), ImageFormat::WebP);
        let frames = read_image_as_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].buffer().dimensions(), (16, 16));
    }

    #[test]
    fn animated_webp_reads_every_frame() {
        let bytes = include_bytes!("../tests/images/animated.webp");
        let frames = read_image_as_frames(bytes).unwrap();
        assert_eq!(frames.len(), 6);
        for frame in &frames {
            assert_eq!(frame.buffer().dimensions(), (200, 200));
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            assert_eq!(numerator / denominator, 100);
        }
    }

    #[test]
    fn animated_webp_converts_to_animated_png() {
        let frames = read_image_as_frames(include_bytes!("../tests/images/animated.webp")).unwrap();
        let apng = encode_frames_as_png(frames).unwrap();
        assert_eq!(&apng[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(read_image_as_frames(&apng).unwrap().len(), 6);
    }

    #[test]
    fn static_tiff_reads_as_one_frame() {
        let bytes = encode_static(&solid(16, 16, [7, 8, 9, 255]), ImageFormat::Tiff);
        let frames = read_image_as_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn animated_gif_reads_every_frame() {
        let bytes = encode_gif(vec![
            frame([255, 0, 0, 255], 100),
            frame([0, 255, 0, 255], 100),
            frame([0, 0, 255, 255], 100),
        ]);
        let frames = read_image_as_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 3);
        for frame in &frames {
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            assert_eq!(numerator / denominator, 100);
        }
    }

    #[test]
    fn apng_reads_every_frame() {
        let bytes = encode_frames_as_png(vec![frame([255, 0, 0, 255], 50), frame([0, 255, 0, 255], 50)]).unwrap();
        let frames = read_image_as_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].buffer().get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(frames[1].buffer().get_pixel(0, 0).0, [0, 255, 0, 255]);
    }

    #[test]
    fn gif_converts_to_animated_png() {
        let gif = encode_gif(vec![frame([255, 0, 0, 255], 100), frame([0, 255, 0, 255], 100)]);
        let apng = encode_frames_as_png(read_image_as_frames(&gif).unwrap()).unwrap();
        assert_eq!(&apng[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(read_image_as_frames(&apng).unwrap().len(), 2);
    }

    #[test]
    fn thumbnail_scales_to_fit_and_encodes_png() {
        let bytes = encode_static(&solid(1000, 500, [12, 34, 56, 255]), ImageFormat::Png);
        let thumbnail = generate_thumbnail(&bytes).unwrap();
        assert_eq!(thumbnail.content_type, "image/png");
        assert_eq!(&thumbnail.bytes[..8], b"\x89PNG\r\n\x1a\n");
        let decoded = image::load_from_memory(&thumbnail.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (400, 200));
    }

    #[test]
    fn thumbnail_accepts_jpeg() {
        let bytes = encode_static(&solid(1000, 500, [30, 60, 90, 255]), ImageFormat::Jpeg);
        let thumbnail = generate_thumbnail(&bytes).unwrap();
        let decoded = image::load_from_memory(&thumbnail.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (400, 200));
    }

    #[test]
    fn garbage_bytes_are_rejected() {
        assert!(read_image_as_frames(b"not an image at all").is_err());
        assert!(generate_thumbnail(b"not an image at all").is_err());
    }

    #[test]
    fn empty_bytes_are_rejected() {
        assert!(read_image_as_frames(&[]).is_err());
        assert!(generate_thumbnail(&[]).is_err());
    }

    /// EXIF block holding an orientation and a GPS pointer, like a phone photo's.
    fn phone_exif(orientation: u16) -> Bytes {
        let mut block = b"MM\x00\x2a\x00\x00\x00\x08\x00\x02".to_vec();
        block.extend_from_slice(&[0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01]);
        block.extend_from_slice(&orientation.to_be_bytes());
        block.extend_from_slice(&[0x00, 0x00]);
        block.extend_from_slice(&[0x88, 0x25, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x26]);
        block.extend_from_slice(&[0x00; 4]);
        Bytes::from(block)
    }

    /// Asserts that `exif` holds nothing but a rotation by 90 degrees.
    fn assert_only_orientation(exif: Option<Bytes>) {
        let exif = exif.expect("orientation should be kept");
        assert_eq!(
            image::metadata::Orientation::from_exif_chunk(&exif),
            Some(image::metadata::Orientation::Rotate90)
        );
        assert!(!exif.windows(2).any(|tag| tag == [0x88, 0x25]), "GPS pointer was kept");
    }

    #[test]
    fn jpeg_is_kept_without_metadata() {
        let mut jpeg = Jpeg::from_bytes(Bytes::from(encode_static(
            &solid(32, 16, [9, 9, 9, 255]),
            ImageFormat::Jpeg,
        )))
        .unwrap();
        jpeg.set_exif(Some(phone_exif(6)));
        jpeg.set_icc_profile(Some(Bytes::from_static(b"color profile")));
        jpeg.segments_mut().insert(
            1,
            JpegSegment::new_with_contents(markers::COM, Bytes::from_static(b"comment")),
        );
        jpeg.segments_mut().insert(
            1,
            JpegSegment::new_with_contents(
                markers::APP1,
                Bytes::from_static(b"http://ns.adobe.com/xap/1.0/\0<gps/>"),
            ),
        );
        let mut bytes = jpeg.encoder().bytes().to_vec();
        bytes.extend_from_slice(b"appended image");

        let stored = prepare_for_storage(bytes).unwrap();
        assert_eq!(stored.content_type, "image/jpeg");
        assert!(!stored.bytes.ends_with(b"appended image"));
        assert!(!stored.bytes.windows(5).any(|w| w == b"<gps/"), "XMP was kept");

        let stored = Jpeg::from_bytes(Bytes::from(stored.bytes)).unwrap();
        assert_only_orientation(stored.exif());
        assert_eq!(stored.icc_profile().as_deref(), Some(&b"color profile"[..]));
        assert!(stored.segment_by_marker(markers::COM).is_none());
        assert_eq!(stored.segments_by_marker(markers::APP1).count(), 1);
    }

    #[test]
    fn jpeg_thumbnails_and_appended_data_are_dropped() {
        let mut jpeg = Jpeg::from_bytes(Bytes::from(encode_static(
            &solid(16, 16, [9, 9, 9, 255]),
            ImageFormat::Jpeg,
        )))
        .unwrap();
        // JFIF with a 1x1 thumbnail, then a JFXX thumbnail extension
        jpeg.segments_mut()[0] = JpegSegment::new_with_contents(
            markers::APP0,
            Bytes::from_static(b"JFIF\0\x01\x02\0\0\x01\0\x01\x01\x01\xAA\xBB\xCC"),
        );
        jpeg.segments_mut().insert(
            1,
            JpegSegment::new_with_contents(markers::APP0, Bytes::from_static(b"JFXX\0\x10thumbnail")),
        );
        let after_app0: usize = 2 + jpeg.segments()[..2].iter().map(JpegSegment::len).sum::<usize>();
        let encoded = jpeg.encoder().bytes();
        // padding between segments, which decoders skip
        let mut bytes = [&encoded[..after_app0], &[0, 0], &encoded[after_app0..]].concat();
        bytes.extend_from_slice(b"appended image");

        let stored = prepare_for_storage(bytes).unwrap();
        assert!(!stored.bytes.ends_with(b"appended image"));
        let stored = Jpeg::from_bytes(Bytes::from(stored.bytes)).unwrap();
        let app0: Vec<_> = stored.segments_by_marker(markers::APP0).collect();
        assert_eq!(app0.len(), 1);
        assert_eq!(app0[0].contents().as_ref(), b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0");
    }

    #[test]
    fn upright_jpeg_has_no_exif() {
        let mut jpeg = Jpeg::from_bytes(Bytes::from(encode_static(
            &solid(16, 16, [9, 9, 9, 255]),
            ImageFormat::Jpeg,
        )))
        .unwrap();
        jpeg.set_exif(Some(phone_exif(1)));

        let stored = prepare_for_storage(jpeg.encoder().bytes().into()).unwrap();
        assert!(Jpeg::from_bytes(Bytes::from(stored.bytes)).unwrap().exif().is_none());
    }

    #[test]
    fn png_is_kept_without_metadata() {
        let mut png = Png::from_bytes(Bytes::from(encode_static(
            &solid(32, 16, [9, 9, 9, 255]),
            ImageFormat::Png,
        )))
        .unwrap();
        png.set_exif(Some(phone_exif(6)));
        png.chunks_mut()
            .insert(1, PngChunk::new(*b"tEXt", Bytes::from_static(b"Author\0someone")));

        let stored = prepare_for_storage(png.encoder().bytes().into()).unwrap();
        assert_eq!(stored.content_type, "image/png");

        let stored = Png::from_bytes(Bytes::from(stored.bytes)).unwrap();
        assert_only_orientation(stored.exif());
        assert!(stored.chunk_by_type(*b"tEXt").is_none());
    }

    #[test]
    fn apng_keeps_every_frame() {
        let apng = encode_frames_as_png(vec![frame([255, 0, 0, 255], 50), frame([0, 255, 0, 255], 50)]).unwrap();
        let stored = prepare_for_storage(apng).unwrap();
        assert_eq!(stored.content_type, "image/png");
        assert_eq!(read_image_as_frames(&stored.bytes).unwrap().len(), 2);
    }

    #[test]
    fn webp_is_kept_without_metadata() {
        let mut webp = WebP::from_bytes(Bytes::from(encode_static(
            &solid(32, 16, [9, 9, 9, 255]),
            ImageFormat::WebP,
        )))
        .unwrap();
        webp.set_exif(Some(phone_exif(6)));
        webp.chunks_mut().push(RiffChunk::new(
            CHUNK_XMP,
            RiffContent::Data(Bytes::from_static(b"<gps/>")),
        ));
        let vp8x = webp
            .chunks_mut()
            .iter_mut()
            .find(|chunk| chunk.id() == CHUNK_VP8X)
            .unwrap();
        let mut flags = vp8x.content().data().unwrap().to_vec();
        flags[0] |= XMP_FLAG;
        *vp8x = RiffChunk::new(CHUNK_VP8X, RiffContent::Data(Bytes::from(flags)));

        let stored = prepare_for_storage(webp.encoder().bytes().into()).unwrap();
        assert_eq!(stored.content_type, "image/webp");
        assert_eq!(
            read_image_as_frames(&stored.bytes).unwrap()[0]
                .buffer()
                .get_pixel(0, 0)
                .0,
            [9, 9, 9, 255]
        );

        let stored = WebP::from_bytes(Bytes::from(stored.bytes)).unwrap();
        assert_only_orientation(stored.exif());
        assert!(!stored.has_chunk(CHUNK_XMP));
        assert_eq!(
            stored.chunk_by_id(CHUNK_VP8X).unwrap().content().data().unwrap()[0] & XMP_FLAG,
            0
        );
    }

    #[test]
    fn animated_webp_is_kept() {
        let bytes = include_bytes!("../tests/images/animated.webp");
        let stored = prepare_for_storage(bytes.to_vec()).unwrap();
        assert_eq!(stored.content_type, "image/webp");
        assert_eq!(read_image_as_frames(&stored.bytes).unwrap().len(), 6);
    }

    #[test]
    fn gif_is_kept_as_is() {
        let gif = encode_gif(vec![frame([255, 0, 0, 255], 100), frame([0, 255, 0, 255], 100)]);
        let stored = prepare_for_storage(gif.clone()).unwrap();
        assert_eq!(stored.content_type, "image/gif");
        assert_eq!(stored.bytes, gif);
    }

    #[test]
    fn tiff_is_converted_to_png() {
        let tiff = encode_static(&solid(16, 16, [7, 8, 9, 255]), ImageFormat::Tiff);
        let stored = prepare_for_storage(tiff).unwrap();
        assert_eq!(stored.content_type, "image/png");
        assert_eq!(&stored.bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn huge_images_are_rejected() {
        let mut png = Png::from_bytes(Bytes::from(encode_static(
            &solid(16, 16, [9, 9, 9, 255]),
            ImageFormat::Png,
        )))
        .unwrap();
        let mut header = png.chunk_by_type(*b"IHDR").unwrap().contents().to_vec();
        header[..4].copy_from_slice(&10_000_u32.to_be_bytes());
        header[4..8].copy_from_slice(&6_000_u32.to_be_bytes());
        png.chunks_mut()[0] = PngChunk::new(*b"IHDR", Bytes::from(header));

        assert!(matches!(
            prepare_for_storage(png.encoder().bytes().into()),
            Err(StorageError::TooManyPixels {
                width: 10_000,
                height: 6_000
            })
        ));
    }

    #[test]
    fn avif_is_kept_without_metadata() {
        let original = include_bytes!("../tests/images/metadata.avif");
        let stored = prepare_for_storage(original.to_vec()).unwrap();
        assert_eq!(stored.content_type, "image/avif");
        assert_eq!(stored.bytes.len(), original.len());
        assert!(!stored.bytes.windows(8).any(|w| w == b"40,44.9N"), "XMP was kept");
        assert!(
            !stored.bytes.windows(2).any(|w| w == [0x88, 0x25]),
            "GPS pointer was kept"
        );
        assert!(
            stored.bytes.windows(8).any(|w| w == b"\0\0\0\0MM\0\x2A"),
            "EXIF is no longer valid"
        );
    }

    #[test]
    fn huge_avif_is_rejected() {
        let mut avif = include_bytes!("../tests/images/metadata.avif").to_vec();
        let ispe = avif.windows(4).position(|w| w == b"ispe").unwrap() + 8;
        avif[ispe..ispe + 4].copy_from_slice(&10_000_u32.to_be_bytes());
        avif[ispe + 4..ispe + 8].copy_from_slice(&6_000_u32.to_be_bytes());

        assert!(matches!(
            prepare_for_storage(avif),
            Err(StorageError::TooManyPixels {
                width: 10_000,
                height: 6_000
            })
        ));
    }

    #[test]
    fn animated_avif_is_rejected() {
        assert!(matches!(
            prepare_for_storage(include_bytes!("../tests/images/animated.avif").to_vec()),
            Err(StorageError::UnsupportedFormat("animated AVIF"))
        ));
    }

    #[test]
    fn avif_thumbnail_is_the_stripped_image() {
        let original = include_bytes!("../tests/images/metadata.avif");
        let thumbnail = generate_thumbnail(original).unwrap();
        assert_eq!(thumbnail.content_type, "image/avif");
        assert_eq!(thumbnail.bytes, prepare_for_storage(original.to_vec()).unwrap().bytes);
    }

    #[test]
    fn heic_and_jpeg_xl_are_rejected() {
        assert!(matches!(
            prepare_for_storage(include_bytes!("../tests/images/still.heic").to_vec()),
            Err(StorageError::UnsupportedFormat("HEIC"))
        ));
        assert!(matches!(
            prepare_for_storage(vec![0xFF, 0x0A, 0xFA, 0x7F]),
            Err(StorageError::UnsupportedFormat("JPEG XL"))
        ));
        assert!(matches!(
            prepare_for_storage(b"\0\0\0\x0CJXL \r\n\x87\n\0\0\0\x14ftypjxl ".to_vec()),
            Err(StorageError::UnsupportedFormat("JPEG XL"))
        ));
    }

    #[test]
    fn garbage_is_not_stored() {
        assert!(prepare_for_storage(b"not an image at all".to_vec()).is_err());
        assert!(prepare_for_storage(Vec::new()).is_err());
    }
}
