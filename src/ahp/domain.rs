//! Native ACP configuration and committed filesystem boundaries.
use crate::provider::ModelSelection;
use agenthooksprotocol::hooks::Capabilities;
#[cfg(test)]
use serde_json::json;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(crate) fn supported_hooks() -> BTreeMap<String, Capabilities> {
    [
        ("model.switch.before", Capabilities::none().deny()),
        ("model.switch.after", Capabilities::none()),
        ("config.change.before", Capabilities::none().deny()),
        ("config.change.after", Capabilities::none()),
        ("file.changed", Capabilities::none()),
    ]
    .into_iter()
    .map(|(name, caps)| (name.to_owned(), caps))
    .collect()
}

pub(crate) fn model(selection: &ModelSelection) -> Value {
    Value::Object(Map::from_iter([
        ("id".into(), Value::from(selection.model.as_str())),
        ("provider".into(), Value::from(selection.provider.as_str())),
    ]))
}

fn session_identity(session: &str) -> Value {
    Value::Object(Map::from_iter([("id".into(), Value::from(session))]))
}

pub(crate) fn model_event(
    session: &str,
    before: bool,
    previous: &ModelSelection,
    current: &ModelSelection,
) -> Value {
    let (kind, previous_key, current_key) = if before {
        ("model.switch.before", "current", "proposed")
    } else {
        ("model.switch.after", "previous", "current")
    };
    Value::Object(Map::from_iter([
        ("type".into(), Value::from(kind)),
        ("session".into(), session_identity(session)),
        (previous_key.into(), model(previous)),
        (current_key.into(), model(current)),
        (
            "reason".into(),
            Value::from("ACP session configuration request"),
        ),
    ]))
}

pub(crate) fn config_event(session: &str, before: bool, setting: &str) -> Value {
    let kind = if before {
        "config.change.before"
    } else {
        "config.change.after"
    };
    let change = Value::Object(Map::from_iter([
        ("source".into(), Value::from("acp")),
        ("scope".into(), Value::from("session")),
        ("settings".into(), Value::Array(vec![Value::from(setting)])),
        (
            "summary".into(),
            Value::from(format!("Set ACP session option {setting}")),
        ),
    ]));
    Value::Object(Map::from_iter([
        ("type".into(), Value::from(kind)),
        ("session".into(), session_identity(session)),
        ("change".into(), change),
    ]))
}

pub(crate) fn file_event(session: &str, path: &std::path::Path, operation: &str) -> Value {
    let change = Value::Object(Map::from_iter([
        ("path".into(), Value::from(path.to_string_lossy().as_ref())),
        ("operation".into(), Value::from(operation)),
        ("agentCaused".into(), Value::from(true)),
    ]));
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("file.changed")),
        ("session".into(), session_identity(session)),
        ("changes".into(), Value::Array(vec![change])),
    ]))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;
    use agenthooksprotocol::generated::CatalogueEvent;
    #[tokio::test]
    async fn real_host_delivers_canonical_domain_events() {
        let (host, mut received, server) = super::test_support::host(None).await;
        let selection = ModelSelection::new(ProviderKind::OpenRouter, "test");
        let events = [
            model_event("s", true, &selection, &selection),
            model_event("s", false, &selection, &selection),
            config_event("s", true, "reasoning_effort"),
            config_event("s", false, "reasoning_effort"),
            file_event("s", std::path::Path::new("a"), "update"),
        ];
        for event in &events {
            host.event(event.clone(), vec![], vec![]).await.unwrap();
        }
        host.shutdown().await.unwrap();
        let mut names = Vec::new();
        while let Ok(message) = received.try_recv() {
            assert_eq!(message["method"], "hooks/observe");
            let event = message["params"]["event"].clone();
            serde_json::from_value::<CatalogueEvent>(event.clone()).unwrap();
            names.push(event["type"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            names,
            events
                .iter()
                .map(|event| event["type"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        );
        server.abort();
    }

    #[test]
    fn domain_events_use_canonical_sdk_wire_shapes() {
        let selection = ModelSelection::new(ProviderKind::OpenRouter, "test");
        for mut event in [
            model_event("s", true, &selection, &selection),
            model_event("s", false, &selection, &selection),
            config_event("s", true, "reasoning_effort"),
            config_event("s", false, "reasoning_effort"),
            file_event("s", std::path::Path::new("a"), "update"),
        ] {
            event["id"] = json!("event");
            event["source"] = json!("kit://session/s");
            event["time"] = json!("2026-01-01T00:00:00Z");
            serde_json::from_value::<CatalogueEvent>(event).unwrap();
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
pub(crate) mod test_support {
    use super::*;
    use crate::ahp::{AhpConfig, AhpHost, SessionFacts};
    use std::sync::Arc;

    // A genuine external HTTP hook boundary; all registration, event validation,
    // effect reduction, and delivery still run through the actual SDK.
    pub(crate) async fn host(
        deny: Option<&'static str>,
    ) -> (
        Arc<AhpHost>,
        tokio::sync::mpsc::UnboundedReceiver<Value>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::response::IntoResponse;
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().route("/hooks", axum::routing::post(move |axum::Json(message): axum::Json<Value>| {
            let sender = sender.clone();
            async move {
                sender.send(message.clone()).unwrap();
                if message["method"] == "hooks/intercept" {
                    axum::Json(json!({"jsonrpc":"2.0","id":message["id"],"result":{"protocolVersion":"draft","effects":[{"type":"deny","reason":"domain test policy"}]}})).into_response()
                } else {
                    axum::http::StatusCode::NO_CONTENT.into_response()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/hooks", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let names: Vec<_> = supported_hooks().into_keys().collect();
        let mut grants: BTreeMap<_, _> = names
            .iter()
            .map(|name| (name.clone(), json!({"modes":["observe"]})))
            .collect();
        let mut subscriptions =
            vec![json!({"events":names,"mode":"observe","content":{"default":"metadata"}})];
        if let Some(name) = deny {
            grants.insert(
                name.into(),
                json!({"modes":["observe","intercept"],"capabilities":{"effects":["deny"]}}),
            );
            subscriptions.push(json!({"events":[name],"mode":"intercept","timeoutMs":5000,"failurePolicy":"fail-closed","content":{"default":"metadata"}}));
        }
        let host = AhpHost::new_with_options(AhpConfig {
            registration:json!({"protocolVersion":"draft","hooks":[{"id":"org.kit.domain-tests","transport":{"type":"http","url":endpoint},"subscriptions":subscriptions}]}), grants,
        }, SessionFacts { trigger:"startup".into(), permission_mode:"allow-all".into(), resumed_from:None }, agenthooksprotocol::adapters::registered::BackendOptions { allow_loopback_http:true, ..Default::default() }).unwrap();
        (host, receiver, server)
    }
}
