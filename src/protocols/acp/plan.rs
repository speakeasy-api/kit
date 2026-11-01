//! A bounded rolling view of real task-manager work, not an inferred strategy.
//! The session actor observes events at its existing dequeue boundary: no new
//! consumer can steal completion events or start an autonomous continuation.
use agentkit_task_manager::{TaskEvent, TaskKind};

// Both the actor and structured settlement run in this task. Task-local
// ownership lets each existing dequeue boundary observe without adding an
// event consumer, global registry, shared lock, or changing settlement order.
tokio::task_local! {
    static OBSERVER: Observer;
}

struct Observer {
    plan: std::cell::RefCell<TaskPlan>,
    project: Box<dyn Fn(TaskPlan) + Send>,
}

pub(super) async fn scope<T>(
    project: impl Fn(TaskPlan) + Send + 'static,
    future: impl std::future::Future<Output = T>,
) -> T {
    OBSERVER
        .scope(
            Observer {
                plan: std::cell::RefCell::new(TaskPlan::default()),
                project: Box::new(project),
            },
            future,
        )
        .await
}

pub(super) fn observe(event: &TaskEvent) {
    let _ = OBSERVER.try_with(|observer| {
        let snapshot = {
            let mut plan = observer.plan.borrow_mut();
            plan.observe(event).then(|| plan.clone())
        };
        // Release the reducer borrow before invoking transport code.
        if let Some(snapshot) = snapshot {
            (observer.project)(snapshot);
        }
    });
}

const MAX_ENTRIES: usize = 64;
const MAX_LABEL_CHARS: usize = 160;

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    id: agentkit_core::TaskId,
    label: String,
    background: bool,
    completed: bool,
}

#[derive(Clone, Default)]
pub(super) struct TaskPlan {
    entries: Vec<Entry>,
}

impl TaskPlan {
    /// Failed/cancelled work is removed, never falsely marked successful. v1
    /// has no failed/cancelled status. Results remain on the existing tool cards.
    pub(super) fn observe(&mut self, event: &TaskEvent) -> bool {
        let (snapshot, completed) = match event {
            TaskEvent::Started(snapshot) | TaskEvent::Detached(snapshot) => (snapshot, false),
            TaskEvent::Completed(snapshot, result) if !result.is_error => (snapshot, true),
            TaskEvent::Completed(snapshot, _)
            | TaskEvent::Failed(snapshot, _)
            | TaskEvent::Cancelled(snapshot) => {
                let before = self.entries.len();
                self.entries.retain(|entry| entry.id != snapshot.id);
                return self.entries.len() != before;
            }
            TaskEvent::ContinueRequested => return false,
        };
        let entry = Entry {
            id: snapshot.id.clone(),
            label: snapshot
                .tool_name
                .chars()
                .filter(|character| !character.is_control())
                .take(MAX_LABEL_CHARS)
                .collect(),
            background: snapshot.kind == TaskKind::Background,
            completed,
        };
        if let Some(existing) = self.entries.iter_mut().find(|item| item.id == entry.id) {
            if *existing == entry {
                return false;
            }
            *existing = entry;
        } else {
            if self.entries.len() == MAX_ENTRIES {
                // Prefer keeping current work over historical successful work.
                let oldest = self
                    .entries
                    .iter()
                    .position(|item| item.completed)
                    .unwrap_or(0);
                self.entries.remove(oldest);
            }
            self.entries.push(entry);
        }
        true
    }

    pub(super) fn v1(&self) -> agentkit_acp::SessionUpdate {
        use agentkit_acp::{Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionUpdate};
        SessionUpdate::Plan(Plan::new(
            self.entries
                .iter()
                .map(|entry| {
                    PlanEntry::new(
                        entry.label.clone(),
                        if entry.background {
                            PlanEntryPriority::Low
                        } else {
                            PlanEntryPriority::Medium
                        },
                        if entry.completed {
                            PlanEntryStatus::Completed
                        } else {
                            PlanEntryStatus::InProgress
                        },
                    )
                })
                .collect(),
        ))
    }

