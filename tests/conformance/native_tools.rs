use std::{
    collections::BTreeSet,
    io::{Read as _, Write as _},
    net::TcpListener,
    thread,
};

use agentkit_core::{Item, ItemKind, MetadataMap, SessionId, TurnId};
use agentkit_loop::{ModelAdapter, ModelSession, ModelTurn, SessionConfig, TurnRequest};
use agentkit_provider_anthropic::{AnthropicAdapter, AnthropicConfig};
use agentkit_provider_ollama::{OllamaAdapter, OllamaConfig};
use agentkit_provider_openai::{OpenAIAdapter, OpenAIConfig};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};

use kit::{
    capabilities::native::{
        JSON_SCHEMA_DIALECT, MAX_NATIVE_OUTPUT_BYTES, NativeCatalog, NativeTool,
    },
    domain::{
        config::{Grant, LayerStack, RunConfigContext},
        ids::{PrincipalId, ProjectId, RunId},
    },
};

fn catalog() -> &'static [kit::capabilities::native::NativeToolDescriptor; 6] {
    NativeCatalog::all()
}

fn request() -> TurnRequest {
    TurnRequest {
        session_id: SessionId::new("native-provider-specs"),
        turn_id: TurnId::new("turn-1"),
        transcript: vec![Item::text(ItemKind::User, "inspect the repository")],
        available_tools: catalog().iter().map(|tool| tool.spec().clone()).collect(),
        cache: None,
        structured_output: None,
        metadata: MetadataMap::new(),
    }
}

fn protocol_fake(response: serde_json::Value) -> (String, thread::JoinHandle<serde_json::Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0);
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
            })
            .unwrap();
        while bytes.len() - header_end < length {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0);
            bytes.extend_from_slice(&buffer[..read]);
        }
        let body = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
        let response = serde_json::to_vec(&response).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        )
        .unwrap();
        stream.write_all(&response).unwrap();
        body
    });
    (format!("http://{address}"), handle)
}

fn assert_six_specs(body: serde_json::Value, anthropic: bool) {
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 6);
    let names = tools
        .iter()
        .map(|tool| {
            if anthropic {
                tool["name"].as_str().unwrap()
            } else {
                tool["function"]["name"].as_str().unwrap()
            }
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names,
        catalog()
            .iter()
            .map(|tool| tool.spec().name.0.as_str())
            .collect()
    );
}

#[tokio::test]
async fn actual_provider_request_builders_receive_all_six_native_specs() {
    let anthropic_response = serde_json::json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-test",
        "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
        "stop_sequence": null, "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let completion_response = serde_json::json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "test",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });

    let (url, captured) = protocol_fake(anthropic_response);
    let mut config = AnthropicConfig::new("test", "claude-test", 64)
        .unwrap()
        .with_base_url(url)
        .with_streaming(false);
    config.tool_choice = None;
    let adapter = AnthropicAdapter::new(config).unwrap();
    let mut session = adapter
        .start_session(SessionConfig::new("native-provider-specs"))
        .await
        .unwrap();
    let mut turn = session.begin_turn(request(), None).await.unwrap();
    assert!(turn.next_event(None).await.unwrap().is_some());
    assert_six_specs(captured.join().unwrap(), true);

    let (url, captured) = protocol_fake(completion_response.clone());
    let adapter = OpenAIAdapter::new(
        OpenAIConfig::new("test", "gpt-test")
            .with_base_url(url)
            .with_streaming(false),
    )
    .unwrap();
    let mut session = adapter
        .start_session(SessionConfig::new("native-provider-specs"))
        .await
        .unwrap();
    let mut turn = session.begin_turn(request(), None).await.unwrap();
    assert!(turn.next_event(None).await.unwrap().is_some());
    assert_six_specs(captured.join().unwrap(), false);

    let (url, captured) = protocol_fake(completion_response.clone());
    let adapter = OpenRouterAdapter::new(
        OpenRouterConfig::new("test", "openrouter/test")
            .with_base_url(url)
            .with_streaming(false),
    )
    .unwrap();
    let mut session = adapter
        .start_session(SessionConfig::new("native-provider-specs"))
        .await
        .unwrap();
    let mut turn = session.begin_turn(request(), None).await.unwrap();
    assert!(turn.next_event(None).await.unwrap().is_some());
    assert_six_specs(captured.join().unwrap(), false);

    let (url, captured) = protocol_fake(completion_response);
    let adapter = OllamaAdapter::new(
        OllamaConfig::new("llama-test")
            .with_base_url(url)
            .with_streaming(false),
    )
    .unwrap();
    let mut session = adapter
        .start_session(SessionConfig::new("native-provider-specs"))
        .await
        .unwrap();
    let mut turn = session.begin_turn(request(), None).await.unwrap();
    assert!(turn.next_event(None).await.unwrap().is_some());
    assert_six_specs(captured.join().unwrap(), false);
}

#[test]
fn tool_surface_few() {
    assert_eq!(catalog().len(), 6);
}

#[test]
fn tool_surface_orthogonal() {
    assert_eq!(
        catalog()
            .iter()
            .map(|tool| tool.tool())
            .collect::<BTreeSet<_>>(),
        NativeTool::ALL.into_iter().collect()
    );
}

