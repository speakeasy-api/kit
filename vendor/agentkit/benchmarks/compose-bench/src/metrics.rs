//! Run-level metric collection via the loop's [`LoopObserver`] event stream.
//!
//! The completions adapter emits exactly one `UsageUpdated` event per model
//! request, so counting those events gives the number of API round-trips and
//! summing them gives cumulative token usage. `ToolCallRequested` fires for
//! top-level tool calls only; tools invoked *inside* a compose script do not
//! re-enter the loop and are therefore not counted here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use agentkit_core::{Part, ToolOutput};
use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent};
use serde::Serialize;

#[derive(Default, Clone, Serialize)]
pub struct MetricsState {
    pub model_requests: u64,
    pub tool_calls: u64,
    pub compose_calls: u64,
    /// Compose calls that returned an error to the model (script rejected by
    /// the backend, runtime failure, budget exceeded). Each one costs a repair
    /// round-trip, so this isolates language-fluency friction from the
    /// composition mechanism itself.
    pub compose_failures: u64,
    #[serde(skip)]
    compose_call_ids: BTreeSet<String>,
    /// Runlet diagnostic codes (`RLnnnn`) seen in compose results, with
    /// occurrence counts — the failure taxonomy, queryable without opening
    /// transcripts. Empty for the Lua arm (its errors carry no codes).
    pub diagnostics: BTreeMap<String, u64>,
    pub tool_call_names: BTreeMap<String, u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// High-water mark of (input + cached input + output) tokens in a single
    /// request — i.e. how full the context window got.
    pub peak_context_tokens: u64,
    pub cost_usd: f64,
    pub cost_reported: bool,
}

pub struct MetricsObserver(pub Arc<Mutex<MetricsState>>);

impl LoopObserver for MetricsObserver {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        let mut state = self.0.lock().expect("metrics lock");
        match event {
            AgentEvent::UsageUpdated(usage) => {
                state.model_requests += 1;
                if let Some(tokens) = usage.tokens.as_ref() {
                    let cached = tokens.cached_input_tokens.unwrap_or(0);
                    state.input_tokens += tokens.input_tokens;
                    state.output_tokens += tokens.output_tokens;
                    state.cached_input_tokens += cached;
                    state.cache_write_tokens += tokens.cache_write_input_tokens.unwrap_or(0);
                    state.reasoning_tokens += tokens.reasoning_tokens.unwrap_or(0);
                    let context = tokens.input_tokens + cached + tokens.output_tokens;
                    state.peak_context_tokens = state.peak_context_tokens.max(context);
                }
                if let Some(cost) = usage.cost.as_ref() {
                    state.cost_usd += cost.amount;
                    state.cost_reported = true;
                }
            }
            AgentEvent::ToolCallRequested(call) => {
                state.tool_calls += 1;
                if call.name == agentkit_tool_compose::COMPOSE_TOOL_NAME {
                    state.compose_calls += 1;
                    state.compose_call_ids.insert(call.id.0.clone());
                }
                *state
                    .tool_call_names
                    .entry(call.name.to_string())
                    .or_default() += 1;
            }
            AgentEvent::ToolResultReceived(result) => {
                if state.compose_call_ids.contains(result.call_id.0.as_str()) {
                    if result.is_error {
                        state.compose_failures += 1;
                    }
                    for code in diagnostic_codes(&output_text(&result.output)) {
                        *state.diagnostics.entry(code).or_default() += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

fn output_text(output: &ToolOutput) -> String {
    match output {
        ToolOutput::Text(text) => text.clone(),
        ToolOutput::Structured(value) => value.to_string(),
        ToolOutput::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        ToolOutput::Files(_) => String::new(),
    }
}

/// Extracts `RLnnnn` diagnostic codes (e.g. `RL1008`, `RL2103`).
fn diagnostic_codes(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut codes = Vec::new();
    let mut i = 0;
    while i + 6 <= bytes.len() {
        if bytes[i] == b'R'
            && bytes[i + 1] == b'L'
            && bytes[i + 2..i + 6].iter().all(u8::is_ascii_digit)
            && (i + 6 == bytes.len() || !bytes[i + 6].is_ascii_alphanumeric())
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            codes.push(text[i..i + 6].to_string());
            i += 6;
        } else {
            i += 1;
        }
    }
    codes
}

/// One benchmark run, as persisted to `runs.jsonl`.
#[derive(Serialize)]
pub struct RunRecord {
    pub scenario: String,
    pub arm: String,
    pub rep: u32,
    pub model: String,
    pub wall_ms: u128,
    #[serde(flatten)]
    pub metrics: MetricsState,
    pub accuracy: f64,
    pub submitted: bool,
    pub completed: bool,
    pub failure: Option<String>,
    pub notes: Vec<String>,
}
