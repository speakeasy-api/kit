//! Ephemeral bridge recovery protocol. Admission fences precede FIFO application.
use super::{ActiveSessionRoute, App, QueuedUpdate, Update, UpdateSessionNotification, translate};
use serde::Deserialize;
use std::io::Write;
use std::sync::{Arc, Mutex};
pub(super) const META: &str = "kit/gatewayRecovery";
pub(super) const MAX_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_EVENTS: usize = 4096;
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Kind {
    Begin,
    Replay,
    Commit,
    Failed,
    Live,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Marker {
    pub epoch: u64,
    pub kind: Kind,
    pub attempt: Option<u32>,
    pub max_attempts: Option<u32>,
    pub delay_ms: Option<u64>,
    pub message: Option<String>,
    pub history_available: Option<bool>,
    pub state_snapshot: Option<bool>,
    pub config_snapshot: Option<bool>,
}
#[derive(Debug)]
pub(super) struct Event {
    pub marker: Marker,
    pub session: String,
    pub bytes: usize,
    pub updates: Vec<Update>,
}
/// Callback-only writes, event-loop read-only admission. Lock order: ingress
/// then route. No other owner takes ingress with route held. Guards never cross
/// send/await/application or App destruction. Poison fails closed.
pub(super) struct Ingress {
    epoch: u64,
    begun_epoch: Option<u64>,
    recovering: bool,
    session: Option<String>,
    bytes: usize,
    events: usize,
}
impl Default for Ingress {
    fn default() -> Self {
        Self {
            epoch: 1,
            begun_epoch: None,
            recovering: false,
            session: None,
            bytes: 0,
            events: 0,
        }
    }
}
impl Ingress {
    pub fn for_session(session: Option<String>) -> Self {
        Self {
            session,
            ..Self::default()
        }
    }

    pub fn confirm_session(&mut self, session: &str) {
        self.session = Some(session.to_owned());
    }

    pub fn available(&self, app: &App) -> bool {
        !self.recovering && !app.gateway_blocked && self.epoch == app.gateway_epoch
    }
}
struct Size(usize);
impl Write for Size {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_BYTES {
            return Err(std::io::Error::other("replay limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn notification(
    notification: UpdateSessionNotification,
    ingress: &Arc<Mutex<Ingress>>,
    route: &Arc<Mutex<ActiveSessionRoute>>,
) -> Option<QueuedUpdate> {
    let session = notification.session_id.to_string();
    let mut size = Size(0);
    let sized = serde_json::to_writer(&mut size, &notification).is_ok();
    let marker = notification.meta.as_ref().and_then(|meta| meta.get(META));
    let parsed = marker.and_then(|marker| serde_json::from_value::<Marker>(marker.clone()).ok());
    let mut ingress = ingress.lock().ok()?;
    let mut route = route.lock().ok()?;
    if ingress.session.as_ref().is_some_and(|id| id != &session) {
        return None;
    }
    let mut marker = match parsed.filter(|marker| marker.epoch > 0) {
        Some(marker) => marker,
        None => Marker {
            epoch: ingress.epoch,
            kind: Kind::Failed,
            attempt: None,
            max_attempts: None,
            delay_ms: None,
            message: Some("Invalid gateway recovery metadata".into()),
            history_available: None,
            state_snapshot: None,
            config_snapshot: None,
        },
    };
    if marker.epoch < ingress.epoch {
        return None;
    }
    match marker.kind {
        Kind::Begin => {
            if ingress
                .begun_epoch
                .is_some_and(|epoch| marker.epoch <= epoch)
            {
                return None;
            }
            ingress.epoch = marker.epoch;
            ingress.begun_epoch = Some(marker.epoch);
            ingress.recovering = true;
            ingress.bytes = 0;
            ingress.events = 0;
            ingress.session = Some(session.clone());
            route.generation = route.generation.checked_add(1)?;
        }
        Kind::Live if marker.epoch == ingress.epoch && !ingress.recovering => {}
        Kind::Replay if marker.epoch == ingress.epoch && ingress.recovering => {
            ingress.bytes = ingress.bytes.saturating_add(size.0);
            ingress.events = ingress.events.saturating_add(1);
            if !sized || ingress.bytes > MAX_BYTES || ingress.events > MAX_EVENTS {
                marker.kind = Kind::Failed;
                marker.message = Some("Gateway replay exceeds the safe replay limit".into());
            }
        }
        Kind::Commit if marker.epoch == ingress.epoch && ingress.recovering => {
            // App independently validates the snapshot and keeps its gate shut
            // on failure. Epoch alone never authorizes user input.
            ingress.recovering = false;
        }
        Kind::Failed if marker.epoch == ingress.epoch => {
            if !ingress.recovering {
                route.generation = route.generation.checked_add(1)?;
            }
            ingress.recovering = true;
        }
        _ => return None,
    }
    let generation = route.generation;
    drop(route);
    drop(ingress);
    let updates = if matches!(marker.kind, Kind::Replay | Kind::Live) {
        translate(notification).1
    } else {
        Vec::new()
    };
    Some(QueuedUpdate::for_session(
        generation,
        Update::GatewayRecovery(Event {
            marker,
            session,
            bytes: size.0,
            updates,
        }),
    ))
}
pub(super) fn submission_error(error: &agent_client_protocol::Error) -> String {
    if error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(|value| value.as_str())
        == Some("outcome_unknown")
    {
        "Message acceptance is unknown; it was not retried. Review the recovered transcript before submitting again.".into()
    } else {
        format!("message was not accepted: {}", error.message)
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
    use crate::tui::{BackgroundCompletion, accept_queued_update, spawn_background_workers, wire};
    use base64::Engine;

    fn marked(kind: &str, epoch: u64, update: wire::SessionUpdate) -> UpdateSessionNotification {
        let mut notification = UpdateSessionNotification::new("session", update);
        notification.meta = Some(
            serde_json::from_value(serde_json::json!({ META: {
                "kind": kind, "epoch": epoch, "attempt": 1, "maxAttempts": 5,
                "historyAvailable": true, "configSnapshot": true, "stateSnapshot": true
            }}))
            .unwrap(),
        );
        notification
    }
    fn state() -> wire::SessionUpdate {
        wire::SessionUpdate::StateUpdate(
            wire::StateUpdate::Running(wire::RunningStateUpdate::new()),
        )
    }
    fn route() -> Arc<Mutex<ActiveSessionRoute>> {
        Arc::new(Mutex::new(ActiveSessionRoute {
            id: "session".into(),
            generation: 0,
        }))
    }
    #[test]
    fn begin_fences_queued_updates_and_old_epoch_notifications() {
        let route = route();
        let ingress = Arc::new(Mutex::new(Ingress::default()));
        let old = QueuedUpdate::for_session(0, Update::ConfigOptions(Vec::new()));
        let begin = notification(marked("begin", 2, state()), &ingress, &route).unwrap();
        assert!(accept_queued_update(&route, old).is_none());
        assert!(accept_queued_update(&route, begin).is_some());
        assert!(notification(marked("live", 1, state()), &ingress, &route).is_none());
        assert!(notification(marked("begin", 2, state()), &ingress, &route).is_none());
    }
    #[test]
    fn malformed_marker_is_not_translated_as_live_state() {
        let route = route();
        let ingress = Arc::new(Mutex::new(Ingress::default()));
        let mut bad = marked("live", 1, state());
        bad.meta
            .as_mut()
            .unwrap()
            .insert(META.into(), serde_json::json!({"epoch":"1","kind":"live"}));
        let queued = notification(bad, &ingress, &route).unwrap();
        assert!(
            matches!(queued.update, Update::GatewayRecovery(Event { marker: Marker {kind: Kind::Failed, ..}, updates, .. }) if updates.is_empty())
        );
    }
    #[test]
    fn outcome_unknown_does_not_claim_rejection_or_authorize_retry() {
        let mut error = agent_client_protocol::util::internal_error("lost request");
        error.data =
            Some(serde_json::json!({"reason":"outcome_unknown", "method":"session/prompt"}));
        let message = submission_error(&error);
        assert!(message.contains("acceptance is unknown"));
        assert!(message.contains("not retried"));
        assert!(!message.contains("not accepted"));
    }
    #[test]
    fn poisoned_route_isolates_recovery_and_remote_mutations() {
        let route = route();
        let ingress = Arc::new(Mutex::new(Ingress::default()));
        let _ = std::panic::catch_unwind(|| {
            let _guard = route.lock().unwrap();
            panic!("poison route at ownership boundary");
        });
        let mut app = App::new(
            "/tmp/kit".into(),
            "provider".into(),
            "model".into(),
            String::new(),
        );
        app.gateway_epoch = 1;
        assert!(!crate::tui::recovery_available(&ingress, &route, &app));
        assert!(notification(marked("begin", 2, state()), &ingress, &route).is_none());
    }

    #[test]
    fn recovery_enter_preserves_draft_without_deferring_a_submission() {
        let mut app = App::new(
            "/tmp/kit".into(),
            "provider".into(),
            "model".into(),
            String::new(),
        );
        app.paste("unsent");
        app.gateway_blocked = true;
        let mut pastes = crate::tui::ClipboardPastes::default();
        let action = crate::tui::handle_with_clipboard(
            &mut app,
            &mut pastes,
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
        );
        assert!(matches!(action, crate::tui::Action::None));
        assert_eq!(app.editor.text(), "unsent");
        assert!(pastes.pending.is_none());
    }

    #[tokio::test]
    async fn image_worker_keeps_commit_after_replay_and_installs_once() {
        let route = route();
        let ingress = Arc::new(Mutex::new(Ingress::default()));
        let (completed, mut completions) = tokio::sync::mpsc::channel(1);
        let workers = spawn_background_workers(completed).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let image = crate::tui::app::UserImage::new(
            base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
            "image/png".into(),
            0,
        )
        .unwrap();
        let begin = notification(marked("begin", 2, state()), &ingress, &route).unwrap();
        assert!(workers.try_update(begin).is_ok());
        let mut replay = notification(marked("replay", 2, state()), &ingress, &route).unwrap();
        let Update::GatewayRecovery(event) = &mut replay.update else {
            panic!("recovery update");
        };
        event.updates = vec![Update::UserMessage {
            id: "user".into(),
            text: "replayed".into(),
            images: vec![image],
            append: false,
        }];
        assert!(workers.try_update(replay).is_ok());
        for update in [
            marked(
                "replay",
                2,
                wire::SessionUpdate::ConfigOptionUpdate(wire::ConfigOptionUpdate::new(Vec::new())),
            ),
            marked("replay", 2, state()),
            marked("commit", 2, state()),
        ] {
            assert!(
                workers
                    .try_update(notification(update, &ingress, &route).unwrap())
                    .is_ok()
            );
        }
        let mut app = App::new(
            "/tmp/kit".into(),
            "provider".into(),
            "model".into(),
            String::new(),
        );
        app.start_session("session".into());
        app.gateway_epoch = 1;
        app.note("old transcript");
        app.paste("unsent");
        let mut kinds = Vec::new();
        for _ in 0..5 {
            let completion =
                tokio::time::timeout(std::time::Duration::from_secs(5), completions.recv())
                    .await
                    .unwrap()
                    .unwrap();
            let BackgroundCompletion::Update { queued, images } = completion else {
                panic!("update completion");
            };
            let Update::GatewayRecovery(event) = &queued.update else {
                panic!("recovery update");
            };
            let kind = event.marker.kind;
            kinds.push(kind);
            if matches!(event.updates.first(), Some(Update::UserMessage { .. })) {
                assert_eq!(images.len(), 1);
            }
            app.apply_materialized(accept_queued_update(&route, queued).unwrap(), images);
            if kind != Kind::Commit {
                assert!(
                    matches!(app.blocks.as_slice(), [crate::tui::app::Block::Notice(text)] if text == "old transcript")
                );
            }
        }
        assert_eq!(
            kinds,
            [
                Kind::Begin,
                Kind::Replay,
                Kind::Replay,
                Kind::Replay,
                Kind::Commit
            ]
        );
        assert!(matches!(
            app.blocks.as_slice(),
            [crate::tui::app::Block::User(_)]
        ));
        assert_eq!(app.editor.text(), "unsent");
        assert!(!app.gateway_blocked);
        drop(completions);
        drop(workers);
    }
}
