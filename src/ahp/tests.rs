#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]
use super::*;
use agenthooksprotocol::{
    adapters::registered::ManagedBackend,
    client::{HookError, LocalFuture, ToolContext as HookContext},
};
use agentkit_core::{CancellationController, ToolOutput, ToolResultPart};
use agentkit_tools_core::ToolResult;
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;

// The fake is only the SDK's external transport boundary. Registration,
// validation, settlement, and Kit's native consumer all run unchanged.
struct Backend {
    events: mpsc::UnboundedSender<Value>,
    before: Value,
    after: Value,
    pending: bool,
}

impl ManagedBackend for Backend {
    fn call(&self, request: Value, _: Duration) -> LocalFuture<'_, Result<Value, HookError>> {
        Box::pin(async move {
            let event = request["params"]["event"].clone();
            self.events.send(event.clone()).unwrap();
            if self.pending {
                return std::future::pending().await;
            }
            let effects = if event["type"] == "tool.before" {
                &self.before
            } else {
                &self.after
            };
            Ok(
                json!({"jsonrpc":"2.0","id":request["id"],"result":{"protocolVersion":"draft","effects":effects}}),
            )
        })
    }

    fn shutdown(&self) -> LocalFuture<'_, Result<(), HookError>> {
        Box::pin(async { Ok(()) })
    }
}

fn host(before: Value, after: Value, pending: bool) -> (AhpHost, mpsc::UnboundedReceiver<Value>) {
    host_with_upload(before, after, pending, None)
}

fn host_with_upload(
    before: Value,
    after: Value,
    pending: bool,
    upload: Option<&str>,
) -> (AhpHost, mpsc::UnboundedReceiver<Value>) {
    let (events, received) = mpsc::unbounded_channel();
    let edit_output = after != json!([]);
    let backend = Arc::new(Backend {
        events,
        before,
        after,
        pending,
    });
    let grants = BTreeMap::from([
        (
            "tool.before".into(),
            EventGrant::intercept(Capabilities::none().allow().deny().modify_input()),
        ),
        (
            "tool.after".into(),
            EventGrant::intercept(Capabilities::none().modify("output", true, true)),
        ),
    ]);
    let source = "urn:kit:test".to_owned();
    let mut registration = json!({"protocolVersion":"draft","hooks":[{
        "id":"org.example.test",
        "transport":{"type":"stdio","command":"never-started","lifecycle":"persistent"},
        "subscriptions":[{"events":["tool.before","tool.after"],"mode":"intercept","timeoutMs":1000,"failurePolicy":"fail-closed","content":{"default":"metadata"}}]
    }]});
    if edit_output {
        registration["hooks"][0]["subscriptions"][0]["content"] = json!({"default":"body"});
    }
    if let Some(endpoint) = upload {
        let subscription = &mut registration["hooks"][0]["subscriptions"][0];
        subscription["content"] = json!({"default":"body"});
        subscription["upload"] = json!({"endpoint":endpoint,"maxBytes":4096,"timeoutMs":1000});
    }
    let mut options =
        HooksOptions::new(source.clone(), grants.clone()).with_backend("org.example.test", backend);
    options.backend.allow_loopback_http = true;
    let hooks = Hooks::new(registration, options).unwrap();
    (
        AhpHost {
            hooks: Arc::new(hooks),
            grants,
            source,
            facts: SessionFacts {
                trigger: "startup".into(),
                permission_mode: "test".into(),
                resumed_from: None,
            },
            selection: None,
            model_calls: std::sync::atomic::AtomicU64::new(0),
            settled_prefinish: std::sync::Mutex::new(None),
        },
        received,
    )
}

async fn receive(events: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("SDK did not dispatch expected event")
        .expect("SDK backend disconnected")
}

fn request() -> ToolRequest {
    ToolRequest::new(
        "call-1",
        "shell",
        json!({"command":"original","keep":true}),
        "session-1",
        "turn-1",
    )
}

