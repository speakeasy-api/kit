// Integration crate and its helpers are test-only. Placeholders stay denied.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use agentkit_core::{
    CancellationController, MetadataMap, SessionId, ToolCallId, ToolOutput, TurnCancellation,
    TurnId,
};
use agentkit_tools_core::{
    AllowAllPermissions, BasicToolExecutor, ToolError, ToolExecutionOutcome, ToolExecutionScope,
    ToolName, ToolRequest, ToolSource,
};
use serde_json::json;

#[test]
fn runtime_is_rooted_and_exposes_only_compose() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    assert_eq!(runtime.root(), directory.path().canonicalize().unwrap());

    let visible = runtime.compose(0);
    let specs = visible.specs();
    let names = specs
        .iter()
        .map(|spec| spec.name.0.to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, ["compose"]);

    for child in [
        "docs",
        "shell",
        "edit",
        "subagent",
        "prompt",
        "fork",
        "subagents",
        "close",
        "a2a",
        "tool_search",
        "auth",
        "tool",
    ] {
        assert!(
            visible.get(&ToolName::new(child)).is_some(),
            "missing compose child {child:?}"
        );
    }
}

#[test]
fn compose_advertises_only_configured_acp_harnesses() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "review-agent".into(),
            args: vec!["acp".into()],
            permissions: kit::AcpPermissionPolicy::Deny,
        },
    )]))
    .unwrap();
    let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.kit".into()).unwrap();
    let description = &runtime.compose(0).specs()[0].description;
    assert!(description.contains("\"enum\":[\"acp.kit\",\"acp.review\"]"));
    assert!(!description.contains("acp.unknown"));
}

#[tokio::test]
async fn mcp_meta_tools_are_available_without_an_implicit_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"found = tool_search({ query: "anything" })
return { found }"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("search failed: {outcome:?}")
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(
            json!({"found": {"servers": [], "total_matched": 0, "total_returned": 0, "truncated": false}})
        )
    );

    let outcome = execute_compose(&runtime, r#"return tool({ name: "shell", args: {} })"#).await;
    assert!(matches!(outcome, ToolExecutionOutcome::Failed(_)));
}

#[tokio::test]
async fn discovered_mcp_tools_are_dispatchable_without_being_advertised() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = format!("{}/fixtures/mock-mcp.py", env!("CARGO_MANIFEST_DIR"));
    let config = directory.path().join("mcp.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "mcpServers": {
                "mock": {
                    "command": "python3",
                    "args": [fixture]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let runtime = kit::Runtime::with_mcp_config(
        runtime,
        Some(&config),
        Vec::new(),
        false,
        kit::tools::CredentialStorage::Memory,
    )
    .await
    .unwrap();

    let compose = runtime.compose(0);
    assert_eq!(
        compose
            .specs()
            .iter()
            .map(|spec| spec.name.0.as_str())
            .collect::<Vec<_>>(),
        ["compose"],
        "MCP tools must remain hidden from the model-visible catalog"
    );
    let bypass = execute_compose(
        &runtime,
        r#"found = tool_search({ query: "echo" })
assert(found.total_returned == 1, "mock MCP tool was not discovered")
return mcp_mock_echo({ text: "bypass" })"#,
    )
    .await;
    assert!(
        matches!(bypass, ToolExecutionOutcome::Failed(ToolError::InvalidInput(ref error)) if error.contains("`mcp_mock_echo` is not registered")),
        "raw MCP adapter bypassed the tool meta-tool: {bypass:?}"
    );

    let outcome = execute_compose(
        &runtime,
        r#"found = tool_search({ query: "echo" })
assert(found.total_returned == 1, "mock MCP tool was not discovered")
return tool({ name: found.servers[0].tools[0].name, args: { text: "hello" } })"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("discovered MCP tool was not dispatchable: {outcome:?}");
    };
    assert_eq!(result.result.output, ToolOutput::structured(json!("hello")));
}

#[tokio::test]
async fn subagent_selects_an_advertised_acp_model() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture.clone(), "--models".into()],
            permissions: Default::default(),
        },
    )]))
    .unwrap()
    .with_model_policies(BTreeMap::from([(
        "acp.review".into(),
        kit::SubagentHarnessPolicy {
            models: BTreeMap::from([("review".into(), "mock/requested".into())]),
            allow_model_overrides: Some(vec!["mock/requested".into()]),
            ..Default::default()
        },
    )]))
    .unwrap();
    let runtime =
        kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.review".into()).unwrap();

    let outcome = execute_compose(
        &runtime,
        r#"child = subagent({ harness: "acp.review", model: "review", prompt: "MOCK_SELECTED_MODEL" })
branch = fork({ subagent: child, prompt: "MOCK_SELECTED_MODEL" })
return { child: child.output, branch: branch.output }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("model-selecting subagent failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({
            "child": "mock/requested",
            "branch": "mock/requested"
        }))
    );

    let outcome = execute_compose(
        &runtime,
        r#"return subagent({ model: "mock/default", prompt: "never runs" })"#,
    )
    .await;
    let ToolExecutionOutcome::Failed(error) = outcome else {
        panic!("disallowed model override unexpectedly succeeded: {outcome:?}");
    };
    assert!(format!("{error:?}").contains("is not allowed for ACP harness"));

    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let runtime = kit::Runtime::with_acp_harnesses(
        kit::Runtime::new(directory.path(), "gpt-5.4").unwrap(),
        harnesses,
        "acp.review".into(),
    )
    .unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"return subagent({ model: "mock/requested", prompt: "never runs" })"#,
    )
    .await;
    let ToolExecutionOutcome::Failed(error) = outcome else {
        panic!("unsupported model selection unexpectedly succeeded: {outcome:?}");
    };
    assert!(format!("{error:?}").contains("does not advertise a selectable model session option"));
}

