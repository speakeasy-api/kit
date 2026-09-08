use super::app::MediaImage;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageDecoder, ImageReader, Limits};
use ratatui::{
    Frame,
    layout::{Rect, Size},
};
use ratatui_image::{
    Resize,
    picker::{Picker, ProtocolType, cap_parser::QueryStdioOptions},
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};
use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Semaphore, oneshot};

const TERMINAL_QUERY_TIMEOUT: Duration = Duration::from_millis(150);
// Preserve the existing user attachment limit; external/managed acquisition
// remains stricter (8 MiB) while sharing this decoder and pixel budgets.
const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;
const MAX_BASE64_BYTES: usize = MAX_SOURCE_BYTES.div_ceil(3) * 4;
const MAX_QUEUED_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_DECODED_ALLOCATION: u64 = 64 * 1024 * 1024;
const MAX_DECODED_BACKING_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 16;
const MAX_DIMENSION: u32 = 8_192;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
// Bound protocol input, including cell padding, to 256 KiB RGBA. Retain full
// decoded resolution separately: resize never resamples an earlier thumbnail.
// At most 16 protocols plus two in-flight replacements live. Reserve 8 MiB
// each for encoder scratch, strings and render copies (32 bytes per pixel),
// rather than counting only RGBA input. Codec/library overhead is additional.
const MAX_PROTOCOL_PIXELS: u32 = 65_536;
const MAX_JOBS: usize = 2;
// Permits belong to blocking closures, NOT join handles or cache entries.
// Clear, suspend/resume and dropped receivers cannot free running capacity.
// Admission uses try_acquire: there are no spawn_blocking permit waiters.
static WORKERS: Semaphore = Semaphore::const_new(MAX_JOBS);
pub(super) const RESERVED_ROWS: u16 = 12;

