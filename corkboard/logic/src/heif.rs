//! Reads HEIF containers (AVIF and HEIC) and removes metadata from AVIF images in place.
//!
//! EXIF and XMP are stored as items whose data lives elsewhere in the file. Overwriting that data
//! with an empty EXIF block or blank XMP removes the metadata without moving anything, so every
//! offset stays valid. AVIF stores orientation in its own properties rather than in EXIF, so the
//! image displays the same.

use crate::metadata::MetadataError;

/// Kinds of HEIF images, told apart by their brands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeifKind {
    /// AV1 still images, which every browser displays.
    Avif,
    /// Animated AV1 images, whose metadata lives in tracks rather than items.
    AvifSequence,
    /// HEVC and other HEIF images, which only Safari displays.
    Heic,
}

/// Brands of HEIF files that aren't AVIF.
const HEIC_BRANDS: [[u8; 4]; 8] = [
    *b"heic", *b"heix", *b"heim", *b"heis", *b"hevc", *b"hevx", *b"mif1", *b"msf1",
];

/// Top-level boxes an AVIF still image is made of.
const AVIF_TOP_LEVEL_BOXES: [[u8; 4]; 5] = [*b"ftyp", *b"meta", *b"mdat", *b"free", *b"skip"];

/// Content type of XMP items.
const XMP_CONTENT_TYPE: &[u8] = b"application/rdf+xml";

/// EXIF item payload holding no tags: the offset of the TIFF header, then a big-endian TIFF header
/// pointing at an IFD with no entries. Decoders such as libavif reject the image when the payload
/// isn't valid, so it can't simply be zeroed.
const EMPTY_EXIF: [u8; 18] = [0, 0, 0, 0, b'M', b'M', 0, 0x2A, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0];

/// Kinds of metadata items.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Metadata {
    Exif,
    Xmp,
}

impl Metadata {
    /// The empty form of this metadata, `len` bytes long.
    fn blank(self, len: usize) -> Vec<u8> {
        match self {
            Self::Exif => {
                let mut blank = EMPTY_EXIF.to_vec();
                blank.resize(len, 0);
                blank
            }
            Self::Xmp => vec![b' '; len],
        }
    }
}

/// Builds the error for a malformed AVIF.
fn malformed(reason: &'static str) -> MetadataError {
    MetadataError::Avif(reason)
}

/// Reads big-endian values from a byte slice.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], MetadataError> {
        let end = self.pos.checked_add(len).ok_or_else(|| malformed("truncated box"))?;
        let bytes = self.data.get(self.pos..end).ok_or_else(|| malformed("truncated box"))?;
        self.pos = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, MetadataError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, MetadataError> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into().unwrap_or_default()))
    }

    fn u32(&mut self) -> Result<u32, MetadataError> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().unwrap_or_default()))
    }

    fn fourcc(&mut self) -> Result<[u8; 4], MetadataError> {
        Ok(self.bytes(4)?.try_into().unwrap_or_default())
    }

    /// Reads an unsigned integer stored in `size` bytes, where `size` is 0, 4 or 8.
    fn sized(&mut self, size: u8) -> Result<u64, MetadataError> {
        match size {
            0 => Ok(0),
            4 => Ok(u64::from(self.u32()?)),
            8 => Ok(u64::from_be_bytes(self.bytes(8)?.try_into().unwrap_or_default())),
            _ => Err(malformed("invalid field size")),
        }
    }

    /// Reads a null-terminated string, without its terminator.
    fn cstring(&mut self) -> Result<&'a [u8], MetadataError> {
        let rest = self.data.get(self.pos..).unwrap_or_default();
        let len = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| malformed("unterminated string"))?;
        self.pos += len + 1;
        Ok(&rest[..len])
    }

    /// Reads the version and flags of a full box, returning the version.
    fn full_box_version(&mut self) -> Result<u8, MetadataError> {
        let version = self.u8()?;
        self.bytes(3)?;
        Ok(version)
    }
}

/// A box within a file, with the absolute range of its contents.
struct BoxRef {
    kind: [u8; 4],
    start: usize,
    end: usize,
}