// All helpers here exercise the public runtime API against the external ACP fixture.
fn config_options_runtime(
    directory: &std::path::Path,
    config_options: BTreeMap<String, serde_json::Value>,
    advertise: bool,
) -> Arc<kit::Runtime> {
    let mut args = vec![
        format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR")),
        "--models".into(),
        format!(
            "--idle-config-release={}",
            directory.join("idle-release").display()
        ),
        format!(
            "--idle-config-sent={}",
            directory.join("idle-sent").display()
        ),
    ];
    if advertise {
        args.push("--config-options".into());
    }
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args,
            permissions: Default::default(),
        },
    )]))
    .unwrap()
    .with_model_policies(BTreeMap::from([(
        "acp.review".into(),
        kit::SubagentHarnessPolicy {
            config_options,
            ..Default::default()
        },
    )]))
    .unwrap();
    kit::Runtime::with_acp_harnesses(
        kit::Runtime::new(directory, "gpt-5.4").unwrap(),
        harnesses,
        "acp.review".into(),
    )
    .unwrap()
}

fn latest_config_values(handle: &serde_json::Value) -> serde_json::Value {
    let snapshot = handle["updates"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|item| item["sessionUpdate"] == "config_option_update")
        .unwrap_or_else(|| panic!("missing config snapshot: {handle}"));
    config_values(&snapshot["configOptions"])
}

fn config_values(options: &serde_json::Value) -> serde_json::Value {
    serde_json::Value::Object(
        options
            .as_array()
            .unwrap()
            .iter()
            .map(|option| {
                (
                    option["id"].as_str().unwrap().to_owned(),
                    option["currentValue"].clone(),
                )
            })
            .collect(),
    )
}

#[tokio::test]
async fn subagent_applies_select_and_boolean_config_options_and_returns_response_state() {
    for reasoning_key in ["reasoning_effort", "thought_level"] {
        let directory = tempfile::tempdir().unwrap();
        let runtime = config_options_runtime(
            directory.path(),
            BTreeMap::from([
                (reasoning_key.into(), json!("high")),
                ("mode".into(), json!("plan")),
                ("custom_enabled".into(), json!(true)),
            ]),
            true,
        );
        let outcome = execute_compose(
            &runtime,
            r#"return subagent({ prompt: "MOCK_CONFIG_OPTIONS" })"#,
        )
        .await;
        let ToolExecutionOutcome::Completed(result) = outcome else {
            panic!("configuring {reasoning_key} failed: {outcome:?}");
        };
        let ToolOutput::Structured(handle) = result.result.output else {
            panic!("missing structured subagent handle");
        };
        let expected = json!({
            "model": "mock/default", "reasoning_effort": "high",
            "mode": "plan", "custom_enabled": true,
        });
        // The prompt emits no config notification: the handle must retain setup responses.
        assert_eq!(latest_config_values(&handle), expected);
        let observed: serde_json::Value =
            serde_json::from_str(handle["output"].as_str().unwrap()).unwrap();
        assert_eq!(config_values(&observed), expected);
    }
}

