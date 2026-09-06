//! Opt-in, operation-local tracing history for existing fatal diagnostics.
//! This is a partial history, not an effects ledger or a causal error chain.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tracing::{
    Span, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, Registry, layer::Context, registry::LookupSpan};

const MAX_FRAGMENTS: usize = 24;
const MAX_DEPTH: usize = 8;
const MAX_FIELDS: usize = 6;
const MAX_VALUE_BYTES: usize = 32;
const MAX_SNAPSHOT_BYTES: usize = 12 * 1024;
const TARGET: &str = "kit::telemetry::error_spans";

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod task_manager_tests;

/// The future instrumented with this span must include execution AND error logging.
/// Each call starts a fresh history even when nested within another operation.
pub(crate) fn operation(surface: &'static str) -> Span {
    tracing::info_span!(target: TARGET, parent: Span::current(), "kit.operation", surface)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    fragments: Vec<Fragment>,
    /// Indicates a collection bound, not whether observations are complete.
    truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Fragment {
    name: String,
    parent: Option<usize>,
    fields: BTreeMap<String, serde_json::Value>,
}

impl Snapshot {
    pub(crate) fn valid(&self) -> bool {
        !self.fragments.is_empty()
            && self.fragments.len() <= MAX_FRAGMENTS
            && self.fragments.iter().enumerate().all(|(index, fragment)| {
                (if index == 0 {
                    fragment.name == "kit.operation" && fragment.parent.is_none()
                } else {
                    approved_name(&fragment.name)
                        && fragment.parent.is_some_and(|parent| parent < index)
                }) && {
                    let mut parent = fragment.parent;
                    let mut depth = 0;
                    while let Some(index) = parent {
                        depth += 1;
                        if depth > MAX_DEPTH {
                            return false;
                        }
                        parent = self.fragments[index].parent;
                    }
                    true
                } && fragment.fields.len() <= MAX_FIELDS
                    && fragment
                        .fields
                        .iter()
                        .all(|(key, value)| approved_value(key, value))
            })
            && serde_json::to_vec_pretty(self).is_ok_and(|bytes| bytes.len() <= MAX_SNAPSHOT_BYTES)
    }
}

#[derive(Clone)]
struct Capture {
    history: Arc<Mutex<Snapshot>>,
    index: usize,
    depth: usize,
}

pub(crate) struct ErrorSpanLayer;

impl<S> Layer<S> for ErrorSpanLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let metadata = attrs.metadata();
        let root = metadata.target() == TARGET && metadata.name() == "kit.operation";
        let capture = if root {
            Capture {
                history: Arc::new(Mutex::new(Snapshot {
                    fragments: Vec::new(),
                    truncated: false,
                })),
                index: 0,
                depth: 0,
            }
        } else {
            let parent = if attrs.is_contextual() {
                ctx.lookup_current()
            } else {
                attrs.parent().and_then(|parent| ctx.span(parent))
            };
            let Some(mut capture) =
                parent.and_then(|parent| parent.extensions().get::<Capture>().cloned())
            else {
                return;
            };
            capture.depth = capture.depth.saturating_add(1);
            if capture.depth > MAX_DEPTH {
                if let Ok(mut history) = capture.history.try_lock() {
                    history.truncated = true;
                }
                return;
            }
            capture
        };
        let approved =
            root || (metadata.target() == "agentkit_loop" && approved_name(metadata.name()));
        let mut capture = capture;
        if approved {
            // No extension lock is held while locking the operation store.
            let Ok(mut history) = capture.history.try_lock() else {
                return;
            };
            if history.fragments.len() < MAX_FRAGMENTS {
                let mut visitor = Fields::default();
                attrs.record(&mut visitor);
                let parent = (!root).then_some(capture.index);
                capture.index = history.fragments.len();
                history.fragments.push(Fragment {
                    name: metadata.name().into(),
                    parent,
                    fields: visitor.values,
                });
                history.truncated |= visitor.truncated;
            } else {
                history.truncated = true;
                // Do not let records on an omitted span update its parent's fields.
                return;
            }
        }
        span.extensions_mut().insert(capture);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let metadata = span.metadata();
        if !(metadata.target() == TARGET && metadata.name() == "kit.operation"
            || metadata.target() == "agentkit_loop" && approved_name(metadata.name()))
        {
            return;
        }
        let capture = span.extensions().get::<Capture>().cloned();
        let Some(capture) = capture else { return };
        let mut visitor = Fields::default();
        values.record(&mut visitor);
        let Ok(mut history) = capture.history.try_lock() else {
            return;
        };
        let mut truncated = visitor.truncated;
        if let Some(fragment) = history.fragments.get_mut(capture.index) {
            for (key, value) in visitor.values {
                if fragment.fields.contains_key(&key) || fragment.fields.len() < MAX_FIELDS {
                    fragment.fields.insert(key, value);
                } else {
                    truncated = true;
                }
            }
        }
        history.truncated |= truncated;
    }
}