#[test]
fn tool_surface_deterministic() {
    let first = catalog()
        .iter()
        .map(|tool| {
            (
                tool.spec().name.0.clone(),
                tool.schema().normalized_digest(),
                tool.identity().implementation_digest(),
            )
        })
        .collect::<Vec<_>>();
    let second = NativeCatalog::all()
        .iter()
        .map(|tool| {
            (
                tool.spec().name.0.clone(),
                tool.schema().normalized_digest(),
                tool.identity().implementation_digest(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(first, second);
}

#[test]
fn provider_aliases_are_valid_and_bijective_with_canonical_names() {
    let mut aliases = BTreeSet::new();
    let mut canonical = BTreeSet::new();
    for descriptor in catalog() {
        let alias = &descriptor.spec().name.0;
        assert!((1..=64).contains(&alias.len()));
        assert!(
            alias
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        );
        assert!(aliases.insert(alias.clone()));
        assert!(canonical.insert(descriptor.canonical_name()));
        assert_eq!(
            NativeCatalog::by_tool_name(alias).unwrap().tool(),
            descriptor.tool()
        );
        assert_eq!(
            NativeCatalog::by_canonical_name(&descriptor.canonical_name())
                .unwrap()
                .tool(),
            descriptor.tool()
        );
    }
    assert_eq!(
        aliases,
        NativeTool::ALL
            .into_iter()
            .map(NativeTool::provider_alias)
            .collect()
    );
    assert_eq!(
        canonical,
        NativeTool::ALL
            .into_iter()
            .map(NativeTool::canonical_name)
            .collect()
    );
}

#[test]
fn every_native_boundary_rejects_schema_invalid_input() {
    let invalid = [
        (NativeTool::Discover, serde_json::json!({})),
        (
            NativeTool::Search,
            serde_json::json!({"expected_revision":"bad","text":"x","mode":"content","path_prefixes":[],"languages":[]}),
        ),
        (
            NativeTool::Read,
            serde_json::json!({"expected_revision":"bad","path":"x","range":{"kind":"full"}}),
        ),
        (NativeTool::Edit, serde_json::json!({"unexpected":true})),
        (NativeTool::Run, serde_json::json!({"argv":[]})),
        (
            NativeTool::Check,
            serde_json::json!({"profile":"arbitrary","targets":[]}),
        ),
    ];
    for (tool, input) in invalid {
        let descriptor = catalog().iter().find(|entry| entry.tool() == tool).unwrap();
        assert!(
            !jsonschema::validator_for(&descriptor.spec().input_schema)
                .unwrap()
                .is_valid(&input)
        );
    }
}

#[test]
fn tool_surface_output_bounds() {
    for tool in catalog() {
        assert_eq!(
            tool.spec().metadata["kit.output.max_bytes"],
            MAX_NATIVE_OUTPUT_BYTES
        );
        assert!(
            tool.spec()
                .metadata
                .contains_key("agentkit.tool_output_limit")
        );
    }
}

#[test]
fn tool_description_selection() {
    assert!(
        catalog()
            .iter()
            .all(|tool| tool.spec().description.starts_with("Select for"))
    );
}

#[test]
fn tool_description_nonselection() {
    assert!(
        catalog()
            .iter()
            .all(|tool| tool.spec().description.contains("do not select")
                || tool.spec().description.contains("never select"))
    );
}

#[test]
fn tool_description_result() {
    assert!(
        catalog()
            .iter()
            .all(|tool| tool.spec().description.contains("Saves")
                || tool.spec().description.contains("saves"))
    );
}

#[test]
fn tool_description_example() {
    assert!(
        catalog()
            .iter()
            .all(|tool| tool.spec().description.matches("Example:").count() == 1)
    );
}

fn eager(tool: NativeTool) {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let grants = BTreeSet::from([
        Grant::WorkspaceRead,
        Grant::WorkspaceWrite,
        Grant::ProcessSpawn,
        Grant::VerificationTargeted,
    ]);
    let config = LayerStack::safe_defaults()
        .materialize(
            RunConfigContext {
                principal_id,
                project_id,
                run_id: RunId::generate().unwrap(),
            },
            &grants,
        )
        .unwrap();
    let enabled = NativeCatalog::enabled(&config);
    assert!(enabled.iter().any(|entry| entry.tool() == tool));
    for entry in enabled {
        assert_eq!(entry.schema().dialect(), JSON_SCHEMA_DIALECT);
        assert_eq!(
            entry.schema().source_digest(),
            entry.schema().normalized_digest()
        );
        jsonschema::validator_for(&entry.spec().input_schema).unwrap();
    }
}

#[test]
fn eager_tool_discover() {
    eager(NativeTool::Discover);
}

#[test]
fn eager_tool_search() {
    eager(NativeTool::Search);
}

#[test]
fn eager_tool_read() {
    eager(NativeTool::Read);
}

#[test]
fn eager_tool_edit() {
    eager(NativeTool::Edit);
}

#[test]
fn eager_tool_run() {
    eager(NativeTool::Run);
}

#[test]
fn eager_tool_check() {
    eager(NativeTool::Check);
}
