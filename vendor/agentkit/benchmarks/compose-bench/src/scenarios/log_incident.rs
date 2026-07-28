//! Observability investigation: find which service's errors spiked on
//! 2026-06-09, correlate the spike with the deploy that immediately preceded
//! it, and look up the on-call owner. Read-only, multi-tool, needs filtering
//! and cross-referencing rather than brute enumeration.

use agentkit_tools_core::ToolRegistry;
use serde_json::{Value, json};

use crate::scenario::{
    Arm, BenchError, FnTool, Scenario, ScenarioInstance, Score, get_str, get_u64, page_schema,
    paginate, submit_result_tool,
};

const SERVICES: [&str; 4] = ["auth", "checkout", "payments", "search"];

const CULPRIT_SERVICE: &str = "payments";
const CULPRIT_SHA: &str = "9f3c2ab";
const CULPRIT_OWNER: &str = "maya@example.com";

#[derive(Clone)]
struct LogEntry {
    ts: String,
    service: &'static str,
    level: &'static str,
    message: String,
}

fn ts(hour: u64, minute: u64) -> String {
    format!("2026-06-09T{hour:02}:{minute:02}:00Z")
}

fn fixture_logs() -> Vec<LogEntry> {
    let mut logs = Vec::new();
    // Steady INFO noise for every service through the day.
    for (s, service) in SERVICES.iter().enumerate() {
        for slot in 0..18u64 {
            logs.push(LogEntry {
                ts: ts(8 + slot / 2, (slot % 2) * 30 + s as u64),
                service,
                level: "INFO",
                message: format!(
                    "{service}: request handled in {}ms",
                    40 + slot * 3 + s as u64
                ),
            });
        }
    }
    // Background error noise: a few scattered, low-rate errors per service.
    for (s, service) in SERVICES.iter().enumerate() {
        for k in 0..3u64 {
            logs.push(LogEntry {
                ts: ts(9 + k * 3, 7 + s as u64 * 11),
                service,
                level: "ERROR",
                message: format!("{service}: transient upstream 503 (retried ok)"),
            });
        }
    }
    // The incident: payments errors burst right after the 14:05 deploy.
    for k in 0..30u64 {
        logs.push(LogEntry {
            ts: ts(14 + (10 + k * 90 / 60) / 60, (10 + k * 90 / 60) % 60),
            service: "payments",
            level: "ERROR",
            message: "payments: provider timeout after 30000ms (card capture failed)".into(),
        });
    }
    logs.sort_by(|a, b| a.ts.cmp(&b.ts));
    logs
}

fn fixture_deploys(service: &str) -> Vec<Value> {
    let shas: &[(&str, &str)] = match service {
        "auth" => &[("c11ab90", "08:40"), ("d27e441", "12:15")],
        "checkout" => &[("77aa210", "09:30"), ("81b6c02", "15:20")],
        "payments" => &[
            ("4e0d9c1", "09:10"),
            (CULPRIT_SHA, "14:05"),
            ("b8123f7", "16:45"),
        ],
        "search" => &[("f00dbed", "10:55")],
        _ => &[],
    };
    shas.iter()
        .map(|(sha, time)| {
            json!({
                "sha": sha,
                "service": service,
                "deployed_at": format!("2026-06-09T{time}:00Z"),
            })
        })
        .collect()
}

fn owner_email(service: &str) -> Option<&'static str> {
    match service {
        "auth" => Some("liam@example.com"),
        "checkout" => Some("noor@example.com"),
        "payments" => Some(CULPRIT_OWNER),
        "search" => Some("petra@example.com"),
        _ => None,
    }
}

pub struct LogIncident;

