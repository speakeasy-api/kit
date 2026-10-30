//! Announcements grant observation, never permission to prompt or steer.
use super::super::{UpdateSessionNotification, wire};
use super::*;
use agent_client_protocol::schema::MaybeUndefined;

pub(super) struct Association {
    parent: String,
    pub(super) snapshot: wire::SubagentUpdate,
}

impl App {
    pub(super) fn reset_child_associations(&mut self) {
        self.child_sessions.clear();
        for row in self.agents.values_mut() {
            row.focus_can_steer = false;
        }
        for view in self.child_views.values_mut() {
            view.disable("Child connection unavailable; retained transcript is read-only");
        }
    }

    pub(super) fn apply_subagent_session(
        &mut self,
        root: &str,
        notification: UpdateSessionNotification,
    ) {
        let session = notification.session_id.to_string();
        if self.session_id.as_deref().is_some_and(|id| id != root)
            || (session != root && !self.child_sessions.contains_key(&session))
        {
            return;
        }
        if let wire::SessionUpdate::SubagentUpdate(update) = notification.update {
            self.upsert_child(root, &session, update);
            return;
        }
        if session == root {
            self.apply_session_message(&session, notification.update);
            return;
        }
        self.ensure_child_view(&session);
        let retired = self.child_roster_retired(&session);
        if !retired
            && let wire::SessionUpdate::StateUpdate(state) = &notification.update
            && let Some(row) = self.agents.get_mut(&session)
        {
            row.status = work_status(Some(state));
            if row.status != SubagentStatus::Working {
                row.focus_can_steer = false;
            }
            let (generation, status) = (row.generation, row.status);
            self.child_lifecycle(&session, generation, status);
        }
        if !retired && let Some(row) = self.agents.get_mut(&session) {
            match &notification.update {
                wire::SessionUpdate::ToolCallUpdate(tool) => {
                    row.activity.apply(crate::events::SubagentActivity::Tool {
                        id: tool.tool_call_id.to_string(),
                        title: match &tool.title {
                            MaybeUndefined::Value(title) => Some(title.clone()),
                            MaybeUndefined::Null => Some(String::new()),
                            MaybeUndefined::Undefined => None,
                        },
                        running: tool.status.value().map(|status| {
                            matches!(
                                status,
                                wire::ToolCallStatus::Pending | wire::ToolCallStatus::InProgress
                            )
                        }),
                    });
                }
                wire::SessionUpdate::PlanUpdate(plan) => {
                    if let wire::PlanUpdateContent::Items(items) = &plan.plan {
                        let entry = items
                            .entries
                            .iter()
                            .find(|entry| entry.status == wire::PlanEntryStatus::InProgress)
                            .or_else(|| {
                                items
                                    .entries
                                    .iter()
                                    .find(|entry| entry.status == wire::PlanEntryStatus::Pending)
                            })
                            .map(|entry| entry.content.clone());
                        row.activity
                            .apply(crate::events::SubagentActivity::Plan { entry });
                    }
                }
                _ => {}
            }
        }
        let Some(view) = self.child_views.get_mut(&session) else {
            return;
        };
        match notification.update {
            update @ (wire::SessionUpdate::SessionMessage(_)
            | wire::SessionUpdate::SessionMessageChunk(_)) => {
                view.app.apply_session_message(&session, update);
            }
            _ => {
                for update in super::super::translate(notification).1 {
                    // Reporting child commands never authorizes child input.
                    if !matches!(update, Update::AvailableCommands { .. })
                        && !(retired && matches!(update, Update::State(_)))
                    {
                        view.app.apply(update);
                    }
                }
            }
        }
        if !retired && let Some(row) = self.agents.get_mut(&session) {
            row.usage = view.app.usage;
            row.cost = view.app.cost.clone();
        }
    }

    /// Runtime retirement wins over independently delivered ACP activity.
    /// Associations remain available for retained history, not roster revival.
    fn child_roster_retired(&self, id: &str) -> bool {
        if self.cleaned_agent_ids.contains(id)
            || self
                .agent_versions
                .get(id)
                .is_some_and(|(_, rank)| *rank == agent_status_rank(SubagentStatus::Removed))
        {
            return true;
        }
        // Ownership is immutable and cycle-free. Walk retained associations so
        // cleanup also covers descendants announced after the cleanup event.
        let mut ancestor = self
            .child_sessions
            .get(id)
            .map(|association| association.parent.as_str());
        while let Some(parent) = ancestor {
            if self.cleaned_agent_ids.contains(parent)
                || self.cleaned_agent_ancestors.contains(parent)
            {
                return true;
            }
            ancestor = self
                .child_sessions
                .get(parent)
                .map(|association| association.parent.as_str());
        }
        false
    }

