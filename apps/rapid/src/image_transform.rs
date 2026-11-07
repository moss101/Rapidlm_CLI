//! Bounded image crops and deterministic model-sized encodings.
use std::io::Cursor;

use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits};

const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PIXELS: u64 = 16_000_000;
const MAX_EDGE: u32 = 8192;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Options {
    pub region: Option<Region>,
    pub full_resolution: bool,
    pub max_edge: u32,
    pub byte_budget: usize,
}

#[derive(Debug)]
pub(crate) struct Output {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub original: (u32, u32),
    pub region: Region,
    pub dimensions: (u32, u32),
}

/// Recognize a large edge without decoding pixels. Invalid headers are left
/// to the ordinary image read path unless a transform was explicitly asked.
pub(crate) fn exceeds_default_edge(bytes: &[u8]) -> bool {
    let Ok(format) = image::guess_format(bytes) else {
        return false;
    };
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return false;
    }
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    reader
        .into_dimensions()
        .is_ok_and(|(width, height)| width.max(height) > 2000)
}

/// Transform one static PNG, JPEG or WebP image. Failure messages contain no
/// decoder details or source contents and are safe to show to the model.
pub(crate) fn transform(
    bytes: &[u8],
    options: Options,
    cancel: &agent_runtime::CancellationToken,
) -> Result<Output, &'static str> {
    if cancel.is_cancelled() {
        return Err("image transformation cancelled");
    }
    if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
        return Err("image is empty or exceeds the 8 MiB input limit");
    }
    if options.max_edge == 0
        || options.max_edge > MAX_EDGE
        || options.byte_budget == 0
        || options.byte_budget > crate::file_refs::MAX_IMAGE_BYTES
    {
        return Err("invalid image size or byte budget");
    }
    let format = image::guess_format(bytes).map_err(|_| "unsupported or malformed image")?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Err("image transforms support static PNG, JPEG, and WebP only");
    }
    if format == ImageFormat::Png && is_animated_png(bytes)? {
        return Err("animated PNG cannot be cropped or resized");
    }
    if format == ImageFormat::WebP {
        let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
            .map_err(|_| "unsupported or malformed image")?;
        if decoder.has_animation() {
            return Err("animated WebP cannot be cropped or resized");
        }
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| "image exceeds decode limits or is malformed")?;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODE_BYTES
    {
        return Err("image exceeds decode limits");
    }
    let orientation = decoder
        .orientation()
        .map_err(|_| "invalid image orientation")?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| "could not decode image")?;
    if cancel.is_cancelled() {
        return Err("image transformation cancelled");
    }
    image.apply_orientation(orientation);
    let original = image.dimensions();
    let requested = options.region.unwrap_or(Region {
        x: 0,
        y: 0,
        width: original.0,
        height: original.1,
    });
    if requested.width == 0
        || requested.height == 0
        || requested.x >= original.0
        || requested.y >= original.1
    {
        return Err("image region is outside the original image");
    }
    let region = Region {
        x: requested.x,
        y: requested.y,
        width: requested.width.min(original.0 - requested.x),
        height: requested.height.min(original.1 - requested.y),
    };
    image = image.crop_imm(region.x, region.y, region.width, region.height);
    let jpeg = format == ImageFormat::Jpeg;
    if !options.full_resolution {
        image = fit(image, options.max_edge);
    }
    let edges = [2000, 1000, 768, 512, 384, 256, 128, 64, 32, 16, 8, 4, 2, 1];
    loop {
        if cancel.is_cancelled() {
            return Err("image transformation cancelled");
        }
        if jpeg {
            let qualities: &[u8] = if options.full_resolution {
                &[90]
            } else {
                &[90, 80, 60, 40, 20]
            };
            for &quality in qualities {
                if cancel.is_cancelled() {
                    return Err("image transformation cancelled");
                }
                let mut encoded = Vec::new();
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
                    .encode_image(&image)
                    .map_err(|_| "could not encode image")?;
                if encoded.len() <= options.byte_budget {
                    return Ok(Output {
                        bytes: encoded,
                        mime: "image/jpeg",
                        original,
                        region,
                        dimensions: image.dimensions(),
                    });
                }
            }
        } else {
            let mut encoded = Cursor::new(Vec::new());
            image
                .write_to(&mut encoded, ImageFormat::Png)
                .map_err(|_| "could not encode image")?;
            let encoded = encoded.into_inner();
            if encoded.len() <= options.byte_budget {
                return Ok(Output {
                    bytes: encoded,
                    mime: "image/png",
                    original,
                    region,
                    dimensions: image.dimensions(),
                });
            }
        }
        if options.full_resolution {
            return Err(
                "full-resolution image exceeds the byte budget; choose a smaller region or allow resizing",
            );
        }
        let longest = image.width().max(image.height());
        let Some(edge) = edges.iter().copied().find(|edge| *edge < longest) else {
            return Err("image cannot be encoded within the byte budget");
        };
        image = fit(image, edge);
    }
}

