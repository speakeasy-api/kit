//! Only the focused child owns a transcript. Other children retain small UI state.
use super::*;
use crate::protocols::acp::ReadSubagentTranscriptResponse;

const MAX_CHILD_DRAFT: usize = 16 * 1024;
const MAX_PARTIAL_REASONS: usize = 4;
const MAX_PARTIAL_REASON_BYTES: usize = 512;

pub(super) struct ChildUiState {
    editor: Editor,
    generation: u64,
    pending_text: Option<String>,
    steer_notice: Option<String>,
    scroll: usize,
    follow: bool,
}

/// One wall/monotonic anchor per view keeps replay pages and live updates on
/// the same clock. These are observation times, not exact remote start times.
struct ReplayClock {
    unix_ms: u64,
    instant: Instant,
}

impl ReplayClock {
    fn now() -> Self {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self {
            unix_ms: u64::try_from(unix_ms).unwrap_or(u64::MAX),
            instant: Instant::now(),
        }
    }

    fn observed_at(&self, value: &serde_json::Value) -> Option<Instant> {
        let unix_ms = value["kitObservedAtUnixMs"].as_u64()?;
        if unix_ms <= self.unix_ms {
            self.instant
                .checked_sub(Duration::from_millis(self.unix_ms - unix_ms))
        } else {
            self.instant
                .checked_add(Duration::from_millis(unix_ms - self.unix_ms))
        }
    }
}

pub struct ChildView {
    clock: ReplayClock,
    pub app: Box<App>,
    pub notice: String,
    pub generation: u64,
    pub can_steer: bool,
    pub cursor: u64,
    /// Delay reads when caught up or waiting for the spool writer to commit.
    pub read_backoff: bool,
    pub read_enabled: bool,
    active: bool,
    loading: bool,
    restore_scroll: Option<(usize, bool)>,
    partial: bool,
    partial_reasons: Vec<String>,
    partial_reasons_omitted: bool,
    pending_text: Option<String>,
    steer_notice: Option<String>,
}

impl ChildView {
    pub(super) fn disable(&mut self, notice: &str) {
        self.active = false;
        self.can_steer = false;
        self.read_enabled = false;
        self.pending_text = None;
        self.steer_notice = None;
        self.app.finish_replay_incomplete();
        self.notice = notice.into();
    }

    fn refresh_notice(&mut self) {
        if !self.read_enabled {
            return;
        }
        self.notice = if self.pending_text.is_some() {
            "Sending steer to child…"
        } else if self.loading {
            "Loading child transcript…"
        } else if !self.active {
            "Child idle or closed; transcript is read-only"
        } else if !self.can_steer {
            "Read-only: compatible steering is not available for this child"
        } else {
            "Text steering available · Enter to send · Esc back to main"
        }
        .into();
        if self.partial {
            let mut reason = if self.partial_reasons.is_empty() {
                "child sent an unsupported update".to_owned()
            } else {
                self.partial_reasons.join("; ")
            };
            if self.partial_reasons_omitted {
                reason.push_str("; additional limitations omitted");
            }
            self.notice = format!("Partial transcript: {reason} · {}", self.notice);
        }
        if let Some(notice) = &self.steer_notice {
            self.notice = format!("{notice} · {}", self.notice);
        }
    }
}

impl App {
    pub fn leave_child(&mut self) {
        for (id, mut view) in self.child_views.drain() {
            self.child_ui.insert(
                id,
                ChildUiState {
                    editor: std::mem::take(&mut view.app.editor),
                    generation: view.generation,
                    pending_text: view.pending_text,
                    steer_notice: view.steer_notice,
                    scroll: view.restore_scroll.map_or(view.app.scroll, |state| state.0),
                    follow: view.restore_scroll.map_or(view.app.follow, |state| state.1),
                },
            );
        }
        self.child_focus = None;
        self.agents_keyboard_focus = false;
        self.agents_selected = None;
        self.clipboard_route_epoch = self.clipboard_route_epoch.wrapping_add(1);
        self.child_back_area = Rect::default();
    }

    pub fn focus_child(&mut self, id: String) {
        let Some(row) = self.agents.get(&id) else {
            return;
        };
        let generation = row.generation;
        let active = row.status == SubagentStatus::Working;
        let direct = row.parent_id.is_none();
        let can_steer = active && direct && row.focus_can_steer;
        self.leave_child();
        let mut app = App::new(
            self.root.clone(),
            String::new(),
            String::new(),
            String::new(),
        );
        app.show_thoughts = self.show_thoughts;
        app.phase = if active { Phase::Working } else { Phase::Idle };
        let mut pending_text = None;
        let mut steer_notice = None;
        let mut restore_scroll = None;
        if let Some(ui) = self.child_ui.remove(&id) {
            if ui.generation == generation {
                pending_text = ui.pending_text;
                steer_notice = ui.steer_notice;
            }
            app.editor = ui.editor;
            restore_scroll = Some((ui.scroll, ui.follow));
            app.scroll = ui.scroll;
            app.follow = ui.follow;
        }
        let mut view = ChildView {
            clock: ReplayClock::now(),
            app: Box::new(app),
            generation,
            can_steer,
            active,
            notice: if direct {
                String::new()
            } else {
                "Transcript unavailable: descendant inspection is not supported".into()
            },
            cursor: 0,
            read_backoff: false,
            read_enabled: direct,
            loading: true,
            partial: false,
            partial_reasons: Vec::new(),
            partial_reasons_omitted: false,
            pending_text,
            steer_notice,
            restore_scroll,
        };
        view.refresh_notice();
        self.child_views.insert(id.clone(), view);
        self.agents_selected = Some(id.clone());
        self.child_focus = Some(id);
        self.agents_keyboard_focus = false;
        self.selection = None;
        self.press = None;
    }