    fn upsert_child(&mut self, root: &str, parent: &str, update: wire::SubagentUpdate) {
        let id = update.session_id.to_string();
        // Prior announcement and immutable ownership prevent cycles.
        if id.is_empty()
            || id == root
            || id == parent
            || self
                .child_sessions
                .get(&id)
                .is_some_and(|a| a.parent != parent)
        {
            return;
        }
        // Read the incoming patch, not retained metadata: a past grant must
        // not revive after idle, reconnect, or a later generation boundary.
        let steer_patch = match &update.meta {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => Some(None),
            MaybeUndefined::Value(meta) => meta.get("kit/subagent/steer").map(|value| {
                value
                    .get("generation")
                    .and_then(serde_json::Value::as_u64)
                    .map(|generation| {
                        (
                            generation,
                            value
                                .get("canSteer")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false),
                        )
                    })
            }),
        };
        if let Some(Some((generation, _))) = steer_patch
            && self
                .agents
                .get(&id)
                .is_some_and(|row| generation < row.generation)
        {
            return;
        }
        let association = self
            .child_sessions
            .entry(id.clone())
            .or_insert_with(|| Association {
                parent: parent.to_owned(),
                snapshot: wire::SubagentUpdate::new(id.clone()),
            });
        let state_changed = !update.state.is_undefined();
        let snapshot = &mut association.snapshot;
        patch(&mut snapshot.title, update.title);
        patch(&mut snapshot.description, update.description);
        patch(&mut snapshot.capabilities, update.capabilities);
        patch(&mut snapshot.state, update.state);
        patch(&mut snapshot.cwd, update.cwd);
        patch(
            &mut snapshot.additional_directories,
            update.additional_directories,
        );
        patch_meta(&mut snapshot.meta, update.meta);
        let state = snapshot.state.value().cloned();
        let title = snapshot.title.value().cloned();
        let description = snapshot.description.value().cloned();
        if self.child_roster_retired(&id) {
            if let Some(view) = self.child_views.get_mut(&id) {
                view.disable("Child removed; retained transcript is read-only");
            }
            return;
        }
        let row = self.agents.entry(id.clone()).or_insert_with(|| AgentRow {
            id: id.clone(),
            name: id.clone(),
            status: SubagentStatus::Starting,
            outcome: None,
            generation: 0,
            task: String::new(),
            parent_id: (parent != root).then(|| parent.to_owned()),
            parent_name: None,
            harness: String::new(),
            vendor: HarnessVendor::Unknown,
            model: None,
            created_at_unix_ms: 0,
            generation_started_at_unix_ms: 0,
            generation_finished_at_unix_ms: None,
            usage: None,
            cost: None,
            activity: AgentActivity::default(),
            focus_can_steer: false,
        });
        row.name = title.unwrap_or_else(|| id.clone());
        row.task = description.unwrap_or_default();
        if state_changed {
            row.status = work_status(state.as_ref());
            if row.status != SubagentStatus::Working {
                row.focus_can_steer = false;
            }
        }
        let mut generation_changed = false;
        if let Some(Some((generation, _))) = steer_patch {
            generation_changed = generation != row.generation;
            if generation_changed {
                row.generation = generation;
                row.focus_can_steer = false;
            }
        }
        // Standard cancel is never an explicit Kit steering grant. The exact
        // Kit metadata key grants only the current direct child's generation.
        let (generation, status) = (row.generation, row.status);
        if state_changed || generation_changed {
            self.child_lifecycle(&id, generation, status);
        }
        if let Some(grant) = steer_patch {
            self.child_capabilities(
                &id,
                generation,
                parent == root && grant.is_some_and(|(_, can_steer)| can_steer),
            );
        }
        if state_changed
            && let Some(state) = state
            && let Some(view) = self.child_views.get_mut(&id)
        {
            view.app.apply(Update::State(state));
        }
        self.agents_visible = true;
    }

    fn apply_session_message(&mut self, session: &str, update: wire::SessionUpdate) {
        let id = match &update {
            wire::SessionUpdate::SessionMessage(message) => message.message_id.clone(),
            wire::SessionUpdate::SessionMessageChunk(chunk) => chunk.message_id.clone(),
            _ => return,
        };
        let message = self
            .session_messages
            .entry(id.to_string())
            .or_insert_with(|| wire::SessionMessage::new(id.clone()));
        match update {
            wire::SessionUpdate::SessionMessage(update) => {
                patch(&mut message.sender_session_id, update.sender_session_id);
                patch(
                    &mut message.recipient_session_id,
                    update.recipient_session_id,
                );
                patch(&mut message.sender_message_id, update.sender_message_id);
                patch(
                    &mut message.recipient_message_id,
                    update.recipient_message_id,
                );
                patch(&mut message.content, update.content);
                patch_meta(&mut message.meta, update.meta);
            }
            wire::SessionUpdate::SessionMessageChunk(chunk) => {
                if !matches!(message.content, MaybeUndefined::Value(_)) {
                    message.content = MaybeUndefined::Value(Vec::new());
                }
                if let MaybeUndefined::Value(content) = &mut message.content {
                    content.push(chunk.content);
                }
            }
            _ => return,
        }
        let sender = message.sender_session_id.value().map(ToString::to_string);
        let recipient = message
            .recipient_session_id
            .value()
            .map(ToString::to_string);
        let label = if sender.as_deref() == Some(session) {
            format!("To {}", recipient.as_deref().unwrap_or("session"))
        } else if let Some(sender) = sender {
            format!("From {sender}")
        } else {
            "Session message".to_owned()
        };
        let mut content = vec![wire::ContentBlock::Text(wire::TextContent::new(format!(
            "{label}\n"
        )))];
        if let Some(blocks) = message.content.value() {
            content.extend(blocks.iter().cloned());
        }
        let notification = UpdateSessionNotification::new(
            session.to_owned(),
            wire::SessionUpdate::AgentMessage(wire::AgentMessage::new(id).content(content)),
        );
        for update in super::super::translate(notification).1 {
            self.apply(update);
        }
    }
}