/// Uses the retained operation's extensions, including children already closed.
/// Without the layer there are no buffers and no traversal/serialization.
pub(crate) fn snapshot(span: &Span) -> Option<Snapshot> {
    span.with_subscriber(|(id, dispatch)| {
        dispatch.downcast_ref::<ErrorSpanLayer>()?;
        let registry = dispatch.downcast_ref::<Registry>()?;
        let span = registry.span(id)?;
        let capture = span.extensions().get::<Capture>().cloned()?;
        let mut snapshot = capture.history.try_lock().ok()?.clone();
        // Keep encoding outside all locks. A failed/contended capture is optional.
        while serde_json::to_vec_pretty(&snapshot).ok()?.len() > MAX_SNAPSHOT_BYTES {
            snapshot.fragments.pop()?;
            snapshot.truncated = true;
        }
        Some(snapshot)
    })
    .flatten()
}

fn approved_name(name: &str) -> bool {
    matches!(name, "agent.turn" | "agent.execute_tool" | "chat")
}

fn approved_value(key: &str, value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) if value.len() <= MAX_VALUE_BYTES => match key {
            "surface" => matches!(value.as_str(), "prompt" | "a2a" | "acp" | "acp_autonomous"),
            "gen_ai.operation.name" => {
                matches!(value.as_str(), "invoke_agent" | "execute_tool" | "chat")
            }
            "launch_kind" => matches!(value.as_str(), "plain" | "approved"),
            "error.type" => matches!(value.as_str(), "tool_error" | "provider_error"),
            _ => false,
        },
        serde_json::Value::Number(value) => {
            matches!(
                key,
                "transcript.len" | "gen_ai.usage.input_tokens" | "gen_ai.usage.output_tokens"
            ) && value.as_u64().is_some_and(|value| value <= u32::MAX.into())
        }
        serde_json::Value::Bool(_) => key == "saw_tool_call",
        _ => false,
    }
}

#[derive(Default)]
struct Fields {
    values: BTreeMap<String, serde_json::Value>,
    truncated: bool,
}

