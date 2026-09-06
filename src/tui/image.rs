use std::{collections::HashMap, io::Cursor, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageReader, Limits};
use ratatui::{
    Frame,
    layout::{Rect, Size},
};
use ratatui_image::{
    Resize,
    picker::{Picker, ProtocolType, cap_parser::QueryStdioOptions},
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

use super::app::UserImage;

const TERMINAL_QUERY_TIMEOUT: Duration = Duration::from_millis(150);
const MAX_DECODED_ALLOCATION: u64 = 64 * 1024 * 1024;
const MAX_DECODED_BACKING_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 16;
const MAX_DIMENSION: u32 = 8_192;
pub(super) const RESERVED_ROWS: u16 = 12;

#[derive(Clone, Copy)]
pub(super) struct PreparedImage {
    pub key: [u8; 32],
}

struct CacheEntry {
    decoded: Option<image::DynamicImage>,
    decoded_backing_bytes: u64,
    protocol: Option<(u16, SlicedProtocol)>,
    last_used: u64,
}

pub(super) struct ImageRuntime {
    picker: Option<Picker>,
    cache: HashMap<[u8; 32], CacheEntry>,
    decoded_backing_bytes: u64,
    clock: u64,
}

impl ImageRuntime {
    pub fn detect() -> Self {
        let picker = Picker::from_query_stdio_with_options(QueryStdioOptions {
            timeout: TERMINAL_QUERY_TIMEOUT,
            ..QueryStdioOptions::default()
        })
        .ok()
        .filter(|picker| picker.protocol_type() != ProtocolType::Halfblocks);
        Self {
            picker,
            cache: HashMap::new(),
            decoded_backing_bytes: 0,
            clock: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    pub fn clear(&mut self) {
        self.cache.clear();
        self.decoded_backing_bytes = 0;
    }

    pub fn prepare(&mut self, image: &UserImage, width: u16) -> Option<PreparedImage> {
        let picker = self.picker.clone()?;
        if width == 0 {
            return None;
        }
        self.clock = self.clock.wrapping_add(1);
        if !self.cache.contains_key(&image.key) {
            let decoded = decode(image);
            // This only accounts for the decoded image backing buffer. Protocol
            // encoders can allocate additional implementation-defined memory.
            let decoded_backing_bytes = decoded
                .as_ref()
                .map_or(0, |image| image.as_bytes().len() as u64);
            if decoded_backing_bytes > MAX_DECODED_BACKING_BYTES {
                return None;
            }
            self.evict_for(decoded_backing_bytes);
            self.decoded_backing_bytes += decoded_backing_bytes;
            self.cache.insert(
                image.key,
                CacheEntry {
                    decoded,
                    decoded_backing_bytes,
                    protocol: None,
                    last_used: self.clock,
                },
            );
        }
        let entry = self.cache.get_mut(&image.key)?;
        entry.last_used = self.clock;
        if entry
            .protocol
            .as_ref()
            .is_none_or(|(cached_width, _)| *cached_width != width)
        {
            let target = Size::new(width, RESERVED_ROWS);
            let protocol = SlicedProtocol::new_with_resize(
                &picker,
                entry.decoded.as_ref()?.clone(),
                target,
                Resize::Fit(None),
            )
            .ok()?;
            entry.protocol = Some((width, protocol));
        }
        entry.protocol.as_ref()?;
        Some(PreparedImage { key: image.key })
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, image: PreparedImage, area: Rect, y: i16) {
        self.clock = self.clock.wrapping_add(1);
        let Some(entry) = self.cache.get_mut(&image.key) else {
            return;
        };
        entry.last_used = self.clock;
        let Some((_, protocol)) = entry.protocol.as_ref() else {
            return;
        };
        frame.render_widget(
            SlicedImage::new(protocol, SignedPosition::from((0, y))),
            area,
        );
    }

    fn evict_for(&mut self, incoming: u64) {
        while !self.cache.is_empty()
            && (self.cache.len() >= MAX_CACHE_ENTRIES
                || self.decoded_backing_bytes.saturating_add(incoming) > MAX_DECODED_BACKING_BYTES)
        {
            let Some(key) = self
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(entry) = self.cache.remove(&key) {
                self.decoded_backing_bytes = self
                    .decoded_backing_bytes
                    .saturating_sub(entry.decoded_backing_bytes);
            }
        }
    }
}

fn decode(source: &UserImage) -> Option<image::DynamicImage> {
    let bytes = STANDARD.decode(source.data.as_bytes()).ok()?;
    if bytes.len() as u64 > MAX_DECODED_ALLOCATION {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(bytes));
    if let Some(format) = image::ImageFormat::from_mime_type(&source.mime_type) {
        reader.set_format(format);
    } else {
        reader = reader.with_guessed_format().ok()?;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_ALLOCATION);
    reader.limits(limits);
    reader.decode().ok()
}

#[cfg(test)]
mod test_support {
    use super::*;

    impl ImageRuntime {
        pub fn disabled() -> Self {
            Self {
                picker: None,
                cache: HashMap::new(),
                decoded_backing_bytes: 0,
                clock: 0,
            }
        }

        pub fn with_picker(picker: Picker) -> Self {
            Self {
                picker: Some(picker),
                cache: HashMap::new(),
                decoded_backing_bytes: 0,
                clock: 0,
            }
        }

        pub fn cached_entries(&self) -> usize {
            self.cache.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(data: &str) -> UserImage {
        UserImage::new(data.into(), "image/png".into(), 0).unwrap()
    }

    #[test]
    fn disabled_runtime_uses_text_fallback() {
        assert!(
            ImageRuntime::disabled()
                .prepare(&image("invalid"), 40)
                .is_none()
        );
    }

    #[test]
    fn decoded_image_is_reused_when_width_changes() {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(200, 100)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let source = image(&STANDARD.encode(png.into_inner()));
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());

        assert!(runtime.prepare(&source, 20).is_some());
        let decoded_backing_bytes = runtime.decoded_backing_bytes;
        assert_eq!(decoded_backing_bytes, 200 * 100 * 3);
        assert!(runtime.prepare(&source, 40).is_some());
        assert_eq!(runtime.cache.len(), 1);
        assert_eq!(runtime.decoded_backing_bytes, decoded_backing_bytes);
        assert_eq!(runtime.cache[&source.key].protocol.as_ref().unwrap().0, 40);
    }

    #[test]
    fn failed_decodes_are_cached() {
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        let source = image("aW52YWxpZA==");
        assert!(runtime.prepare(&source, 40).is_none());
        assert!(runtime.prepare(&source, 20).is_none());
        assert_eq!(runtime.cache.len(), 1);
        assert!(runtime.cache[&source.key].decoded.is_none());
    }

    #[test]
    fn clear_drops_all_decoded_backing_bytes() {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(20, 10)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let source = image(&STANDARD.encode(png.into_inner()));
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        assert!(runtime.prepare(&source, 20).is_some());

        runtime.clear();

        assert!(runtime.cache.is_empty());
        assert_eq!(runtime.decoded_backing_bytes, 0);
    }
}
