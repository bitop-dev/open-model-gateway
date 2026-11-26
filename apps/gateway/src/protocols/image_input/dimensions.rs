//! An image's pixel dimensions, read from its header (#24), so a route whose
//! provider caps them refuses the image before admission, with the field
//! named, instead of the provider refusing it after.
//!
//! Best effort and bounded: every read is a checked slice of the decoded
//! bytes, nothing allocates, and the JPEG walk advances by at least two bytes
//! per segment. A header this does not understand, or one that states a
//! zero dimension, gives `None`, and the provider decides.
use crate::inference::types::ImageType;

/// Width and height in pixels, when the header states them.
pub(crate) fn dimensions(kind: ImageType, bytes: &[u8]) -> Option<(u32, u32)> {
    let (width, height) = match kind {
        ImageType::Png => png(bytes)?,
        ImageType::Gif => (le16(bytes, 6)?, le16(bytes, 8)?),
        ImageType::Webp => webp(bytes)?,
        ImageType::Jpeg => jpeg(bytes)?,
    };
    (width > 0 && height > 0).then_some((width, height))
}

fn take<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}
fn be16(bytes: &[u8], at: usize) -> Option<u32> {
    take::<2>(bytes, at).map(|b| u32::from(u16::from_be_bytes(b)))
}
fn le16(bytes: &[u8], at: usize) -> Option<u32> {
    take::<2>(bytes, at).map(|b| u32::from(u16::from_le_bytes(b)))
}
fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    take::<3>(bytes, at).map(|[a, b, c]| u32::from_le_bytes([a, b, c, 0]))
}

/// The signature, then the first chunk, which must be `IHDR`: width and
/// height as big-endian 32-bit values.
fn png(bytes: &[u8]) -> Option<(u32, u32)> {
    (bytes.get(12..16)? == b"IHDR").then_some(())?;
    let width = u32::from_be_bytes(take(bytes, 16)?);
    let height = u32::from_be_bytes(take(bytes, 20)?);
    Some((width, height))
}

/// `RIFF....WEBP`, then the first chunk: lossy `VP8 `, lossless `VP8L` or
/// extended `VP8X` (the canvas).
fn webp(bytes: &[u8]) -> Option<(u32, u32)> {
    match bytes.get(12..16)? {
        b"VP8 " => {
            // Frame tag (3 bytes), then the start code 9d 01 2a.
            (bytes.get(23..26)? == [0x9d, 0x01, 0x2a]).then_some(())?;
            Some((le16(bytes, 26)? & 0x3fff, le16(bytes, 28)? & 0x3fff))
        }
        b"VP8L" => {
            (*bytes.get(20)? == 0x2f).then_some(())?;
            let bits = u32::from_le_bytes(take(bytes, 21)?);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => Some((le24(bytes, 24)? + 1, le24(bytes, 27)? + 1)),
        _ => None,
    }
}

/// The first start-of-frame segment's height and width. Segments are walked
/// by their stated lengths from just after the SOI marker; a scan or the end
/// of the image before any frame header gives `None`.
fn jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    loop {
        if *bytes.get(at)? != 0xff {
            return None;
        }
        let marker = *bytes.get(at + 1)?;
        match marker {
            // Fill byte before a marker.
            0xff => at += 1,
            // Markers without a length.
            0x01 | 0xd0..=0xd8 => at += 2,
            // Start of scan or end of image: no frame header came first.
            0xd9 | 0xda => return None,
            // Start of frame (every SOFn; C4, C8 and CC are other segments).
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                return Some((be16(bytes, at + 7)?, be16(bytes, at + 5)?));
            }
            _ => {
                let length = usize::try_from(be16(bytes, at + 2)?).ok()?;
                if length < 2 {
                    return None;
                }
                at = at.checked_add(2 + length)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b
    }

    fn webp(chunk: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
        b.extend_from_slice(chunk);
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(payload);
        b
    }

    #[test]
    fn each_format_states_its_dimensions() {
        assert_eq!(
            dimensions(ImageType::Png, &png_header(8001, 20)),
            Some((8001, 20))
        );
        let gif = [b"GIF89a".as_slice(), &[0x41, 0x1f, 0x02, 0x00]].concat();
        assert_eq!(dimensions(ImageType::Gif, &gif), Some((8001, 2)));
        // Lossy: frame tag, start code, 14-bit width and height.
        let lossy = webp(
            b"VP8 ",
            &[0, 0, 0, 0x9d, 0x01, 0x2a, 0x41, 0x1f, 0x10, 0x00],
        );
        assert_eq!(dimensions(ImageType::Webp, &lossy), Some((8001, 16)));
        // Lossless: signature, then width-1 and height-1 in 14 bits each.
        let bits: u32 = 8000 | (99 << 14);
        let lossless = webp(b"VP8L", &[[0x2f].as_slice(), &bits.to_le_bytes()].concat());
        assert_eq!(dimensions(ImageType::Webp, &lossless), Some((8001, 100)));
        // Extended: flags and reserved, then canvas width-1 and height-1.
        let extended = webp(b"VP8X", &[0, 0, 0, 0, 0x40, 0x1f, 0x00, 0x09, 0x00, 0x00]);
        assert_eq!(dimensions(ImageType::Webp, &extended), Some((8001, 10)));
        // JPEG: SOI, an APP0 segment, fill bytes, then a progressive SOF2.
        let jpeg = [
            [0xff, 0xd8].as_slice(),
            &[0xff, 0xe0, 0x00, 0x04, 0xaa, 0xbb],
            &[0xff, 0xff],
            &[0xff, 0xc2, 0x00, 0x11, 0x08, 0x00, 0x30, 0x1f, 0x41, 0x03],
        ]
        .concat();
        assert_eq!(dimensions(ImageType::Jpeg, &jpeg), Some((8001, 48)));
    }

    #[test]
    fn headers_that_state_no_dimension_give_none() {
        // Truncated, a zero dimension, another first chunk, a scan before
        // any frame, a DHT (C4) segment that is not a frame header, and a
        // segment length below its own two bytes.
        assert_eq!(dimensions(ImageType::Png, &png_header(1, 1)[..20]), None);
        assert_eq!(dimensions(ImageType::Png, &png_header(0, 5)), None);
        let mut other = png_header(1, 1);
        other[12..16].copy_from_slice(b"tEXt");
        assert_eq!(dimensions(ImageType::Png, &other), None);
        assert_eq!(dimensions(ImageType::Webp, &webp(b"ALPH", &[0; 10])), None);
        for jpeg in [
            vec![0xff, 0xd8, 0xff, 0xda, 0x00, 0x08],
            vec![0xff, 0xd8, 0xff, 0xc4, 0x00, 0x02, 0xff, 0xd9],
            vec![0xff, 0xd8, 0xff, 0xe1, 0x00, 0x01, 0xff, 0xc0],
            vec![0xff, 0xd8, 0x00],
            vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00],
        ] {
            assert_eq!(dimensions(ImageType::Jpeg, &jpeg), None, "{jpeg:x?}");
        }
    }

    proptest! {
        #[test]
        fn any_bytes_are_read_without_panicking(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            for kind in ImageType::ALL {
                let _ = dimensions(kind, &bytes);
                let jpeg = [[0xff, 0xd8].as_slice(), &bytes].concat();
                let _ = dimensions(kind, &jpeg);
            }
        }
    }
}