fn fit(image: DynamicImage, edge: u32) -> DynamicImage {
    if image.width().max(image.height()) <= edge {
        image
    } else {
        image.resize(edge, edge, image::imageops::FilterType::Lanczos3)
    }
}

fn is_animated_png(bytes: &[u8]) -> Result<bool, &'static str> {
    let mut offset = 8usize;
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset.saturating_add(8))
            .ok_or("malformed PNG")?;
        let length =
            u32::from_be_bytes(header[..4].try_into().map_err(|_| "malformed PNG")?) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|value| value.checked_add(length))
            .ok_or("malformed PNG")?;
        if end > bytes.len() {
            return Err("malformed PNG");
        }
        if &header[4..8] == b"acTL" {
            return Ok(true);
        }
        if &header[4..8] == b"IDAT" || &header[4..8] == b"IEND" {
            return Ok(false);
        }
        offset = end;
    }
    Err("malformed PNG")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn fixture(format: ImageFormat) -> Vec<u8> {
        let image =
            RgbaImage::from_fn(4, 3, |x, y| Rgba([(x * 40) as u8, (y * 60) as u8, 17, 255]));
        let mut output = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut output, format)
            .unwrap();
        output.into_inner()
    }

    fn options() -> Options {
        Options {
            region: None,
            full_resolution: false,
            max_edge: 2000,
            byte_budget: crate::file_refs::MAX_IMAGE_BYTES,
        }
    }

    fn run(bytes: &[u8], options: Options) -> Result<Output, &'static str> {
        transform(bytes, options, &agent_runtime::CancellationToken::new())
    }

    #[test]
    fn png_crop_preserves_the_requested_pixels() {
        let input = fixture(ImageFormat::Png);
        let output = run(
            &input,
            Options {
                region: Some(Region {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                }),
                full_resolution: true,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(output.original, (4, 3));
        assert_eq!(output.dimensions, (2, 2));
        assert_eq!(output.mime, "image/png");
        let decoded = image::load_from_memory(&output.bytes).unwrap().to_rgba8();
        assert_eq!(decoded.get_pixel(0, 0), &Rgba([40, 60, 17, 255]));
        assert_eq!(decoded.get_pixel(1, 1), &Rgba([80, 120, 17, 255]));
        let clipped = run(
            &input,
            Options {
                region: Some(Region {
                    x: 3,
                    y: 2,
                    width: u32::MAX,
                    height: u32::MAX,
                }),
                full_resolution: true,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(clipped.dimensions, (1, 1));
        assert_eq!((clipped.region.width, clipped.region.height), (1, 1));
    }

    #[test]
    fn static_webp_and_jpeg_can_be_cropped_and_resized() {
        for format in [ImageFormat::WebP, ImageFormat::Jpeg] {
            let input = fixture(format);
            let output = run(
                &input,
                Options {
                    max_edge: 2,
                    ..options()
                },
            )
            .unwrap();
            assert!(output.dimensions.0 <= 2 && output.dimensions.1 <= 2);
            assert!(image::load_from_memory(&output.bytes).is_ok());
            assert_eq!(
                output.mime,
                if format == ImageFormat::Jpeg {
                    "image/jpeg"
                } else {
                    "image/png"
                }
            );
        }
    }

    #[test]
    fn jpeg_and_webp_crops_contain_the_selected_region() {
        let pixels = RgbaImage::from_fn(64, 64, |x, y| match (x >= 32, y >= 32) {
            (false, false) => Rgba([240, 10, 10, 255]),
            (true, false) => Rgba([10, 240, 10, 255]),
            (false, true) => Rgba([10, 10, 240, 255]),
            (true, true) => Rgba([240, 240, 10, 255]),
        });
        for format in [ImageFormat::Jpeg, ImageFormat::WebP] {
            let mut input = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(pixels.clone())
                .write_to(&mut input, format)
                .unwrap();
            let output = run(
                input.get_ref(),
                Options {
                    region: Some(Region {
                        x: 32,
                        y: 32,
                        width: 32,
                        height: 32,
                    }),
                    full_resolution: true,
                    ..options()
                },
            )
            .unwrap();
            let decoded = image::load_from_memory(&output.bytes).unwrap().to_rgba8();
            assert_eq!(decoded.dimensions(), (32, 32));
            let pixel = decoded.get_pixel(16, 16).0;
            assert!(
                pixel[0] > 220 && pixel[1] > 220 && pixel[2] < 30 && pixel[3] == 255,
                "{format:?}: {pixel:?}"
            );
        }
    }

    #[test]
    fn jpeg_orientation_is_applied_before_region_coordinates() {
        let jpeg = fixture(ImageFormat::Jpeg);
        // Exif TIFF orientation 6 rotates the original 4x3 image to 3x4.
        let exif = [
            b'E', b'x', b'i', b'f', 0, 0, b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 1, 3, 0, 1, 0,
            0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
        ];
        let length = (exif.len() + 2) as u16;
        let mut oriented = vec![0xff, 0xd8, 0xff, 0xe1];
        oriented.extend_from_slice(&length.to_be_bytes());
        oriented.extend_from_slice(&exif);
        oriented.extend_from_slice(&jpeg[2..]);
        let output = run(
            &oriented,
            Options {
                region: Some(Region {
                    x: 2,
                    y: 3,
                    width: 1,
                    height: 1,
                }),
                full_resolution: true,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(output.original, (3, 4));
        assert_eq!(output.dimensions, (1, 1));
    }

    #[test]
    fn oversized_png_is_resized_to_the_byte_budget_without_losing_alpha() {
        let mut seed = 0x1234_5678_u32;
        let pixels = RgbaImage::from_fn(1024, 1024, |_, _| {
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            };
            Rgba([next(), next(), next(), next()])
        });
        let mut input = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels)
            .write_to(&mut input, ImageFormat::Png)
            .unwrap();
        assert!(input.get_ref().len() > crate::file_refs::MAX_IMAGE_BYTES);
        let output = run(input.get_ref(), options()).unwrap();
        assert!(output.bytes.len() <= crate::file_refs::MAX_IMAGE_BYTES);
        assert!(output.dimensions.0 < 1024);
        assert!(
            image::load_from_memory(&output.bytes)
                .unwrap()
                .color()
                .has_alpha()
        );
    }

    #[test]
    fn bad_regions_budget_and_animated_formats_fail_explicitly() {
        use base64::Engine as _;
        let input = fixture(ImageFormat::Png);
        assert!(
            run(
                &input,
                Options {
                    region: Some(Region {
                        x: 4,
                        y: 0,
                        width: 1,
                        height: 1
                    }),
                    ..options()
                }
            )
            .is_err()
        );
        assert!(
            run(
                &input,
                Options {
                    region: Some(Region {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 1
                    }),
                    ..options()
                }
            )
            .is_err()
        );
        assert!(
            run(
                &input,
                Options {
                    full_resolution: true,
                    byte_budget: 1,
                    ..options()
                }
            )
            .unwrap_err()
            .contains("byte budget")
        );
        assert!(run(&vec![0; MAX_INPUT_BYTES + 1], options()).is_err());
        assert!(run(b"GIF89a", options()).is_err());
        // Two-frame 2x2 lossless WebP fixture; transforming it must not
        // silently discard the animation.
        let animated_webp = base64::engine::general_purpose::STANDARD.decode(
            "UklGRoQAAABXRUJQVlA4WAoAAAACAAAAAQAAAQAAQU5JTQYAAAAAAAAAAABBTk1GKAAAAAAAAAAAAAEAAAEAAGQAAAJWUDhMDwAAAC8BQAAABxD9j/4HIqL/AQBBTk1GKAAAAAAAAAAAAAEAAAEAAGQAAABWUDhMDwAAAC8BQAAABxDR//4HIqL/AQA="
        ).unwrap();
        assert!(
            run(&animated_webp, options())
                .unwrap_err()
                .contains("animated WebP")
        );
        let animated_png = [
            b"\x89PNG\r\n\x1a\n".as_slice(),
            &[0, 0, 0, 0],
            b"acTL",
            &[0, 0, 0, 0],
        ]
        .concat();
        assert!(
            run(&animated_png, options())
                .unwrap_err()
                .contains("animated PNG")
        );
        let cancelled = agent_runtime::CancellationToken::new();
        cancelled.cancel();
        assert!(
            transform(&input, options(), &cancelled)
                .unwrap_err()
                .contains("cancelled")
        );
    }
}
