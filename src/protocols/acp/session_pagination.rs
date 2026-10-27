//! Connection-local snapshots. Offset-only cursors cannot identify a snapshot.
//! TTL limits cursor validity; expired allocations are reclaimed lazily on insertion
//! or connection teardown, always subject to the count and byte bounds.
use crate::session::CatalogEntry;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
const MAX_SNAPSHOTS: usize = 8;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(300);
#[derive(Debug)]
pub(super) enum Error {
    InvalidCursor,
    Unavailable,
}
pub(super) struct Cursor {
    id: String,
    offset: usize,
}
impl Cursor {
    pub(super) fn parse(value: &str) -> Result<Self, Error> {
        let mut parts = value.split(':');
        let version = parts.next();
        let id = parts.next().filter(|id| !id.is_empty());
        let offset = parts.next().and_then(|offset| offset.parse::<usize>().ok());
        match (version, id, offset, parts.next()) {
            (Some("snapshot-v1"), Some(id), Some(offset), None) if offset > 0 => Ok(Self {
                id: id.into(),
                offset,
            }),
            _ => Err(Error::InvalidCursor),
        }
    }
}
struct Snapshot {
    id: String,
    root: PathBuf,
    entries: Arc<Vec<CatalogEntry>>,
    created: Instant,
    bytes: usize,
}
#[derive(Default)]
pub(super) struct CatalogPages {
    // Only start writes: expiry/eviction followed by insertion. No other locks,
    // awaits or callbacks under this guard. Poison isolates this cache; it is
    // never recovered and does not poison other server state or connections.
    snapshots: Mutex<VecDeque<Snapshot>>,
}
pub(super) struct Page {
    pub(super) entries: Arc<Vec<CatalogEntry>>,
    pub(super) range: std::ops::Range<usize>,
    pub(super) next: Option<String>,
}
impl CatalogPages {
    pub(super) fn start(
        &self,
        root: PathBuf,
        entries: Vec<CatalogEntry>,
        size: usize,
    ) -> Result<Page, Error> {
        if entries.len() <= size {
            return Ok(Page {
                range: 0..entries.len(),
                entries: Arc::new(entries),
                next: None,
            });
        }
        let bytes = retained_bytes(root.capacity(), &entries, entries.capacity());
        let snapshot = Snapshot {
            id: crate::session::new_id(),
            root,
            entries: Arc::new(entries),
            created: Instant::now(),
            bytes,
        };
        let page = snapshot.page(0, size);
        // Never issue an unusable continuation for an oversized catalog.
        if bytes > MAX_BYTES {
            return Err(Error::Unavailable);
        }
        let mut retired = Vec::with_capacity(MAX_SNAPSHOTS);
        let mut snapshots = self.snapshots.lock().map_err(|_| Error::Unavailable)?;
        while snapshots
            .front()
            .is_some_and(|old| old.created.elapsed() >= TTL)
            || snapshots.len() >= MAX_SNAPSHOTS
            || snapshots.iter().map(|s| s.bytes).sum::<usize>() > MAX_BYTES - bytes
        {
            if let Some(old) = snapshots.pop_front() {
                retired.push(old);
            } else {
                break;
            }
        }
        snapshots.push_back(snapshot);
        drop(snapshots);
        drop(retired);
        Ok(page)
    }
    pub(super) fn resume(&self, root: &Path, cursor: Cursor, size: usize) -> Result<Page, Error> {
        let snapshots = self.snapshots.lock().map_err(|_| Error::InvalidCursor)?;
        let snapshot = snapshots
            .iter()
            .find(|s| s.id == cursor.id && s.root == root && s.created.elapsed() < TTL)
            .ok_or(Error::InvalidCursor)?;
        if cursor.offset >= snapshot.entries.len() || !cursor.offset.is_multiple_of(size) {
            return Err(Error::InvalidCursor);
        }
        Ok(snapshot.page(cursor.offset, size))
    }
}
impl Snapshot {
    fn page(&self, offset: usize, size: usize) -> Page {
        let end = self.entries.len().min(offset.saturating_add(size));
        Page {
            entries: Arc::clone(&self.entries),
            range: offset..end,
            next: (end < self.entries.len()).then(|| format!("snapshot-v1:{}:{end}", self.id)),
        }
    }
}
fn retained_bytes(root_capacity: usize, entries: &[CatalogEntry], capacity: usize) -> usize {
    // Include allocated capacity, not merely text length, and path storage.
    std::mem::size_of::<Snapshot>()
        + root_capacity
        + 128
        + capacity * std::mem::size_of::<CatalogEntry>()
        + entries
            .iter()
            .map(|e| {
                e.id.capacity()
                    + e.title.as_ref().map_or(0, String::capacity)
                    + e.preview.as_ref().map_or(0, String::capacity)
                    + e.additional_directories.capacity() * std::mem::size_of::<PathBuf>()
                    + e.additional_directories
                        .iter()
                        .map(PathBuf::capacity)
                        .sum::<usize>()
            })
            .sum::<usize>()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
mod tests {
    use super::*;
    fn entries(prefix: &str) -> Vec<CatalogEntry> {
        (0..3)
            .map(|i| CatalogEntry {
                id: format!("{prefix}{i}"),
                title: None,
                preview: None,
                is_subagent: false,
                additional_directories: vec![],
                updated_at: 0,
            })
            .collect()
    }
    fn resume(cache: &CatalogPages, root: &str, cursor: &str) -> Result<Page, Error> {
        cache.resume(Path::new(root), Cursor::parse(cursor)?, 1)
    }
    #[test]
    fn session_list_snapshots_preserve_mutated_catalog_and_independent_roots() {
        let cache = CatalogPages::default();
        let original = cache
            .start("a".into(), entries("old"), 1)
            .unwrap()
            .next
            .unwrap();
        let fresh = cache
            .start("a".into(), entries("new"), 1)
            .unwrap()
            .next
            .unwrap();
        let other = cache
            .start("b".into(), entries("other"), 1)
            .unwrap()
            .next
            .unwrap();
        for (root, cursor, expected) in [
            ("a", &original, "old1"),
            ("a", &fresh, "new1"),
            ("b", &other, "other1"),
        ] {
            let page = resume(&cache, root, cursor).unwrap();
            assert_eq!(page.entries[page.range][0].id, expected);
        }
        assert!(matches!(
            resume(&cache, "b", &original),
            Err(Error::InvalidCursor)
        ));
        assert!(matches!(
            resume(&CatalogPages::default(), "a", &original),
            Err(Error::InvalidCursor)
        ));
    }
    #[test]
    fn session_list_retention_bounds_and_stale_cursors() {
        let cache = CatalogPages::default();
        let first = cache
            .start("a".into(), entries("old"), 1)
            .unwrap()
            .next
            .unwrap();
        for _ in 0..MAX_SNAPSHOTS {
            cache.start("a".into(), entries("new"), 1).unwrap();
        }
        assert_eq!(cache.snapshots.lock().unwrap().len(), MAX_SNAPSHOTS);
        assert!(matches!(
            resume(&cache, "a", &first),
            Err(Error::InvalidCursor)
        ));
        let current = cache
            .start("a".into(), entries("current"), 1)
            .unwrap()
            .next
            .unwrap();
        for snapshot in cache.snapshots.lock().unwrap().iter_mut() {
            snapshot.created = Instant::now() - TTL;
        }
        assert!(matches!(
            resume(&cache, "a", &current),
            Err(Error::InvalidCursor)
        ));
        cache.start("a".into(), entries("fresh"), 1).unwrap();
        assert_eq!(cache.snapshots.lock().unwrap().len(), 1);
    }
    #[test]
    fn session_list_memory_budget_rejects_oversize_without_invalidating_existing() {
        let cache = CatalogPages::default();
        let first = cache
            .start("a".into(), entries("old"), 1)
            .unwrap()
            .next
            .unwrap();
        let mut large = entries("large");
        large[0].preview = Some("x".repeat(MAX_BYTES));
        assert!(matches!(
            cache.start("a".into(), large, 1),
            Err(Error::Unavailable)
        ));
        assert!(resume(&cache, "a", &first).is_ok());
        for _ in 0..3 {
            let mut medium = entries("medium");
            medium[0].preview = Some("x".repeat(MAX_BYTES / 2));
            cache.start("a".into(), medium, 1).unwrap();
        }
        let snapshots = cache.snapshots.lock().unwrap();
        assert!(snapshots.iter().map(|s| s.bytes).sum::<usize>() <= MAX_BYTES);
        assert_eq!(snapshots.len(), 1);
    }
    #[test]
    fn session_list_poison_is_isolated_to_one_cache() {
        let cache = CatalogPages::default();
        let first = cache
            .start("a".into(), entries("old"), 1)
            .unwrap()
            .next
            .unwrap();
        let _ = std::panic::catch_unwind(|| {
            let _guard = cache.snapshots.lock().unwrap();
            panic!("poison");
        });
        assert!(matches!(
            resume(&cache, "a", &first),
            Err(Error::InvalidCursor)
        ));
        assert!(matches!(
            cache.start("a".into(), entries("new"), 1),
            Err(Error::Unavailable)
        ));
        let other = CatalogPages::default();
        let cursor = other
            .start("a".into(), entries("other"), 1)
            .unwrap()
            .next
            .unwrap();
        assert!(resume(&other, "a", &cursor).is_ok());
    }
    #[test]
    fn session_list_invalid_cursor_offsets() {
        let cache = CatalogPages::default();
        let cursor = cache
            .start("a".into(), entries("old"), 1)
            .unwrap()
            .next
            .unwrap();
        let prefix = cursor.rsplit_once(':').unwrap().0;
        for suffix in ["0", "3", "18446744073709551615", "nope", "1:extra"] {
            assert!(matches!(
                resume(&cache, "a", &format!("{prefix}:{suffix}")),
                Err(Error::InvalidCursor)
            ));
        }
        assert!(Cursor::parse("offset:1").is_err());
    }
}