#[tokio::test]
async fn subagent_retains_config_notifications_across_prompt_and_native_fork() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = config_options_runtime(
        directory.path(),
        BTreeMap::from([
            ("thought_level".into(), json!("high")),
            ("mode".into(), json!("code")),
            ("custom_enabled".into(), json!(true)),
        ]),
        true,
    );
    let outcome = execute_compose(
        &runtime,
        r#"child = subagent({ model: "mock/requested", prompt: "MOCK_CONFIG_UPDATE" })
continued = prompt({ subagent: child, prompt: "MOCK_CONFIG_OPTIONS" })
branch = fork({ subagent: continued, prompt: "MOCK_CONFIG_OPTIONS" })
return { child, continued, branch }"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("config notification lifecycle failed: {outcome:?}");
    };
    let ToolOutput::Structured(value) = result.result.output else {
        panic!("missing structured lifecycle output");
    };
    for key in ["child", "continued", "branch"] {
        let expected = json!({
            "model": "mock/requested",
            "reasoning_effort": "low", "mode": "plan", "custom_enabled": false,
        });
        assert_eq!(latest_config_values(&value[key]), expected, "{key}");
        if key != "child" {
            let observed: serde_json::Value =
                serde_json::from_str(value[key]["output"].as_str().unwrap()).unwrap();
            assert_eq!(config_values(&observed), expected, "{key}");
        }
    }
}

#[tokio::test]
async fn subagent_retains_idle_config_notifications_on_next_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = config_options_runtime(
        directory.path(),
        BTreeMap::from([("thought_level".into(), json!("high"))]),
        true,
    );
    let source: Arc<dyn ToolSource> = Arc::new(runtime.compose(0));
    let outcome = execute_compose_source(
        Arc::clone(&source),
        r#"return subagent({ prompt: "MOCK_IDLE_CONFIG_UPDATE" })"#,
        None,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("initial prompt failed: {outcome:?}");
    };
    let ToolOutput::Structured(child) = result.result.output else {
        panic!("missing child handle");
    };
    assert_eq!(latest_config_values(&child)["reasoning_effort"], "high");
    // Release the notification only after the runtime has returned the completed child.
    std::fs::write(directory.path().join("idle-release"), "release").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !directory.path().join("idle-sent").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture did not send idle config notification");
    let outcome = execute_compose_source(
        source,
        &format!(
            "return prompt({{ subagent: {}, prompt: \"MOCK_CONFIG_OPTIONS\" }})",
            serde_json::to_string(&child).unwrap()
        ),
        None,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("continuing after idle notification failed: {outcome:?}");
    };
    let ToolOutput::Structured(continued) = result.result.output else {
        panic!("missing continued handle");
    };
    assert_eq!(
        latest_config_values(&continued),
        json!({"model": "mock/default", "reasoning_effort": "low",
               "mode": "plan", "custom_enabled": false})
    );
}

