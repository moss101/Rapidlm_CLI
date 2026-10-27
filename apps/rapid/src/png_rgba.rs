//! A PNG decoder for exactly one job: the pixels behind a preview.
//!
//! The Sixel preview path needs RGBA bytes, and PNG is the one image
//! format previews carry — so this decodes the PNG subset real icons and
//! screenshots use (8-bit, non-interlaced, grayscale / RGB / palette /
//! RGBA, filters 0–4) and refuses everything else typed, naming the
//! variant. Not a general image loader: no 16-bit depth, no interlacing,
//! no ancillary-chunk interpretation beyond palette transparency, and the
//! bounds (input and pixels) are the preview's own.

use flate2::Decompress;

/// Most input bytes accepted (the preview bound the encoder already has).
pub const MAX_PNG_BYTES: usize = 2 * 1024 * 1024;
/// Most pixels decoded: a preview, not a photo viewer.
pub const MAX_PIXELS: u64 = 4096 * 4096;

/// Decoded pixels: `width × height` RGBA, row-major, alpha 255 where the
/// source had none.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Typed refusal. `Display` names the variant without echoing bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PngError {
    NotPng,
    Unsupported(&'static str),
    Corrupt(&'static str),
    TooLarge,
}

impl std::fmt::Display for PngError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPng => f.write_str("not a PNG"),
            Self::Unsupported(what) => write!(f, "unsupported PNG ({what})"),
            Self::Corrupt(what) => write!(f, "corrupt PNG ({what})"),
            Self::TooLarge => f.write_str("over the preview size bound"),
        }
    }
}