fn patch<T>(stored: &mut MaybeUndefined<T>, incoming: MaybeUndefined<T>) {
    if !incoming.is_undefined() {
        *stored = incoming;
    }
}

fn patch_meta(stored: &mut MaybeUndefined<wire::Meta>, incoming: MaybeUndefined<wire::Meta>) {
    match incoming {
        MaybeUndefined::Undefined => {}
        MaybeUndefined::Null => *stored = MaybeUndefined::Null,
        MaybeUndefined::Value(patch) => {
            if !matches!(stored, MaybeUndefined::Value(_)) {
                *stored = MaybeUndefined::Value(wire::Meta::new());
            }
            if let MaybeUndefined::Value(meta) = stored {
                for (key, value) in patch {
                    if value.is_null() {
                        meta.remove(&key);
                    } else {
                        meta.insert(key, value);
                    }
                }
            }
        }
    }
}

fn work_status(state: Option<&wire::StateUpdate>) -> SubagentStatus {
    match state {
        Some(wire::StateUpdate::Running(_) | wire::StateUpdate::RequiresAction(_)) => {
            SubagentStatus::Working
        }
        Some(wire::StateUpdate::Idle(_)) => SubagentStatus::Idle,
        _ => SubagentStatus::Starting,
    }
}
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::disallowed_methods
)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn app() -> App {
        let mut app = App::new(
            PathBuf::from("."),
            String::new(),
            String::new(),
            String::new(),
        );
        app.start_session("root".into());
        app
    }
    fn send(app: &mut App, session: &str, update: Value) {
        for update in crate::tui::subagent_sessions::translate(
            serde_json::from_value(json!({"sessionId": session, "update": update})).unwrap(),
            "root",
        ) {
            app.apply(update);
        }
    }
    fn announce(app: &mut App, parent: &str, child: &str) {
        let notification = UpdateSessionNotification::new(
            parent.to_owned(),
            wire::SessionUpdate::SubagentUpdate(
                wire::SubagentUpdate::new(child.to_owned())
                    .title("Worker")
                    .state(wire::StateUpdate::Running(wire::RunningStateUpdate::new())),
            ),
        );
        for update in crate::tui::subagent_sessions::translate(notification, "root") {
            app.apply(update);
        }
    }
    fn chunk(app: &mut App, session: &str, text: &str) {
        send(
            app,
            session,
            json!({"sessionUpdate": "agent_message_chunk", "messageId": "answer", "content": {"type": "text", "text": text}}),
        );
    }
    #[test]
    fn announcements_are_required_and_ownership_cannot_change() {
        let mut app = app();
        chunk(&mut app, "child", "ignored");
        assert!(app.child_views.is_empty());
        announce(&mut app, "root", "child");
        announce(&mut app, "child", "nested");
        announce(&mut app, "root", "nested");
        announce(&mut app, "nested", "root");
        assert_eq!(app.child_sessions["nested"].parent, "child");
        assert!(!app.child_sessions.contains_key("root"));
        chunk(&mut app, "nested", "nested answer");
        assert!(!app.child_views["nested"].app.blocks.is_empty());
        assert!(app.blocks.is_empty());
        assert!(!app.child_views["nested"].can_steer);
    }
    #[test]
    fn interleaved_child_transcripts_survive_focus_switches() {
        let mut app = app();
        announce(&mut app, "root", "a");
        announce(&mut app, "root", "b");
        chunk(&mut app, "a", "one");
        app.focus_child("a".into());
        chunk(&mut app, "b", "two");
        assert_eq!(app.child_focus.as_deref(), Some("a"));
        app.leave_child();
        chunk(&mut app, "a", "three");
        app.focus_child("a".into());
        assert_eq!(app.child_views.len(), 2);
        assert_eq!(app.child_views["a"].app.blocks.len(), 1);
        assert_eq!(app.child_views["b"].app.blocks.len(), 1);
        assert!(app.blocks.is_empty());
    }
    #[test]
    fn patches_and_mirrored_state_do_not_recreate_the_child() {
        let mut app = app();
        announce(&mut app, "root", "child");
        chunk(&mut app, "child", "answer");
        send(
            &mut app,
            "root",
            json!({"sessionUpdate": "subagent_update", "sessionId": "child", "description": "role"}),
        );
        assert_eq!(app.agents["child"].name, "Worker");
        send(
            &mut app,
            "root",
            json!({"sessionUpdate": "subagent_update", "sessionId": "child", "title": null, "state": null}),
        );
        assert_eq!(app.agents["child"].name, "child");
        assert_eq!(app.agents["child"].status, SubagentStatus::Starting);
        send(
            &mut app,
            "child",
            json!({"sessionUpdate":"state_update", "state":"idle"}),
        );
        let blocks_after_idle = app.child_views["child"].app.blocks.len();
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "state":{"state":"idle"}}),
        );
        assert_eq!(app.agents["child"].status, SubagentStatus::Idle);
        // Ordinary idle rendering may append a stop notice. The mirrored
        // association must neither repeat it nor replace the observed answer.
        let child = &app.child_views["child"].app;
        assert_eq!(child.blocks.len(), blocks_after_idle);
        assert!(matches!(&child.blocks[0], Block::Agent(text) if text == "answer"));
    }
    #[test]
    fn mirrored_idle_preserves_terminal_outcomes_in_either_order() {
        for mirror_first in [false, true] {
            let mut app = app();
            announce(&mut app, "root", "child");
            send(
                &mut app,
                "child",
                json!({"sessionUpdate":"tool_call_update", "toolCallId":"tool", "title":"work", "status":"in_progress"}),
            );
            let state = json!({"state":"idle", "stopReason":"cancelled"});
            let mirror =
                json!({"sessionUpdate":"subagent_update", "sessionId":"child", "state":state});
            let ordinary =
                json!({"sessionUpdate":"state_update", "state":"idle", "stopReason":"cancelled"});
            if mirror_first {
                send(&mut app, "root", mirror);
                send(&mut app, "child", ordinary);
            } else {
                send(&mut app, "child", ordinary);
                send(&mut app, "root", mirror);
            }
            let child = &app.child_views["child"].app;
            assert!(
                matches!(&child.blocks[0], Block::Tool(call) if call.status == ToolCallStatus::Failed)
            );
            assert_eq!(
                child
                    .blocks
                    .iter()
                    .filter(
                        |block| matches!(block, Block::Notice(text) if text == "turn interrupted")
                    )
                    .count(),
                1
            );
        }
    }

    #[test]
    fn messages_allow_chunks_before_identity_and_metadata_only_patches() {
        let mut app = app();
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"session_message_chunk", "messageId":"m", "content":{"type":"text", "text":"hello"}}),
        );
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"session_message", "messageId":"m", "senderSessionId":"root", "recipientSessionId":"child"}),
        );
        assert_eq!(
            serde_json::to_value(&app.session_messages["m"]).unwrap()["content"][0]["text"],
            "hello"
        );
        assert_eq!(app.blocks.len(), 1);
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"session_message", "messageId":"m", "content":null}),
        );
        assert!(matches!(
            app.session_messages["m"].content,
            MaybeUndefined::Null
        ));
    }
    #[test]
    fn reconnect_revokes_associations_and_live_steering() {
        let mut app = app();
        announce(&mut app, "root", "child");
        chunk(&mut app, "child", "before");
        app.child_capabilities("child", 0, true);
        assert!(app.child_views["child"].can_steer);
        app.reset_child_associations();
        chunk(&mut app, "child", "must not append");
        assert!(app.child_sessions.is_empty());
        assert!(!app.agents["child"].focus_can_steer);
        assert!(!app.child_views["child"].can_steer);
        app.start_session("new".into());
        assert!(app.child_views.is_empty());
    }
    #[test]
    fn idle_revokes_steering_before_a_child_view_exists() {
        let mut app = app();
        announce(&mut app, "root", "child");
        app.child_capabilities("child", 0, true);
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "state":{"state":"idle"}}),
        );
        announce(&mut app, "root", "child");
        app.focus_child("child".into());
        assert!(!app.child_views["child"].can_steer);
    }
    #[test]
    fn background_child_creation_preserves_root_navigation() {
        let mut app = app();
        announce(&mut app, "root", "child");
        app.agents_keyboard_focus = true;
        app.agents_selected = Some("child".into());
        let epoch = app.clipboard_route_epoch;
        chunk(&mut app, "child", "hello");
        assert!(app.child_focus.is_none());
        assert!(app.agents_keyboard_focus);
        assert_eq!(app.agents_selected.as_deref(), Some("child"));
        assert_eq!(app.clipboard_route_epoch, epoch);
    }

    #[test]
    fn native_association_patches_preserve_omission_and_clear_null() {
        let mut app = app();
        announce(&mut app, "root", "child");
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "capabilities":{"cancel":{}}, "_meta":{"one":1,"two":2}}),
        );
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "description":"role", "_meta":{"one":null,"three":3}}),
        );
        let snapshot = &app.child_sessions["child"].snapshot;
        assert!(snapshot.capabilities.value().is_some());
        assert_eq!(snapshot.title.value().map(String::as_str), Some("Worker"));
        let meta = snapshot.meta.value().unwrap();
        assert!(!meta.contains_key("one"));
        assert_eq!(meta["two"], 2);
        assert_eq!(meta["three"], 3);
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "capabilities":null, "_meta":null}),
        );
        assert!(matches!(
            app.child_sessions["child"].snapshot.capabilities,
            MaybeUndefined::Null
        ));
        assert!(matches!(
            app.child_sessions["child"].snapshot.meta,
            MaybeUndefined::Null
        ));
    }

    #[test]
    fn initial_mirrored_state_is_applied_when_focusing_later() {
        let mut app = app();
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "state":{"state":"requires_action"}}),
        );
        assert!(!app.child_views.contains_key("child"));
        app.focus_child("child".into());
        assert!(app.child_views["child"].app.phase == Phase::Blocked);
    }

    #[test]
    fn mirrored_requires_action_survives_metadata_only_patches() {
        let mut app = app();
        announce(&mut app, "root", "child");
        chunk(&mut app, "child", "hello");
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "state":{"state":"requires_action"}}),
        );
        assert!(app.child_views["child"].app.phase == Phase::Blocked);
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "title":"Renamed"}),
        );
        assert!(app.child_views["child"].app.phase == Phase::Blocked);
    }

    fn steer_grant(app: &mut App, parent: &str, child: &str, generation: u64, can_steer: bool) {
        send(
            app,
            parent,
            json!({"sessionUpdate":"subagent_update", "sessionId":child,
            "_meta":{"kit/subagent/steer":{"generation":generation,"canSteer":can_steer}}}),
        );
    }

    #[test]
    fn kit_metadata_grants_only_the_current_generation_and_fences_pending_results() {
        let mut app = app();
        announce(&mut app, "root", "child");
        app.focus_child("child".into());
        assert!(!app.child_views["child"].can_steer);
        steer_grant(&mut app, "root", "child", 2, true);
        assert_eq!(app.agents["child"].generation, 2);
        assert_eq!(app.child_views["child"].generation, 2);
        assert!(app.child_views["child"].can_steer);
        steer_grant(&mut app, "root", "child", 1, false);
        assert!(app.child_views["child"].can_steer);
        app.paste("draft");
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::SteerChild { generation: 2, .. }
        ));
        steer_grant(&mut app, "root", "child", 3, false);
        app.child_steer_finished("child", 2, Ok(()));
        assert_eq!(app.child_views["child"].generation, 3);
        assert_eq!(app.child_views["child"].app.editor.text(), "draft");
        assert!(!app.child_views["child"].can_steer);
        steer_grant(&mut app, "root", "child", 2, true);
        assert_eq!(app.agents["child"].generation, 3);
        assert!(!app.child_views["child"].can_steer);
    }

    #[test]
    fn retained_kit_metadata_never_revives_an_idle_or_unknown_child_grant() {
        for state in ["idle", "unknown"] {
            let mut app = app();
            announce(&mut app, "root", "child");
            steer_grant(&mut app, "root", "child", 2, true);
            app.focus_child("child".into());
            send(
                &mut app,
                "root",
                json!({"sessionUpdate":"subagent_update", "sessionId":"child", "state":{"state":state}}),
            );
            assert!(!app.child_views["child"].can_steer);
            announce(&mut app, "root", "child");
            send(
                &mut app,
                "root",
                json!({"sessionUpdate":"subagent_update", "sessionId":"child", "_meta":{"unrelated":true}}),
            );
            assert!(!app.child_views["child"].can_steer);
            steer_grant(&mut app, "root", "child", 2, true);
            assert!(app.child_views["child"].can_steer);
        }
    }

    #[test]
    fn cleared_or_malformed_kit_grants_revoke_and_nested_grants_are_denied() {
        let mut app = app();
        announce(&mut app, "root", "child");
        app.focus_child("child".into());
        for meta in [
            json!(null),
            json!({"kit/subagent/steer":null}),
            json!({"kit/subagent/steer":{"generation":2}}),
            json!({"kit/subagent/steer":{"generation":"2","canSteer":true}}),
        ] {
            steer_grant(&mut app, "root", "child", 2, true);
            assert!(app.child_views["child"].can_steer);
            send(
                &mut app,
                "root",
                json!({"sessionUpdate":"subagent_update", "sessionId":"child", "_meta":meta}),
            );
            assert!(!app.child_views["child"].can_steer);
        }
        announce(&mut app, "child", "nested");
        steer_grant(&mut app, "child", "nested", 2, true);
        app.focus_child("nested".into());
        assert!(!app.child_views["nested"].can_steer);
        steer_grant(&mut app, "root", "child", 2, true);
        app.reset_child_associations();
        announce(&mut app, "root", "child");
        app.focus_child("child".into());
        assert!(!app.child_views["child"].can_steer);
    }

    fn runtime_state(
        app: &mut App,
        id: &str,
        generation: u64,
        status: SubagentStatus,
        outcome: Option<GenerationOutcome>,
    ) {
        app.apply(Update::Runtime(RuntimeEvent::SessionStarted {
            session_id: "root".into(),
        }));
        app.apply_runtime_at(
            RuntimeEvent::SubagentStateChanged {
                id: id.into(),
                name: id.into(),
                status,
                outcome,
                generation,
                task: "task".into(),
                parent_id: None,
                parent_name: None,
                harness: "acp.kit".into(),
                vendor: HarnessVendor::Kit,
                model: None,
                created_at_unix_ms: 100,
                generation_started_at_unix_ms: 100,
                generation_finished_at_unix_ms: (status == SubagentStatus::Removed)
                    .then_some(10_000),
            },
            10_000,
        );
    }

    #[test]
    fn retirement_wins_in_either_stream_order_without_dropping_retained_history() {
        for runtime_first in [false, true] {
            let mut app = app();
            announce(&mut app, "root", "child");
            runtime_state(&mut app, "child", 1, SubagentStatus::Working, None);
            chunk(&mut app, "child", "retained answer");
            let unknown = json!({"sessionUpdate":"subagent_update", "sessionId":"child", "state":{"state":"unknown"}});
            if runtime_first {
                runtime_state(&mut app, "child", 1, SubagentStatus::Removed, None);
                send(&mut app, "root", unknown);
            } else {
                send(&mut app, "root", unknown);
                runtime_state(&mut app, "child", 1, SubagentStatus::Removed, None);
            }
            // Repeated removal is already fenced by the recorded terminal rank.
            runtime_state(&mut app, "child", 1, SubagentStatus::Removed, None);
            assert!(!app.agents.contains_key("child"));
            assert!(app.child_sessions.contains_key("child"));
            app.focus_child("child".into());
            assert_eq!(app.child_focus.as_deref(), Some("child"));
            assert!(
                matches!(&app.child_views["child"].app.blocks[0], Block::Agent(text) if text == "retained answer")
            );
            assert!(!app.child_views["child"].can_steer);
            // Late transcript content remains observable, without a roster row.
            chunk(&mut app, "child", " tail");
            assert!(!app.agents.contains_key("child"));
            assert!(
                matches!(&app.child_views["child"].app.blocks[0], Block::Agent(text) if text == "retained answer tail")
            );
        }
    }

    #[test]
    fn streamed_updates_do_not_overwrite_a_retained_failed_runtime_row() {
        let mut app = app();
        announce(&mut app, "root", "child");
        runtime_state(&mut app, "child", 1, SubagentStatus::Working, None);
        chunk(&mut app, "child", "answer");
        runtime_state(
            &mut app,
            "child",
            1,
            SubagentStatus::Removed,
            Some(GenerationOutcome::Failed),
        );
        let failed = app.agents["child"].clone();
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "title":"late", "state":{"state":"unknown"}}),
        );
        send(
            &mut app,
            "child",
            json!({"sessionUpdate":"state_update", "state":"running"}),
        );
        assert_eq!(app.agents["child"], failed);
        assert!(app.child_views["child"].app.phase == Phase::Idle);
        // Only a newer runtime lifecycle can clear the terminal version fence.
        runtime_state(&mut app, "child", 2, SubagentStatus::Working, None);
        announce(&mut app, "root", "child");
        assert_eq!(app.agents["child"].generation, 2);
        assert_eq!(app.agents["child"].status, SubagentStatus::Working);
    }

    #[test]
    fn descendant_cleanup_fences_known_and_late_announced_descendants() {
        for cleanup_first in [false, true] {
            let mut app = app();
            announce(&mut app, "root", "parent");
            announce(&mut app, "parent", "nested");
            chunk(&mut app, "nested", "history");
            app.apply(Update::Runtime(RuntimeEvent::SessionStarted {
                session_id: "root".into(),
            }));
            if cleanup_first {
                app.apply(Update::Runtime(RuntimeEvent::SubagentDescendantsRemoved {
                    ancestor_id: "parent".into(),
                }));
                announce(&mut app, "parent", "nested");
            } else {
                announce(&mut app, "parent", "nested");
                app.apply(Update::Runtime(RuntimeEvent::SubagentDescendantsRemoved {
                    ancestor_id: "parent".into(),
                }));
            }
            announce(&mut app, "nested", "late-grandchild");
            announce(&mut app, "parent", "late-child");
            assert!(!app.agents.contains_key("nested"));
            assert!(!app.agents.contains_key("late-grandchild"));
            assert!(!app.agents.contains_key("late-child"));
            assert!(app.agents.contains_key("parent"));
            assert!(app.child_views.contains_key("nested"));
            assert!(!app.child_views["nested"].can_steer);
        }
    }

    #[test]
    fn standard_capabilities_never_enable_kit_steering() {
        let mut app = app();
        announce(&mut app, "root", "child");
        send(
            &mut app,
            "root",
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "capabilities":{"cancel":{}}}),
        );
        app.focus_child("child".into());
        assert!(!app.child_views["child"].can_steer);
    }
}