// The parent runs the real subagent/prompt tools. Only the nested Kit's HTTP
// inference provider is fake; its ACP v2 transport and turn capture are real.
#[tokio::test]
async fn kit_subagent_overlapping_prompt_callers_get_their_own_complete_results() {
    use axum::{Json, Router, body::Body, extract::State, response::IntoResponse, routing::post};
    use serde_json::Value;
    use tokio::{net::TcpListener, sync::mpsc};

    type Replies = mpsc::Sender<String>;
    type Requests = mpsc::Sender<(Value, Replies)>;

    async fn inference(
        State(requests): State<Requests>,
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
        let (reply, chunks) = mpsc::channel::<String>(1);
        requests.send((body, reply)).await.unwrap();
        let stream = futures_util::stream::unfold(chunks, |mut chunks| async {
            chunks
                .recv()
                .await
                .map(|chunk| (Ok::<_, std::convert::Infallible>(chunk), chunks))
        });
        (
            [("content-type", "text/event-stream")],
            Body::from_stream(stream),
        )
    }

    async fn answer(reply: &Replies, text: &str, finish: Option<&str>) {
        let event = json!({"id":"controlled", "choices":[{
            "index":0, "delta":{"role":"assistant", "content":text}, "finish_reason":finish,
        }]});
        reply.send(format!("data: {event}\n\n")).await.unwrap();
        if finish.is_some() {
            reply.send("data: [DONE]\n\n".into()).await.unwrap();
        }
    }

    fn result(outcome: ToolExecutionOutcome) -> Value {
        let ToolExecutionOutcome::Completed(result) = outcome else {
            panic!("parent tool failed: {outcome:?}");
        };
        let ToolOutput::Structured(value) = result.result.output else {
            panic!("parent tool returned non-structured output");
        };
        value
    }

    async fn next_request(
        requests: &mut mpsc::Receiver<(Value, Replies)>,
        prompt: &str,
    ) -> Replies {
        let (body, reply) = requests.recv().await.expect("missing inference request");
        let messages = body["messages"].as_array().unwrap();
        let last_user = messages
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .unwrap();
        assert!(
            last_user["content"].to_string().contains(prompt),
            "expected {prompt}, got {last_user}"
        );
        reply
    }

    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/stream", listener.local_addr().unwrap());
    let (requests, mut received) = mpsc::channel(4);
    let provider = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(post(inference)).with_state(requests),
        )
        .await
        .unwrap();
    });
    let runtime = kit::Runtime::new_with_provider_and_credentials(
        directory.path(),
        "test/model",
        kit::ProviderKind::OpenRouter,
        kit::tools::CredentialStorage::Memory,
    )
    .unwrap();
    // Select the built-in Kit profile with the Cargo-built executable, not the
    // test executable that current_exe() would find in this integration crate.
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "kit".into(),
        kit::AcpHarnessProfile {
            command: "/usr/bin/env".into(),
            args: vec![
                "-i".into(),
                format!("HOME={}", directory.path().display()),
                "OPENROUTER_API_KEY=local-test-key".into(),
                format!("OPENROUTER_BASE_URL={url}"),
                env!("CARGO_BIN_EXE_kit").into(),
                "acp".into(),
                "--protocol-version".into(),
                "2".into(),
            ],
            permissions: kit::AcpPermissionPolicy::Deny,
        },
    )]))
    .unwrap();
    let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.kit".into()).unwrap();
    let source: Arc<dyn ToolSource> = Arc::new(runtime.compose(0));
    let scope = ToolExecutionScope {
        executor: Arc::new(BasicToolExecutor::new([source])),
        session_id: SessionId::new("parent"),
        turn_id: TurnId::new("turn"),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
    };
    let call = |id, name, input| {
        scope.execute_child(ToolRequest {
            call_id: ToolCallId::new(id),
            tool_name: ToolName::new(name),
            input,
            session_id: scope.session_id.clone(),
            turn_id: scope.turn_id.clone(),
            metadata: MetadataMap::new(),
        })
    };

    let finished = tokio::time::timeout(Duration::from_secs(20), async {
        let seed = call("seed", "subagent", json!({"prompt":"seed-prompt"}));
        tokio::pin!(seed);
        let seed_reply = tokio::select! {
            reply = next_request(&mut received, "seed-prompt") => reply,
            early = &mut seed => panic!("seed settled before inference: {early:?}"),
        };
        answer(&seed_reply, "seed-answer", Some("stop")).await;
        drop(seed_reply);
        let child = result(seed.await);
        assert_eq!(child["output"], "seed-answer");
        let a = call(
            "a",
            "prompt",
            json!({"subagent":child,"prompt":"caller-A-prompt"}),
        );
        let b = call(
            "b",
            "prompt",
            json!({"subagent":child,"prompt":"caller-B-prompt"}),
        );
        tokio::pin!(a, b);
        let a_reply = tokio::select! {
            reply = next_request(&mut received, "caller-A-prompt") => reply,
            early = &mut a => panic!("A settled before inference: {early:?}"),
        };
        // Poll B while A's provider response is still held. Today B waits in
        // the parent; this test does not prescribe how the wire admits B.
        assert!(
            futures_util::poll!(&mut b).is_pending(),
            "B settled while A was still held"
        );
        answer(&a_reply, "only-A-answer", Some("stop")).await;
        drop(a_reply);
        let (a, b, ()) = tokio::join!(&mut a, &mut b, async {
            let b_reply = next_request(&mut received, "caller-B-prompt").await;
            answer(&b_reply, "only-B-prefix", None).await;
            answer(&b_reply, "-and-B-suffix", Some("stop")).await;
        });
        let a = result(a);
        let b = result(b);
        assert_eq!(a["output"], "only-A-answer");
        assert_eq!(b["output"], "only-B-prefix-and-B-suffix");
        assert_eq!(a["id"], child["id"]);
        assert_eq!(b["id"], child["id"]);
        assert_eq!(a["generation"], 2);
        assert_eq!(b["generation"], 3);
    })
    .await;
    provider.abort();
    finished.expect("nested Kit prompts did not settle");
}

