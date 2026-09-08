//! Geometry uses encoded channel values, not a color-managed conversion.
//! Allocation audit (image 0.25.10 / png 0.18): decode is limited to 64 MiB;
//! compressed PNG metadata is rejected before decoder construction because its
//! expansion is not covered by pixel limits. Other metadata is bounded by the
//! 8 MiB input. RGBA conversion/orientation hold at most two 64 MiB pixel buffers.
//! Resize converts to premultiplied RGBA32F, holding float source + destination
//! and the RGBA32F vertical intermediate and a small weight vector. Conversion
//! peaks at 20 bytes/pixel (RGBA8 + RGBA32F). The intermediate has a separate
//! 128 MiB ceiling; the float source is dropped before output conversion.
//! PNG's level compressor buffers the entire compressed stream before writing;
//! allow twice the raw scanline size for its Vec capacity, plus 8 MiB capped
//! output and 8 MiB codec/row overhead. These are live-work estimates, not RSS
//! guarantees. Encoded input and old geometry buffers are dropped before encode.
use super::*;
use image::{ImageEncoder as _, RgbaImage, imageops};
use std::io;

const MAX_SCRATCH_BYTES: u64 = 128 * 1024 * 1024;
const MAX_LIVE_BYTES: u64 = 256 * 1024 * 1024;
const CODEC_HEADROOM: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum Transform {
    Rotate {
        degrees: u32,
    },
    Crop {
        aspect_ratio: AspectRatio,
        anchor: Anchor,
    },
    Resize {
        width: u32,
        height: u32,
        fit: Fit,
    },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AspectRatio {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Anchor {
    Center,
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Fit {
    Contain,
    Cover,
    Stretch,
}

impl FileStore {
    pub(crate) fn transform(
        &self,
        session: &str,
        selected: &FileReference,
        transform: Transform,
        cancellation: Option<&TurnCancellation>,
    ) -> Result<FileReference> {
        check_cancelled(cancellation)?;
        let bytes = self.resolve(session, selected)?;
        check_cancelled(cancellation)?;
        let mut decoder = bounded_decoder(&bytes, image::guess_format(&bytes).map_err(display)?)?;
        let orientation = decoder.orientation().map_err(display)?;
        check_cancelled(cancellation)?;
        let decoded = DynamicImage::from_decoder(decoder).map_err(display)?;
        check_cancelled(cancellation)?;
        drop(bytes);
        // Convert before orientation to bound copies even for 16-bit input.
        let mut normalized = DynamicImage::ImageRgba8(decoded.into_rgba8());
        check_cancelled(cancellation)?;
        normalized.apply_orientation(orientation);
        check_cancelled(cancellation)?;
        let source = normalized.into_rgba8();
        let result = geometry(source, transform, cancellation)?;
        check_cancelled(cancellation)?;
        let (width, height) = result.dimensions();
        let scanlines = (u64::from(width) * 4 + 1) * u64::from(height);
        if u64::from(width) * u64::from(height) * 4
            + 2 * scanlines
            + MAX_FILE_BYTES
            + CODEC_HEADROOM
            > MAX_LIVE_BYTES
        {
            return Err("PNG encode exceeds live pixel work budget".into());
        }
        let mut output = CappedWriter {
            bytes: Vec::new(),
            cancellation,
        };
        // Explicit level compression avoids the fast codec's fallback keeping
        // both compressed and uncompressed streams alive simultaneously.
        image::codecs::png::PngEncoder::new_with_quality(
            &mut output,
            image::codecs::png::CompressionType::Level(6),
            image::codecs::png::FilterType::Adaptive,
        )
        .write_image(
            result.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(display)?;
        check_cancelled(cancellation)?;
        drop(result);
        self.publish(
            session,
            output.bytes,
            "transformed.png".into(),
            "image/png".into(),
            ImageDimensions { width, height },
            cancellation,
        )
    }

    /// Disk-only, create-new export. Successful sync is the commit point: never
    /// report cancellation after it. Failed files are retained, since unlinking
    /// a pathname after a write failure could remove somebody else's replacement.
    pub(crate) fn export(
        &self,
        session: &str,
        selected: &FileReference,
        path: &Path,
        cancellation: Option<&TurnCancellation>,
    ) -> Result<u64> {
        check_cancelled(cancellation)?;
        let bytes = self.resolve(session, selected)?;
        check_cancelled(cancellation)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|error| format!("export {}: {error}", path.display()))?;
        let write = || -> Result<()> {
            for chunk in bytes.chunks(64 * 1024) {
                check_cancelled(cancellation)?;
                file.write_all(chunk).map_err(display)?;
            }
            check_cancelled(cancellation)?;
            file.sync_all().map_err(display)
        };
        let mut write = write;
        write().map_err(|error| {
            format!(
                "export {}: {error}; partial-or-complete file retained at this path",
                path.display()
            )
        })?;
        Ok(bytes.len() as u64)
    }
}

fn dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err("image dimensions exceed nonzero 8192 / 16 megapixel limits".into());
    }
    Ok(())
}

fn inscribed(width: u32, height: u32, ratio: AspectRatio) -> Result<(u32, u32)> {
    if ratio.width == 0
        || ratio.height == 0
        || ratio.width > MAX_DIMENSION
        || ratio.height > MAX_DIMENSION
    {
        return Err("aspect ratio components must be positive integers at most 8192".into());
    }
    let (w, h) = if u64::from(width) * u64::from(ratio.height)
        > u64::from(height) * u64::from(ratio.width)
    {
        (
            (u64::from(height) * u64::from(ratio.width) / u64::from(ratio.height)) as u32,
            height,
        )
    } else {
        (
            width,
            (u64::from(width) * u64::from(ratio.height) / u64::from(ratio.width)) as u32,
        )
    };
    dimensions(w, h)?;
    Ok((w, h))
}

fn crop(source: RgbaImage, ratio: AspectRatio, anchor: Anchor) -> Result<RgbaImage> {
    let (width, height) = inscribed(source.width(), source.height(), ratio)?;
    let dx = source.width() - width;
    let dy = source.height() - height;
    let x = match anchor {
        Anchor::TopLeft | Anchor::Left | Anchor::BottomLeft => 0,
        Anchor::TopRight | Anchor::Right | Anchor::BottomRight => dx,
        _ => dx / 2,
    };
    let y = match anchor {
        Anchor::TopLeft | Anchor::Top | Anchor::TopRight => 0,
        Anchor::BottomLeft | Anchor::Bottom | Anchor::BottomRight => dy,
        _ => dy / 2,
    };
    Ok(imageops::crop_imm(&source, x, y, width, height).to_image())
}

fn geometry(
    source: RgbaImage,
    transform: Transform,
    cancellation: Option<&TurnCancellation>,
) -> Result<RgbaImage> {
    check_cancelled(cancellation)?;
    let result = match transform {
        Transform::Rotate { degrees } => match degrees {
            90 => imageops::rotate90(&source),
            180 => imageops::rotate180(&source),
            270 => imageops::rotate270(&source),
            _ => return Err("rotation must be 90, 180, or 270 degrees clockwise".into()),
        },
        Transform::Crop {
            aspect_ratio,
            anchor,
        } => crop(source, aspect_ratio, anchor)?,
        Transform::Resize { width, height, fit } => {
            dimensions(width, height)?;
            let (source, width, height) = match fit {
                Fit::Stretch => (source, width, height),
                Fit::Cover => {
                    let source = crop(source, AspectRatio { width, height }, Anchor::Center)?;
                    check_cancelled(cancellation)?;
                    (source, width, height)
                }
                Fit::Contain => {
                    let (width, height) = inscribed(
                        width,
                        height,
                        AspectRatio {
                            width: source.width(),
                            height: source.height(),
                        },
                    )?;
                    (source, width, height)
                }
            };
            let scratch = u64::from(source.width()) * u64::from(height) * 16;
            // Every dimension is already bounded by 8192, so these u64
            // products cannot overflow. Include both float conversion peaks
            // and all simultaneously live resize buffers before allocating.
            let source_pixels = u64::from(source.width()) * u64::from(source.height());
            let target_pixels = u64::from(width) * u64::from(height);
            let live = (source_pixels + target_pixels) * 16 + scratch + CODEC_HEADROOM;
            if scratch > MAX_SCRATCH_BYTES
                || live > MAX_LIVE_BYTES
                || source_pixels * 20 + CODEC_HEADROOM > MAX_LIVE_BYTES
                || target_pixels * 20 + CODEC_HEADROOM > MAX_LIVE_BYTES
            {
                return Err("resize exceeds scratch or live pixel work budget".into());
            }
            check_cancelled(cancellation)?;
            let mut source = DynamicImage::ImageRgba8(source).into_rgba32f();
            for row in source.rows_mut() {
                check_cancelled(cancellation)?;
                for pixel in row {
                    let alpha = pixel[3];
                    for channel in &mut pixel.0[..3] {
                        *channel *= alpha;
                    }
                }
            }
            check_cancelled(cancellation)?;
            let mut resized =
                imageops::resize(&source, width, height, imageops::FilterType::Triangle);
            drop(source);
            check_cancelled(cancellation)?;
            for row in resized.rows_mut() {
                check_cancelled(cancellation)?;
                for pixel in row {
                    let alpha = pixel[3];
                    for channel in &mut pixel.0[..3] {
                        *channel = if alpha > 0.0 {
                            (*channel / alpha).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                    }
                }
            }
            check_cancelled(cancellation)?;
            let mut result = DynamicImage::ImageRgba32F(resized).into_rgba8();
            for row in result.rows_mut() {
                check_cancelled(cancellation)?;
                for pixel in row {
                    if pixel[3] == 0 {
                        pixel.0[..3].fill(0);
                    }
                }
            }
            result
        }
    };
    check_cancelled(cancellation)?;
    Ok(result)
}

struct CappedWriter<'a> {
    bytes: Vec<u8>,
    cancellation: Option<&'a TurnCancellation>,
}
impl io::Write for CappedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_cancelled(self.cancellation).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_FILE_BYTES - self.bytes.len() as u64 {
            return Err(io::Error::other(
                "transformed PNG exceeds 8 MiB output limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        check_cancelled(self.cancellation).map_err(io::Error::other)
    }
}

/// Scan chunk envelopes without decompressing metadata. Reject compressed text
/// and profiles rather than relying on codecs' nonuniform metadata limits.
/// Import still preserves accepted ancillary chunks byte-for-byte.
pub(super) fn check_png_metadata(bytes: &[u8]) -> Result<()> {
    let mut offset = 8_usize;
    while offset < bytes.len() {
        let header = bytes.get(offset..offset + 8).ok_or("truncated PNG chunk")?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .ok_or("invalid PNG chunk length")?;
        if end > bytes.len() {
            return Err("truncated PNG chunk".into());
        }
        if matches!(&header[4..8], b"iCCP" | b"zTXt" | b"iTXt") {
            return Err("compressed or international PNG metadata is not supported".into());
        }
        // The decoder stops at IEND; preserve opaque trailing import bytes.
        if &header[4..8] == b"IEND" {
            break;
        }
        offset = end;
    }
    Ok(())
}
