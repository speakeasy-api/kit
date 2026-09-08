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
    Loading(tokio::task::Id),
    Ready(MediaImage),
    Failed(String),
}

struct Entry {
    state: State,
    used: u64,
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
        self.retained = 0;
        // Stop old async stages, including requests following DNS. Actual blocking
        // closures retain global admission until completion even after this abort.
        self.jobs.abort_all();
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
            let Some(entry) = self.entries.get_mut(&done.source) else {
                continue;
            };
            entry.state = match done.image {
                Ok(image) if self.retained.saturating_add(image.data.len()) <= MAX_SOURCE_BYTES => {
                    self.retained += image.data.len();
                    self.changed_sources.insert(done.source);
                    State::Ready(image)
                }
                Ok(_) => State::Failed("image source budget exceeded".into()),
                Err(error) => State::Failed(error),
            };
            changed = true;
        }
        changed
    }

    pub fn request(&mut self, source: &str) {
        if source.len() > 4096 {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        if !self.entries.contains_key(source) {
            if self.entries.len() == MAX_ENTRIES {
                let oldest = self
                    .entries
                    .iter()
                    .filter(|(_, entry)| !matches!(entry.state, State::Loading(_)))
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(source, _)| source.clone());
                let Some(oldest) = oldest else { return };
                if let Some(Entry {
                    state: State::Ready(image),
                    ..
                }) = self.entries.remove(&oldest)
                {
                    self.retained = self.retained.saturating_sub(image.data.len());
                    self.changed_sources.insert(oldest);
                }
            }
            self.entries.insert(
                source.to_owned(),
                Entry {
                    state: State::Waiting,
                    used: self.clock,
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