/// Lists the boxes laid out back to back in `data[start..end]`.
fn boxes(data: &[u8], start: usize, end: usize) -> Result<Vec<BoxRef>, MetadataError> {
    let mut found = Vec::new();
    let mut pos = start;
    while pos < end {
        let mut reader = Reader::new(data.get(pos..end).ok_or_else(|| malformed("truncated box"))?);
        let size = reader.u32()?;
        let kind = reader.fourcc()?;
        let size = match size {
            0 => end - pos,
            1 => usize::try_from(reader.sized(8)?).map_err(|_| malformed("box too large"))?,
            size => size as usize,
        };
        let box_end = pos.checked_add(size).filter(|&e| e <= end && size >= reader.pos);
        let box_end = box_end.ok_or_else(|| malformed("box overflows its parent"))?;
        found.push(BoxRef {
            kind,
            start: pos + reader.pos,
            end: box_end,
        });
        pos = box_end;
    }
    Ok(found)
}

/// Tells HEIF files apart by the brands in their leading `ftyp` box.
pub fn kind(data: &[u8]) -> Option<HeifKind> {
    let mut reader = Reader::new(data);
    let size = reader.u32().ok()? as usize;
    if reader.fourcc().ok()? != *b"ftyp" || size < 16 || size > data.len() {
        return None;
    }
    let brands: Vec<[u8; 4]> = data[8..size]
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 1) // minor version
        .map(|(_, brand)| *brand)
        .collect();

    if brands.contains(b"avis") {
        Some(HeifKind::AvifSequence)
    } else if brands.contains(b"avif") {
        Some(HeifKind::Avif)
    } else if brands.iter().any(|brand| HEIC_BRANDS.contains(brand)) {
        Some(HeifKind::Heic)
    } else {
        None
    }
}

/// Where an item's data is stored.
struct Location {
    construction_method: u16,
    base_offset: u64,
    extents: Vec<(u64, u64)>,
}

/// Reads the `iinf` box, returning the IDs of items holding EXIF or XMP.
fn metadata_items(data: &[u8], iinf: &BoxRef) -> Result<Vec<(u32, Metadata)>, MetadataError> {
    let mut reader = Reader::new(&data[iinf.start..iinf.end]);
    let version = reader.full_box_version()?;
    if version == 0 {
        reader.u16()?;
    } else {
        reader.u32()?;
    }

    let mut items = Vec::new();
    for infe in boxes(data, iinf.start + reader.pos, iinf.end)? {
        if infe.kind != *b"infe" {
            continue;
        }
        let mut reader = Reader::new(&data[infe.start..infe.end]);
        let version = reader.full_box_version()?;
        let (id, item_type) = match version {
            0 | 1 => (u32::from(reader.u16()?), *b"mime"),
            2 => (u32::from(reader.u16()?), {
                reader.u16()?;
                reader.fourcc()?
            }),
            _ => (reader.u32()?, {
                reader.u16()?;
                reader.fourcc()?
            }),
        };
        if version < 2 {
            reader.u16()?;
        }
        reader.cstring()?; // item name

        let metadata = match &item_type {
            b"Exif" => Some(Metadata::Exif),
            b"mime" if reader.cstring()? == XMP_CONTENT_TYPE => Some(Metadata::Xmp),
            _ => None,
        };
        if let Some(metadata) = metadata {
            items.push((id, metadata));
        }
    }
    Ok(items)
}

/// Reads the `iloc` box, returning where each item's data is stored.
fn locations(data: &[u8], iloc: &BoxRef) -> Result<Vec<(u32, Location)>, MetadataError> {
    let mut reader = Reader::new(&data[iloc.start..iloc.end]);
    let version = reader.full_box_version()?;
    let sizes = reader.u8()?;
    let (offset_size, length_size) = (sizes >> 4, sizes & 0xF);
    let sizes = reader.u8()?;
    let (base_offset_size, index_size) = (sizes >> 4, if version == 0 { 0 } else { sizes & 0xF });
    let item_count = if version < 2 {
        u32::from(reader.u16()?)
    } else {
        reader.u32()?
    };

    let mut found = Vec::new();
    for _ in 0..item_count {
        let id = if version < 2 {
            u32::from(reader.u16()?)
        } else {
            reader.u32()?
        };
        let construction_method = if version == 0 { 0 } else { reader.u16()? & 0xF };
        reader.u16()?; // data reference index
        let base_offset = reader.sized(base_offset_size)?;
        let extent_count = reader.u16()?;
        let mut extents = Vec::with_capacity(usize::from(extent_count));
        for _ in 0..extent_count {
            reader.sized(index_size)?;
            extents.push((reader.sized(offset_size)?, reader.sized(length_size)?));
        }
        found.push((
            id,
            Location {
                construction_method,
                base_offset,
                extents,
            },
        ));
    }
    Ok(found)
}