impl Scenario for LogIncident {
    fn name(&self) -> &'static str {
        "log-incident"
    }

    fn setup(&self, _arm: Arm) -> Result<ScenarioInstance, BenchError> {
        let logs = fixture_logs();

        let list_services = FnTool::new(
            "list_services",
            "List the names of all monitored services.",
            json!({ "type": "object", "additionalProperties": false }),
            json!({ "type": "array", "items": { "type": "string" } }),
            move |_input| Ok(json!(SERVICES)),
        )
        .read_only();

        let search_data = logs;
        let search_logs = FnTool::new(
            "search_logs",
            "Search log entries for 2026-06-09, filtered by service, level, and/or an ISO-8601 time range. Sorted by timestamp, 20 per page.",
            json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string" },
                    "level": { "type": "string", "enum": ["INFO", "ERROR"] },
                    "since": { "type": "string", "description": "inclusive ISO-8601 lower bound" },
                    "until": { "type": "string", "description": "exclusive ISO-8601 upper bound" },
                    "page": { "type": "integer", "minimum": 1, "default": 1 }
                },
                "additionalProperties": false
            }),
            page_schema(json!({
                "type": "object",
                "properties": {
                    "ts": { "type": "string" },
                    "service": { "type": "string" },
                    "level": { "type": "string" },
                    "message": { "type": "string" }
                }
            })),
            move |input| {
                let service = input.get("service").and_then(Value::as_str);
                let level = input.get("level").and_then(Value::as_str);
                let since = input.get("since").and_then(Value::as_str);
                let until = input.get("until").and_then(Value::as_str);
                let page = get_u64(input, "page", 1);
                let items: Vec<Value> = search_data
                    .iter()
                    .filter(|e| service.is_none_or(|s| e.service == s))
                    .filter(|e| level.is_none_or(|l| e.level == l))
                    .filter(|e| since.is_none_or(|s| e.ts.as_str() >= s))
                    .filter(|e| until.is_none_or(|u| e.ts.as_str() < u))
                    .map(|e| {
                        json!({
                            "ts": e.ts,
                            "service": e.service,
                            "level": e.level,
                            "message": e.message,
                        })
                    })
                    .collect();
                Ok(paginate(items, page, 20))
            },
        )
        .read_only()
        // First call per run fails with a transient 503 (see FnTool::flaky).
        .flaky(1);

        let get_deploys = FnTool::new(
            "get_deploys",
            "List deploys for a service on 2026-06-09 (sha, deployed_at).",
            json!({
                "type": "object",
                "properties": { "service": { "type": "string" } },
                "required": ["service"],
                "additionalProperties": false
            }),
            json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "sha": { "type": "string" },
                        "service": { "type": "string" },
                        "deployed_at": { "type": "string" }
                    }
                }
            }),
            move |input| {
                let service = get_str(input, "service")?;
                if !SERVICES.contains(&service) {
                    return Err(format!("unknown service {service}"));
                }
                Ok(Value::Array(fixture_deploys(service)))
            },
        )
        .read_only();

        let get_service_owners = FnTool::new(
            "get_service_owners",
            "Look up the on-call owner email for a service.",
            json!({
                "type": "object",
                "properties": { "service": { "type": "string" } },
                "required": ["service"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string" },
                    "owner_email": { "type": "string" }
                }
            }),
            move |input| {
                let service = get_str(input, "service")?;
                let owner =
                    owner_email(service).ok_or_else(|| format!("unknown service {service}"))?;
                Ok(json!({ "service": service, "owner_email": owner }))
            },
        )
        .read_only();

        let (submit, submission) = submit_result_tool(json!({
            "type": "object",
            "properties": {
                "service": { "type": "string", "description": "service whose errors spiked" },
                "deploy_sha": { "type": "string", "description": "sha of the deploy that triggered the spike" },
                "owner_email": { "type": "string", "description": "on-call owner of that service" }
            },
            "required": ["service", "deploy_sha", "owner_email"]
        }));

        let tools = ToolRegistry::new()
            .with(list_services)
            .with(search_logs)
            .with(get_deploys)
            .with(get_service_owners)
            .with(submit);

        let user_prompt = "On 2026-06-09 one of our services started failing in production. \
             Using the monitoring tools, identify: (1) the service whose ERROR rate spiked well \
             above its background level, (2) the sha of the deploy to that service which \
             immediately preceded the spike, and (3) the on-call owner's email for that service. \
             Every service logs occasional transient errors all day — the incident is a sustained \
             burst. When done, submit {\"service\": .., \"deploy_sha\": .., \"owner_email\": ..} \
             via submit_result."
            .to_string();

        let scorer_submission = submission.clone();
        let scorer = Box::new(move || {
            let submitted = scorer_submission.lock().expect("submission lock").clone();
            let field = |key: &str| -> Option<String> {
                submitted
                    .as_ref()
                    .and_then(|v| v.get(key))
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_lowercase())
            };
            let mut hits = 0u32;
            if field("service").as_deref() == Some(CULPRIT_SERVICE) {
                hits += 1;
            }
            if field("deploy_sha").as_deref() == Some(CULPRIT_SHA) {
                hits += 1;
            }
            if field("owner_email").as_deref() == Some(CULPRIT_OWNER) {
                hits += 1;
            }
            Score {
                accuracy: f64::from(hits) / 3.0,
                notes: vec![format!(
                    "expected service={CULPRIT_SERVICE} sha={CULPRIT_SHA} owner={CULPRIT_OWNER}; got {submitted:?}"
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
    fn incident_burst_dominates_background_noise() {
        let logs = fixture_logs();
        let payments_errors = logs
            .iter()
            .filter(|e| e.service == "payments" && e.level == "ERROR")
            .count();
        for service in SERVICES.iter().filter(|s| **s != "payments") {
            let errors = logs
                .iter()
                .filter(|e| e.service == *service && e.level == "ERROR")
                .count();
            assert!(payments_errors >= errors * 5, "{service} noise too loud");
        }
        // Burst starts after the culprit deploy at 14:05 and not before.
        let first_burst = logs
            .iter()
            .find(|e| e.message.contains("provider timeout"))
            .expect("burst exists");
        assert!(first_burst.ts.as_str() > "2026-06-09T14:05:00Z");
    }
}