#[derive(Clone, Copy)]
pub(super) struct PreparedImage {
    pub key: [u8; 32],
}
type Decoded = Arc<image::DynamicImage>;
type JobResult = Result<(Decoded, SlicedProtocol), &'static str>;
struct CacheEntry {
    source: Option<(Arc<str>, String)>,
    decoded: Option<Decoded>,
    protocol: Option<(u16, SlicedProtocol)>,
    width: u16,
    running: bool,
    error: Option<&'static str>,
    deferred_at: Option<u64>,
    last_used: u64,
}
struct Job {
    key: [u8; 32],
    generation: u64,
    width: u16,
    receiver: oneshot::Receiver<JobResult>,
}
pub(super) struct ImageRuntime {
    pub(super) markdown: super::markdown_images::MarkdownImages,
    picker: Option<Picker>,
    cache: HashMap<[u8; 32], CacheEntry>,
    visible: HashSet<[u8; 32]>,
    visibility_generation: u64,
    jobs: Vec<Job>,
    generation: u64,
    clock: u64,
}
impl ImageRuntime {
    fn new(picker: Option<Picker>) -> Self {
        Self {
            markdown: super::markdown_images::MarkdownImages::new(),
            picker,
            cache: HashMap::new(),
            visible: HashSet::new(),
            visibility_generation: 0,
            jobs: Vec::new(),
            generation: 0,
            clock: 0,
        }
    }
    pub fn detect() -> Self {
        Self::new(
            Picker::from_query_stdio_with_options(QueryStdioOptions {
                timeout: TERMINAL_QUERY_TIMEOUT,
                ..QueryStdioOptions::default()
            })
            .ok()
            .filter(|picker| picker.protocol_type() != ProtocolType::Halfblocks),
        )
    }
    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }
    pub fn set_visible_keys(&mut self, keys: impl IntoIterator<Item = [u8; 32]>) {
        // The caller supplies actual visible occurrences, including pending and
        // deferred sources. Decode-cache residency must not define visibility.
        let visible = keys.into_iter().collect();
        if self.visible != visible {
            self.visible = visible;
            self.visibility_generation = self.visibility_generation.wrapping_add(1);
        }
    }

    pub fn clear(&mut self) {
        self.visible.clear();
        self.visibility_generation = self.visibility_generation.wrapping_add(1);
        self.markdown.clear();
        self.cache.clear();
        self.generation = self.generation.wrapping_add(1);
        // Stale results retain their job slot and reservation until completion.
    }
    pub fn status(&self, key: &[u8; 32]) -> &'static str {
        if !self.enabled() {
            return "image display unavailable";
        }
        match self.cache.get(key) {
            Some(entry) if entry.error.is_some() => entry.error.unwrap_or("image unavailable"),
            Some(entry) if entry.deferred_at.is_some() => "image deferred (visible image budget)",
            None if self.cache.len() >= MAX_CACHE_ENTRIES => {
                "image deferred (visible image budget)"
            }
            Some(entry) if entry.protocol.is_some() && !entry.running => "image ready",
            _ => "image loading",
        }
    }
    /// Drain completions on the TUI tick; redraw when true. Never waits.
    pub fn pending(&self) -> bool {
        self.markdown.pending()
            || !self.jobs.is_empty()
            || self.cache.values().any(|entry| {
                entry.error.is_none()
                    && entry.deferred_at.is_none()
                    && (entry.source.is_some()
                        || entry
                            .protocol
                            .as_ref()
                            .is_none_or(|(width, _)| *width != entry.width))
            })
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let mut index = 0;
        while index < self.jobs.len() {
            let result = match self.jobs[index].receiver.try_recv() {
                Ok(result) => result,
                Err(oneshot::error::TryRecvError::Empty) => {
                    index += 1;
                    continue;
                }
                Err(oneshot::error::TryRecvError::Closed) => Err("image worker failed"),
            };
            let job = self.jobs.swap_remove(index);
            if job.generation != self.generation {
                continue;
            }
            if !self.cache.contains_key(&job.key) {
                continue;
            }
            let admitted = match &result {
                Ok((decoded, _)) => self.reserve_decoded(&job.key, decoded.as_bytes().len() as u64),
                Err(_) => true,
            };
            let Some(entry) = self.cache.get_mut(&job.key) else {
                continue;
            };
            entry.running = false;
            changed = true;
            match result {
                Ok((decoded, protocol)) if admitted => {
                    entry.decoded = Some(decoded);
                    if entry.width == job.width {
                        entry.protocol = Some((job.width, protocol));
                    }
                }
                Ok(_) => {
                    // Drop the unadmittable result, not another visible image.
                    // Only a meaningful visible-set change permits a new decode.
                    entry.decoded = None;
                    entry.protocol = None;
                    entry.deferred_at = Some(self.visibility_generation);
                }
                Err(error) => entry.error = Some(error),
            }
        }
        self.schedule();
        changed
    }
    fn reserve_decoded(&mut self, key: &[u8; 32], bytes: u64) -> bool {
        loop {
            let retained = self
                .cache
                .iter()
                .filter(|(cached, _)| *cached != key)
                .filter_map(|(_, entry)| entry.decoded.as_ref())
                .map(|decoded| decoded.as_bytes().len() as u64)
                .sum::<u64>();
            if retained.saturating_add(bytes) <= MAX_DECODED_BACKING_BYTES {
                return true;
            }
            let victim = self
                .cache
                .iter()
                .filter(|(cached, entry)| {
                    *cached != key
                        && !self.visible.contains(*cached)
                        && !entry.running
                        && entry.decoded.is_some()
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key);
            let Some(victim) = victim else { return false };
            self.cache.remove(&victim);
        }
    }

    pub fn prepare(&mut self, image: &MediaImage, width: u16) -> Option<PreparedImage> {
        if !self.enabled() || width == 0 {
            return None;
        }
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.cache.get(&image.key)
            && let Some(generation) = entry.deferred_at
        {
            if generation == self.visibility_generation {
                return None;
            }
            while self.queued_bytes().saturating_add(image.data.len()) > MAX_QUEUED_SOURCE_BYTES {
                if !self.evict() {
                    return None;
                }
            }
            if let Some(entry) = self.cache.get_mut(&image.key) {
                entry.deferred_at = None;
                entry.source = Some((image.data.clone(), image.mime_type.clone()));
            }
        }
        if !self.cache.contains_key(&image.key) {
            if image.data.len() > MAX_BASE64_BYTES || image.mime_type.len() > 256 {
                return None;
            }
            while self.cache.len() >= MAX_CACHE_ENTRIES
                || self.queued_bytes().saturating_add(image.data.len()) > MAX_QUEUED_SOURCE_BYTES
            {
                if !self.evict() {
                    return None;
                }
            }
            self.cache.insert(
                image.key,
                CacheEntry {
                    source: Some((image.data.clone(), image.mime_type.clone())),
                    decoded: None,
                    protocol: None,
                    width,
                    running: false,
                    error: None,
                    deferred_at: None,
                    last_used: self.clock,
                },
            );
        }
        let entry = self.cache.get_mut(&image.key)?;
        entry.last_used = self.clock;
        entry.width = width;
        if entry
            .protocol
            .as_ref()
            .is_some_and(|(cached, _)| *cached != width)
        {
            entry.protocol = None;
        }
        let ready = entry.protocol.is_some();
        self.schedule();
        ready.then_some(PreparedImage { key: image.key })
    }
    fn queued_bytes(&self) -> usize {
        self.cache
            .values()
            .filter_map(|entry| entry.source.as_ref())
            .map(|(data, _)| data.len())
            .sum()
    }
    fn evict(&mut self) -> bool {
        let key = self
            .cache
            .iter()
            .filter(|(key, entry)| !entry.running && !self.visible.contains(*key))
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| *key);
        key.is_some_and(|key| self.cache.remove(&key).is_some())
    }
    fn schedule(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let picker = picker.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            for entry in self.cache.values_mut() {
                entry.error = Some("image runtime unavailable");
                entry.source = None;
            }
            return;
        };
        while self.jobs.len() < MAX_JOBS {
            let Some(key) = self
                .cache
                .iter()
                .filter(|(_, entry)| {
                    !entry.running
                        && entry.error.is_none()
                        && entry.deferred_at.is_none()
                        && entry.protocol.is_none()
                })
                .max_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            let Ok(permit) = WORKERS.try_acquire() else {
                break;
            };
            let Some(entry) = self.cache.get_mut(&key) else {
                break;
            };
            let source = entry.source.take();
            let decoded = entry.decoded.clone();
            let width = entry.width;
            entry.running = true;
            let (sender, receiver) = oneshot::channel();
            self.jobs.push(Job {
                key,
                generation: self.generation,
                width,
                receiver,
            });
            let picker = picker.clone();
            handle.spawn_blocking(move || {
                let _permit = permit;
                let result = (|| {
                    let decoded = match decoded {
                        Some(decoded) => decoded,
                        None => {
                            let (data, mime) = source.ok_or("image source unavailable")?;
                            Arc::new(
                                decode(&data, &mime)
                                    .ok_or("image decode failed or exceeds limits")?,
                            )
                        }
                    };
                    let protocol = protocol(&picker, &decoded, width)
                        .ok_or("image protocol unavailable or exceeds limits")?;
                    Ok((decoded, protocol))
                })();
                let _ = sender.send(result);
            });
        }
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
}
fn decode(data: &str, mime: &str) -> Option<image::DynamicImage> {
    if data.len() > MAX_BASE64_BYTES {
        return None;
    }
    let bytes = STANDARD.decode(data.as_bytes()).ok()?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(bytes));
    if let Some(format) = image::ImageFormat::from_mime_type(mime) {
        reader.set_format(format);
    } else {
        reader = reader.with_guessed_format().ok()?;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_ALLOCATION);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODED_ALLOCATION
    {
        return None;
    }
    let orientation = decoder.orientation().ok()?;
    let decoded = image::DynamicImage::from_decoder(decoder).ok()?;
    // Conversion/orientation transiently retain two 64 MiB buffers per worker.
    // Two workers also hold at most 2*(14 MiB base64 + 10 MiB encoded source).
    let mut decoded = image::DynamicImage::ImageRgba8(decoded.into_rgba8());
    decoded.apply_orientation(orientation);
    Some(decoded)
}
fn protocol(picker: &Picker, decoded: &image::DynamicImage, width: u16) -> Option<SlicedProtocol> {
    let font = picker.font_size();
    let height = u32::from(font.height).checked_mul(u32::from(RESERVED_ROWS))?;
    let cell_pixels = u32::from(font.width).checked_mul(height)?;
    if cell_pixels == 0 || height > u32::from(u16::MAX) {
        return None;
    }
    let columns = u32::from(width)
        .min(512)
        .min(MAX_PROTOCOL_PIXELS / cell_pixels);
    if columns == 0 {
        return None;
    }
    let pixels = columns * u32::from(font.width);
    if pixels > u32::from(u16::MAX) {
        return None;
    }
    // No full-resolution clone or full-image floating-point filter scratch.
    let fitted = decoded.resize(pixels, height, image::imageops::FilterType::Nearest);
    SlicedProtocol::new_with_resize(
        picker,
        fitted,
        Size::new(columns as u16, RESERVED_ROWS),
        Resize::Fit(Some(image::imageops::FilterType::Nearest)),
    )
    .ok()
}
#[cfg(test)]
mod test_support {
    use super::*;
    impl ImageRuntime {
        pub fn disabled() -> Self {
            Self::new(None)
        }
        pub fn with_picker(picker: Picker) -> Self {
            Self::new(Some(picker))
        }
        pub fn cached_entries(&self) -> usize {
            self.cache.len()
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn source(seed: u8) -> MediaImage {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            200,
            100,
            image::Rgba([seed, 80, 120, 255]),
        ))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
        MediaImage::new(STANDARD.encode(png.into_inner()), "image/png".into(), 0).unwrap()
    }

