//! Presentation-only external image snapshots. Never publishes files or model attachments.
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::task::JoinSet;

use super::{
    app::MediaImage,
    image_source::{self, ImagePolicy},
};

const MAX_ENTRIES: usize = 32;
const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_JOBS: usize = 2;

enum State {
    Waiting,
    Deferred,
    Loading(tokio::task::Id),
    Ready(MediaImage),
    Failed(String),
}

struct Entry {
    state: State,
    used: u64,
    key: Option<[u8; 32]>,
}

struct Completion {
    generation: u64,
    source: String,
    image: Result<MediaImage, String>,
}

pub(super) struct MarkdownImages {
    root: PathBuf,
    session: String,
    generation: u64,
    policy: ImagePolicy,
    entries: HashMap<String, Entry>,
    visible: HashSet<String>,
    changed_sources: HashSet<String>,
    jobs: JoinSet<Completion>,
    clock: u64,
    retained: usize,
}

impl MarkdownImages {
    pub fn new() -> Self {
        Self {
            root: PathBuf::new(),
            session: String::new(),
            generation: 0,
            policy: ImagePolicy::from_environment(),
            entries: HashMap::new(),
            visible: HashSet::new(),
            changed_sources: HashSet::new(),
            jobs: JoinSet::new(),
            clock: 0,
            retained: 0,
        }
    }

    pub fn context(&mut self, root: &Path, session: Option<&str>) {
        let session = session.unwrap_or_default();
        if self.root != root || self.session != session {
            self.clear();
            self.root = root.to_path_buf();
            self.session = session.to_owned();
        }
    }