#[tokio::test]
async fn subagent_rejects_unsupported_or_mismatched_config_options() {
    for (advertise, key, value) in [
        (false, "thought_level", json!("high")),
        (true, "unknown_option", json!("high")),
        (true, "reasoning_effort", json!("impossible")),
        (true, "reasoning_effort", json!(true)),
        (true, "custom_enabled", json!("true")),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let runtime = config_options_runtime(
            directory.path(),
            BTreeMap::from([(key.into(), value.clone())]),
            advertise,
        );
        let outcome =
            execute_compose(&runtime, r#"return subagent({ prompt: "must not run" })"#).await;
        let ToolExecutionOutcome::Failed(error) = outcome else {
            panic!("invalid {key}={value} unexpectedly succeeded: {outcome:?}");
        };
        assert!(format!("{error:?}").contains(key), "{error:?}");
    }
}

#[tokio::test]
async fn structured_subagent_output_can_drive_runlet_control_flow() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture],
            permissions: kit::AcpPermissionPolicy::Deny,
        },
    )]))
    .unwrap();
    let runtime =
        kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.review".into()).unwrap();

    let outcome = execute_compose(
        &runtime,
        r#"review = subagent({
  harness: "acp.review",
  prompt: "MOCK_STRUCTURED_OUTPUT",
  output_schema: {
    type: "object",
    properties: {
      approved: { type: "boolean" },
      reason: { type: "string" }
    },
    required: ["approved", "reason"],
    additionalProperties: false
  }
})
return if review.output.approved {
  return { reason: review.output.reason }
} else {
  return fail("REJECTED", review.output.reason)
}"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("structured compose failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({"reason": "mock approved"}))
    );
}

#[tokio::test]
async fn compose_lists_and_closes_subagents_by_handle_or_id() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let runtime =
        kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.review".into()).unwrap();

    let outcome = execute_compose(
        &runtime,
        r#"first = subagent({ prompt: "first" })
second = subagent({ prompt: "second" })
before = after [first, second] { return subagents({}) }
closed_first = after before { return close(first) }
closed_second = after closed_first { return close({ id: second.id }) }
after_close = after closed_second { return subagents({}) }
return { before, closed_first, closed_second, after_close }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("subagent lifecycle compose failed: {outcome:?}");
    };
    let ToolOutput::Structured(value) = result.result.output else {
        panic!("compose did not return structured output");
    };
    assert_eq!(value["before"].as_array().unwrap().len(), 2);
    assert!(value["after_close"].as_array().unwrap().is_empty());
    let before_ids = value["before"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| child["id"].clone())
        .collect::<Vec<_>>();
    assert!(before_ids.contains(&value["closed_first"]["id"]));
    assert!(before_ids.contains(&value["closed_second"]["id"]));
    assert_ne!(value["closed_first"]["id"], value["closed_second"]["id"]);
}

#[tokio::test]
async fn subagent_exposes_bounded_rich_updates_without_changing_text_output() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "rich".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture, "--require-compaction".into()],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.rich".into()).unwrap();

    let outcome = execute_compose(
        &runtime,
        r#"child = subagent({ harness: "acp.rich", prompt: "MOCK_RICH_OUTPUT" })
return { output: child.output, updates: child.updates }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("rich compose failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({
            "output": "rich done",
            "updates": {
                "items": [
                    {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {
                            "type": "image",
                            "data": "aGVsbG8=",
                            "mimeType": "image/png"
                        }
                    },
                    {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "call-1",
                        "title": "Inspect files"
                    },
                    {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": "call-1",
                        "status": "completed"
                    },
                    {
                        "sessionUpdate": "plan",
                        "entries": [
                            {"content": "Inspect", "priority": "high", "status": "completed"}
                        ]
                    },
                    {"sessionUpdate": "usage_update", "used": 10, "size": 100,
                     "cost": {"amount": 0.25, "currency": "USD"}},
                    {"sessionUpdate": "session_info_update", "title": "Child title"},
                    {"sessionUpdate": "available_commands_update", "availableCommands": []},
                    {"sessionUpdate": "notice", "severity": "info", "title": "Working"},
                    {"sessionUpdate": "compaction_update", "compactionId": "compact-1",
                     "status": "in_progress"},
                    {"sessionUpdate": "compaction_summary_chunk", "compactionId": "compact-1",
                     "content": {"type": "text", "text": "Retained context"}}
                ],
                "truncated": false
            }
        }))
    );
}