    pub(super) fn child_capabilities(&mut self, id: &str, generation: u64, can_steer: bool) {
        let Some(row) = self.agents.get_mut(id) else {
            return;
        };
        if row.generation != generation {
            return;
        }
        row.focus_can_steer =
            can_steer && row.parent_id.is_none() && row.status == SubagentStatus::Working;
        if let Some(view) = self.child_views.get_mut(id)
            && view.generation == generation
        {
            view.can_steer = row.focus_can_steer;
            view.refresh_notice();
        }
    }

    // Epoch also rejects responses after switching away and back to the same child.
    pub fn child_read_target(&self) -> Option<(String, u64, u64, u64)> {
        let id = self.child_focus.as_ref()?;
        let view = self.child_views.get(id)?;
        view.read_enabled.then(|| {
            (
                id.clone(),
                view.generation,
                self.clipboard_route_epoch,
                view.cursor,
            )
        })
    }

    pub fn child_read_finished(
        &mut self,
        target: &(String, u64, u64, u64),
        result: Result<ReadSubagentTranscriptResponse, String>,
    ) {
        if self.child_read_target().as_ref() != Some(target) {
            return;
        }
        let (id, generation, _, cursor) = target;
        let Some(view) = self.child_views.get_mut(id) else {
            return;
        };
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                view.read_enabled = false;
                view.notice = format!("Transcript unavailable: {error}. Reopen child to retry.");
                return;
            }
        };
        if response.generation != *generation {
            view.read_enabled = false;
            view.notice = "Transcript generation changed; reopen child to resync".into();
            return;
        }
        if response.next_cursor < *cursor
            || (!response.updates.is_empty() && response.next_cursor == *cursor)
        {
            view.read_enabled = false;
            view.notice = "Invalid transcript cursor; reopen child to resync".into();
            return;
        }
        // Empty pending-writer pages are valid even before the first record.
        // Do not spin on them while the disk writer is starting or catching up.
        view.read_backoff = response.caught_up || response.next_cursor == *cursor;
        for value in response.updates {
            if matches!(
                value["sessionUpdate"].as_str(),
                Some("kit_transcript_partial" | "kit_transcript_truncated")
            ) {
                view.partial = true;
                let reason = value["reason"]
                    .as_str()
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or("backend reported incomplete transcript history");
                let end = reason.floor_char_boundary(reason.len().min(MAX_PARTIAL_REASON_BYTES));
                let mut reason_text: String = reason[..end]
                    .chars()
                    .map(|character| {
                        if character.is_control() {
                            ' '
                        } else {
                            character
                        }
                    })
                    .collect();
                if end < reason.len() {
                    reason_text.push('…');
                }
                if !view.partial_reasons.contains(&reason_text) {
                    if view.partial_reasons.len() < MAX_PARTIAL_REASONS {
                        view.partial_reasons.push(reason_text);
                    } else {
                        view.partial_reasons_omitted = true;
                    }
                }
                continue;
            }
            let observed_at = view.clock.observed_at(&value);
            let Ok(update) = serde_json::from_value(value) else {
                view.partial = true;
                continue;
            };
            // Session metadata is intentionally not transcript content. Other
            // accepted wire variants that translate to nothing must not vanish
            // silently (notably v2 terminal output and terminal lifecycle).
            use agent_client_protocol::schema::v2::SessionUpdate;
            let metadata = matches!(
                &update,
                SessionUpdate::SessionInfoUpdate(_)
                    | SessionUpdate::AvailableCommandsUpdate(_)
                    | SessionUpdate::ConfigOptionUpdate(_)
                    | SessionUpdate::UsageUpdate(_)
                    | SessionUpdate::StateUpdate(_)
            );
            let (_, updates) = super::super::translate(
                agent_client_protocol::schema::v2::UpdateSessionNotification::new(
                    id.clone(),
                    update,
                ),
            );
            if updates.is_empty() && !metadata {
                view.partial = true;
            }
            for update in updates {
                view.app.apply_at(update, observed_at);
            }
        }
        view.cursor = response.next_cursor;
        if response.caught_up {
            view.loading = false;
            if !view.active {
                view.app.finish_replay_incomplete();
            }
            if let Some((scroll, follow)) = view.restore_scroll.take() {
                view.app.scroll = scroll;
                view.app.follow = follow;
            }
        }
        view.refresh_notice();
    }

    pub(super) fn child_lifecycle(&mut self, id: &str, generation: u64, status: SubagentStatus) {
        let Some(view) = self.child_views.get(id) else {
            if let Some(ui) = self.child_ui.get_mut(id)
                && (generation > ui.generation
                    || (generation == ui.generation && status != SubagentStatus::Working))
            {
                ui.steer_notice = None;
            }
            return;
        };
        if generation < view.generation {
            return;
        }
        if generation != view.generation {
            if self.agents.contains_key(id) {
                self.focus_child(id.to_owned());
            } else {
                if let Some(view) = self.child_views.get_mut(id) {
                    view.disable("Child removed; transcript is read-only");
                }
                return;
            }
        }
        let Some(view) = self.child_views.get_mut(id) else {
            return;
        };
        if view.active != (status == SubagentStatus::Working) {
            // Reject an in-flight read sampled before this lifecycle boundary.
            // A fresh read must drain the terminal tail before closing clocks.
            self.clipboard_route_epoch = self.clipboard_route_epoch.wrapping_add(1);
            view.steer_notice = None;
        }
        view.active = status == SubagentStatus::Working;
        if view.active {
            view.app.phase = Phase::Working;
        } else {
            view.can_steer = false;
            // Lifecycle can precede the final spool page. Keep timing boundaries
            // open until a subsequent caught-up read consumes that tail.
            view.read_backoff = false;
        }
        view.refresh_notice();
    }

    pub(super) fn child_steer_finished(
        &mut self,
        id: &str,
        generation: u64,
        result: Result<(), String>,
    ) {
        let notice = match &result {
            Ok(()) => "Steer accepted; waiting for child delivery".to_owned(),
            Err(error) => format!("Steer result: {error}"),
        };
        let Some(view) = self.child_views.get_mut(id) else {
            if let Some(ui) = self.child_ui.get_mut(id)
                && ui.generation == generation
                && let Some(text) = ui.pending_text.take()
            {
                if result.is_ok() && ui.editor.text() == text {
                    ui.editor.clear();
                }
                ui.steer_notice = Some(notice);
            }
            return;
        };
        if view.generation != generation {
            return;
        }
        let Some(text) = view.pending_text.take() else {
            return;
        };
        if result.is_ok() && view.app.editor.text() == text {
            view.app.editor.clear();
        }
        view.notice.clone_from(&notice);
        view.steer_notice = Some(notice);
    }

    pub(super) fn paste_child(&mut self, text: &str) {
        let Some(view) = self
            .child_focus
            .as_ref()
            .and_then(|id| self.child_views.get_mut(id))
        else {
            return;
        };
        if view.active && view.can_steer && view.pending_text.is_none() {
            let remaining = MAX_CHILD_DRAFT.saturating_sub(view.app.editor.text().len());
            let mut end = text.len().min(remaining);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            view.app.editor.insert_str(&text[..end]);
        }
    }

    fn select_agent(&mut self, down: bool) {
        let ids: Vec<_> = self
            .agent_tree_rows()
            .iter()
            .map(|entry| entry.row.id.clone())
            .collect();
        if ids.is_empty() {
            self.agents_selected = None;
            return;
        }
        let index = self
            .agents_selected
            .as_ref()
            .and_then(|id| ids.iter().position(|candidate| candidate == id));
        let next = match index {
            Some(index) if down => (index + 1).min(ids.len() - 1),
            Some(index) => index.saturating_sub(1),
            None => 0,
        };
        self.agents_selected = Some(ids[next].clone());
        if next < self.agents_scroll {
            self.agents_scroll = next;
        }
        if next >= self.agents_scroll + self.agents_viewport.max(1) {
            self.agents_scroll = next + 1 - self.agents_viewport.max(1);
        }
    }

    pub(super) fn handle_focus_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.agents_keyboard_focus = !self.agents_keyboard_focus;
            self.agents_visible = true;
            if self.agents_selected.is_none() {
                self.select_agent(true);
            }
            return Some(Action::Redraw);
        }
        if self.agents_keyboard_focus {
            match key.code {
                KeyCode::Esc => {
                    self.agents_keyboard_focus = false;
                    if self.child_focus.is_some() {
                        self.leave_child();
                    }
                }
                KeyCode::Up => self.select_agent(false),
                KeyCode::Down | KeyCode::Tab => self.select_agent(true),
                KeyCode::Enter => {
                    if let Some(id) = self.agents_selected.clone() {
                        self.focus_child(id);
                    }
                }
                _ => {}
            }
            return Some(Action::Redraw);
        }
        let id = self.child_focus.clone()?;
        if key.code == KeyCode::Esc {
            self.leave_child();
            return Some(Action::Redraw);
        }
        let Some(view) = self.child_views.get_mut(&id) else {
            return Some(Action::None);
        };
        match key.code {
            KeyCode::PageUp => view.app.scroll_by(-(view.app.viewport.max(2) as isize - 1)),
            KeyCode::PageDown => view.app.scroll_by(view.app.viewport.max(2) as isize - 1),
            KeyCode::Home => view.app.scroll_to_top(),
            KeyCode::End => view.app.scroll_to_bottom(),
            KeyCode::Up => view.app.scroll_by(-1),
            KeyCode::Down => view.app.scroll_by(1),
            _ if !view.active || !view.can_steer => {
                view.refresh_notice();
            }
            _ if view.pending_text.is_some() => {}
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if view.app.editor.text().len() < MAX_CHILD_DRAFT {
                    view.app.editor.insert_char('\n');
                }
            }
            KeyCode::Enter => {
                let text = view.app.editor.text().to_owned();
                if !text.trim().is_empty() {
                    view.steer_notice = None;
                    view.pending_text = Some(text.clone());
                    view.notice = "Sending steer to child…".into();
                    return Some(Action::SteerChild {
                        id,
                        generation: view.generation,
                        text,
                    });
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if view.app.editor.text().len() + character.len_utf8() <= MAX_CHILD_DRAFT {
                    view.app.editor.insert_char(character);
                }
            }
            KeyCode::Backspace => view.app.editor.backspace(),
            KeyCode::Delete => view.app.editor.delete_forward(),
            KeyCode::Left => view.app.editor.move_left(),
            KeyCode::Right => view.app.editor.move_right(),
            _ => {}
        }
        Some(Action::Redraw)
    }

    pub(super) fn handle_focus_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if self.child_focus.is_some()
                && self
                    .child_back_area
                    .contains((mouse.column, mouse.row).into())
            {
                self.leave_child();
                self.agents_keyboard_focus = false;
                return Some(Action::Redraw);
            }
            if self.agents_area.contains((mouse.column, mouse.row).into()) {
                let offset = mouse.row.saturating_sub(self.agents_area.y + 1);
                let row = usize::from(offset / 3);
                if mouse.row <= self.agents_area.y || row >= self.agents_viewport {
                    return Some(Action::None);
                }
                let index = self.agents_scroll + row;
                let id = self
                    .agent_tree_rows()
                    .get(index)
                    .map(|entry| entry.row.id.clone());
                if let Some(id) = id {
                    self.focus_child(id);
                }
                return Some(Action::Redraw);
            }
        }
        self.child_focus.as_ref()?;
        if self.agents_area.contains((mouse.column, mouse.row).into()) {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_agents_by(-3),
                MouseEventKind::ScrollDown => self.scroll_agents_by(3),
                _ => {}
            }
            return Some(Action::Redraw);
        }
        let Some(view) = self.child_views.get_mut(self.child_focus.as_ref()?) else {
            return Some(Action::None);
        };
        // Reuse selection, links and local card expansion, but do not permit a
        // tool action (cancel/detach, session navigation, etc.) to escape to root.
        let action = view.app.handle_mouse(mouse);
        Some(match action {
            Action::None | Action::Redraw | Action::Copy(_) => action,
            _ => Action::None,
        })
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
    fn app() -> App {
        App::new(
            PathBuf::from("."),
            "test".into(),
            "test".into(),
            String::new(),
        )
    }

    fn lifecycle(app: &mut App, id: &str, generation: u64, status: SubagentStatus) {
        app.apply(Update::Runtime(RuntimeEvent::SubagentStateChanged {
            id: id.into(),
            name: id.into(),
            status,
            outcome: None,
            generation,
            task: "task".into(),
            parent_id: None,
            parent_name: None,
            harness: "acp.kit".into(),
            vendor: HarnessVendor::Kit,
            model: None,
            created_at_unix_ms: 1,
            generation_started_at_unix_ms: 1,
            generation_finished_at_unix_ms: None,
        }));
    }

    fn page(
        generation: u64,
        next_cursor: u64,
        caught_up: bool,
        text: &str,
    ) -> ReadSubagentTranscriptResponse {
        ReadSubagentTranscriptResponse {
            generation,
            next_cursor,
            caught_up,
            updates: vec![
                serde_json::json!({"sessionUpdate": "agent_message_chunk", "messageId": "message", "content": {"type": "text", "text": text}}),
            ],
        }
    }

    fn read(app: &mut App, text: &str, caught_up: bool) {
        let target = app.child_read_target().unwrap();
        app.child_read_finished(&target, Ok(page(target.1, target.3 + 1, caught_up, text)));
    }

    fn key(app: &mut App, code: KeyCode) -> Action {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn replay_clock_preserves_offsets_and_unknown_observations() {
        let instant = Instant::now();
        let clock = ReplayClock {
            unix_ms: 10_000,
            instant,
        };
        let value = |ms| serde_json::json!({"kitObservedAtUnixMs": ms});
        assert_eq!(
            clock.observed_at(&value(8_000)),
            instant.checked_sub(Duration::from_secs(2))
        );
        assert_eq!(
            clock.observed_at(&value(12_000)),
            instant.checked_add(Duration::from_secs(2))
        );
        assert_eq!(clock.observed_at(&serde_json::json!({})), None);
        assert_eq!(
            clock.observed_at(&serde_json::json!({"kitObservedAtUnixMs": null})),
            None
        );
    }

    #[test]
    fn completed_tool_duration_is_stable_across_late_load_and_reopen() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        let instant = Instant::now();
        for opened_at in [10_000, 30_000] {
            app.focus_child("child".into());
            app.child_views.get_mut("child").unwrap().clock = ReplayClock {
                unix_ms: opened_at,
                instant,
            };
            for (cursor, update) in [
                serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "tool", "title": "shell", "status": "in_progress", "kitObservedAtUnixMs": 2_000}),
                serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "tool", "status": "completed", "kitObservedAtUnixMs": 5_000}),
            ].into_iter().enumerate() {
                let target = app.child_read_target().unwrap();
                app.child_read_finished(&target, Ok(ReadSubagentTranscriptResponse {
                    generation: 1, next_cursor: cursor as u64 + 1, caught_up: true, updates: vec![update],
                }));
                if cursor == 0 {
                    let call = app.child_views["child"].app.tool_call("tool").unwrap();
                    assert!(call.running());
                    assert_eq!(call.started, instant.checked_sub(Duration::from_millis(opened_at - 2_000)));
                }
            }
            assert_eq!(
                app.child_views["child"]
                    .app
                    .tool_call("tool")
                    .unwrap()
                    .elapsed(),
                Some(3_000)
            );
            app.leave_child();
        }
    }

    #[test]
    fn terminal_lifecycle_waits_for_tail_before_closing_reasoning() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.focus_child("child".into());
        app.child_views.get_mut("child").unwrap().clock = ReplayClock {
            unix_ms: 10_000,
            instant: Instant::now(),
        };
        let thought = serde_json::json!({"sessionUpdate": "agent_thought_chunk", "messageId": "thought", "content": {"type": "text", "text": "thinking"}, "kitObservedAtUnixMs": 2_000});
        let target = app.child_read_target().unwrap();
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: 1,
                caught_up: true,
                updates: vec![thought],
            }),
        );
        let stale_target = app.child_read_target().unwrap();
        lifecycle(&mut app, "child", 1, SubagentStatus::Idle);
        assert_ne!(app.child_read_target().unwrap(), stale_target);
        app.child_read_finished(
            &stale_target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: 1,
                caught_up: true,
                updates: vec![],
            }),
        );
        let target = app.child_read_target().unwrap();
        app.child_read_finished(&target, Ok(ReadSubagentTranscriptResponse { generation: 1, next_cursor: 2, caught_up: true, updates: vec![serde_json::json!({"sessionUpdate": "agent_message_chunk", "messageId": "answer", "content": {"type": "text", "text": "done"}, "kitObservedAtUnixMs": 5_000})] }));
        assert!(
            app.child_views["child"]
                .app
                .blocks
                .iter()
                .any(|block| matches!(
                    block,
                    Block::Thought {
                        closed: true,
                        millis: Some(3_000),
                        ..
                    }
                ))
        );
    }

    #[test]
    fn inactive_child_without_terminal_records_does_not_keep_aging() {
        for initially_active in [false, true] {
            let mut app = app();
            lifecycle(
                &mut app,
                "child",
                1,
                if initially_active {
                    SubagentStatus::Working
                } else {
                    SubagentStatus::Idle
                },
            );
            app.focus_child("child".into());
            app.child_views.get_mut("child").unwrap().clock = ReplayClock {
                unix_ms: 10_000,
                instant: Instant::now(),
            };
            let target = app.child_read_target().unwrap();
            app.child_read_finished(&target, Ok(ReadSubagentTranscriptResponse {
                generation: 1, next_cursor: 2, caught_up: true, updates: vec![
                    serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "tool", "title": "shell", "status": "in_progress", "kitObservedAtUnixMs": 2_000}),
                    serde_json::json!({"sessionUpdate": "agent_thought_chunk", "messageId": "thought", "content": {"type": "text", "text": "thinking"}, "kitObservedAtUnixMs": 3_000}),
                ],
            }));
            if initially_active {
                lifecycle(&mut app, "child", 1, SubagentStatus::Idle);
                let target = app.child_read_target().unwrap();
                app.child_read_finished(
                    &target,
                    Ok(ReadSubagentTranscriptResponse {
                        generation: 1,
                        next_cursor: 2,
                        caught_up: true,
                        updates: vec![],
                    }),
                );
            }
            let child = &app.child_views["child"].app;
            let call = child.tool_call("tool").unwrap();
            assert!(call.running(), "missing outcome must not become success");
            assert_eq!(call.elapsed(), None);
            assert!(child.blocks.iter().any(|block| matches!(
                block,
                Block::Thought {
                    closed: true,
                    millis: None,
                    ..
                }
            )));
        }
    }

    #[test]
    fn focus_navigation_preserves_root_roster_draft_and_scroll() {
        let mut app = app();
        lifecycle(&mut app, "alpha", 1, SubagentStatus::Working);
        lifecycle(&mut app, "beta", 1, SubagentStatus::Working);
        assert!(app.child_views.is_empty());
        assert!(app.child_read_target().is_none());
        app.editor.insert_str("root draft");
        app.follow = false;
        app.scroll = 7;
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        key(&mut app, KeyCode::Down);
        assert_eq!(app.agents_selected.as_deref(), Some("beta"));
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.child_focus.as_deref(), Some("alpha"));
        assert!(app.child_views["alpha"].notice.contains("Loading"));
        read(&mut app, "snapshot", false);
        assert!(app.child_views["alpha"].notice.contains("Loading"));
        read(&mut app, " live", true);
        assert!(!app.child_views["alpha"].notice.contains("Loading"));
        assert_eq!(app.child_read_target().unwrap().3, 2);
        read(&mut app, " continued live", true);
        assert_eq!(app.child_read_target().unwrap().3, 3);
        assert_eq!(app.child_views["alpha"].app.blocks.len(), 1);
        app.apply(Update::AgentMessage {
            id: "root".into(),
            text: "root runs".into(),
            append: true,
        });
        assert_eq!(app.blocks.len(), 1);
        assert_eq!(app.agent_tree_rows().len(), 2);
        key(&mut app, KeyCode::Esc);
        assert!(app.child_views.is_empty());
        assert!(app.child_read_target().is_none());
        assert_eq!(app.editor.text(), "root draft");
        assert_eq!(app.scroll, 7);
        assert!(!app.follow);
    }

    #[test]
    fn only_focus_retains_transcript_and_reopening_replays_without_size_cutoff() {
        let mut app = app();
        for i in 0..32 {
            let id = format!("child-{i}");
            lifecycle(&mut app, &id, 1, SubagentStatus::Working);
            app.child_capabilities(&id, 1, true);
        }
        assert!(app.child_views.is_empty());
        app.focus_child("child-0".into());
        app.paste("small draft");
        read(&mut app, &"x".repeat(2 * 1024 * 1024 + 1), true);
        assert!(!app.child_views["child-0"].app.blocks.is_empty());
        app.child_views.get_mut("child-0").unwrap().app.scroll = 3;
        app.child_views.get_mut("child-0").unwrap().app.follow = false;
        for i in 1..32 {
            app.focus_child(format!("child-{i}"));
            read(&mut app, "snapshot", true);
            assert_eq!(app.child_views.len(), 1);
            assert!(!app.child_views.contains_key("child-0"));
        }
        app.focus_child("child-0".into());
        assert!(app.child_views["child-0"].app.blocks.is_empty());
        assert_eq!(app.child_read_target().unwrap().3, 0);
        assert_eq!(app.child_views["child-0"].app.editor.text(), "small draft");
        read(&mut app, "replayed", true);
        assert_eq!(app.child_views["child-0"].app.scroll, 3);
        assert!(!app.child_views["child-0"].app.follow);
    }

    #[test]
    fn stale_pages_are_rejected_across_focus_generation_and_session_changes() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.focus_child("child".into());
        let first = app.child_read_target().unwrap();
        app.leave_child();
        app.focus_child("child".into());
        app.child_read_finished(&first, Ok(page(1, 1, true, "stale")));
        assert!(app.child_views["child"].app.blocks.is_empty());
        let second = app.child_read_target().unwrap();
        lifecycle(&mut app, "child", 2, SubagentStatus::Working);
        assert_eq!(app.child_read_target().unwrap().1, 2);
        app.child_read_finished(&second, Ok(page(1, 1, true, "stale")));
        assert!(app.child_views["child"].app.blocks.is_empty());
        let third = app.child_read_target().unwrap();
        app.start_session("new-root".into());
        app.child_read_finished(&third, Ok(page(2, 1, true, "stale")));
        assert!(app.child_views.is_empty());
        assert!(app.child_ui.is_empty());
    }

    #[test]
    fn errors_and_invalid_cursors_are_explicit_and_reopening_resyncs() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.focus_child("child".into());
        let target = app.child_read_target().unwrap();
        app.child_read_finished(&target, Err("disk read failed".into()));
        assert!(app.child_views["child"].notice.contains("disk read failed"));
        assert!(app.child_read_target().is_none());
        app.focus_child("child".into());
        let target = app.child_read_target().unwrap();
        app.child_read_finished(&target, Ok(page(1, 0, false, "invalid")));
        assert!(app.child_views["child"].notice.contains("cursor"));
        assert!(app.child_views["child"].app.blocks.is_empty());
        app.focus_child("child".into());
        let target = app.child_read_target().unwrap();
        app.child_read_finished(&target, Ok(page(2, 1, true, "wrong generation")));
        assert!(app.child_views["child"].notice.contains("generation"));
        assert!(app.child_views["child"].app.blocks.is_empty());
    }

    #[test]
    fn steering_stays_generation_safe_even_when_focus_moves() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.child_capabilities("child", 1, true);
        app.focus_child("child".into());
        app.paste("steer");
        assert!(matches!(
            key(&mut app, KeyCode::Enter),
            Action::SteerChild { generation: 1, .. }
        ));
        app.leave_child();
        app.focus_child("child".into());
        assert!(matches!(key(&mut app, KeyCode::Enter), Action::Redraw));
        app.child_steer_finished("child", 0, Ok(()));
        assert_eq!(app.child_views["child"].app.editor.text(), "steer");
        app.child_steer_finished("child", 1, Ok(()));
        assert!(app.child_views["child"].app.editor.is_empty());
        app.paste("next");
        key(&mut app, KeyCode::Enter);
        lifecycle(&mut app, "child", 2, SubagentStatus::Working);
        app.child_steer_finished("child", 1, Ok(()));
        assert_eq!(app.child_views["child"].app.editor.text(), "next");
        assert!(!app.child_views["child"].can_steer);
        assert!(app.blocks.is_empty());
    }

    #[test]
    fn descendants_have_clear_unsupported_notice_and_no_reads() {
        let mut app = app();
        lifecycle(&mut app, "nested", 1, SubagentStatus::Working);
        app.agents.get_mut("nested").unwrap().parent_id = Some("parent".into());
        app.child_capabilities("nested", 1, true);
        app.focus_child("nested".into());
        assert!(app.child_read_target().is_none());
        assert!(!app.child_views["nested"].can_steer);
        assert!(app.child_views["nested"].notice.contains("descendant"));
    }

    #[test]
    fn roster_escape_and_back_button_release_the_focused_transcript() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.focus_child("child".into());
        read(&mut app, "snapshot", true);
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        key(&mut app, KeyCode::Esc);
        assert!(app.child_focus.is_none());
        assert!(app.child_views.is_empty());
        assert!(!app.agents_keyboard_focus);
        assert!(app.agents_selected.is_none());
        app.focus_child("child".into());
        read(&mut app, "snapshot", true);
        // Back must release roster focus even while keyboard navigation owns it.
        app.agents_keyboard_focus = true;
        app.child_back_area = Rect::new(0, 0, 20, 1);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.child_focus.is_none());
        assert!(app.child_views.is_empty());
        assert!(!app.agents_keyboard_focus);
        assert!(app.agents_selected.is_none());

        app.focus_child("child".into());
        key(&mut app, KeyCode::Esc);
        assert!(app.child_focus.is_none());
        assert!(!app.agents_keyboard_focus);
        assert!(app.agents_selected.is_none());
    }

    #[test]
    fn pending_writer_pages_keep_loading_and_back_off_until_records_arrive() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.focus_child("child".into());
        assert!(!app.child_views["child"].read_backoff);
        // The spool path may not exist yet; subsequent reads can also wait for
        // pending records without advancing the cursor.
        for _ in 0..2 {
            let target = app.child_read_target().unwrap();
            app.child_read_finished(
                &target,
                Ok(ReadSubagentTranscriptResponse {
                    generation: 1,
                    next_cursor: 0,
                    caught_up: false,
                    updates: vec![],
                }),
            );
            assert_eq!(app.child_read_target().unwrap(), target);
            assert!(app.child_views["child"].read_backoff);
            assert!(app.child_views["child"].notice.contains("Loading"));
            assert!(app.child_views["child"].app.blocks.is_empty());
        }
        read(&mut app, "snapshot", false);
        assert!(!app.child_views["child"].read_backoff);
        assert!(app.child_views["child"].notice.contains("Loading"));
        let target = app.child_read_target().unwrap();
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: target.3,
                caught_up: false,
                updates: vec![],
            }),
        );
        assert!(app.child_views["child"].read_backoff);
        read(&mut app, " caught up", true);
        assert!(app.child_views["child"].read_backoff);
        assert!(!app.child_views["child"].notice.contains("Loading"));
        let target = app.child_read_target().unwrap();
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: target.3,
                caught_up: true,
                updates: vec![],
            }),
        );
        assert!(app.child_views["child"].read_backoff);
        read(&mut app, " live", false);
        assert!(!app.child_views["child"].read_backoff);
        assert_eq!(app.child_views["child"].app.blocks.len(), 1);
    }

    #[test]
    fn unchanged_nonempty_and_regressing_pages_are_rejected() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        for caught_up in [false, true] {
            app.focus_child("child".into());
            let target = app.child_read_target().unwrap();
            app.child_read_finished(&target, Ok(page(1, 0, caught_up, "invalid")));
            assert!(app.child_read_target().is_none());
            assert!(app.child_views["child"].notice.contains("cursor"));
            assert!(app.child_views["child"].app.blocks.is_empty());
        }
        app.focus_child("child".into());
        read(&mut app, "snapshot", false);
        let target = app.child_read_target().unwrap();
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: 0,
                caught_up: false,
                updates: vec![],
            }),
        );
        assert!(app.child_read_target().is_none());
        assert!(app.child_views["child"].notice.contains("cursor"));
    }

    #[test]
    fn unsupported_terminal_content_marks_replay_partial_but_metadata_does_not() {
        use agent_client_protocol::schema::v2::SessionUpdate;
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.focus_child("child".into());
        let metadata =
            serde_json::json!({"sessionUpdate": "session_info_update", "title": "Child title"});
        assert!(matches!(
            serde_json::from_value::<SessionUpdate>(metadata.clone()).unwrap(),
            SessionUpdate::SessionInfoUpdate(_)
        ));
        let target = app.child_read_target().unwrap();
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: 1,
                caught_up: true,
                updates: vec![metadata],
            }),
        );
        assert!(!app.child_views["child"].notice.contains("Partial"));
        let output = serde_json::json!({"sessionUpdate": "terminal_output_chunk", "terminalId": "shell", "data": "aGkK"});
        assert!(matches!(
            serde_json::from_value::<SessionUpdate>(output.clone()).unwrap(),
            SessionUpdate::TerminalOutputChunk(_)
        ));
        let target = app.child_read_target().unwrap();
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: 2,
                caught_up: true,
                updates: vec![output],
            }),
        );
        assert_eq!(app.child_read_target().unwrap().3, 2);
        assert!(
            app.child_views["child"]
                .notice
                .contains("Partial transcript")
        );
        read(&mut app, "later supported text", true);
        assert!(
            app.child_views["child"]
                .notice
                .contains("Partial transcript")
        );
    }

    #[test]
    fn steer_results_survive_empty_pages_and_focus_switches_until_explicit_send() {
        for result in [
            Ok(()),
            Err("child rejected the steer".to_owned()),
            Err("child steering timed out; delivery is unknown".to_owned()),
        ] {
            let mut app = app();
            lifecycle(&mut app, "child", 1, SubagentStatus::Working);
            app.child_capabilities("child", 1, true);
            app.focus_child("child".into());
            read(&mut app, "snapshot", true);
            app.paste("steer draft");
            key(&mut app, KeyCode::Enter);
            app.child_steer_finished("child", 1, result.clone());
            let expected = match &result {
                Ok(()) => "Steer accepted",
                Err(error) => error.as_str(),
            };
            for _ in 0..2 {
                let target = app.child_read_target().unwrap();
                app.child_read_finished(
                    &target,
                    Ok(ReadSubagentTranscriptResponse {
                        generation: 1,
                        next_cursor: target.3,
                        caught_up: true,
                        updates: vec![],
                    }),
                );
                assert!(app.child_views["child"].notice.contains(expected));
            }
            app.leave_child();
            app.focus_child("child".into());
            assert!(app.child_views["child"].notice.contains(expected));
            read(&mut app, "replayed", true);
            assert!(app.child_views["child"].notice.contains(expected));
            if result.is_err() {
                assert_eq!(app.child_views["child"].app.editor.text(), "steer draft");
            } else {
                app.paste("next steer");
            }
            assert!(matches!(
                key(&mut app, KeyCode::Enter),
                Action::SteerChild { .. }
            ));
            assert!(!app.child_views["child"].notice.contains(expected));
        }
    }

    #[test]
    fn parked_steer_result_is_restored_with_draft_and_cleared_on_generation_change() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.child_capabilities("child", 1, true);
        app.focus_child("child".into());
        app.paste("draft");
        key(&mut app, KeyCode::Enter);
        app.leave_child();
        app.child_steer_finished("child", 1, Err("delivery is unknown".into()));
        app.focus_child("child".into());
        assert!(
            app.child_views["child"]
                .notice
                .contains("delivery is unknown")
        );
        assert_eq!(app.child_views["child"].app.editor.text(), "draft");
        lifecycle(&mut app, "child", 2, SubagentStatus::Working);
        assert!(
            !app.child_views["child"]
                .notice
                .contains("delivery is unknown")
        );
        assert_eq!(app.child_views["child"].app.editor.text(), "draft");
    }

    #[test]
    fn backend_partial_reasons_remain_visible_with_sticky_steer_results_and_live_pages() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.child_capabilities("child", 1, true);
        app.focus_child("child".into());
        app.paste("steer");
        key(&mut app, KeyCode::Enter);
        app.child_steer_finished("child", 1, Err("delivery is unknown".into()));
        let reasons = [
            "ACP does not identify its echo; reported messages may repeat the prompt",
            "Inherited transcript before this fork is unavailable",
        ];
        let target = app.child_read_target().unwrap();
        app.child_read_finished(&target, Ok(ReadSubagentTranscriptResponse {
            generation: 1, next_cursor: 1, caught_up: false,
            updates: reasons.iter().map(|reason| serde_json::json!({"sessionUpdate": "kit_transcript_partial", "reason": reason})).collect(),
        }));
        for reason in reasons {
            assert!(app.child_views["child"].notice.contains(reason));
        }
        assert!(
            app.child_views["child"]
                .notice
                .contains("delivery is unknown")
        );
        assert!(app.child_views["child"].notice.contains("Loading"));
        read(&mut app, "supported live text", true);
        let target = app.child_read_target().unwrap();
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: target.3,
                caught_up: true,
                updates: vec![],
            }),
        );
        for reason in reasons {
            assert!(app.child_views["child"].notice.contains(reason));
        }
        assert!(
            app.child_views["child"]
                .notice
                .contains("delivery is unknown")
        );
        assert!(
            !app.child_views["child"]
                .notice
                .contains("child sent an unsupported update")
        );
    }

    #[test]
    fn oversized_record_omission_reason_survives_live_and_empty_pages() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.focus_child("child".into());
        let target = app.child_read_target().unwrap();
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: 1,
                caught_up: false,
                updates: vec![serde_json::json!({
                    "sessionUpdate": "kit_transcript_truncated",
                    "reason": "One inspection update exceeded 1 MiB and was omitted; later updates remain available"
                })],
            }),
        );
        read(&mut app, "supported live text", true);
        let target = app.child_read_target().unwrap();
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: target.3,
                caught_up: true,
                updates: vec![],
            }),
        );
        let notice = &app.child_views["child"].notice;
        assert!(notice.contains("exceeded 1 MiB"));
        assert!(notice.contains("later updates remain available"));
    }

    #[test]
    fn backend_partial_reasons_are_bounded_deduplicated_and_utf8_safe() {
        let mut app = app();
        lifecycle(&mut app, "child", 1, SubagentStatus::Working);
        app.focus_child("child".into());
        let target = app.child_read_target().unwrap();
        let mut updates = vec![
            serde_json::json!({"sessionUpdate": "kit_transcript_partial", "reason": "known limitation"});
            2
        ];
        updates.extend((0..8).map(|index| serde_json::json!({"sessionUpdate": "kit_transcript_partial", "reason": format!("{index}\n{}", "界".repeat(600))})));
        app.child_read_finished(
            &target,
            Ok(ReadSubagentTranscriptResponse {
                generation: 1,
                next_cursor: 1,
                caught_up: true,
                updates,
            }),
        );
        let view = &app.child_views["child"];
        assert_eq!(view.partial_reasons.len(), MAX_PARTIAL_REASONS);
        assert_eq!(
            view.partial_reasons
                .iter()
                .filter(|reason| *reason == "known limitation")
                .count(),
            1
        );
        assert!(
            view.partial_reasons
                .iter()
                .all(|reason| reason.len() <= MAX_PARTIAL_REASON_BYTES + '…'.len_utf8())
        );
        assert!(!view.notice.contains('\n'));
        assert!(view.notice.contains("additional limitations omitted"));
    }
}
