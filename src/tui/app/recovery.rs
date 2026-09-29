//! Single-owner transactional replay: visible App never observes a partial cut.
use super::*;
use crate::tui::recovery::{Event, Kind, MAX_BYTES, MAX_EVENTS};

pub(super) struct Candidate {
    app: Box<App>,
    epoch: u64,
    session: String,
    bytes: usize,
    events: usize,
    config: bool,
    state_after_config: bool,
}

impl App {
    fn recovery_failed(&mut self, message: &str) {
        self.gateway_candidate = None;
        self.gateway_blocked = true;
        self.gateway_status = Some(format!(
            "Recovery failed: {message}. Outcome unknown; do not retry blindly. Use kit gateway list and --remote-session; --remote-no-replay explicitly omits history."
        ));
    }

    pub(super) fn apply_recovery(&mut self, event: Event, images: Vec<MaterializedImage>) {
        let Event {
            marker,
            session,
            bytes,
            updates,
        } = event;
        if marker.epoch < self.gateway_epoch {
            return;
        }
        match marker.kind {
            Kind::Begin => {
                if marker.epoch == self.gateway_epoch && self.gateway_candidate.is_some() {
                    return;
                }
                if self.session_id.as_ref().is_some_and(|id| id != &session) {
                    return;
                }
                let mut candidate = App::new(
                    self.root.clone(),
                    self.provider.clone(),
                    self.model.clone(),
                    self.a2a.clone(),
                );
                candidate.session_id = Some(session.clone());
                candidate.can_steer = self.can_steer;
                candidate.can_replace_steer = self.can_replace_steer;
                self.gateway_candidate = Some(Candidate {
                    app: Box::new(candidate),
                    epoch: marker.epoch,
                    session,
                    bytes: 0,
                    events: 0,
                    config: false,
                    state_after_config: false,
                });
                self.gateway_epoch = marker.epoch;
                self.gateway_blocked = true;
                self.model_switch = None;
                self.model_dialog = None;
                self.effort_dialog = None;
                self.cancel_clipboard_placeholders();
                // The replacement target belongs to the old connection, but
                // both unsent drafts belong to the client. Restore the ordinary
                // composer and keep the edited text available for manual recall.
                let steering_draft = self
                    .steer_edit
                    .as_ref()
                    .map(|_| self.editor.text().to_owned());
                self.cancel_steer_edit();
                if let Some(text) = steering_draft {
                    self.editor.remember(text);
                }
                self.selected_steer = None;
                self.queue_focused = false;
                self.queue_handoff = false;
                self.steer_mutations.clear();
                self.gateway_status = Some(format!(
                    "Reconnecting {}/{}{} · previous outcome unknown; no automatic resubmission",
                    marker.attempt.unwrap_or(1),
                    marker.max_attempts.unwrap_or(1),
                    marker
                        .delay_ms
                        .map(|ms| format!(" (retry in {ms}ms)"))
                        .unwrap_or_default(),
                ));
            }
            Kind::Replay => {
                let Some(candidate) = self.gateway_candidate.as_mut().filter(|candidate| {
                    candidate.epoch == marker.epoch && candidate.session == session
                }) else {
                    return;
                };
                candidate.bytes = candidate.bytes.saturating_add(bytes);
                candidate.events = candidate.events.saturating_add(1);
                if candidate.bytes > MAX_BYTES || candidate.events > MAX_EVENTS {
                    self.recovery_failed("replay limit exceeded");
                    return;
                }
                for image in images {
                    candidate.app.attachment_cache.admit(image);
                }
                let follows_config = candidate.config;
                candidate.config = matches!(updates.as_slice(), [Update::ConfigOptions(_)]);
                candidate.state_after_config =
                    follows_config && matches!(updates.as_slice(), [Update::State(_)]);
                for update in updates {
                    if let Update::ConfigOptions(options) = &update {
                        crate::tui::refresh_config_state(&mut candidate.app, Some(options));
                    }
                    // Historical state has no live observation timestamp and
                    // never reaches voice or deferred-submit observers.
                    candidate.app.apply_at(update, None);
                }
            }
            Kind::Commit => {
                if !self.gateway_blocked && self.gateway_candidate.is_none() {
                    return;
                }
                if marker.epoch != self.gateway_epoch {
                    return;
                }
                let valid = self.gateway_candidate.as_ref().is_some_and(|candidate| {
                    candidate.epoch == marker.epoch
                        && candidate.session == session
                        && candidate.state_after_config
                }) && marker.config_snapshot == Some(true)
                    && marker.state_snapshot == Some(true)
                    && marker.history_available.is_some();
                if !valid {
                    self.recovery_failed("incomplete or mismatched snapshot");
                    return;
                }
                let Some(candidate) = self.gateway_candidate.take() else {
                    self.recovery_failed("missing validated snapshot");
                    return;
                };
                let mut candidate = candidate.app;
                candidate.gateway_epoch = marker.epoch;
                candidate.gateway_status = (marker.history_available == Some(false)).then(||
                    "History unavailable · transcript omitted by explicit --remote-no-replay; current state is synchronized".into());
                // Preserve client-owned state AND attachment lifetime owners.
                // All transcript indexes/caches/runtime state stay with the
                // candidate. No callbacks, locks or awaits occur in this commit.
                macro_rules! retain { ($($field:ident),* $(,)?) => { $(
                    std::mem::swap(&mut candidate.$field, &mut self.$field);
                )* }; }
                retain!(
                    editor,
                    attachments,
                    retained_attachment_files,
                    next_attachment,
                    submitted_attachment,
                    clipboard_route_epoch,
                    auth_methods,
                    voice_enabled,
                    can_steer,
                    can_replace_steer,
                    show_thoughts,
                    agents_visible,
                    show_logs,
                    logs,
                    storage_pending,
                    storage_exhausted,
                    next_model_switch,
                    next_steer_token,
                    next_file_search_revision
                );
                candidate.next_attachment = candidate.next_attachment.max(self.next_attachment);
                candidate.follow = self.follow;
                candidate.scroll = if self.follow { usize::MAX } else { 0 };
                *self = *candidate;
            }
            Kind::Failed => {
                if marker.epoch != self.gateway_epoch && self.gateway_epoch != 0 {
                    return;
                }
                self.gateway_epoch = marker.epoch;
                self.recovery_failed(marker.message.as_deref().unwrap_or("gateway unavailable"));
            }
            Kind::Live => {
                if self.gateway_blocked
                    || marker.epoch != self.gateway_epoch
                    || self.session_id.as_deref() != Some(session.as_str())
                {
                    return;
                }
                for image in images {
                    self.attachment_cache.admit(image);
                }
                for update in updates {
                    if let Update::ConfigOptions(options) = &update {
                        crate::tui::refresh_config_state(self, Some(options));
                    }
                    self.apply(update);
                }
            }
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
    use crate::tui::recovery::Marker;
    use agent_client_protocol::schema::v2::{IdleStateUpdate, RunningStateUpdate};

    fn app() -> App {
        let mut app = App::new(
            "/tmp/kit".into(),
            "provider".into(),
            "old".into(),
            String::new(),
        );
        app.start_session("session".into());
        app.gateway_epoch = 1;
        app.apply(Update::AgentMessage {
            id: "old".into(),
            text: "old transcript".into(),
            append: false,
        });
        app.paste("unsent draft");
        app
    }
    fn event(kind: Kind, updates: Vec<Update>) -> Event {
        Event {
            marker: Marker {
                epoch: 2,
                kind,
                attempt: Some(2),
                max_attempts: Some(5),
                delay_ms: None,
                message: None,
                history_available: Some(true),
                state_snapshot: Some(true),
                config_snapshot: Some(true),
            },
            session: "session".into(),
            bytes: 100,
            updates,
        }
    }
    fn replay(app: &mut App, update: Update) {
        app.apply(Update::GatewayRecovery(event(Kind::Replay, vec![update])));
    }
    fn snapshot(app: &mut App, running: bool) {
        use agent_client_protocol::schema::v2::{
            SessionConfigOption, SessionConfigSelectGroup, SessionConfigSelectOption,
        };
        let model = if running {
            "running-model"
        } else {
            "idle-model"
        };
        let value = format!("openrouter:{model}");
        replay(
            app,
            Update::ConfigOptions(vec![SessionConfigOption::select(
                "model",
                "Model",
                value.clone(),
                vec![SessionConfigSelectGroup::new(
                    "openrouter",
                    "OpenRouter",
                    vec![SessionConfigSelectOption::new(value, model)],
                )],
            )]),
        );
        replay(
            app,
            Update::State(if running {
                StateUpdate::Running(RunningStateUpdate::new())
            } else {
                StateUpdate::Idle(IdleStateUpdate::new())
            }),
        );
    }
    fn text(app: &App) -> String {
        app.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Agent(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn active_modified_steer(app: &mut App) {
        app.can_steer = true;
        app.can_replace_steer = true;
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        app.apply(Update::SteerAccepted {
            editable: true,
            id: "old-target".into(),
            text: "queued steering".into(),
        });
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.editing_steer());
        app.paste(" modified but unsent");
        assert_eq!(app.editor.text(), "queued steering modified but unsent");
    }

    fn assert_both_drafts_recallable(app: &mut App) {
        assert!(!app.editing_steer());
        assert_eq!(app.editor.text(), "unsent draft");
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Action::None
        ));
        assert_eq!(app.editor.text(), "queued steering modified but unsent");
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Action::None
        ));
        assert_eq!(app.editor.text(), "unsent draft");
    }

    #[test]
    fn failed_recovery_preserves_active_modified_steer_and_ordinary_draft() {
        let mut app = app();
        active_modified_steer(&mut app);
        let before = text(&app);
        app.apply(Update::GatewayRecovery(event(Kind::Begin, Vec::new())));
        assert_both_drafts_recallable(&mut app);
        app.apply(Update::GatewayRecovery(event(Kind::Failed, Vec::new())));
        assert!(app.gateway_blocked);
        assert_eq!(text(&app), before);
        assert_eq!(app.pending_steers[0].text, "queued steering");
        assert_both_drafts_recallable(&mut app);
    }

    #[test]
    fn committed_recovery_preserves_active_modified_steer_without_old_target() {
        for running in [false, true] {
            let mut app = app();
            active_modified_steer(&mut app);
            app.apply(Update::GatewayRecovery(event(Kind::Begin, Vec::new())));
            // Even if the same target survives replay, the unsent edit must not
            // retain replacement authority or be automatically submitted.
            replay(
                &mut app,
                Update::SteerAccepted {
                    editable: true,
                    id: "old-target".into(),
                    text: "queued steering".into(),
                },
            );
            snapshot(&mut app, running);
            app.apply(Update::GatewayRecovery(event(Kind::Commit, Vec::new())));
            assert!(!app.gateway_blocked);
            if running {
                assert_eq!(app.pending_steers[0].text, "queued steering");
            } else {
                assert!(app.pending_steers.is_empty());
            }
            assert_both_drafts_recallable(&mut app);
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            app.last_key = None;
            assert!(matches!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit { prompt, .. }
                    if prompt.text == "queued steering modified but unsent"
            ));
        }
    }

    #[test]
    fn failed_and_invalid_replays_preserve_visible_transcript_and_draft() {
        for failure in [Kind::Failed, Kind::Commit] {
            let mut app = app();
            let old = text(&app);
            app.apply(Update::GatewayRecovery(event(Kind::Begin, Vec::new())));
            replay(
                &mut app,
                Update::AgentMessage {
                    id: "new".into(),
                    text: "partial".into(),
                    append: false,
                },
            );
            assert_eq!(text(&app), old);
            app.apply(Update::GatewayRecovery(event(failure, Vec::new())));
            assert_eq!(text(&app), old);
            assert_eq!(app.editor.text(), "unsent draft");
            assert!(app.gateway_blocked);
        }
    }
    #[test]
    fn commit_replaces_once_retains_draft_and_uses_authoritative_state() {
        for running in [true, false] {
            let mut app = app();
            app.apply(Update::GatewayRecovery(event(Kind::Begin, Vec::new())));
            replay(
                &mut app,
                Update::AgentMessage {
                    id: "new".into(),
                    text: "complete".into(),
                    append: false,
                },
            );
            snapshot(&mut app, !running);
            snapshot(&mut app, running);
            app.apply(Update::GatewayRecovery(event(Kind::Commit, Vec::new())));
            assert_eq!(
                app.blocks
                    .iter()
                    .filter(|block| matches!(block, Block::Agent(_)))
                    .count(),
                1
            );
            assert!(text(&app).contains("complete"));
            assert!(!text(&app).contains("old transcript"));
            assert_eq!(app.editor.text(), "unsent draft");
            assert!(app.phase == if running { Phase::Working } else { Phase::Idle });
            assert_eq!(
                app.model,
                if running {
                    "running-model"
                } else {
                    "idle-model"
                }
            );
            assert!(!app.gateway_blocked);
            let before = text(&app);
            app.apply(Update::GatewayRecovery(event(Kind::Commit, Vec::new())));
            assert_eq!(text(&app), before);
            assert!(!app.gateway_blocked);
        }
    }
    #[test]
    fn commit_preserves_attachment_ownership_and_composer() {
        let mut app = app();
        app.attach(
            "/tmp/draft.png".into(),
            "image/png",
            AttachmentKind::Image,
            3,
        );
        let draft = app.editor.text().to_string();
        let placeholder = app.attachments[0].placeholder.clone();
        app.apply(Update::GatewayRecovery(event(Kind::Begin, Vec::new())));
        snapshot(&mut app, true);
        app.apply(Update::GatewayRecovery(event(Kind::Commit, Vec::new())));
        assert_eq!(app.editor.text(), draft);
        assert_eq!(app.attachments.len(), 1);
        assert_eq!(app.attachments[0].placeholder, placeholder);
        app.attach(
            "/tmp/next.png".into(),
            "image/png",
            AttachmentKind::Image,
            3,
        );
        assert_ne!(
            app.attachments[0].placeholder,
            app.attachments[1].placeholder
        );
    }

    #[test]
    fn nonadjacent_or_interrupted_snapshot_tail_is_rejected() {
        for ignored in [true, false] {
            let mut app = app();
            let old = text(&app);
            app.apply(Update::GatewayRecovery(event(Kind::Begin, Vec::new())));
            snapshot(&mut app, true);
            if ignored {
                app.apply(Update::GatewayRecovery(event(Kind::Replay, Vec::new())));
            } else {
                replay(
                    &mut app,
                    Update::AgentMessage {
                        id: "late".into(),
                        text: "not a snapshot tail".into(),
                        append: false,
                    },
                );
                replay(
                    &mut app,
                    Update::State(StateUpdate::Running(RunningStateUpdate::new())),
                );
            }
            app.apply(Update::GatewayRecovery(event(Kind::Commit, Vec::new())));
            assert_eq!(text(&app), old);
            assert!(app.gateway_blocked);
        }
    }

    #[test]
    fn wrong_session_live_update_cannot_mutate_confirmed_view() {
        let mut app = app();
        let old = text(&app);
        let mut wrong = event(
            Kind::Live,
            vec![Update::AgentMessage {
                id: "wrong".into(),
                text: "wrong session".into(),
                append: false,
            }],
        );
        wrong.marker.epoch = 1;
        wrong.session = "other".into();
        app.apply(Update::GatewayRecovery(wrong));
        assert_eq!(text(&app), old);
    }

    #[test]
    fn omitted_history_is_persistent_and_never_guesses_idle() {
        let mut app = app();
        app.apply(Update::GatewayRecovery(event(Kind::Begin, Vec::new())));
        snapshot(&mut app, true);
        let mut commit = event(Kind::Commit, Vec::new());
        commit.marker.history_available = Some(false);
        app.apply(Update::GatewayRecovery(commit));
        assert!(app.blocks.is_empty());
        assert!(app.phase == Phase::Working);
        assert!(
            app.gateway_status
                .as_deref()
                .unwrap()
                .contains("History unavailable")
        );
    }
    #[test]
    fn over_limit_and_mismatched_commits_cannot_install_candidate() {
        let mut app = app();
        let old = text(&app);
        app.apply(Update::GatewayRecovery(event(Kind::Begin, Vec::new())));
        let mut oversized = event(Kind::Replay, Vec::new());
        oversized.bytes = MAX_BYTES + 1;
        app.apply(Update::GatewayRecovery(oversized));
        snapshot(&mut app, true);
        app.apply(Update::GatewayRecovery(event(Kind::Commit, Vec::new())));
        assert_eq!(text(&app), old);
        assert!(app.gateway_blocked);
        app.apply(Update::GatewayRecovery(event(Kind::Begin, Vec::new())));
        snapshot(&mut app, true);
        let mut wrong = event(Kind::Commit, Vec::new());
        wrong.session = "other".into();
        app.apply(Update::GatewayRecovery(wrong));
        assert_eq!(text(&app), old);
    }
}