    async fn settle(runtime: &mut ImageRuntime, source: &MediaImage) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while runtime.status(&source.key) == "image loading" {
                runtime.poll();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn settle_work(runtime: &mut ImageRuntime) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while runtime.pending() {
                runtime.poll();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn visible_decoded_pressure_settles_and_recovers_after_scroll() {
        let sources: Vec<_> = (0..3)
            .map(|seed| {
                let mut bytes = Cursor::new(Vec::new());
                image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                    4096,
                    4096,
                    image::Rgb([seed, 80, 120]),
                ))
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
                MediaImage::new(STANDARD.encode(bytes.into_inner()), "image/png".into(), 0).unwrap()
            })
            .collect();
        let keys: Vec<_> = sources.iter().map(|source| source.key).collect();
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        runtime.set_visible_keys(keys.clone());
        for source in &sources {
            runtime.prepare(source, 40);
        }
        settle_work(&mut runtime).await;
        let deferred = sources
            .iter()
            .find(|source| runtime.status(&source.key).contains("deferred"))
            .unwrap();
        assert!(
            sources
                .iter()
                .any(|source| runtime.status(&source.key) == "image ready")
        );
        for _ in 0..3 {
            runtime.set_visible_keys(keys.clone());
            for source in &sources {
                runtime.prepare(source, 40);
            }
            assert!(!runtime.pending());
        }
        // Resize rebuilds only admitted protocols; it is not more decode capacity.
        let retained = runtime
            .cache
            .iter()
            .filter_map(|(key, entry)| entry.decoded.clone().map(|decoded| (*key, decoded)))
            .collect::<Vec<_>>();
        for source in &sources {
            runtime.prepare(source, 60);
        }
        settle_work(&mut runtime).await;
        assert!(runtime.status(&deferred.key).contains("deferred"));
        for (key, decoded) in retained {
            assert!(Arc::ptr_eq(
                &decoded,
                runtime.cache[&key].decoded.as_ref().unwrap()
            ));
        }
        runtime.set_visible_keys([deferred.key]);
        runtime.prepare(deferred, 60);
        settle_work(&mut runtime).await;
        assert_eq!(runtime.status(&deferred.key), "image ready");
        assert!(
            runtime
                .cache
                .values()
                .filter_map(|entry| entry.decoded.as_ref())
                .map(|image| image.as_bytes().len() as u64)
                .sum::<u64>()
                <= MAX_DECODED_BACKING_BYTES
        );
        runtime.clear();
        runtime.set_visible_keys([sources[0].key]);
        runtime.prepare(&sources[0], 40);
        settle_work(&mut runtime).await;
        assert_eq!(runtime.status(&sources[0].key), "image ready");
    }

