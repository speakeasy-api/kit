//! Helpdesk triage: scan paginated tickets, read bodies, escalate the ones
//! that match a multi-field predicate, and report the affected ids.
//!
//! Granular cost profile: 3 list pages + ~20 body reads + 6 updates + submit,
//! each a model round-trip. A compose script can do the whole sweep in one.

use std::sync::{Arc, Mutex};

use agentkit_tools_core::ToolRegistry;
use serde_json::{Value, json};

use crate::scenario::{
    Arm, BenchError, FnTool, Scenario, ScenarioInstance, Score, f1, get_str, get_u64, page_schema,
    paginate, submit_result_tool,
};

/// "Today" inside the fixture, also stated in the prompt.
const NOW: &str = "2026-06-10";

#[derive(Clone)]
struct Ticket {
    id: String,
    subject: String,
    status: &'static str,
    created_at: String,
    age_days: u64,
    body: String,
    priority: String,
    tags: Vec<String>,
}

fn fixture() -> Vec<Ticket> {
    (1..=30u64)
        .map(|i| {
            let status = if i % 3 == 0 { "closed" } else { "open" };
            let age_days = (i * 5) % 17;
            // Day arithmetic kept trivial: every fixture date lands in May/June 2026.
            let (month, day) = if age_days < 10 {
                (6, 10 - age_days)
            } else {
                (5, 41 - age_days)
            };
            let body = if i % 2 == 0 {
                format!(
                    "Customer reports a billing problem with order #{:04} and is \
                     requesting a refund for the full amount.",
                    1000 + i
                )
            } else {
                format!(
                    "Customer cannot log in to their account after the latest \
                     update; order #{:04} unaffected.",
                    1000 + i
                )
            };
            Ticket {
                id: format!("T-{}", 1000 + i),
                subject: format!("Order issue #{:04}", 1000 + i),
                status,
                created_at: format!("2026-{month:02}-{day:02}"),
                age_days,
                body,
                priority: "normal".into(),
                tags: vec!["inbound".into()],
            }
        })
        .collect()
}

fn is_target(ticket: &Ticket) -> bool {
    ticket.status == "open" && ticket.age_days > 7 && ticket.body.contains("refund")
}

pub struct SupportTriage;