    pub fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.changed_sources.extend(self.entries.keys().cloned());
        self.entries.clear();
        self.visible.clear();
        self.retained = 0;
        // Stop old async stages, including requests following DNS. Actual blocking
        // closures retain global admission until completion even after this abort.
        self.jobs.abort_all();
    }

    /// Register the complete viewport before polling or requesting sources. The
    /// first unique destinations form a stable, bounded admission set.
    pub fn set_visible<'a>(&mut self, sources: impl IntoIterator<Item = &'a str>) {
        let mut visible = HashSet::new();
        for source in sources {
            if source.len() > 4096 {
                continue;
            }
            visible.insert(source.to_owned());
            if visible.len() == MAX_ENTRIES {
                break;
            }
        }
        if visible == self.visible {
            return;
        }
        self.visible = visible;
        for entry in self.entries.values_mut() {
            if matches!(entry.state, State::Deferred) {
                entry.state = State::Waiting;
            }
        }
    }

    pub fn is_visible(&self, source: &str) -> bool {
        self.visible.contains(source)
    }

    /// Authorized content identity survives byte-pressure deferral.
    pub fn key(&self, source: &str) -> Option<[u8; 32]> {
        self.entries.get(source)?.key
    }

    pub fn take_changed_sources(&mut self) -> HashSet<String> {
        std::mem::take(&mut self.changed_sources)
    }

    pub fn pending(&self) -> bool {
        !self.jobs.is_empty()
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(result) = self.jobs.try_join_next() {
            let done = match result {
                Ok(done) => done,
                Err(error) => {
                    for entry in self.entries.values_mut() {
                        if matches!(entry.state, State::Loading(id) if id == error.id()) {
                            entry.state = State::Failed("image worker failed".into());
                            changed = true;
                        }
                    }
                    continue;
                }
            };
            if done.generation != self.generation {
                continue;
            }
            if !self.entries.contains_key(&done.source) {
                continue;
            }
            let state = match done.image {
                Ok(image) => {
                    if let Some(entry) = self.entries.get_mut(&done.source) {
                        entry.key = Some(image.key);
                    }
                    let bytes = image.data.len();
                    // Reclaim offscreen snapshots, never the visible working set.
                    // Capacity deferrals retry only when that set changes.
                    if bytes <= MAX_SOURCE_BYTES {
                        while self.retained.saturating_add(bytes) > MAX_SOURCE_BYTES {
                            if !self.evict_oldest(true) {
                                break;
                            }
                        }
                    }
                    if self.retained.saturating_add(bytes) <= MAX_SOURCE_BYTES {
                        self.retained += bytes;
                        self.changed_sources.insert(done.source.clone());
                        State::Ready(image)
                    } else {
                        State::Deferred
                    }
                }
                Err(error) => State::Failed(error),
            };
            if let Some(entry) = self.entries.get_mut(&done.source) {
                entry.state = state;
            }
            changed = true;
        }
        changed
    }

    /// Byte pressure reclaims only ready snapshots; entry pressure can also
    /// reclaim idle placeholders and failures. Neither may remove active jobs.
    fn evict_oldest(&mut self, ready_only: bool) -> bool {
        let oldest = self
            .entries
            .iter()
            .filter(|(source, entry)| {
                !self.visible.contains(*source)
                    && (matches!(entry.state, State::Ready(_))
                        || (!ready_only && !matches!(entry.state, State::Loading(_))))
            })
            .min_by_key(|(_, entry)| entry.used)
            .map(|(source, _)| source.clone());
        let Some(oldest) = oldest else { return false };
        if let Some(Entry {
            state: State::Ready(image),
            ..
        }) = self.entries.remove(&oldest)
        {
            self.retained = self.retained.saturating_sub(image.data.len());
            self.changed_sources.insert(oldest);
        }
        true
    }

    pub fn request(&mut self, source: &str) {
        if source.len() > 4096 || (!self.visible.is_empty() && !self.is_visible(source)) {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        if !self.entries.contains_key(source) {
            if self.entries.len() == MAX_ENTRIES && !self.evict_oldest(false) {
                return;
            }
            self.entries.insert(
                source.to_owned(),
                Entry {
                    state: State::Waiting,
                    used: self.clock,
                    key: None,
                },
            );
        }
        let Some(entry) = self.entries.get_mut(source) else {
            return;
        };
        entry.used = self.clock;
        if !matches!(entry.state, State::Waiting)
            || self.jobs.len() >= MAX_JOBS
            || self.session.is_empty()
        {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let source = source.to_owned();
        let root = self.root.clone();
        let session = self.session.clone();
        let policy = self.policy.clone();
        let generation = self.generation;
        let job = self.jobs.spawn_on(
            async move {
                let image = image_source::resolve(&source, &root, &session, &policy)
                    .await
                    .and_then(|(bytes, mime)| {
                        MediaImage::new(STANDARD.encode(bytes), mime, 0)
                            .ok_or_else(|| "image source budget exceeded".to_owned())
                    });
                Completion {
                    generation,
                    source,
                    image,
                }
            },
            &handle,
        );
        entry.state = State::Loading(job.id());
    }

    pub fn image(&self, source: &str) -> Option<&MediaImage> {
        match &self.entries.get(source)?.state {
            State::Ready(image) => Some(image),
            _ => None,
        }
    }

    pub fn status(&self, source: &str) -> &str {
        if source.len() > 4096 {
            return "image source too long";
        }
        match self.entries.get(source).map(|entry| &entry.state) {
            Some(State::Ready(_)) => "image",
            Some(State::Failed(error)) => error,
            Some(State::Deferred) => "image deferred (visible image budget)",
            _ => "image loading",
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use std::{io::Cursor, time::Duration};

    fn write_png(root: &Path, name: &str) {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(root.join(name), bytes.into_inner()).unwrap();
    }

    fn write_large_png(root: &Path, name: &str, value: u8) {
        use image::{
            ImageEncoder as _,
            codecs::png::{CompressionType, FilterType, PngEncoder},
        };
        let pixels = vec![value; 1792 * 1366 * 3];
        let mut bytes = Vec::new();
        PngEncoder::new_with_quality(&mut bytes, CompressionType::Level(0), FilterType::NoFilter)
            .write_image(&pixels, 1792, 1366, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert!((7 * 1024 * 1024..8 * 1024 * 1024).contains(&bytes.len()));
        std::fs::write(root.join(name), bytes).unwrap();
    }

    async fn settle(images: &mut MarkdownImages) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while images.pending() {
                images.poll();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn local_snapshot_and_negative_cache_reset_on_session_change() {
        let root = tempfile::tempdir().unwrap();
        write_png(root.path(), "image.png");
        let mut images = MarkdownImages::new();
        images.context(root.path(), Some("first"));
        images.request("image.png");
        assert_eq!(images.status("image.png"), "image loading");
        settle(&mut images).await;
        assert!(images.image("image.png").is_some());
        std::fs::remove_file(root.path().join("image.png")).unwrap();
        images.request("image.png");
        assert!(!images.pending());
        assert!(images.image("image.png").is_some());
        images.context(root.path(), Some("second"));
        images.request("image.png");
        settle(&mut images).await;
        assert!(images.image("image.png").is_none());
        assert_eq!(images.status("image.png"), "local image unavailable");
        write_png(root.path(), "image.png");
        images.request("image.png");
        assert!(!images.pending());
        images.clear();
        images.request("image.png");
        settle(&mut images).await;
        assert!(images.image("image.png").is_some());
    }

    #[tokio::test]
    async fn source_byte_pressure_evicts_lru_and_revisited_images_recover() {
        let root = tempfile::tempdir().unwrap();
        let sources = ["first.png", "second.png", "third.png", "fourth.png"];
        for (index, source) in sources.iter().enumerate() {
            // Real, individually valid ~7 MiB PNGs, with distinct pixel content.
            write_large_png(root.path(), source, index as u8);
        }
        let mut images = MarkdownImages::new();
        images.context(root.path(), Some("session"));
        let mut encoded_bytes = 0;
        for source in &sources[..3] {
            images.request(source);
            settle(&mut images).await;
            encoded_bytes += images.image(source).unwrap().data.len();
            assert_eq!(images.retained, encoded_bytes);
            assert!(images.retained <= MAX_SOURCE_BYTES);
            assert_eq!(
                images.take_changed_sources(),
                HashSet::from([source.to_string()])
            );
        }

        // Visiting the first again makes the second the least recently used.
        images.request(sources[0]);
        images.request(sources[3]);
        settle(&mut images).await;
        assert!(images.image(sources[3]).is_some());
        assert!(images.image(sources[0]).is_some());
        assert!(images.image(sources[1]).is_none());
        assert!(images.image(sources[2]).is_some());
        assert_eq!(
            images.take_changed_sources(),
            HashSet::from([sources[1].to_owned(), sources[3].to_owned()])
        );
        assert_eq!(
            images.retained,
            sources
                .iter()
                .filter_map(|source| images.image(source))
                .map(|image| image.data.len())
                .sum::<usize>()
        );
        assert!(images.retained <= MAX_SOURCE_BYTES);

        // Scroll back without clearing the cache: reacquire the evicted source
        // and invalidate both the newly ready and newly evicted source layouts.
        images.request(sources[1]);
        settle(&mut images).await;
        assert!(images.image(sources[1]).is_some());
        assert!(images.image(sources[2]).is_none());
        assert_eq!(
            images.take_changed_sources(),
            HashSet::from([sources[1].to_owned(), sources[2].to_owned()])
        );
        assert_eq!(
            images.retained,
            sources
                .iter()
                .filter_map(|source| images.image(source))
                .map(|image| image.data.len())
                .sum::<usize>()
        );
        assert!(images.retained <= MAX_SOURCE_BYTES);
    }

    async fn settle_viewport(images: &mut MarkdownImages, sources: &[&str]) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                images.set_visible(sources.iter().copied());
                images.poll();
                for source in sources {
                    images.request(source);
                }
                if !images.pending() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn visible_source_byte_pressure_settles_and_recovers_on_viewport_change() {
        let root = tempfile::tempdir().unwrap();
        let sources = ["first.png", "second.png", "third.png", "fourth.png"];
        for (index, source) in sources.iter().enumerate() {
            write_large_png(root.path(), source, index as u8);
        }
        let mut images = MarkdownImages::new();
        images.context(root.path(), Some("session"));
        settle_viewport(&mut images, &sources).await;
        let ready: Vec<_> = sources
            .iter()
            .copied()
            .filter(|source| images.image(source).is_some())
            .collect();
        let deferred: Vec<_> = sources
            .iter()
            .copied()
            .filter(|source| images.status(source) == "image deferred (visible image budget)")
            .collect();
        assert_eq!(ready.len(), 3);
        assert_eq!(deferred.len(), 1);
        let key = images.key(deferred[0]).unwrap();
        images.take_changed_sources();
        for _ in 0..8 {
            images.set_visible(sources);
            assert!(!images.poll());
            for source in sources {
                images.request(source);
            }
            assert!(!images.pending());
            assert!(ready.iter().all(|source| images.image(source).is_some()));
            assert_eq!(images.key(deferred[0]), Some(key));
            assert!(images.take_changed_sources().is_empty());
        }
        // Shrinking the viewport (scroll or resize) releases offscreen snapshots.
        settle_viewport(&mut images, &deferred).await;
        assert!(images.image(deferred[0]).is_some());
        assert_eq!(images.key(deferred[0]), Some(key));
        settle_viewport(&mut images, &sources).await;
        for source in sources {
            images.request(source);
        }
        assert!(!images.pending());
        assert!(images.retained <= MAX_SOURCE_BYTES);
        images.context(root.path(), Some("replacement"));
        assert!(sources.iter().all(|source| images.key(source).is_none()));
        settle_viewport(&mut images, &sources).await;
        assert_eq!(
            sources
                .iter()
                .filter(|source| images.image(source).is_some())
                .count(),
            3
        );
        assert!(!images.pending());
    }

    #[tokio::test]
    async fn visible_entry_overflow_keeps_first_unique_sources_and_scroll_recovers() {
        let root = tempfile::tempdir().unwrap();
        let sources: Vec<_> = (0..MAX_ENTRIES + 2)
            .map(|index| format!("{index}.png"))
            .collect();
        for source in &sources {
            write_png(root.path(), source);
        }
        let viewport: Vec<_> = sources.iter().map(String::as_str).collect();
        let mut images = MarkdownImages::new();
        images.context(root.path(), Some("session"));
        settle_viewport(&mut images, &viewport).await;
        for _ in 0..8 {
            // Repeated occurrences do not consume extra admission slots.
            images.set_visible(viewport.iter().flat_map(|source| [*source, *source]));
            assert!(!images.poll());
            for (index, source) in sources.iter().enumerate() {
                assert_eq!(images.is_visible(source), index < MAX_ENTRIES);
                images.request(source);
                assert_eq!(images.image(source).is_some(), index < MAX_ENTRIES);
            }
            assert!(!images.pending());
        }
        settle_viewport(&mut images, &viewport[2..]).await;
        assert!(
            viewport[2..]
                .iter()
                .all(|source| images.image(source).is_some())
        );
        settle_viewport(&mut images, &viewport[..MAX_ENTRIES]).await;
        assert!(
            viewport[..MAX_ENTRIES]
                .iter()
                .all(|source| images.image(source).is_some())
        );
        assert!(images.retained <= MAX_SOURCE_BYTES);
    }

    #[tokio::test]
    async fn entry_pressure_preserves_active_acquisitions() {
        let root = tempfile::tempdir().unwrap();
        write_png(root.path(), "first.png");
        write_png(root.path(), "second.png");
        let mut images = MarkdownImages::new();
        images.context(root.path(), Some("session"));
        images.request("first.png");
        images.request("second.png");
        // Without polling, these entries remain Loading even if a worker has
        // finished. Fill the remaining entry slots, then request one more.
        for index in 0..MAX_ENTRIES - 1 {
            images.request(&format!("waiting-{index}.png"));
        }
        assert_eq!(images.entries.len(), MAX_ENTRIES);
        assert!(!images.entries.contains_key("waiting-0.png"));
        assert!(images.take_changed_sources().is_empty());
        settle(&mut images).await;
        assert!(images.image("first.png").is_some());
        assert!(images.image("second.png").is_some());
        assert_eq!(
            images.take_changed_sources(),
            HashSet::from(["first.png".to_owned(), "second.png".to_owned(),])
        );
    }

    #[tokio::test]
    async fn session_reset_discards_inflight_results() {
        let root = tempfile::tempdir().unwrap();
        write_png(root.path(), "image.png");
        let mut images = MarkdownImages::new();
        images.context(root.path(), Some("first"));
        images.request("image.png");
        images.context(root.path(), Some("second"));
        // No yield occurred after spawn: reset cancels the actual pending future,
        // not merely publication of a completed read into another session.
        let cancelled = images.jobs.join_next().await.unwrap().err().unwrap();
        assert!(cancelled.is_cancelled());
        assert!(images.image("image.png").is_none());
        images.request("image.png");
        settle(&mut images).await;
        assert!(images.image("image.png").is_some());
    }
}