#[tokio::test]
async fn accepted_input_merge_reaches_native_request() {
    let (host, mut events) = host(
        json!([{"type":"modify","target":"input","operation":"merge","value":{"command":"changed"}}]),
        json!([]),
        false,
    );
    let ToolBeforeOutcome::Invoke { request, event } = host.before(request(), None, None).await
    else {
        panic!("expected invocation")
    };
    assert_eq!(request.input, json!({"command":"changed","keep":true}));
    let before = receive(&mut events).await;
    assert_eq!(event.id, before["id"]);
    assert_eq!(event.source, before["source"]);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn settled_denial_emits_skipped_receipt_with_effective_input_and_lineage() {
    let (host, mut events) = host(
        json!([
            {"type":"modify","target":"input","operation":"merge","value":{"command":"changed"}},
            {"type":"deny","reason":"policy"}
        ]),
        json!([]),
        false,
    );
    assert!(matches!(
        host.before(request(), None, None).await,
        ToolBeforeOutcome::Complete(ToolExecutionOutcome::FailedBeforeInvocation(_))
    ));
    let before = receive(&mut events).await;
    let after = receive(&mut events).await;
    assert_eq!(after["type"], "tool.after");
    assert_eq!(after["parentEventId"], before["id"]);
    assert_eq!(after["source"], before["source"]);
    assert_eq!(after["outcome"], "denied");
    assert_eq!(after["execution"]["status"], "skipped");
    assert_eq!(after["execution"]["reason"], "policy");
    assert_eq!(
        after["tool"]["input"],
        json!({"command":"changed","keep":true})
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_preparation_does_not_fabricate_denial_receipt() {
    let (host, mut events) = host(json!([]), json!([]), false);
    let mut invalid = request();
    invalid
        .metadata
        .insert("kit.ahp.mcp".into(), json!({"invalid":true}));
    assert!(matches!(
        host.before(invalid, None, None).await,
        ToolBeforeOutcome::Complete(ToolExecutionOutcome::FailedBeforeInvocation(_))
    ));
    assert!(
        events.try_recv().is_err(),
        "preparation failure must not emit tool.after"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_cancellation_interrupts_pending_before() {
    let (host, mut events) = host(json!([]), json!([]), true);
    let controller = CancellationController::new();
    let before = host.before(request(), None, Some(controller.handle().checkpoint()));
    let cancel = async {
        assert_eq!(receive(&mut events).await["type"], "tool.before");
        controller.interrupt();
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(before, cancel)
    })
    .await
    .expect("pending hook did not cancel");
    assert!(matches!(
        outcome,
        ToolBeforeOutcome::Complete(ToolExecutionOutcome::FailedBeforeInvocation(_))
    ));
    assert!(
        events.try_recv().is_err(),
        "cancellation is not a policy denial receipt"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn after_replacement_is_consumed_as_native_tool_output() {
    // Inline tool output is editable without a receiver upload endpoint.
    let replacement = ToolOutput::text("replacement");
    let canonical_replacement = json!([{"id":"kit-message-0","synthesized":true,"role":"tool","parts":[{"id":"kit-message-0/part/0","kind":"text","mediaType":"text/plain","selection":"body","text":"replacement"}]}]);
    let (host, mut events) = host_with_upload(
        json!([]),
        json!([{"type":"modify","target":"output","operation":"replace","value":canonical_replacement}]),
        false,
        None,
    );
    let request = request();
    let result = ToolExecutionOutcome::Completed(ToolResult::new(ToolResultPart::success(
        request.call_id.clone(),
        ToolOutput::text("original"),
    )));
    let outcome = host
        .after(
            &request,
            ToolEventIdentity {
                source: host.source.clone(),
                id: "before-1".into(),
            },
            result,
            None,
        )
        .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("expected completed native output, got {outcome:?}")
    };
    assert_eq!(
        serde_json::to_value(result.result.output).unwrap(),
        serde_json::to_value(replacement).unwrap()
    );
    let after = receive(&mut events).await;
    assert_eq!(after["parentEventId"], "before-1");
    assert_eq!(after["execution"]["status"], "executed");
    assert_eq!(after["items"][0]["parts"][0]["text"], "original");
    host.shutdown().await.unwrap();
}

fn stdio() -> Value {
    json!({"type":"stdio","command":"never-started","lifecycle":"persistent"})
}

fn content_host(
    event: &str,
    selection: &str,
    observe: bool,
    effects: Value,
    pending: bool,
    limits: [usize; 3],
    upload: Option<&str>,
) -> (AhpHost, mpsc::UnboundedReceiver<Value>) {
    let (events, received) = mpsc::unbounded_channel();
    let backend = Arc::new(Backend {
        events,
        before: json!([]),
        after: effects,
        pending,
    });
    let capabilities = match event {
        "turn.start" | "user.message.inbound" => Capabilities::none().modify("prompt", true, true),
        "context.compact.before" => Capabilities::none().modify("instructions", true, false),
        "context.compact.after" => Capabilities::none().modify("summary", true, false),
        _ => Capabilities::none(),
    };
    let grants = BTreeMap::from([(
        event.to_owned(),
        if observe {
            EventGrant::observe()
        } else {
            EventGrant::intercept(capabilities)
        },
    )]);
    let source = "urn:kit:test:content".to_owned();
    let mut subscription = json!({"events":[event],"mode":if observe {"observe"} else {"intercept"},
        "timeoutMs":1000,"failurePolicy":"fail-closed","content":{"default":selection}});
    if observe {
        subscription
            .as_object_mut()
            .unwrap()
            .remove("failurePolicy");
        subscription.as_object_mut().unwrap().remove("timeoutMs");
    }
    if let Some(endpoint) = upload {
        subscription["upload"] = json!({"endpoint":endpoint,"maxBytes":4096,"timeoutMs":1000});
    }
    let registration = json!({"protocolVersion":"draft","hooks":[{"id":"org.example.content","transport":stdio(),"subscriptions":[subscription]}]});
    let mut options = HooksOptions::new(source.clone(), grants.clone())
        .with_backend("org.example.content", backend);
    options.backend.allow_loopback_http = true;
    options.max_body_bytes = limits[0];
    options.max_stored_bytes = limits[1];
    options.max_stored_entries = limits[2];
    let hooks = Hooks::new(registration, options).unwrap();
    (
        AhpHost {
            hooks: Arc::new(hooks),
            grants,
            source,
            facts: SessionFacts {
                trigger: "startup".into(),
                permission_mode: "test".into(),
                resumed_from: None,
            },
            selection: None,
            model_calls: std::sync::atomic::AtomicU64::new(0),
            settled_prefinish: std::sync::Mutex::new(None),
        },
        received,
    )
}

fn native_prompt(text: &str) -> Value {
    serde_json::to_value(vec![agentkit_core::Item::new(
        agentkit_core::ItemKind::User,
        vec![agentkit_core::Part::text(text)],
    )])
    .unwrap()
}

fn canonical_prompt(text: &str) -> Value {
    json!([{"id":"kit-message-0","synthesized":true,"role":"user","parts":[{
        "id":"kit-message-0/part/0","kind":"text","mediaType":"text/plain","selection":"body","text":text
    }]}])
}

fn turn_start() -> Value {
    json!({"type":"turn.start","session":{"id":"session-1"},"turn":{"id":"turn-1"},"trigger":"external","items":[]})
}

#[tokio::test]
async fn inline_prompt_replacement_reaches_same_request_without_upload() {
    let (host, mut events) = content_host(
        "turn.start",
        "body",
        false,
        json!([{"type":"modify","target":"prompt","operation":"replace","value":canonical_prompt("replacement")}]),
        false,
        [1, 1, 1],
        None,
    );
    let settled = host
        .event(
            turn_start(),
            vec![("/items/0".into(), native_prompt("original"))],
            vec![("prompt".into(), "/items/0".into())],
        )
        .await
        .unwrap();
    assert_eq!(settled.bodies["/items/0"], native_prompt("replacement"));
    assert_eq!(settled.event["items"][0]["parts"][0]["text"], "replacement");
    let delivered = receive(&mut events).await;
    assert_eq!(delivered["items"][0]["parts"][0]["text"], "original");
    assert!(
        delivered["items"][0]["parts"][0]
            .get("attachment")
            .is_none()
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn inbound_prompt_uses_canonical_message_messages() {
    let (host, mut events) = content_host(
        "user.message.inbound",
        "body",
        false,
        json!([{"type":"modify","target":"prompt","operation":"replace","value":canonical_prompt("edited")}]),
        false,
        [1, 1, 1],
        None,
    );
    let event = json!({"type":"user.message.inbound","session":{"id":"session-1"},
        "message":{"channel":"loop-input","sender":"host","messages":[]}});
    let settled = host
        .event(
            event,
            vec![("/message/messages".into(), native_prompt("input"))],
            vec![("prompt".into(), "/message/messages".into())],
        )
        .await
        .unwrap();
    assert_eq!(settled.bodies["/message/messages"], native_prompt("edited"));
    assert_eq!(
        receive(&mut events).await["message"]["messages"][0]["parts"][0]["text"],
        "input"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn inline_observation_selection_does_not_change_native_writeback() {
    for selection in ["body", "metadata", "omit"] {
        let (host, mut events) = content_host(
            "turn.end",
            selection,
            true,
            json!([]),
            false,
            [1, 1, 1],
            None,
        );
        let native = native_prompt("a transcript that exceeds the attachment byte budget");
        let event = json!({"type":"turn.end","session":{"id":"session-1"},"turn":{"id":"turn-1"},
            "outcome":"completed","continuationCount":0,"items":[]});
        let settled = host
            .event(event, vec![("/items/0".into(), native.clone())], vec![])
            .await
            .unwrap();
        assert_eq!(settled.bodies["/items/0"], native);
        let delivered = receive(&mut events).await;
        let part = &delivered["items"][0]["parts"][0];
        assert_eq!(part["selection"], selection);
        if selection == "body" {
            assert_eq!(
                part["text"],
                "a transcript that exceeds the attachment byte budget"
            );
        } else {
            assert!(part.get("text").is_none());
        }
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn specialized_instructions_settle_as_effective_inline_text() {
    specialized_replacement("context.compact.before", "/instructions", "instructions").await;
}

#[tokio::test]
async fn specialized_summary_settles_as_effective_inline_text() {
    specialized_replacement("context.compact.after", "/summary", "summary").await;
}

async fn specialized_replacement(name: &str, pointer: &str, target: &str) {
    let (host, mut events) = content_host(
        name,
        "body",
        false,
        json!([{"type":"modify","target":target,"operation":"replace","value":projection::text_parts("edited text")}]),
        false,
        [1, 1, 1],
        None,
    );
    let event = if name == "context.compact.before" {
        json!({"type":name,"session":{"id":"session-1"},"trigger":"manual","items":[],"instructions":[]})
    } else {
        json!({"type":name,"session":{"id":"session-1"},"summary":[],"removed":[{"id":"item-1"}],"execution":{"status":"executed"}})
    };
    let mut direct = event.clone();
    *direct.pointer_mut(pointer).unwrap() = projection::text_parts("original text");
    let sdk = if name == "context.compact.before" {
        let input: agenthooksprotocol::inputs::ContextCompactBeforeInput =
            serde_json::from_value(direct).unwrap();
        host.hooks
            .context_compact_before(input)
            .initial_state(Decision::Allow)
            .await
            .unwrap()
    } else {
        let input: agenthooksprotocol::inputs::ContextCompactAfterInput =
            serde_json::from_value(direct).unwrap();
        host.hooks
            .context_compact_after(input)
            .initial_state(Decision::Allow)
            .await
            .unwrap()
    };
    assert!(
        sdk.outcome.failures.is_empty(),
        "{:?}",
        sdk.outcome.failures
    );
    receive(&mut events).await;
    let settled = host
        .event(
            event,
            vec![(pointer.into(), json!("original text"))],
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(settled.bodies[pointer], "edited text");
    let delivered = receive(&mut events).await;
    assert_eq!(
        delivered.pointer(pointer).unwrap()[0]["text"],
        "original text"
    );
    assert_eq!(
        settled.event.pointer(pointer).unwrap()[0]["text"],
        "edited text"
    );
    host.shutdown().await.unwrap();
}

async fn upload_receiver() -> (
    String,
    mpsc::UnboundedReceiver<Vec<u8>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/upload", listener.local_addr().unwrap());
    let (sent, received) = mpsc::unbounded_channel();
    let router = axum::Router::new().route("/upload", axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
        let sent = sent.clone();
        async move {
            sent.send(body.to_vec()).unwrap();
            (axum::http::StatusCode::CREATED, axum::Json(json!({"ref":"receiver-content","size":body.len(),"sha256":headers["ahp-content-sha256"].to_str().unwrap()})))
        }
    }));
    let serving = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (endpoint, received, serving)
}

fn native_attachment(bytes: Vec<u8>) -> Value {
    serde_json::to_value(vec![agentkit_core::Item::new(
        agentkit_core::ItemKind::User,
        vec![agentkit_core::Part::file(
            agentkit_core::DataRef::inline_bytes(bytes),
        )],
    )])
    .unwrap()
}

#[tokio::test]
async fn native_binary_upload_preserves_exact_bytes_and_releases_capacity() {
    let (endpoint, mut uploads, receiver) = upload_receiver().await;
    let (host, mut events) = content_host(
        "turn.start",
        "body",
        false,
        json!([]),
        false,
        [4, 4, 1],
        Some(&endpoint),
    );
    for bytes in [vec![0, 255, 1, 128], vec![9, 8, 0, 255], vec![2, 3, 4, 5]] {
        let native = native_attachment(bytes.clone());
        let settled = host
            .event(
                turn_start(),
                vec![("/items/0".into(), native.clone())],
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(settled.bodies["/items/0"], native);
        assert_eq!(uploads.recv().await.unwrap(), bytes);
        let delivered = receive(&mut events).await;
        assert_eq!(delivered["items"][0]["parts"][0]["kind"], "attachment");
        assert_eq!(delivered["items"][0]["parts"][0]["selection"], "body");
        assert_eq!(
            delivered["items"][0]["parts"][0]["body"]["ref"],
            "receiver-content"
        );
        let _: agenthooksprotocol::generated::CanonicalMessages =
            serde_json::from_value(delivered["items"].clone()).unwrap();
    }
    host.shutdown().await.unwrap();
    receiver.abort();
    assert!(receiver.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn oversized_binary_failure_does_not_consume_next_invocation_budget() {
    let (endpoint, mut uploads, receiver) = upload_receiver().await;
    let (host, mut events) = content_host(
        "turn.start",
        "body",
        false,
        json!([]),
        false,
        [4, 4, 1],
        Some(&endpoint),
    );
    assert!(
        host.event(
            turn_start(),
            vec![("/items/0".into(), native_attachment(vec![1; 5]))],
            vec![]
        )
        .await
        .is_err()
    );
    assert!(events.try_recv().is_err());
    assert!(uploads.try_recv().is_err());
    let native = native_attachment(vec![0, 255, 1, 2]);
    let settled = host
        .event(
            turn_start(),
            vec![("/items/0".into(), native.clone())],
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(settled.bodies["/items/0"], native);
    assert_eq!(uploads.recv().await.unwrap(), vec![0, 255, 1, 2]);
    receive(&mut events).await;
    host.shutdown().await.unwrap();
    receiver.abort();
    assert!(receiver.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn cancelled_binary_invocation_releases_its_attachment_owner() {
    let (host, mut events) = content_host(
        "turn.start",
        "metadata",
        false,
        json!([]),
        true,
        [4, 4, 1],
        None,
    );
    for bytes in [vec![1, 2, 3, 4], vec![4, 3, 2, 1]] {
        let controller = CancellationController::new();
        let dispatch = host.event_with_cancellation(
            turn_start(),
            vec![("/items/0".into(), native_attachment(bytes))],
            vec![],
            Some(controller.handle().checkpoint()),
        );
        let cancel = async {
            let delivered = receive(&mut events).await;
            assert_eq!(delivered["items"][0]["parts"][0]["selection"], "metadata");
            controller.interrupt();
        };
        let (settled, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(dispatch, cancel)
        })
        .await
        .unwrap();
        assert!(matches!(settled, Err(message) if message == "AHP boundary cancelled"));
    }
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_binary_attachment_cannot_be_replaced_by_inline_text() {
    let (endpoint, mut uploads, receiver) = upload_receiver().await;
    let (host, mut events) = content_host(
        "turn.start",
        "body",
        false,
        json!([{"type":"modify","target":"prompt","operation":"replace","value":canonical_prompt("not the original bytes")}]),
        false,
        [4, 4, 1],
        Some(&endpoint),
    );
    let settled = host
        .event(
            turn_start(),
            vec![("/items/0".into(), native_attachment(vec![0, 255, 1, 2]))],
            vec![("prompt".into(), "/items/0".into())],
        )
        .await;
    assert!(settled.is_err(), "binary attachment must remain immutable");
    assert_eq!(uploads.recv().await.unwrap(), vec![0, 255, 1, 2]);
    assert_eq!(
        receive(&mut events).await["items"][0]["parts"][0]["kind"],
        "attachment"
    );
    host.shutdown().await.unwrap();
    receiver.abort();
    assert!(receiver.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn sdk_binary_result_keeps_immutable_bytes_after_next_dispatch_and_shutdown() {
    use agenthooksprotocol::content::ContentAccess;
    let (endpoint, mut uploads, receiver) = upload_receiver().await;
    let (host, mut events) = content_host(
        "turn.start",
        "body",
        false,
        json!([]),
        false,
        [4, 4, 1],
        Some(&endpoint),
    );
    let event = || {
        let mut event = turn_start();
        event["items"] = json!([{"id":"binary-message","role":"user","parts":[{
            "id":"binary-part","kind":"attachment","mediaType":"application/octet-stream","selection":"metadata","size":4
        }]}]);
        event
    };
    let input = |bytes| {
        let facts: agenthooksprotocol::inputs::TurnStartInput =
            serde_json::from_value(event()).unwrap();
        let sources = vec![BinarySource {
            path: vec!["items".into(), "0".into(), "parts".into(), "0".into()],
            source: agenthooksprotocol::Attachment::bytes(bytes),
        }];
        let mut sources = sources;
        let items = owned_messages(facts.items.clone(), &["items".into()], &mut sources).unwrap();
        assert!(sources.is_empty());
        facts.with_sources().with_items(items)
    };
    let first = host
        .hooks
        .turn_start(input(vec![0, 255, 1, 2]))
        .await
        .unwrap();
    assert_eq!(uploads.recv().await.unwrap(), vec![0, 255, 1, 2]);
    receive(&mut events).await;
    // A retained result owns its bytes, but no longer charges invocation capacity.
    let second = host
        .hooks
        .turn_start(input(vec![9, 8, 7, 6]))
        .await
        .unwrap();
    assert_eq!(uploads.recv().await.unwrap(), vec![9, 8, 7, 6]);
    receive(&mut events).await;
    host.shutdown().await.unwrap();
    let path = "/items/0/parts/0";
    let original = first
        .content
        .resolve_selected(path, first.effective_event.pointer(path).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(&*original, &[0, 255, 1, 2]);
    let next = second
        .content
        .resolve_selected(path, second.effective_event.pointer(path).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(&*next, &[9, 8, 7, 6]);
    receiver.abort();
    assert!(receiver.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn inline_content_limit_rejects_original_and_effective_oversize() {
    let oversized = "x".repeat(BODY_LIMIT + 1);
    for effects in [
        json!([]),
        json!([{"type":"modify","target":"prompt","operation":"replace","value":canonical_prompt(&oversized)}]),
    ] {
        let original = if effects.as_array().unwrap().is_empty() {
            &oversized
        } else {
            "small"
        };
        let (host, _events) = content_host(
            "turn.start",
            "body",
            false,
            effects,
            false,
            [BODY_LIMIT, ATTACHMENT_BYTE_LIMIT, ATTACHMENT_OWNER_LIMIT],
            None,
        );
        let error = match host
            .event(
                turn_start(),
                vec![("/items/0".into(), native_prompt(original))],
                vec![("prompt".into(), "/items/0".into())],
            )
            .await
        {
            Ok(_) => panic!("oversized inline content must not settle"),
            Err(error) => error,
        };
        assert!(error.contains("inline content exceeds 4 MiB"), "{error}");
        host.shutdown().await.unwrap();
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct CallerArguments {
    command: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct CallerResult {
    answer: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct CallerProvenance {
    source: String,
}

#[tokio::test]
async fn typed_tool_settlement_preserves_opaque_facts_and_caller_candidate() {
    let (host, mut events) = host(json!([]), json!([]), false);
    let mut event = tool_event("tool.before", &request());
    let opaque = json!({"nested":[null,{"parts":[42],"ref":"not-an-attachment"}]});
    event["vendor"] = opaque.clone();
    event["native"] = json!({"private":opaque});
    event["extensions"] = json!({"org.example.vendor":opaque});
    let settled = host
        .hooks
        .tool_input(CallerArguments {
            command: "original".into(),
        })
        .context(HookContext::new(event))
        .result_type::<CallerResult>()
        .provenance_type::<CallerProvenance>()
        .initial_state(Decision::Allow)
        .initial_candidate(
            json!({"value":{"answer":"cached"},"provenance":{"source":"native"},"vendor":opaque}),
        )
        .await
        .unwrap();
    let _: &agenthooksprotocol::hooks::ToolOutcome<
        CallerArguments,
        CallerResult,
        CallerProvenance,
    > = &settled;
    let canonical = serde_json::to_value(&settled.settled_event).unwrap();
    assert_eq!(canonical["vendor"], opaque);
    assert_eq!(canonical["native"]["private"], opaque);
    assert_eq!(canonical["extensions"]["org.example.vendor"], opaque);
    assert_eq!(canonical["tool"]["input"], settled.effective_input);
    let candidate = settled.candidate.unwrap();
    assert_eq!(candidate.value.answer, "cached");
    assert_eq!(
        candidate.provenance,
        agenthooksprotocol::Presence::Present(CallerProvenance {
            source: "native".into()
        })
    );
    assert_eq!(candidate.additional_properties["vendor"], opaque);
    let delivered = receive(&mut events).await;
    assert_eq!(canonical["id"], delivered["id"]);
    assert_eq!(canonical["source"], delivered["source"]);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn caller_input_decode_error_retains_accepted_effects_without_authorizing_execution() {
    let effects = json!([
        {"type":"modify","target":"input","operation":"merge","value":{"command":42}},
        {"type":"allow"}
    ]);
    let (host, _events) = host(effects.clone(), json!([]), false);
    let error = host
        .hooks
        .tool_input(CallerArguments {
            command: "original".into(),
        })
        .context(HookContext::new(tool_event("tool.before", &request())))
        .initial_state(Decision::Allow)
        .await
        .err()
        .expect("caller decode must fail closed despite accepted allow");
    assert_eq!(
        error.code(),
        agenthooksprotocol::DeliveryDiagnosticCode::ProtocolRejection
    );
    let accepted = error.accepted_settlement().unwrap();
    assert_eq!(accepted["permission"], "allow");
    assert_eq!(accepted["effectiveValue"]["command"], 42);
    assert_eq!(accepted["responses"][0]["result"]["effects"], effects);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn caller_result_and_provenance_contract_errors_are_sdk_failures() {
    for candidate in [
        json!({"value":{"answer":42},"provenance":{"source":"native"}}),
        json!({"value":{"answer":"cached"},"provenance":{"source":42}}),
    ] {
        let (host, _events) = host(json!([]), json!([]), false);
        let result = host
            .hooks
            .tool_input(CallerArguments {
                command: "original".into(),
            })
            .context(HookContext::new(tool_event("tool.before", &request())))
            .result_type::<CallerResult>()
            .provenance_type::<CallerProvenance>()
            .initial_candidate(candidate)
            .initial_state(Decision::Allow)
            .await;
        assert!(
            result.is_err(),
            "invalid caller candidate must not grant execution"
        );
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn named_canonical_settlement_keeps_opaque_extensions_during_native_writeback() {
    let (host, _events) = content_host(
        "turn.start",
        "body",
        false,
        json!([]),
        false,
        [4096, 4096, 16],
        None,
    );
    let opaque = json!({"nested":[null,{"parts":[42],"ref":"not-an-attachment"}]});
    let mut event = turn_start();
    event["extensions"] = json!({"org.example.vendor":opaque});
    event["native"] = json!({"private":opaque});
    let mut original = native_prompt("unchanged");
    original[0]["metadata"] = json!({"private":opaque});
    let settled = host
        .event(event, vec![("/items/0".into(), original.clone())], vec![])
        .await
        .unwrap();
    assert_eq!(settled.event["extensions"]["org.example.vendor"], opaque);
    assert_eq!(settled.event["native"]["private"], opaque);
    assert_eq!(settled.bodies["/items/0"], original);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn typed_accepted_response_effects_match_effective_tool_input() {
    let effects = json!([
        {"type":"modify","target":"input","operation":"merge","value":{"command":"changed"}},
        {"type":"allow"}
    ]);
    let (host, _events) = host(effects.clone(), json!([]), false);
    let settled = host
        .hooks
        .tool_input(CallerArguments {
            command: "original".into(),
        })
        .context(HookContext::new(tool_event("tool.before", &request())))
        .initial_state(Decision::Allow)
        .await
        .unwrap();
    let _: &Vec<agenthooksprotocol::MessageEffect> = &settled.outcome.messages;
    let _: &Vec<agenthooksprotocol::InjectEffect> = &settled.outcome.injections;
    let _: &Vec<agenthooksprotocol::InterceptResponse> = &settled.outcome.responses;
    let response = serde_json::to_value(&settled.outcome.responses[0]).unwrap();
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["id"].is_string());
    assert_eq!(response["result"]["effects"], effects);
    assert_eq!(settled.input.unwrap().command, "changed");
    let canonical = serde_json::to_value(&settled.settled_event).unwrap();
    assert_eq!(canonical["tool"]["input"]["command"], "changed");
    host.shutdown().await.unwrap();
}

async fn execute_edit_contract(
    input: Value,
    effect: Value,
) -> (ToolExecutionOutcome, Vec<Value>, String) {
    use agentkit_core::{MetadataMap, SessionId, TurnId};
    use agentkit_tools_core::{
        AllowAllPermissions, BasicToolExecutor, ToolExecutionScope, ToolExecutor, ToolRegistry,
    };
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("target.txt"), "original").unwrap();
    let (host, mut events) = host(json!([effect]), json!([]), false);
    let host = Arc::new(host);
    let inner = BasicToolExecutor::from_registry(
        ToolRegistry::new().with(crate::tools::EditTool::new(root.path().to_path_buf())),
    );
    let executor: Arc<dyn ToolExecutor> = Arc::new(crate::tools::ahp::AhpToolExecutor::new(
        Arc::new(inner),
        host.clone(),
    ));
    let scope = ToolExecutionScope {
        executor: executor.clone(),
        session_id: SessionId::new("session-1"),
        turn_id: TurnId::new("turn-1"),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
    };
    let request = ToolRequest::new("edit-contract", "edit", input, "session-1", "turn-1");
    let outcome = executor
        .execute_owned(request, scope.nested_context(MetadataMap::new()))
        .await;
    let content = std::fs::read_to_string(root.path().join("target.txt")).unwrap();
    let mut received = Vec::new();
    while let Ok(event) = events.try_recv() {
        received.push(event);
    }
    host.shutdown().await.unwrap();
    (outcome, received, content)
}

#[tokio::test]
async fn native_argument_contract_valid_merge_and_replace_execute_real_edit() {
    for operation in ["merge", "replace"] {
        let input = json!({"op":"edit","path":"target.txt","hunks":[{"old":"original","new":"unused"}],"extension":{"kept":true}});
        let replacement = if operation == "merge" {
            json!({"hunks":[{"old":"original","new":"changed"}]})
        } else {
            json!({"op":"edit","path":"target.txt","hunks":[{"old":"original","new":"changed"}],"extension":{"kept":true}})
        };
        let (outcome, events, content) = execute_edit_contract(
            input,
            json!({
                "type":"modify","target":"input","operation":operation,"value":replacement,
            }),
        )
        .await;
        assert!(
            matches!(outcome, ToolExecutionOutcome::Completed(_)),
            "{outcome:?}"
        );
        assert_eq!(content, "changed");
        let before = &events[0];
        let after = events
            .iter()
            .find(|event| event["type"] == "tool.after")
            .unwrap();
        assert_eq!(after["parentEventId"], before["id"]);
        assert_eq!(after["tool"]["input"]["extension"], json!({"kept":true}));
        assert_eq!(after["tool"]["input"]["hunks"][0]["new"], "changed");
    }
}

#[tokio::test]
async fn native_argument_contract_invalid_edits_are_sdk_denials_before_invocation() {
    for (operation, invalid) in [
        ("replace", json!({"op":"delete","path":17})),
        ("replace", json!({"op":"delete"})),
        (
            "replace",
            json!({"op":"edit","path":"target.txt","hunks":[{"old":"original","new":false}]}),
        ),
        ("replace", json!({"op":"unsupported","path":"target.txt"})),
        ("merge", json!({"path":false})),
        (
            "merge",
            json!({"op":"edit","hunks":[{"old":17,"new":"changed"}]}),
        ),
    ] {
        let (outcome, events, content) = execute_edit_contract(
            json!({"op":"delete","path":"target.txt"}),
            json!({"type":"modify","target":"input","operation":operation,"value":invalid}),
        )
        .await;
        assert!(
            matches!(
                outcome,
                ToolExecutionOutcome::FailedBeforeInvocation(ToolError::PermissionDenied(_))
            ),
            "must be an SDK fail-closed denial, not native decoder failure: {outcome:?}"
        );
        assert_eq!(content, "original");
        let after = events
            .iter()
            .find(|event| event["type"] == "tool.after")
            .unwrap();
        assert_eq!(after["execution"]["status"], "skipped");
        assert_eq!(after["outcome"], "denied");
        assert_eq!(
            after["tool"]["input"],
            json!({"op":"delete","path":"target.txt"})
        );
    }
}

#[tokio::test]
async fn native_argument_contract_rejects_invalid_initial_input_without_delivery() {
    let (outcome, events, content) =
        execute_edit_contract(json!({"op":"delete","path":false}), json!({"type":"allow"})).await;
    assert!(matches!(
        outcome,
        ToolExecutionOutcome::FailedBeforeInvocation(_)
    ));
    assert!(events.is_empty());
    assert_eq!(content, "original");
}