#[tokio::test]
async fn subagent_preserves_v1_and_v2_diff_and_terminal_wire_content() {
    for v2 in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
        let mut args = vec![format!(
            "{}/fixtures/mock-acp.py",
            env!("CARGO_MANIFEST_DIR")
        )];
        if v2 {
            args.push("--v2".into());
        }
        let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
            "rich".into(),
            kit::AcpHarnessProfile {
                command: "python3".into(),
                args,
                permissions: Default::default(),
            },
        )]))
        .unwrap();
        let runtime =
            kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.rich".into()).unwrap();
        let outcome = execute_compose(
            &runtime,
            r#"return subagent({prompt: "MOCK_TOOL_CONTENT"})"#,
        )
        .await;
        let ToolExecutionOutcome::Completed(result) = outcome else {
            panic!("child failed: {outcome:?}");
        };
        let ToolOutput::Structured(value) = result.result.output else {
            panic!("expected JSON");
        };
        assert_eq!(value["output"], "tool content done");
        assert_eq!(value["updates"]["truncated"], false);
        let items = value["updates"]["items"].as_array().unwrap();
        assert_eq!(items[0]["rawOutput"], json!({"stdout": "child output"}));
        let diff = &items[0]["content"][1];
        assert_eq!(diff["type"], "diff");
        if v2 {
            assert_eq!(diff["changes"][0]["path"], "/tmp/child.txt");
            assert_eq!(items[1]["terminalId"], "terminal-1");
            assert_eq!(items[2]["content"][0]["terminalId"], "terminal-1");
            assert_eq!(items[3]["data"], "Y2hpbGQgb3V0cHV0Cg==");
            assert_eq!(items[4]["exitStatus"]["exitCode"], 0);
        } else {
            assert_eq!(diff["oldText"], "old\n");
            assert_eq!(diff["newText"], "new\n");
        }
    }
}

#[tokio::test]
async fn compose_dispatches_bundled_docs_search_through_runlet() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"return docs({ query: "Kit documentation troubleshooting" })"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("compose failed: {outcome:?}");
    };
    let ToolOutput::Structured(output) = result.result.output else {
        panic!("compose returned a non-JSON result");
    };
    assert_eq!(output["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        output["matches"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(output["matches"].as_array().unwrap().len() <= 5);
}

#[tokio::test]
async fn compose_preserves_large_shell_output_for_internal_consumers() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"result = shell({ command: "python3 -c 'print(\"x\" * 12000, end=\"\")'" })
written = edit({ op: "add", path: "copied.txt", content: result.stdout })
return written"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("compose failed: {outcome:?}");
    };
    assert_eq!(
        std::fs::read(directory.path().join("copied.txt")).unwrap(),
        vec![b'x'; 12000]
    );
    assert!(!format!("{:?}", result.result.output).contains("output spilled"));
}

#[tokio::test]
async fn compose_spills_large_final_output_only_at_the_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"result = shell({ command: "python3 -c 'print(\"x\" * 12000, end=\"\")'" })
return { stdout: result.stdout }"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("compose failed: {outcome:?}");
    };
    let ToolOutput::Structured(output) = result.result.output else {
        panic!("compose returned a non-JSON result");
    };
    let artifact = output["artifact"].as_str().unwrap();
    let stored = std::fs::read_to_string(artifact).unwrap();

    assert!(
        output["preview"]
            .as_str()
            .unwrap()
            .contains("compose output spilled")
    );
    assert!(!stored.contains("shell output spilled"));
    assert_eq!(stored.matches('x').count(), 12000);
    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir(std::path::Path::new(artifact).parent().unwrap());
}

