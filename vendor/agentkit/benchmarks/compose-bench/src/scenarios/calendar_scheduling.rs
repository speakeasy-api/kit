//! Cross-calendar scheduling: find the earliest 60-minute slot, on a 30-minute
//! boundary inside working hours, where all four people are free. Requires
//! fetching availability per (person, day) — 20 reads in the granular arm —
//! then a small constraint solve.

use agentkit_tools_core::ToolRegistry;
use serde_json::{Value, json};

use crate::scenario::{
    Arm, BenchError, FnTool, Scenario, ScenarioInstance, Score, get_str, submit_result_tool,
};

const USERS: [(&str, &str); 4] = [
    ("U1", "Alice"),
    ("U2", "Bob"),
    ("U3", "Carol"),
    ("U4", "Dan"),
];

const DATES: [&str; 5] = [
    "2026-06-15",
    "2026-06-16",
    "2026-06-17",
    "2026-06-18",
    "2026-06-19",
];

#[cfg(test)]
const WORK_START_MIN: u32 = 9 * 60;
#[cfg(test)]
const WORK_END_MIN: u32 = 17 * 60;

const EXPECTED_START: &str = "2026-06-17T14:00:00Z";
const EXPECTED_END: &str = "2026-06-17T15:00:00Z";

/// Busy intervals as (user, date, start "HH:MM", end "HH:MM").
const BUSY: &[(&str, &str, &str, &str)] = &[
    // Monday: union of busy blocks covers 09:00-17:00 for the group.
    ("U1", "2026-06-15", "09:00", "12:00"),
    ("U2", "2026-06-15", "11:30", "15:00"),
    ("U3", "2026-06-15", "14:30", "17:00"),
    ("U4", "2026-06-15", "10:00", "11:00"),
    // Tuesday: ditto.
    ("U2", "2026-06-16", "09:00", "13:30"),
    ("U1", "2026-06-16", "13:00", "17:00"),
    ("U4", "2026-06-16", "11:00", "12:00"),
    // Wednesday: first common >= 60-minute gap opens at 14:00.
    ("U3", "2026-06-17", "09:00", "10:00"),
    ("U1", "2026-06-17", "09:00", "11:00"),
    ("U2", "2026-06-17", "10:30", "14:00"),
    ("U4", "2026-06-17", "15:30", "16:30"),
    // Thursday/Friday: mostly free (irrelevant — Wednesday wins).
    ("U1", "2026-06-18", "09:00", "09:30"),
    ("U3", "2026-06-19", "16:00", "17:00"),
];

#[cfg(test)]
fn parse_min(value: &str) -> u32 {
    let (h, m) = value.split_once(':').expect("HH:MM");
    h.parse::<u32>().expect("hour") * 60 + m.parse::<u32>().expect("minute")
}

/// Recomputes the earliest common slot from the fixture (used by tests to pin
/// the expected answer, so fixture edits can't silently break scoring).
#[cfg(test)]
fn earliest_common_slot() -> Option<(String, String)> {
    for date in DATES {
        let mut start = WORK_START_MIN;
        while start + 60 <= WORK_END_MIN {
            let end = start + 60;
            let all_free = USERS.iter().all(|(id, _)| {
                BUSY.iter()
                    .filter(|(u, d, _, _)| u == id && *d == date)
                    .all(|(_, _, s, e)| {
                        let (busy_start, busy_end) = (parse_min(s), parse_min(e));
                        end <= busy_start || start >= busy_end
                    })
            });
            if all_free {
                let fmt = |m: u32| format!("{date}T{:02}:{:02}:00Z", m / 60, m % 60);
                return Some((fmt(start), fmt(end)));
            }
            start += 30;
        }
    }
    None
}

pub struct CalendarScheduling;

