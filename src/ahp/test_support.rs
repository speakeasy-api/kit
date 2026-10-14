use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agenthooksprotocol::adapters::registered::ManagedBackend;
use agenthooksprotocol::client::{HookError, LocalFuture};
use serde_json::{Value, json};

use super::{Hooks, options};

const BACKEND: &str = "dev.kit.test";

/// In-process hook backend answering each event type with fixed effects.
#[derive(Default)]
pub(crate) struct Backend {
    effects: Mutex<HashMap<String, Value>>,
    events: Mutex<Vec<Value>>,
}

impl Backend {
    pub(crate) fn respond(&self, event: &str, effects: Value) {
        self.effects.lock().unwrap().insert(event.into(), effects);
    }

    pub(crate) fn all_events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }

    pub(crate) fn events(&self, event: &str) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|delivered| delivered["type"] == event)
            .cloned()
            .collect()
    }
}

impl ManagedBackend for Backend {
    fn call(&self, request: Value, _: Duration) -> LocalFuture<'_, Result<Value, HookError>> {
        Box::pin(async move {
            let event = request["params"]["event"].clone();
            self.events.lock().unwrap().push(event.clone());
            if request["method"] == "hooks/observe" {
                return Ok(Value::Null);
            }
            let effects = event["type"]
                .as_str()
                .and_then(|name| self.effects.lock().unwrap().get(name).cloned())
                .unwrap_or_else(|| json!([]));
            Ok(json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"protocolVersion": "draft", "effects": effects},
            }))
        })
    }

    fn shutdown(&self) -> LocalFuture<'_, Result<(), HookError>> {
        Box::pin(async { Ok(()) })
    }
}

pub(crate) fn intercept(events: &[&str]) -> Value {
    json!({
        "events": events,
        "mode": "intercept",
        "timeoutMs": 1000,
        "failurePolicy": "fail-closed",
        "content": {"default": "body"},
    })
}

pub(crate) fn observe(events: &[&str]) -> Value {
    json!({"events": events, "mode": "observe", "content": {"default": "body"}})
}

pub(crate) fn hooks(subscriptions: Vec<Value>) -> (Arc<Hooks>, Arc<Backend>) {
    let backend = Arc::new(Backend::default());
    (hooks_with_backend(subscriptions, backend.clone()), backend)
}

pub(crate) fn hooks_with_backend(
    subscriptions: Vec<Value>,
    backend: Arc<dyn ManagedBackend>,
) -> Arc<Hooks> {
    let registration = serde_json::from_value(json!({
        "protocolVersion": "draft",
        "hooks": [{
            "id": BACKEND,
            "transport": {"type": "stdio", "command": "unused", "lifecycle": "persistent"},
            "subscriptions": subscriptions,
        }],
    }))
    .unwrap();
    let options = options().unwrap().with_backend(BACKEND, backend);
    Arc::new(Hooks::new(registration, options).unwrap())
}

pub(crate) fn text_message(text: &str) -> Value {
    json!([{
        "id": "edited",
        "role": "user",
        "parts": [{
            "id": "edited/0",
            "kind": "text",
            "mediaType": "text/plain",
            "selection": "body",
            "text": text,
        }],
    }])
}