#[tokio::test]
async fn compose_spills_results_above_the_dependency_default_limit() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"result = shell({ command: "printf '%1100000s' '' | tr ' ' x" })
return { stdout: result.stdout }"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("compose failed: {outcome:?}");
    };
    let ToolOutput::Structured(output) = result.result.output else {
        panic!("compose returned a non-JSON result");
    };
    let artifact = output["artifact"].as_str().unwrap();

    assert!(output["original_bytes"].as_u64().unwrap() > 1024 * 1024);
    assert_eq!(
        std::fs::read_to_string(artifact)
            .unwrap()
            .matches('x')
            .count(),
        1_100_000
    );
    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir(std::path::Path::new(artifact).parent().unwrap());
}

#[tokio::test]
async fn compose_dispatches_hidden_shell_through_runlet() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"result = shell({ command: "printf hidden-ok" })
return { code: result.exit_code, ok: result.success, stdout: result.stdout, stderr: result.stderr }"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("compose failed: {outcome:?}");
    };
    let ToolOutput::Structured(output) = result.result.output else {
        panic!("compose returned a non-JSON result");
    };
    assert_eq!(
        output,
        json!({
            "code": 0,
            "ok": true,
            "stdout": "hidden-ok",
            "stderr": ""
        })
    );
}

#[tokio::test]
async fn independent_top_level_tool_calls_run_concurrently() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"first = shell({ command: "touch first; for i in $(seq 1 100); do test -e second && exit 0; sleep 0.02; done; exit 1" })
second = shell({ command: "touch second; for i in $(seq 1 100); do test -e first && exit 0; sleep 0.02; done; exit 1" })
return { first: first.success, second: second.success }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("concurrent compose failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({"first": true, "second": true}))
    );
}

#[tokio::test]
async fn after_orders_calls_without_data_flow() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"prerequisite = shell({ command: "sleep 0.1; touch ready" })
dependent = after prerequisite {
  return shell({ command: "test -e ready" })
}
return { prerequisite: prerequisite.success, dependent: dependent.success }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("ordered compose failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({"prerequisite": true, "dependent": true}))
    );
}

#[tokio::test]
async fn compose_rejects_invalid_hidden_tool_input() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(&runtime, "return shell({ timeout_seconds: 1 })").await;

    assert!(
        matches!(
            outcome,
            ToolExecutionOutcome::FailedBeforeInvocation(_) | ToolExecutionOutcome::Failed(_)
        ),
        "invalid input unexpectedly ran: {outcome:?}"
    );
}

#[tokio::test]
async fn cancelling_a_turn_stops_a_running_shell_command() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let controller = CancellationController::new();
    let cancellation = TurnCancellation::new(controller.handle());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        controller.interrupt();
    });

    let started = Instant::now();
    let outcome = execute_compose_cancelled(
        &runtime,
        r#"return shell({ command: "sleep 30", timeout_seconds: 600 })"#,
        Some(cancellation),
    )
    .await;

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancellation waited for the command: {outcome:?}"
    );
    assert!(
        matches!(outcome, ToolExecutionOutcome::Failed(_)),
        "cancelled shell did not fail the compose run: {outcome:?}"
    );
}

async fn execute_compose(runtime: &Arc<kit::Runtime>, script: &str) -> ToolExecutionOutcome {
    execute_compose_cancelled(runtime, script, None).await
}

async fn execute_compose_cancelled(
    runtime: &Arc<kit::Runtime>,
    script: &str,
    cancellation: Option<TurnCancellation>,
) -> ToolExecutionOutcome {
    execute_compose_source(Arc::new(runtime.compose(0)), script, cancellation).await
}

async fn execute_compose_source(
    source: Arc<dyn ToolSource>,
    script: &str,
    cancellation: Option<TurnCancellation>,
) -> ToolExecutionOutcome {
    // HOME/session/call-scoped artifacts must not share a spill directory with
    // another parallel invocation that may remove it during cleanup.
    static NEXT_CALL: AtomicUsize = AtomicUsize::new(0);
    let call_id = ToolCallId::new(format!(
        "compose-test-{}",
        NEXT_CALL.fetch_add(1, Ordering::Relaxed)
    ));
    let executor = Arc::new(BasicToolExecutor::new([source]));
    let scope = ToolExecutionScope {
        executor,
        session_id: SessionId::new("test"),
        turn_id: TurnId::new("turn"),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation,
    };
    scope
        .execute_child(ToolRequest {
            call_id,
            tool_name: ToolName::new("compose"),
            input: json!({"script": script}),
            session_id: SessionId::new("test"),
            turn_id: TurnId::new("turn"),
            metadata: MetadataMap::new(),
        })
        .await
}
