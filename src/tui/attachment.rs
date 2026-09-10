use std::{collections::HashMap, fs::File, io, io::Cursor, io::Write, path::Path, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageFormat, ImageReader, Limits};
use tempfile::{Builder, TempPath};

const MAX_SESSION_FILES: usize = 64;
const MAX_SESSION_BYTES: usize = 64 * 1024 * 1024;
const MAX_DECODED_ALLOCATION: u64 = 64 * 1024 * 1024;
const MAX_DIMENSION: u32 = 8_192;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = 14 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct TemporaryAttachment {
    path: TempPath,
}

impl TemporaryAttachment {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

/// Allocates an owned, private PNG path. Dropping the returned `TempPath`
/// removes it, including on encoder error paths.
pub(super) fn create_private_png() -> io::Result<(File, TempPath)> {
    create_private_image(".png")
}

fn create_private_image(suffix: &str) -> io::Result<(File, TempPath)> {
    // tempfile creates private files (0600 on Unix) and owns every failure path.
    Ok(Builder::new()
        .prefix("kit-image-")
        .suffix(suffix)
        .tempfile()?
        .into_parts())
}

pub(super) fn own_temp_path(path: TempPath) -> Arc<TemporaryAttachment> {
    Arc::new(TemporaryAttachment { path })
}

#[derive(Default)]
pub(super) struct RetainedAttachmentFiles {
    files: Vec<Arc<TemporaryAttachment>>,
    bytes: u64,
}

impl RetainedAttachmentFiles {
    pub(super) fn clear(&mut self) {
        self.files.clear();
        self.bytes = 0;
    }

    pub(super) fn extend(&mut self, files: impl IntoIterator<Item = Arc<TemporaryAttachment>>) {
        for file in files {
            let Ok(metadata) = file.path().metadata() else {
                continue;
            };
            let bytes = metadata.len();
            if self.files.len() >= MAX_SESSION_FILES
                || bytes > (MAX_SESSION_BYTES as u64).saturating_sub(self.bytes)
            {
                continue;
            }
            self.bytes += bytes;
            self.files.push(file);
        }
    }
}

pub(super) struct MaterializedImage {
    key: [u8; 32],
    file: Arc<TemporaryAttachment>,
    pub(super) bytes: usize,
}

/// Validates and materializes one bounded inline image. This performs decoding
/// and filesystem I/O, so callers must run it outside the terminal event loop.
pub(super) fn materialize_image(
    key: [u8; 32],
    encoded: &str,
    mime_type: &str,
    max_bytes: usize,
) -> Option<MaterializedImage> {
    if encoded.len() > MAX_ENCODED_BYTES {
        return None;
    }
    let bytes = STANDARD.decode(encoded).ok()?;
    if bytes.len() > MAX_IMAGE_BYTES || bytes.len() > max_bytes {
        return None;
    }
    let (format, suffix) = match mime_type {
        "image/png" => (ImageFormat::Png, ".png"),
        "image/jpeg" => (ImageFormat::Jpeg, ".jpg"),
        "image/gif" => (ImageFormat::Gif, ".gif"),
        "image/webp" => (ImageFormat::WebP, ".webp"),
        _ => return None,
    };
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_ALLOCATION);
    reader.limits(limits);
    drop(reader.decode().ok()?);
    let (mut file, path) = create_private_image(suffix).ok()?;
    file.write_all(&bytes).ok()?;
    file.flush().ok()?;
    let size = bytes.len();
    drop(file);
    Some(MaterializedImage {
        key,
        file: own_temp_path(path),
        bytes: size,
    })
}

#[derive(Default)]
pub(super) struct SessionAttachmentCache {
    files: HashMap<[u8; 32], Arc<TemporaryAttachment>>,
    bytes: usize,
}

impl SessionAttachmentCache {
    pub(super) fn clear(&mut self) {
        self.files.clear();
        self.bytes = 0;
    }

    /// Admits a worker-produced file without evicting existing session links.
    pub(super) fn admit(&mut self, image: MaterializedImage) {
        if self.files.contains_key(&image.key)
            || self.files.len() >= MAX_SESSION_FILES
            || image.bytes > MAX_SESSION_BYTES.saturating_sub(self.bytes)
        {
            return;
        }
        self.bytes += image.bytes;
        self.files.insert(image.key, image.file);
    }

    pub(super) fn image_uri(&self, key: [u8; 32]) -> Option<String> {
        let file = self.files.get(&key)?;
        url::Url::from_file_path(file.path())
            .ok()
            .map(|uri| uri.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
mod tests {
    use super::*;

    fn source(format: ImageFormat, color: u8) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([color, 40, 60]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, format)
            .unwrap();
        bytes.into_inner()
    }

    fn cached_path(
        cache: &mut SessionAttachmentCache,
        bytes: &[u8],
        mime: &str,
    ) -> Option<std::path::PathBuf> {
        let key = *blake3::hash(bytes).as_bytes();
        cache.admit(materialize_image(
            key,
            &STANDARD.encode(bytes),
            mime,
            MAX_IMAGE_BYTES,
        )?);
        let uri = cache.image_uri(key)?;
        url::Url::parse(&uri).ok()?.to_file_path().ok()
    }

    #[test]
    fn cache_preserves_image_encoding_reuses_files_and_cleans_up() {
        let mut cache = SessionAttachmentCache::default();
        for (format, mime, extension) in [
            (ImageFormat::Png, "image/png", "png"),
            (ImageFormat::Jpeg, "image/jpeg", "jpg"),
            (ImageFormat::Gif, "image/gif", "gif"),
            (ImageFormat::WebP, "image/webp", "webp"),
        ] {
            let bytes = source(format, 20);
            let path = cached_path(&mut cache, &bytes, mime).unwrap();
            assert_eq!(path.extension().unwrap(), extension);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(cached_path(&mut cache, &bytes, mime).unwrap(), path);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(path.metadata().unwrap().permissions().mode() & 0o077, 0);
            }
            cache.clear();
            assert!(!path.exists());
        }
    }

    #[test]
    fn cache_admission_is_bounded_without_evicting_existing_links() {
        let mut cache = SessionAttachmentCache::default();
        let mut paths = Vec::new();
        for color in 0..MAX_SESSION_FILES {
            let bytes = source(ImageFormat::Png, color as u8);
            paths.push(cached_path(&mut cache, &bytes, "image/png").unwrap());
        }
        let extra = source(ImageFormat::Png, MAX_SESSION_FILES as u8);
        assert!(cached_path(&mut cache, &extra, "image/png").is_none());
        assert!(paths.iter().all(|path| path.exists()));
        drop(cache);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn invalid_or_oversized_sources_do_not_become_image_links() {
        let mut cache = SessionAttachmentCache::default();
        assert!(cached_path(&mut cache, b"not a PNG", "image/png").is_none());
        assert!(
            materialize_image(
                [0; 32],
                &"A".repeat(MAX_ENCODED_BYTES + 1),
                "image/png",
                MAX_IMAGE_BYTES,
            )
            .is_none()
        );
        let png = source(ImageFormat::Png, 20);
        assert!(
            materialize_image([1; 32], &STANDARD.encode(&png), "image/png", png.len() - 1,)
                .is_none()
        );
        assert!(cached_path(&mut cache, &png, "image/jpeg").is_none());
        assert!(cached_path(&mut cache, &png, "image/svg+xml").is_none());
    }
}