impl Scenario for SupportTriage {
    fn name(&self) -> &'static str {
        "support-triage"
    }

    fn setup(&self, _arm: Arm) -> Result<ScenarioInstance, BenchError> {
        let world = Arc::new(Mutex::new(fixture()));

        let list_world = world.clone();
        let list_tickets = FnTool::new(
            "list_tickets",
            "List helpdesk tickets (id, subject, status, created_at only — bodies require get_ticket). Optionally filter by status. 10 per page.",
            json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["open", "closed"] },
                    "page": { "type": "integer", "minimum": 1, "default": 1 }
                },
                "additionalProperties": false
            }),
            page_schema(json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "subject": { "type": "string" },
                    "status": { "type": "string" },
                    "created_at": { "type": "string" }
                }
            })),
            move |input| {
                let status = input.get("status").and_then(Value::as_str);
                let page = get_u64(input, "page", 1);
                let tickets = list_world.lock().expect("world lock");
                let items: Vec<Value> = tickets
                    .iter()
                    .filter(|t| status.is_none_or(|s| t.status == s))
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "subject": t.subject,
                            "status": t.status,
                            "created_at": t.created_at,
                        })
                    })
                    .collect();
                Ok(paginate(items, page, 10))
            },
        )
        .read_only()
        // First call per run fails with a transient 503 (see FnTool::flaky).
        .flaky(1);

        let get_world = world.clone();
        let get_ticket = FnTool::new(
            "get_ticket",
            "Fetch one ticket in full, including its body, priority, and tags.",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "subject": { "type": "string" },
                    "status": { "type": "string" },
                    "created_at": { "type": "string" },
                    "body": { "type": "string" },
                    "priority": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                }
            }),
            move |input| {
                let id = get_str(input, "id")?;
                let tickets = get_world.lock().expect("world lock");
                let ticket = tickets
                    .iter()
                    .find(|t| t.id == id)
                    .ok_or_else(|| format!("no ticket with id {id}"))?;
                Ok(json!({
                    "id": ticket.id,
                    "subject": ticket.subject,
                    "status": ticket.status,
                    "created_at": ticket.created_at,
                    "body": ticket.body,
                    "priority": ticket.priority,
                    "tags": ticket.tags,
                }))
            },
        )
        .read_only();

        let update_world = world.clone();
        let update_ticket = FnTool::new(
            "update_ticket",
            "Update a ticket's priority and/or append tags. Returns the updated ticket summary.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "priority": { "type": "string", "enum": ["low", "normal", "high", "urgent"] },
                    "add_tags": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "priority": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                }
            }),
            move |input| {
                let id = get_str(input, "id")?.to_string();
                let mut tickets = update_world.lock().expect("world lock");
                let ticket = tickets
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or_else(|| format!("no ticket with id {id}"))?;
                if let Some(priority) = input.get("priority").and_then(Value::as_str) {
                    ticket.priority = priority.to_string();
                }
                if let Some(tags) = input.get("add_tags").and_then(Value::as_array) {
                    for tag in tags.iter().filter_map(Value::as_str) {
                        if !ticket.tags.iter().any(|t| t == tag) {
                            ticket.tags.push(tag.to_string());
                        }
                    }
                }
                Ok(json!({ "id": ticket.id, "priority": ticket.priority, "tags": ticket.tags }))
            },
        );

        let (submit, submission) = submit_result_tool(json!({
            "type": "object",
            "properties": {
                "escalated_ticket_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "IDs of every ticket you escalated"
                }
            },
            "required": ["escalated_ticket_ids"]
        }));

        let tools = ToolRegistry::new()
            .with(list_tickets)
            .with(get_ticket)
            .with(update_ticket)
            .with(submit);

        let user_prompt = format!(
            "Today is {NOW}. In our helpdesk, escalate every OPEN ticket that was created \
             MORE than 7 days ago AND whose body mentions a refund: set its priority to \
             \"high\" and add the tag \"billing-escalation\". Subjects do not mention \
             refunds — you must check ticket bodies. Do not modify any other ticket. \
             When done, submit {{\"escalated_ticket_ids\": [..]}} via submit_result."
        );

        let scorer_world = world;
        let scorer_submission = submission.clone();
        let scorer = Box::new(move || {
            let tickets = scorer_world.lock().expect("world lock");
            let targets: Vec<String> = tickets
                .iter()
                .filter(|t| is_target(t))
                .map(|t| t.id.clone())
                .collect();
            let mut correct = 0usize;
            let mut wrongly_touched = 0usize;
            for ticket in tickets.iter() {
                let escalated = ticket.priority == "high"
                    && ticket.tags.iter().any(|t| t == "billing-escalation");
                let modified = ticket.priority != "normal"
                    || ticket.tags.len() != 1
                    || ticket.tags[0] != "inbound";
                if is_target(ticket) {
                    if escalated {
                        correct += 1;
                    }
                } else if modified {
                    wrongly_touched += 1;
                }
            }
            let state_score = (correct as f64 / targets.len() as f64
                - wrongly_touched as f64 / targets.len() as f64)
                .clamp(0.0, 1.0);

            let submitted = scorer_submission.lock().expect("submission lock").clone();
            let submitted_ids: Vec<String> = submitted
                .as_ref()
                .and_then(|v| v.get("escalated_ticket_ids"))
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let answer_score = f1(&submitted_ids, &targets);

            Score {
                accuracy: 0.6 * state_score + 0.4 * answer_score,
                notes: vec![
                    format!("targets={targets:?}"),
                    format!(
                        "state: {correct}/{} escalated, {wrongly_touched} wrongly touched",
                        targets.len()
                    ),
                    format!("answer ids={submitted_ids:?} f1={answer_score:.2}"),
                ],
            }
        });

        Ok(ScenarioInstance {
            tools,
            user_prompt,
            permissions: None,
            submission,
            scorer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_has_a_meaningful_target_set() {
        let tickets = fixture();
        let targets: Vec<&str> = tickets
            .iter()
            .filter(|t| is_target(t))
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(
            targets,
            vec!["T-1002", "T-1010", "T-1016", "T-1020", "T-1022", "T-1026"]
        );
        // Distractors exist on every axis of the predicate.
        assert!(
            tickets
                .iter()
                .any(|t| t.status == "closed" && t.body.contains("refund"))
        );
        assert!(
            tickets
                .iter()
                .any(|t| t.status == "open" && t.age_days <= 7 && t.body.contains("refund"))
        );
        assert!(
            tickets
                .iter()
                .any(|t| t.status == "open" && t.age_days > 7 && !t.body.contains("refund"))
        );
    }
}