    #[tokio::test]
    async fn visible_entry_pressure_does_not_cycle_decoding() {
        let sources: Vec<_> = (0..MAX_CACHE_ENTRIES + 2)
            .map(|seed| source(seed as u8))
            .collect();
        let keys: Vec<_> = sources.iter().map(|source| source.key).collect();
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        runtime.set_visible_keys(keys.clone());
        for source in &sources {
            runtime.prepare(source, 40);
        }
        settle_work(&mut runtime).await;
        for _ in 0..3 {
            runtime.set_visible_keys(keys.clone());
            for source in &sources {
                runtime.prepare(source, 40);
            }
            assert!(!runtime.pending());
        }
        let last = sources.last().unwrap();
        assert!(runtime.status(&last.key).contains("deferred"));
        runtime.set_visible_keys([last.key]);
        runtime.prepare(last, 40);
        settle_work(&mut runtime).await;
        assert_eq!(runtime.status(&last.key), "image ready");
    }

    #[test]
    fn disabled_runtime_uses_text_fallback() {
        assert!(ImageRuntime::disabled().prepare(&source(0), 40).is_none());
    }

    #[tokio::test]
    async fn completion_renders_and_resize_retains_full_resolution() {
        let source = source(1);
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        assert!(runtime.prepare(&source, 20).is_none());
        settle(&mut runtime, &source).await;
        assert_eq!(runtime.status(&source.key), "image ready");
        let decoded = runtime.cache[&source.key].decoded.as_ref().unwrap().clone();
        assert_eq!((decoded.width(), decoded.height()), (200, 100));
        let mut terminal = Terminal::new(TestBackend::new(40, RESERVED_ROWS)).unwrap();
        let prepared = runtime.prepare(&source, 20).unwrap();
        terminal
            .draw(|frame| runtime.render(frame, prepared, frame.area(), 0))
            .unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.symbol() != " " || cell.bg != ratatui::style::Color::Reset)
        );
        assert!(runtime.prepare(&source, 40).is_none());
        settle(&mut runtime, &source).await;
        assert!(runtime.prepare(&source, 40).is_some());
        assert!(Arc::ptr_eq(
            &decoded,
            runtime.cache[&source.key].decoded.as_ref().unwrap()
        ));
        assert_eq!(runtime.cached_entries(), 1);
    }

    #[tokio::test]
    async fn common_decoder_handles_supported_formats_and_corrupt_signatures() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::Gif,
            image::ImageFormat::WebP,
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(3, 2)
                .write_to(&mut bytes, format)
                .unwrap();
            let source = MediaImage::new(
                STANDARD.encode(bytes.into_inner()),
                format.to_mime_type().into(),
                0,
            )
            .unwrap();
            let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
            assert!(runtime.prepare(&source, 40).is_none());
            settle(&mut runtime, &source).await;
            assert_eq!(runtime.status(&source.key), "image ready");
        }
        let source =
            MediaImage::new(STANDARD.encode(b"\x89PNG\r\n\x1a\n"), "image/png".into(), 0).unwrap();
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        runtime.prepare(&source, 40);
        settle(&mut runtime, &source).await;
        assert_eq!(
            runtime.status(&source.key),
            "image decode failed or exceeds limits"
        );
    }

    #[tokio::test]
    async fn failures_are_negative_cached() {
        let source = MediaImage::new("aW52YWxpZA==".into(), "image/png".into(), 0).unwrap();
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        assert!(runtime.prepare(&source, 40).is_none());
        settle(&mut runtime, &source).await;
        assert_eq!(
            runtime.status(&source.key),
            "image decode failed or exceeds limits"
        );
        assert!(runtime.prepare(&source, 20).is_none());
        assert!(!runtime.poll());
        assert_eq!(runtime.cached_entries(), 1);
    }

    #[tokio::test]
    async fn clear_discards_old_completions_and_allows_same_source_again() {
        let source = source(2);
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        runtime.prepare(&source, 20);
        runtime.clear();
        assert_eq!(runtime.cached_entries(), 0);
        runtime.prepare(&source, 40);
        settle(&mut runtime, &source).await;
        assert!(runtime.prepare(&source, 40).is_some());
        assert_eq!(runtime.cache[&source.key].protocol.as_ref().unwrap().0, 40);
        runtime.clear();
        assert_eq!(runtime.cached_entries(), 0);
    }

    #[tokio::test]
    async fn queue_and_cache_remain_bounded() {
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        for seed in 0..40 {
            runtime.prepare(&source(seed), 40);
            assert!(runtime.cached_entries() <= MAX_CACHE_ENTRIES);
            assert!(runtime.queued_bytes() <= MAX_QUEUED_SOURCE_BYTES);
        }
        let final_source = source(41);
        runtime.prepare(&final_source, 40);
        settle(&mut runtime, &final_source).await;
        assert!(runtime.prepare(&final_source, 40).is_some());
    }

    #[tokio::test]
    async fn exif_orientation_is_applied_before_protocol_geometry() {
        use image::ImageEncoder as _;
        let mut exif = b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
        exif.extend(6_u16.to_le_bytes());
        exif.extend([0; 6]);
        let pixels = image::RgbaImage::from_pixel(3, 2, image::Rgba([120, 80, 40, 255]));
        let mut png = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut png);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .write_image(pixels.as_raw(), 3, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        let source = MediaImage::new(STANDARD.encode(png), "image/png".into(), 0).unwrap();
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        runtime.prepare(&source, 20);
        settle(&mut runtime, &source).await;
        let decoded = runtime.cache[&source.key].decoded.as_ref().unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 3));
    }

    #[test]
    fn rejects_source_and_pixel_budgets() {
        assert!(decode(&"A".repeat(MAX_BASE64_BYTES + 1), "image/png").is_none());
        // A compact PNG can describe more than 16 MP while remaining below 8 MiB.
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_luma8(8192, 2049)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        assert!(decode(&STANDARD.encode(png.into_inner()), "image/png").is_none());
    }
}
