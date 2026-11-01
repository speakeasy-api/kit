use std::{path::PathBuf, time::Duration};

use agentkit_acp::{self as v1, v2::wire as v2};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Metadata {
    title: Option<String>,
    updated_at: String,
}

impl From<crate::session::CatalogEntry> for Metadata {
    fn from(entry: crate::session::CatalogEntry) -> Self {
        let updated_at = entry.updated_at_rfc3339();
        Self {
            title: entry.title,
            updated_at,
        }
    }
}

impl Metadata {
    pub(super) fn v1(&self) -> v1::SessionUpdate {
        v1::SessionUpdate::SessionInfoUpdate(
            v1::SessionInfoUpdate::new()
                .title(self.title.clone())
                .updated_at(self.updated_at.clone()),
        )
    }

    pub(super) fn v2(&self) -> v2::SessionUpdate {
        v2::SessionUpdate::SessionInfoUpdate(
            v2::SessionInfoUpdate::new()
                .title(self.title.clone())
                .updated_at(self.updated_at.clone()),
        )
    }
}

/// External renames do not go through the transcript writer. Refresh only the
/// attached session, using the catalog cache rather than scanning the workspace.
/// The actor owns this future: activation starts it, and close/abort drops it.
pub(super) async fn poll<E>(
    root: PathBuf,
    session_id: String,
    mut publish: impl FnMut(&Metadata) -> Result<(), E>,
) {
    let mut previous = None;
    loop {
        let root = root.clone();
        let id = session_id.clone();
        let result =
            tokio::task::spawn_blocking(move || crate::session::catalog_entry(&root, &id)).await;
        if let Ok(Ok(Some(entry))) = result {
            let current = Metadata::from(entry);
            if previous.as_ref() != Some(&current) {
                if publish(&current).is_err() {
                    return;
                }
                previous = Some(current);
            }
        }
        // A transient incomplete append/read must not clear known metadata.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use agentkit_core::{Item, ItemKind, Timestamp};
    use agentkit_loop::TranscriptObserver;

    #[test]
    fn both_protocols_send_explicit_title_clears_and_catalog_timestamps() {
        let metadata = Metadata {
            title: None,
            updated_at: "2026-07-12T09:30:00Z".into(),
        };
        for value in [
            serde_json::to_value(metadata.v1()).unwrap(),
            serde_json::to_value(metadata.v2()).unwrap(),
        ] {
            assert_eq!(value["sessionUpdate"], "session_info_update");
            assert!(value.get("title").unwrap().is_null());
            assert_eq!(value["updatedAt"], metadata.updated_at);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn session_info_tracks_activity_external_rename_and_stops_with_owner() {
        let root = tempfile::tempdir().unwrap();
        let opened = crate::session::open(
            root.path(),
            "metadata-live",
            false,
            false,
            vec![Item::text(ItemKind::System, "System")],
        )
        .unwrap();
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(poll(
            root.path().into(),
            "metadata-live".into(),
            move |info| send.send(info.clone()),
        ));
        let initial = receive.recv().await.unwrap();
        let catalog = crate::session::catalog(root.path()).unwrap().pop().unwrap();
        assert_eq!(initial, Metadata::from(catalog.clone()));
        assert_eq!(initial.title, None);

        let item = Item::text(ItemKind::User, "First user prompt")
            .with_created_at(Timestamp(catalog.updated_at + 10_000));
        opened
            .observer
            .on_transcript_replaced(&"metadata-live".into(), &[item]);
        let changed = receive.recv().await.unwrap();
        assert_eq!(changed.title.as_deref(), Some("First user prompt"));
        assert_ne!(changed.updated_at, initial.updated_at);
        assert_eq!(
            changed,
            Metadata::from(crate::session::catalog(root.path()).unwrap().pop().unwrap())
        );

        crate::session::set_display_name(root.path(), "metadata-live", Some("Renamed")).unwrap();
        let renamed = receive.recv().await.unwrap();
        assert_eq!(renamed.title.as_deref(), Some("Renamed"));
        assert_eq!(renamed.updated_at, changed.updated_at);
        crate::session::set_display_name(root.path(), "metadata-live", None).unwrap();
        assert_eq!(receive.recv().await.unwrap(), changed);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(receive.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn session_info_stops_after_failed_delivery() {
        let root = tempfile::tempdir().unwrap();
        let _opened = crate::session::open(
            root.path(),
            "metadata-closed",
            false,
            false,
            vec![Item::text(ItemKind::User, "Title")],
        )
        .unwrap();
        poll(root.path().into(), "metadata-closed".into(), |_| Err(())).await;
    }
}
