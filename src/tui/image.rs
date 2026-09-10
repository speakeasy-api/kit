use std::{
    collections::{HashMap, HashSet},
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

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
    loader: Option<Loader>,
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
            loader: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    pub fn clear(&mut self) {
        self.loader = None;
        self.cache.clear();
        self.decoded_backing_bytes = 0;
    }

    pub fn prepare(&mut self, image: &UserImage, width: u16) -> Option<PreparedImage> {
        self.picker.as_ref()?;
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
        self.prepare_cached(image.key, width)
    }

    fn prepare_cached(&mut self, key: [u8; 32], width: u16) -> Option<PreparedImage> {
        let picker = self.picker.clone()?;
        let entry = self.cache.get_mut(&key)?;
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
        Some(PreparedImage { key })
    }

    /// Only queued work keeps the event loop ticking; failures remain cached.
    pub fn pending(&self) -> bool {
        self.loader
            .as_ref()
            .is_some_and(|loader| !loader.pending.is_empty())
    }

    pub fn prepare_destination(
        &mut self,
        destination: &str,
        root: &Path,
        width: u16,
    ) -> Option<PreparedImage> {
        let source = resolve_destination(destination, root)?;
        let key = *blake3::hash(format!("markdown:{source:?}").as_bytes()).as_bytes();
        self.prepare_background(key, || LoadSource::Destination(source), width)
    }

    pub fn prepare_assistant(&mut self, image: &UserImage, width: u16) -> Option<PreparedImage> {
        self.prepare_background(image.key, || LoadSource::Bytes(image.clone()), width)
    }

    fn prepare_background(
        &mut self,
        key: [u8; 32],
        source: impl FnOnce() -> LoadSource,
        width: u16,
    ) -> Option<PreparedImage> {
        self.picker.as_ref()?;
        if width == 0 {
            return None;
        }
        self.clock = self.clock.wrapping_add(1);
        if self.loader.is_none() {
            self.loader = Loader::start();
        }
        self.poll();
        if self.cache.contains_key(&key) {
            return self.prepare_cached(key, width);
        }
        let loader = self.loader.as_mut()?;
        // Remember failures so redraw does not repeatedly contact an endpoint.
        // Successful entries use the bounded decoded-image LRU cache.
        if !loader.seen.contains(&key)
            && loader.seen.len() < 4096
            && loader.pending.len() < 3
            && loader.requests.try_send((key, source())).is_ok()
        {
            loader.seen.insert(key);
            loader.pending.insert(key);
        }
        None
    }

    pub fn poll(&mut self) {
        let Some(loader) = &mut self.loader else {
            return;
        };
        let completed: Vec<_> = loader.results.try_iter().collect();
        for (loaded_key, decoded) in completed {
            if let Some(loader) = &mut self.loader {
                loader.pending.remove(&loaded_key);
            }
            let bytes = decoded
                .as_ref()
                .map_or(0, |image| image.as_bytes().len() as u64);
            self.evict_for(bytes);
            self.decoded_backing_bytes += bytes;
            self.cache.insert(
                loaded_key,
                CacheEntry {
                    decoded,
                    decoded_backing_bytes: bytes,
                    protocol: None,
                    last_used: self.clock,
                },
            );
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
                if entry.decoded.is_some()
                    && let Some(loader) = &mut self.loader
                {
                    loader.seen.remove(&key);
                }
                self.decoded_backing_bytes = self
                    .decoded_backing_bytes
                    .saturating_sub(entry.decoded_backing_bytes);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Destination {
    File(PathBuf),
    Http(url::Url),
}

fn resolve_destination(value: &str, root: &Path) -> Option<Destination> {
    if let Ok(url) = url::Url::parse(value) {
        return match url.scheme() {
            "http" | "https" if url.host_str().is_some() => Some(Destination::Http(url)),
            "file" => url.to_file_path().ok().map(Destination::File),
            _ => None,
        };
    }
    Some(Destination::File(root.join(value)))
}

enum LoadSource {
    Bytes(UserImage),
    Destination(Destination),
}
type Loaded = ([u8; 32], Option<image::DynamicImage>);
struct Loader {
    requests: mpsc::SyncSender<([u8; 32], LoadSource)>,
    results: mpsc::Receiver<Loaded>,
    seen: HashSet<[u8; 32]>,
    pending: HashSet<[u8; 32]>,
}

impl Loader {
    fn start() -> Option<Self> {
        let (requests, work) = mpsc::sync_channel::<([u8; 32], LoadSource)>(2);
        let (completed, results) = mpsc::sync_channel(2);
        std::thread::Builder::new()
            .name("tui-image-loader".into())
            .spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .connect_timeout(Duration::from_secs(3))
                    .redirect(reqwest::redirect::Policy::limited(5))
                    .build()
                    .ok();
                while let Ok((key, source)) = work.recv() {
                    let decoded = match source {
                        LoadSource::Bytes(image) => decode(&image),
                        LoadSource::Destination(destination) => {
                            load_destination(destination, client.as_ref())
                        }
                    };
                    // Bound the pixels copied and encoded on the draw thread too.
                    let decoded = decoded.map(|image| {
                        if image.width() > 1600 || image.height() > 1200 {
                            image.thumbnail(1600, 1200)
                        } else {
                            image
                        }
                    });
                    if completed.send((key, decoded)).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            requests,
            results,
            seen: HashSet::new(),
            pending: HashSet::new(),
        })
    }
}

const MAX_FETCH_BYTES: u64 = 10 * 1024 * 1024;
fn load_destination(
    destination: Destination,
    client: Option<&reqwest::blocking::Client>,
) -> Option<image::DynamicImage> {
    let reader: Box<dyn Read> = match destination {
        Destination::File(path) => {
            // Reject devices, directories and pipes before opening them.
            let metadata = std::fs::metadata(&path).ok()?;
            if !metadata.is_file() || metadata.len() > MAX_FETCH_BYTES {
                return None;
            }
            Box::new(std::fs::File::open(path).ok()?)
        }
        Destination::Http(url) => {
            let response = client?.get(url).send().ok()?.error_for_status().ok()?;
            if response
                .content_length()
                .is_some_and(|len| len > MAX_FETCH_BYTES)
            {
                return None;
            }
            Box::new(response)
        }
    };
    let mut bytes = Vec::new();
    reader
        .take(MAX_FETCH_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_FETCH_BYTES {
        return None;
    }
    decode_bytes(bytes, "")
}

fn decode(source: &UserImage) -> Option<image::DynamicImage> {
    let bytes = STANDARD.decode(source.data.as_bytes()).ok()?;
    if bytes.len() as u64 > MAX_DECODED_ALLOCATION {
        return None;
    }
    decode_bytes(bytes, &source.mime_type)
}

fn decode_bytes(bytes: Vec<u8>, mime_type: &str) -> Option<image::DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes));
    if let Some(format) = image::ImageFormat::from_mime_type(mime_type) {
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod test_support {
    use super::*;

    impl ImageRuntime {
        pub fn disabled() -> Self {
            Self {
                picker: None,
                cache: HashMap::new(),
                decoded_backing_bytes: 0,
                clock: 0,
                loader: None,
            }
        }

        pub fn with_picker(picker: Picker) -> Self {
            Self {
                picker: Some(picker),
                cache: HashMap::new(),
                decoded_backing_bytes: 0,
                clock: 0,
                loader: None,
            }
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
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;

    fn image(data: &str) -> UserImage {
        UserImage::new(data.into(), "image/png".into(), 0).unwrap()
    }

    #[test]
    fn destinations_resolve_against_project_root_without_io() {
        let root = Path::new("/project");
        assert_eq!(
            resolve_destination("images/a b.png", root),
            Some(Destination::File(root.join("images/a b.png")))
        );
        assert_eq!(
            resolve_destination("/tmp/a.png", root),
            Some(Destination::File(PathBuf::from("/tmp/a.png")))
        );
        assert_eq!(
            resolve_destination("file:///tmp/a%20b.png", root),
            Some(Destination::File(PathBuf::from("/tmp/a b.png")))
        );
        assert!(matches!(
            resolve_destination("https://example.invalid/a.png", root),
            Some(Destination::Http(_))
        ));
        assert!(matches!(
            resolve_destination("http://127.0.0.1/a.png", root),
            Some(Destination::Http(_))
        ));
        assert!(resolve_destination("data:image/png;base64,AQID", root).is_none());
        assert!(resolve_destination("ftp://example.invalid/a.png", root).is_none());
        assert!(resolve_destination("file://other-host/a.png", root).is_none());
    }

    fn png_bytes() -> Vec<u8> {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(4, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        png.into_inner()
    }

    #[test]
    fn local_loader_guesses_format_and_rejects_directories_and_oversize_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image with spaces");
        std::fs::write(&path, png_bytes()).unwrap();
        let decoded = load_destination(Destination::File(path.clone()), None).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 2));
        assert!(load_destination(Destination::File(directory.path().to_owned()), None).is_none());
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_FETCH_BYTES + 1)
            .unwrap();
        assert!(load_destination(Destination::File(path), None).is_none());
    }

    #[test]
    fn http_loader_reads_only_loopback_and_checks_status_and_size() {
        use std::io::Write as _;
        for (status, length, body, valid) in [
            ("200 OK", png_bytes().len() as u64, png_bytes(), true),
            ("404 Not Found", 0, Vec::new(), false),
            ("200 OK", MAX_FETCH_BYTES + 1, Vec::new(), false),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                let _ = stream.write_all(&body);
            });
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            let destination =
                resolve_destination(&format!("http://{address}/image"), Path::new("/")).unwrap();
            assert_eq!(
                load_destination(destination, Some(&client)).is_some(),
                valid
            );
            server.join().unwrap();
        }
    }

    #[test]
    fn failed_assistant_decodes_are_cached_without_requeuing() {
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        let source = image("invalid");
        assert!(runtime.prepare_assistant(&source, 40).is_none());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while runtime.pending() && std::time::Instant::now() < deadline {
            runtime.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!runtime.pending(), "background decode did not finish");
        assert!(runtime.prepare_assistant(&source, 40).is_none());
        assert!(!runtime.pending(), "failed source was queued again");
    }

    #[test]
    fn assistant_decode_runs_in_bounded_worker() {
        let mut runtime = ImageRuntime::with_picker(Picker::halfblocks());
        let source = UserImage::new(STANDARD.encode(png_bytes()), "image/png".into(), 0).unwrap();
        assert!(runtime.prepare_assistant(&source, 40).is_none());
        // Receive the worker result through its real boundary, not a timing assertion.
        let (key, decoded) = runtime
            .loader
            .as_ref()
            .unwrap()
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(key, source.key);
        assert_eq!(decoded.unwrap().width(), 4);
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