    pub(super) fn v2(&self) -> agentkit_acp::v2::wire::SessionUpdate {
        use agentkit_acp::v2::wire::{
            PlanEntry, PlanEntryPriority, PlanEntryStatus, PlanItems, PlanUpdate,
            PlanUpdateContent, SessionUpdate,
        };
        SessionUpdate::PlanUpdate(PlanUpdate::new(PlanUpdateContent::Items(PlanItems::new(
            "kit.tasks",
            self.entries
                .iter()
                .map(|entry| {
                    PlanEntry::new(
                        entry.label.clone(),
                        if entry.background {
                            PlanEntryPriority::Low
                        } else {
                            PlanEntryPriority::Medium
                        },
                        if entry.completed {
                            PlanEntryStatus::Completed
                        } else {
                            PlanEntryStatus::InProgress
                        },
                    )
                })
                .collect(),
        ))))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::disallowed_methods
)]
mod tests {
    use super::*;
    use agentkit_core::{MetadataMap, TaskId, ToolCallId, ToolResultPart, TurnId};
    use agentkit_task_manager::TaskSnapshot;
    use serde_json::{Value, json};

    fn snapshot(id: &str) -> TaskSnapshot {
        TaskSnapshot {
            id: TaskId::new(id),
            turn_id: TurnId::new("turn"),
            call_id: ToolCallId::new(id),
            tool_name: "compose".into(),
            kind: TaskKind::Foreground,
            metadata: MetadataMap::new(),
        }
    }

    fn entries(plan: &TaskPlan) -> (Value, Value) {
        let v1 = serde_json::to_value(plan.v1()).unwrap();
        let v2 = serde_json::to_value(plan.v2()).unwrap();
        assert_eq!(v1["sessionUpdate"], "plan");
        assert_eq!(v2["sessionUpdate"], "plan_update");
        assert_eq!(v2["plan"]["planId"], "kit.tasks");
        (v1["entries"].clone(), v2["plan"]["entries"].clone())
    }

    #[test]
    fn lifecycle_updates_both_wire_versions_without_inventing_steps() {
        let mut plan = TaskPlan::default();
        let mut task = snapshot("a");
        assert!(!plan.observe(&TaskEvent::ContinueRequested));
        assert!(plan.observe(&TaskEvent::Started(task.clone())));
        assert!(!plan.observe(&TaskEvent::Started(task.clone())));
        let (v1, v2) = entries(&plan);
        assert_eq!(v1, v2);
        assert_eq!(
            v1,
            json!([{ "content": "compose", "priority": "medium", "status": "in_progress" }])
        );
        task.kind = TaskKind::Background;
        assert!(plan.observe(&TaskEvent::Detached(task.clone())));
        assert_eq!(entries(&plan).0[0]["priority"], "low");
        let result = ToolResultPart::success(
            task.call_id.clone(),
            agentkit_core::ToolOutput::structured(json!({"private": "not projected"})),
        );
        assert!(plan.observe(&TaskEvent::Completed(task, result)));
        let (v1, v2) = entries(&plan);
        assert_eq!(v1, v2);
        assert_eq!(v1[0]["status"], "completed");
        assert!(!v1.to_string().contains("private"));
    }

    #[test]
    fn unsuccessful_work_is_removed_not_marked_successful() {
        let mut plan = TaskPlan::default();
        let task = snapshot("a");
        plan.observe(&TaskEvent::Started(task.clone()));
        assert!(plan.observe(&TaskEvent::Cancelled(task.clone())));
        assert_eq!(entries(&plan).0, json!([]));
        assert!(!plan.observe(&TaskEvent::Cancelled(task.clone())));
        plan.observe(&TaskEvent::Started(task.clone()));
        let result = ToolResultPart::error(
            task.call_id.clone(),
            agentkit_core::ToolOutput::Text("private error payload".into()),
        );
        assert!(plan.observe(&TaskEvent::Completed(task, result)));
        assert_eq!(entries(&plan).0, json!([]));
    }

    #[test]
    fn rolling_plan_and_labels_are_bounded() {
        let mut plan = TaskPlan::default();
        for index in 0..MAX_ENTRIES + 10 {
            let mut task = snapshot(&index.to_string());
            task.tool_name = "界\n".repeat(MAX_LABEL_CHARS + 10);
            plan.observe(&TaskEvent::Started(task));
        }
        let (v1, v2) = entries(&plan);
        assert_eq!(v1, v2);
        assert_eq!(v1.as_array().unwrap().len(), MAX_ENTRIES);
        for entry in v1.as_array().unwrap() {
            assert_eq!(
                entry["content"].as_str().unwrap().chars().count(),
                MAX_LABEL_CHARS
            );
        }
    }
}