impl Fields {
    fn insert(&mut self, field: &Field, value: serde_json::Value) {
        if !approved_value(field.name(), &value) {
            return;
        }
        if self.values.contains_key(field.name()) || self.values.len() < MAX_FIELDS {
            self.values.insert(field.name().into(), value);
        } else {
            self.truncated = true;
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        // Reject before allocation; identifiers/content are intentionally not collected.
        if value.len() <= MAX_VALUE_BYTES {
            self.insert(field, value.into());
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, value.into());
    }
    fn record_debug(&mut self, _: &Field, _: &dyn fmt::Debug) {
        // Includes Display wrappers: never format arbitrary user/provider payloads.
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
    use tracing::Instrument as _;
    use tracing_subscriber::prelude::*;

    #[test]
    fn disabled_layer_has_no_capture() {
        tracing::subscriber::with_default(tracing_subscriber::registry(), || {
            let operation = operation("prompt");
            assert!(snapshot(&operation).is_none());
            operation.with_subscriber(|(id, dispatch)| {
                let registry = dispatch.downcast_ref::<Registry>().unwrap();
                assert!(
                    registry
                        .span(id)
                        .unwrap()
                        .extensions()
                        .get::<Capture>()
                        .is_none()
                );
            });
        });
    }

    #[test]
    fn closed_children_and_late_records_survive_without_exporter() {
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(ErrorSpanLayer),
            || {
                let operation = operation("prompt");
                operation.in_scope(|| {
                    let child = tracing::info_span!(target: "agentkit_loop", "agent.execute_tool",
                    launch_kind = "plain", "error.type" = tracing::field::Empty);
                    child.record("error.type", "tool_error");
                });
                let context = snapshot(&operation).unwrap();
                assert!(context.valid());
                assert_eq!(context.fragments.len(), 2);
                assert_eq!(context.fragments[1].parent, Some(0));
                assert_eq!(context.fragments[1].fields["error.type"], "tool_error");
                assert_eq!(context.fragments[1].fields["launch_kind"], "plain");
            },
        );
    }

    #[test]
    fn privacy_rejects_content_identifiers_debug_and_untrusted_targets() {
        struct NeverFormat;
        impl fmt::Debug for NeverFormat {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("must not format");
            }
        }
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(ErrorSpanLayer),
            || {
                let operation = operation("prompt");
                operation.in_scope(|| {
                let child = tracing::info_span!(target: "agentkit_loop", "chat",
                    "gen_ai.input.messages" = ?NeverFormat,
                    "gen_ai.output.messages" = "SECRET",
                    "gen_ai.conversation.id" = ?NeverFormat,
                    "gen_ai.operation.name" = "chat",
                    "gen_ai.usage.input_tokens" = u64::MAX,
                    "error.type" = ?NeverFormat);
                child.record("gen_ai.output.messages", "SECRET".repeat(100_000));
                child.record("gen_ai.operation.name", "https://secret.invalid/token");
                child.record("gen_ai.conversation.id", "../../SECRET");
                let _untrusted = tracing::info_span!(target: "untrusted", "chat", "gen_ai.operation.name" = "chat");
            });
                let context = snapshot(&operation).unwrap();
                assert_eq!(context.fragments.len(), 2);
                assert_eq!(context.fragments[1].fields.len(), 1);
                assert_eq!(context.fragments[1].fields["gen_ai.operation.name"], "chat");
                let encoded = serde_json::to_string(&context).unwrap();
                assert!(!encoded.contains("SECRET"));
                assert!(!encoded.contains("secret.invalid"));
            },
        );
    }

    #[test]
    fn bounds_depth_count_and_snapshot_size() {
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(ErrorSpanLayer),
            || {
                let operation = operation("prompt");
                operation.in_scope(|| {
                for _ in 0..1000 {
                    let _child = tracing::info_span!(target: "agentkit_loop", "chat", "gen_ai.operation.name" = "chat");
                }
            });
                let context = snapshot(&operation).unwrap();
                assert_eq!(context.fragments.len(), MAX_FRAGMENTS);
                assert!(context.truncated);
                assert!(context.valid());

                let deep = super::operation("acp");
                let mut parent = deep.clone();
                for _ in 0..100 {
                    parent = tracing::info_span!(target: "agentkit_loop", parent: &parent, "chat");
                }
                let context = snapshot(&deep).unwrap();
                assert!(context.truncated);
                assert_eq!(context.fragments.len(), MAX_DEPTH + 1);
            },
        );
    }

    #[test]
    fn unavailable_capture_is_omitted() {
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(ErrorSpanLayer),
            || {
                let operation = operation("prompt");
                operation.with_subscriber(|(id, dispatch)| {
                    let registry = dispatch.downcast_ref::<Registry>().unwrap();
                    let capture = registry
                        .span(id)
                        .unwrap()
                        .extensions()
                        .get::<Capture>()
                        .unwrap()
                        .clone();
                    let _lock = capture.history.lock().unwrap();
                    assert!(snapshot(&operation).is_none());
                    operation.record("surface", "a2a"); // contended collection cannot block or fail
                });
                assert!(snapshot(&operation).is_some());
            },
        );
    }

    #[tokio::test]
    async fn explicitly_instrumented_spawns_keep_separate_operation_histories() {
        use tracing::instrument::WithSubscriber as _;
        let subscriber = tracing_subscriber::registry().with(ErrorSpanLayer);
        async {
            let first = operation("prompt");
            let second = operation("a2a");
            let one = tokio::spawn(async {
                tokio::task::yield_now().await;
                let _child = tracing::info_span!(target: "agentkit_loop", "chat", "gen_ai.operation.name" = "chat");
            }.instrument(first.clone()).with_current_subscriber());
            let two = tokio::spawn(async {
                let _child = tracing::info_span!(target: "agentkit_loop", "agent.execute_tool", launch_kind = "approved");
                tokio::task::yield_now().await;
            }.instrument(second.clone()).with_current_subscriber());
            one.await.unwrap();
            two.await.unwrap();
            assert_eq!(snapshot(&first).unwrap().fragments[1].name, "chat");
            assert_eq!(snapshot(&second).unwrap().fragments[1].name, "agent.execute_tool");
            assert_eq!(snapshot(&first).unwrap().fragments.len(), 2);
            assert_eq!(snapshot(&second).unwrap().fragments.len(), 2);
        }.with_subscriber(subscriber).await;
    }
}