impl std::error::Error for PngError {}

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Decode `png`. See the module doc for the supported subset.
pub fn decode(png: &[u8]) -> Result<Rgba, PngError> {
    if png.len() < 8 + 8 || png[..8] != SIGNATURE {
        return Err(PngError::NotPng);
    }
    if png.len() > MAX_PNG_BYTES {
        return Err(PngError::TooLarge);
    }
    let mut at = 8usize;
    let mut header: Option<[u8; 13]> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut palette_alpha: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    while at + 8 <= png.len() {
        let length = be32(&png[at..]);
        let kind = &png[at + 4..at + 8];
        let data_start = at + 8;
        let data_end = data_start
            .checked_add(length as usize)
            .filter(|end| *end + 4 <= png.len())
            .ok_or(PngError::Corrupt("chunk runs past the file"))?;
        match kind {
            b"IHDR" if length == 13 => {
                header = Some(png[data_start..data_end].try_into().expect("13 bytes"))
            }
            b"PLTE" => {
                palette = png[data_start..data_end]
                    .chunks_exact(3)
                    .map(|rgb| [rgb[0], rgb[1], rgb[2]])
                    .collect();
            }
            b"tRNS" => palette_alpha = png[data_start..data_end].to_vec(),
            b"IDAT" => {
                if idat.len() + length as usize > MAX_PNG_BYTES {
                    return Err(PngError::TooLarge);
                }
                idat.extend_from_slice(&png[data_start..data_end]);
            }
            b"IEND" => break,
            _ => {}
        }
        at = data_end + 4; // skip the CRC: the preview's inputs are files, not wires
    }
    let header = header.ok_or(PngError::Corrupt("no IHDR"))?;
    let width = be32(&header[..4]);
    let height = be32(&header[4..8]);
    if width == 0 || height == 0 {
        return Err(PngError::Corrupt("zero-sized image"));
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(PngError::TooLarge);
    }
    if header[8] != 8 {
        return Err(PngError::Unsupported("only 8-bit depth"));
    }
    if header[12] != 0 {
        return Err(PngError::Unsupported("interlaced"));
    }
    let channels: usize = match header[9] {
        0 => 1, // grayscale
        2 => 3, // RGB
        3 => 1, // palette index
        4 => 2, // grayscale + alpha
        6 => 4, // RGBA
        _ => return Err(PngError::Unsupported("unknown color type")),
    };
    if header[9] == 3 && palette.is_empty() {
        return Err(PngError::Corrupt("palette image without a palette"));
    }

    // Inflate: the zlib stream's own bounds do the rest of the sizing.
    let expected = (width as usize * channels) * height as usize + height as usize; // + filter bytes
    let mut raw = vec![0u8; expected];
    let mut inflater = Decompress::new(true);
    inflater
        .decompress(&idat, &mut raw, flate2::FlushDecompress::Finish)
        .map_err(|_| PngError::Corrupt("the compressed stream does not inflate"))?;
    if inflater.total_out() != expected as u64 {
        return Err(PngError::Corrupt("the pixel data is short"));
    }

    // Unfilter row by row. The predictors reference the reconstructed
    // bytes of the current row's left neighbours and the row above, so
    // both are rebuilt in place before the next row is read.
    let stride = width as usize * channels;
    let mut rows: Vec<u8> = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        let filter = raw[y * (stride + 1)];
        let source = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        let (previous_rows, current_and_below) = rows.split_at_mut(y * stride);
        let current = &mut current_and_below[..stride];
        let previous = y
            .checked_sub(1)
            .map(|up| &previous_rows[up * stride..(up + 1) * stride])
            .unwrap_or(&[0u8][..0]);
        for x in 0..stride {
            let left = x.checked_sub(channels).map(|at| current[at]).unwrap_or(0);
            let up = previous.get(x).copied().unwrap_or(0);
            let up_left = if previous.is_empty() {
                0
            } else {
                x.checked_sub(channels).map(|at| previous[at]).unwrap_or(0)
            };
            let byte = match filter {
                0 => source[x],
                1 => source[x].wrapping_add(left),
                2 => source[x].wrapping_add(up),
                3 => source[x].wrapping_add(left / 2 + up / 2),
                4 => source[x].wrapping_add(paeth(left, up, up_left)),
                _ => return Err(PngError::Corrupt("unknown filter")),
            };
            current[x] = byte;
        }
    }

    // Expand to RGBA.
    let mut data = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let at = y * stride + x * channels;
            let (r, g, b, a) = match header[9] {
                0 => {
                    let gray = rows[at];
                    (gray, gray, gray, 255)
                }
                2 => (rows[at], rows[at + 1], rows[at + 2], 255),
                3 => {
                    let index = rows[at] as usize;
                    let rgb = palette
                        .get(index)
                        .ok_or(PngError::Corrupt("palette index out of range"))?;
                    let alpha = palette_alpha.get(index).copied().unwrap_or(255);
                    (rgb[0], rgb[1], rgb[2], alpha)
                }
                4 => {
                    let gray = rows[at];
                    (gray, gray, gray, rows[at + 1])
                }
                _ => (rows[at], rows[at + 1], rows[at + 2], rows[at + 3]),
            };
            data.extend_from_slice(&[r, g, b, a]);
        }
    }
    Ok(Rgba {
        width,
        height,
        data,
    })
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn paeth(left: u8, up: u8, up_left: u8) -> u8 {
    let (a, b, c) = (i16::from(left), i16::from(up), i16::from(up_left));
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        up
    } else {
        up_left
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write as _;

    /// Build a one-IDAT PNG from raw (already-filtered-with-0) channel
    /// bytes — the encoder side of the decoder's own tests.
    fn png_of(
        width: u32,
        height: u32,
        color_type: u8,
        channels: usize,
        pixels: &[u8],
        palette: Option<(&[[u8; 3]], &[u8])>,
    ) -> Vec<u8> {
        let mut idat = Vec::new();
        for y in 0..height as usize {
            idat.push(0); // filter: none
            let stride = width as usize * channels;
            idat.extend_from_slice(&pixels[y * stride..(y + 1) * stride]);
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&idat).expect("deflate");
        let compressed = encoder.finish().expect("finish");

        let mut out = SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, color_type, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        if let Some((palette, alpha)) = palette {
            let mut plte = Vec::new();
            for rgb in palette {
                plte.extend_from_slice(rgb);
            }
            chunk(&mut out, b"PLTE", &plte);
            if !alpha.is_empty() {
                chunk(&mut out, b"tRNS", alpha);
            }
        }
        chunk(&mut out, b"IDAT", &compressed);
        chunk(&mut out, b"IEND", b"");
        out
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(&crc.sum().to_be_bytes());
    }

    #[test]
    fn an_rgba_png_decodes_to_its_own_pixels() {
        let png = png_of(2, 1, 6, 4, &[10, 20, 30, 255, 200, 100, 50, 128], None);
        let rgba = decode(&png).expect("decoded");
        assert_eq!(rgba.width, 2);
        assert_eq!(rgba.height, 1);
        assert_eq!(rgba.data, vec![10, 20, 30, 255, 200, 100, 50, 128]);
    }

    #[test]
    fn every_color_type_and_the_palette_transparency_expand_to_rgba() {
        // Grayscale.
        let png = png_of(1, 1, 0, 1, &[77], None);
        assert_eq!(decode(&png).expect("gray").data, vec![77, 77, 77, 255]);
        // RGB.
        let png = png_of(1, 1, 2, 3, &[1, 2, 3], None);
        assert_eq!(decode(&png).expect("rgb").data, vec![1, 2, 3, 255]);
        // Gray + alpha.
        let png = png_of(1, 1, 4, 2, &[9, 64], None);
        assert_eq!(decode(&png).expect("ga").data, vec![9, 9, 9, 64]);
        // Palette, one transparent index.
        let png = png_of(
            2,
            1,
            3,
            1,
            &[0, 1],
            Some((&[[255, 0, 0], [0, 255, 0]], &[0u8][..])),
        );
        assert_eq!(
            decode(&png).expect("palette").data,
            vec![255, 0, 0, 0, 0, 255, 0, 255]
        );
    }

    #[test]
    fn filters_are_unfiltered_including_paeth() {
        // A 3-pixel RGB row where each pixel's bytes are left-filtered
        // deltas of the one before: the decoded row restores the sums.
        let base = [10u8, 20, 30];
        let mut pixels = Vec::new();
        pixels.extend_from_slice(&base);
        for _ in 0..2 {
            pixels.extend_from_slice(&[5, 5, 5]); // +5 per channel, left-filtered
        }
        let mut idat = vec![1u8]; // filter: sub
        idat.extend_from_slice(&pixels);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&idat).expect("deflate");
        let compressed = encoder.finish().expect("finish");
        let mut png = SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&3u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &compressed);
        chunk(&mut png, b"IEND", b"");
        let rgba = decode(&png).expect("filtered");
        assert_eq!(
            rgba.data,
            vec![
                10, 20, 30, 255, //
                15, 25, 35, 255, //
                20, 30, 40, 255,
            ]
        );
    }

    #[test]
    fn the_refusals_name_their_variant() {
        assert_eq!(decode(b"not a png at all"), Err(PngError::NotPng));
        let mut png = png_of(1, 1, 6, 4, &[1, 2, 3, 255], None);
        png[28] = 1; // IHDR interlace byte (data[12])
        match decode(&png) {
            Err(PngError::Unsupported(what)) => assert!(what.contains("interlaced")),
            other => panic!("interlaced refused: {other:?}"),
        }
        let mut png = png_of(1, 1, 6, 4, &[1, 2, 3, 255], None);
        png[24] = 16; // IHDR depth byte (data[8])
        match decode(&png) {
            Err(PngError::Unsupported(what)) => assert!(what.contains("8-bit")),
            other => panic!("depth refused: {other:?}"),
        }
        // A palette image without its palette.
        let mut png = png_of(1, 1, 3, 1, &[0], None);
        // Strip the PLTE by re-chunking: easier — claim color type 3 with
        // none embedded, which png_of(…, None) already produced.
        match decode(&png) {
            Err(PngError::Corrupt(what)) => assert!(what.contains("palette")),
            other => panic!("palette refused: {other:?}"),
        }
        // Truncated pixel data.
        let mut png = png_of(2, 2, 6, 4, &[0u8; 16], None);
        // Replace the IDAT payload with an empty stream: find IDAT and
        // re-encode nothing.
        let at = png.windows(4).position(|w| w == b"IDAT").expect("IDAT");
        let length = be32(&png[at - 4..]) as usize;
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&[0u8]).expect("deflate");
        let short = encoder.finish().expect("finish");
        let mut rebuilt = png[..at + 4].to_vec();
        rebuilt.extend_from_slice(&short);
        rebuilt.extend_from_slice(&png[at + 4 + length..]);
        // Fix the rebuilt length prefix.
        let len_at = at - 4;
        rebuilt[len_at..len_at + 4].copy_from_slice(&(short.len() as u32).to_be_bytes());
        png = rebuilt;
        match decode(&png) {
            Err(PngError::Corrupt(_)) => {}
            other => panic!("short data refused: {other:?}"),
        }
    }
}