/// Finds the largest dimensions among the `ispe` properties, which covers the primary image.
fn largest_dimensions(data: &[u8], iprp: &BoxRef) -> Result<(u32, u32), MetadataError> {
    let mut largest = None;
    for ipco in boxes(data, iprp.start, iprp.end)?.iter().filter(|b| b.kind == *b"ipco") {
        for ispe in boxes(data, ipco.start, ipco.end)?.iter().filter(|b| b.kind == *b"ispe") {
            let mut reader = Reader::new(&data[ispe.start..ispe.end]);
            reader.full_box_version()?;
            let dimensions = (reader.u32()?, reader.u32()?);
            let area = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
            if largest.is_none_or(|l| area(dimensions) > area(l)) {
                largest = Some(dimensions);
            }
        }
    }
    largest.ok_or_else(|| malformed("no image dimensions"))
}

/// Blanks out the EXIF and XMP data of an AVIF still image.
///
/// Returns the stripped image and its largest dimensions.
pub fn strip_avif(mut data: Vec<u8>) -> Result<(Vec<u8>, (u32, u32)), MetadataError> {
    let top = boxes(&data, 0, data.len())?;
    if let Some(other) = top.iter().find(|b| !AVIF_TOP_LEVEL_BOXES.contains(&b.kind)) {
        return Err(malformed(if other.kind == *b"moov" {
            "AVIF has tracks without being animated"
        } else {
            "unexpected top-level box"
        }));
    }

    let meta = top
        .iter()
        .find(|b| b.kind == *b"meta")
        .ok_or_else(|| malformed("no meta box"))?;
    let children = boxes(&data, meta.start + 4, meta.end)?;
    let child = |kind: &[u8; 4]| children.iter().find(|b| b.kind == *kind);

    let iprp = child(b"iprp").ok_or_else(|| malformed("no item properties"))?;
    let dimensions = largest_dimensions(&data, iprp)?;

    let metadata = match child(b"iinf") {
        Some(iinf) => metadata_items(&data, iinf)?,
        None => Vec::new(),
    };
    if metadata.is_empty() {
        return Ok((data, dimensions));
    }

    let iloc = child(b"iloc").ok_or_else(|| malformed("no item locations"))?;
    let locations = locations(&data, iloc)?;
    if metadata
        .iter()
        .any(|(id, _)| locations.iter().all(|(located, _)| located != id))
    {
        return Err(malformed("metadata item has no location"));
    }

    let idat_start = child(b"idat").map(|idat| idat.start as u64);
    let mut items = Vec::new();
    for (id, location) in locations {
        let Some(&(_, kind)) = metadata.iter().find(|(item, _)| *item == id) else {
            continue;
        };
        let base = match location.construction_method {
            0 => location.base_offset,
            1 => idat_start
                .ok_or_else(|| malformed("no idat box"))?
                .checked_add(location.base_offset)
                .ok_or_else(|| malformed("metadata item out of bounds"))?,
            _ => return Err(malformed("unsupported item construction method")),
        };
        let mut ranges = Vec::new();
        for (offset, length) in location.extents {
            let start = base.checked_add(offset).and_then(|s| usize::try_from(s).ok());
            let end = start.and_then(|s| s.checked_add(usize::try_from(length).ok()?));
            match (start, end) {
                (Some(start), Some(end)) if length > 0 && end <= data.len() => ranges.push(start..end),
                _ => return Err(malformed("metadata item out of bounds")),
            }
        }
        items.push((kind, ranges));
    }

    for (kind, ranges) in items {
        let mut blank = kind.blank(ranges.iter().map(ExactSizeIterator::len).sum()).into_iter();
        for range in ranges {
            for byte in &mut data[range] {
                *byte = blank.next().unwrap_or_default();
            }
        }
    }
    Ok((data, dimensions))
}