impl Scenario for CalendarScheduling {
    fn name(&self) -> &'static str {
        "calendar-scheduling"
    }

    fn setup(&self, _arm: Arm) -> Result<ScenarioInstance, BenchError> {
        let list_users = FnTool::new(
            "list_users",
            "List the team members whose calendars are visible.",
            json!({ "type": "object", "additionalProperties": false }),
            json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" }
                    }
                }
            }),
            move |_input| {
                Ok(Value::Array(
                    USERS
                        .iter()
                        .map(|(id, name)| json!({ "id": id, "name": name }))
                        .collect(),
                ))
            },
        )
        .read_only();

        let get_availability = FnTool::new(
            "get_availability",
            "Get one user's busy intervals (UTC) for one date. Times outside the returned intervals are free.",
            json!({
                "type": "object",
                "properties": {
                    "user_id": { "type": "string" },
                    "date": { "type": "string", "description": "YYYY-MM-DD" }
                },
                "required": ["user_id", "date"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "user_id": { "type": "string" },
                    "date": { "type": "string" },
                    "busy": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "start": { "type": "string", "description": "HH:MM UTC" },
                                "end": { "type": "string", "description": "HH:MM UTC" }
                            }
                        }
                    }
                }
            }),
            move |input| {
                let user_id = get_str(input, "user_id")?;
                let date = get_str(input, "date")?;
                if !USERS.iter().any(|(id, _)| *id == user_id) {
                    return Err(format!("unknown user {user_id}"));
                }
                if !DATES.contains(&date) {
                    return Err(format!("no calendar data for {date}"));
                }
                let busy: Vec<Value> = BUSY
                    .iter()
                    .filter(|(u, d, _, _)| *u == user_id && *d == date)
                    .map(|(_, _, start, end)| json!({ "start": start, "end": end }))
                    .collect();
                Ok(json!({ "user_id": user_id, "date": date, "busy": busy }))
            },
        )
        .read_only()
        // First call per run fails with a transient 503 (see FnTool::flaky).
        .flaky(1);

        let (submit, submission) = submit_result_tool(json!({
            "type": "object",
            "properties": {
                "start": { "type": "string", "description": "ISO-8601 UTC, e.g. 2026-06-15T09:00:00Z" },
                "end": { "type": "string", "description": "ISO-8601 UTC" }
            },
            "required": ["start", "end"]
        }));

        let tools = ToolRegistry::new()
            .with(list_users)
            .with(get_availability)
            .with(submit);

        let user_prompt = "Schedule a 60-minute meeting for the whole team (everyone returned by \
             list_users) during the week of 2026-06-15 to 2026-06-19. Constraints: the slot must \
             lie entirely within working hours 09:00-17:00 UTC, must start on a 30-minute \
             boundary, and every team member must be free for the full hour. Find the EARLIEST \
             such slot in the week. When done, submit {\"start\": .., \"end\": ..} as ISO-8601 \
             UTC timestamps via submit_result."
            .to_string();

        let scorer_submission = submission.clone();
        let scorer = Box::new(move || {
            let submitted = scorer_submission.lock().expect("submission lock").clone();
            let canon = |value: Option<&str>| -> String {
                value
                    .unwrap_or_default()
                    .trim()
                    .replace("+00:00", "Z")
                    .replace(".000Z", "Z")
                    .to_uppercase()
            };
            let start = canon(
                submitted
                    .as_ref()
                    .and_then(|v| v.get("start"))
                    .and_then(Value::as_str),
            );
            let end = canon(
                submitted
                    .as_ref()
                    .and_then(|v| v.get("end"))
                    .and_then(Value::as_str),
            );
            // Accept second-less variants like 2026-06-17T14:00Z.
            let matches = |got: &str, want: &str| got == want || got == want.replace(":00Z", "Z");
            let mut accuracy = 0.0;
            if matches(&start, EXPECTED_START) {
                accuracy += 0.5;
            }
            if matches(&end, EXPECTED_END) {
                accuracy += 0.5;
            }
            Score {
                accuracy,
                notes: vec![format!(
                    "expected {EXPECTED_START}..{EXPECTED_END}, got {start}..{end}"
                )],
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
    fn expected_slot_matches_recomputed_ground_truth() {
        let (start, end) = earliest_common_slot().expect("a common slot must exist");
        assert_eq!(start, EXPECTED_START);
        assert_eq!(end, EXPECTED_END);
    }
}
