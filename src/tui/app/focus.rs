//! Child views consume their automatically delivered ACP session streams.
use super::*;

const MAX_CHILD_DRAFT: usize = 16 * 1024;
pub struct ChildView {
    pub app: Box<App>,
    pub notice: String,
    pub generation: u64,
    pub can_steer: bool,
    active: bool,
    pending_text: Option<String>,
    steer_notice: Option<String>,
}

impl ChildView {
    pub(super) fn disable(&mut self, notice: &str) {
        self.active = false;
        self.can_steer = false;
        self.pending_text = None;
        self.steer_notice = None;
        self.app.finish_replay_incomplete();
        self.notice = notice.into();
    }

    fn refresh_notice(&mut self) {
        self.notice = if self.pending_text.is_some() {
            "Sending steer to child…"
        } else if !self.active {
            "Child not confirmed working; transcript is read-only"
        } else if !self.can_steer {
            "Read-only: compatible steering is not available for this child"
        } else {
            "Text steering available · Enter to send · Esc back to main"
        }
        .into();
        if let Some(notice) = &self.steer_notice {
            self.notice = format!("{notice} · {}", self.notice);
        }
    }
}

impl App {
    pub fn leave_child(&mut self) {
        self.child_focus = None;
        self.agents_keyboard_focus = false;
        self.agents_selected = None;
        self.clipboard_route_epoch = self.clipboard_route_epoch.wrapping_add(1);
        self.child_back_area = Rect::default();
    }

    pub fn focus_child(&mut self, id: String) {
        self.leave_child();
        self.ensure_child_view(&id);
        if !self.child_views.contains_key(&id) {
            return;
        }
        self.agents_selected = Some(id.clone());
        self.child_focus = Some(id);
        self.agents_keyboard_focus = false;
        self.selection = None;
        self.press = None;
    }

    pub(super) fn ensure_child_view(&mut self, id: &str) {
        if self.child_views.contains_key(id) {
            return;
        }
        let Some(row) = self.agents.get(id) else {
            return;
        };
        let generation = row.generation;
        let active = row.status == SubagentStatus::Working;
        let direct = row.parent_id.is_none();
        let can_steer = active && direct && row.focus_can_steer;
        let mut app = App::new(
            self.root.clone(),
            String::new(),
            String::new(),
            String::new(),
        );
        app.show_thoughts = self.show_thoughts;
        app.cost = row.cost.clone();
        app.usage = row.usage;
        app.phase = if active { Phase::Working } else { Phase::Idle };
        if let Some(state) = self
            .child_sessions
            .get(id)
            .and_then(|association| association.snapshot.state.value())
            .cloned()
        {
            app.apply(Update::State(state));
        }
        let mut view = ChildView {
            app: Box::new(app),
            generation,
            can_steer,
            active,
            notice: String::new(),
            pending_text: None,
            steer_notice: None,
        };
        view.refresh_notice();
        self.child_views.insert(id.to_owned(), view);
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

    pub(super) fn child_lifecycle(&mut self, id: &str, generation: u64, status: SubagentStatus) {
        let Some(view) = self.child_views.get(id) else {
            return;
        };
        if generation < view.generation {
            return;
        }
        let Some(view) = self.child_views.get_mut(id) else {
            return;
        };
        if generation != view.generation {
            view.generation = generation;
            view.pending_text = None;
            view.steer_notice = None;
            view.can_steer = false;
        }
        if view.active != (status == SubagentStatus::Working) {
            self.clipboard_route_epoch = self.clipboard_route_epoch.wrapping_add(1);
            view.steer_notice = None;
        }
        view.active = status == SubagentStatus::Working;
        if view.active {
            view.app.phase = Phase::Working;
        } else {
            view.can_steer = false;
            if status == SubagentStatus::Removed {
                view.app.finish_replay_incomplete();
            }
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
        // Drags and releases still reach the child, so a scrollbar drag or
        // selection can cross into the roster beside its transcript.
        if self.agents_area.contains((mouse.column, mouse.row).into())
            && !matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
        {
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

    fn key(app: &mut App, code: KeyCode) -> Action {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
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
}
