use std::{
    collections::BTreeSet,
    fs,
    io::{Read as _, Write as _},
    net::TcpListener,
    thread,
    time::Duration,
};

use agentkit_core::{Item, ItemKind, MetadataMap, SessionId, TurnId};
use agentkit_loop::{ModelAdapter, ModelSession, ModelTurn, SessionConfig, TurnRequest};
use agentkit_provider_anthropic::{AnthropicAdapter, AnthropicConfig};
use agentkit_provider_ollama::{OllamaAdapter, OllamaConfig};
use agentkit_provider_openai::{OpenAIAdapter, OpenAIConfig};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};

use kit::{
    capabilities::{
        kernel::identity::{Digest, DigestAlgorithm},
        native::{JSON_SCHEMA_DIALECT, MAX_NATIVE_OUTPUT_BYTES, NativeCatalog, NativeTool},
    },
    domain::{
        config::{Grant, LayerStack, RunConfigContext},
        ids::{PrincipalId, ProjectId, RunId},
    },
    workspace::{
        edit::ir::RootRelativePath,
        graph::structure::{GraphOptions, StructureGraphProvider},
        index::meta::{IndexOptions, MetadataIndex},
        map::{
            ExpansionRequest, MapBound, MapError, MapLimits, RelationshipKind,
            RepositoryMapRequest, build_repository_map_with_structure,
        },
        revision::{ManagedWorkspace, RevisionOptions},
    },
};

fn catalog() -> &'static [kit::capabilities::native::NativeToolDescriptor; 5] {
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
        generation: Default::default(),
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

fn assert_native_specs(body: serde_json::Value, anthropic: bool) {
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);
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
async fn actual_provider_request_builders_receive_all_native_specs() {
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
    assert_native_specs(captured.join().unwrap(), true);

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
    assert_native_specs(captured.join().unwrap(), false);

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
    assert_native_specs(captured.join().unwrap(), false);

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
    assert_native_specs(captured.join().unwrap(), false);
}

#[test]
fn tool_surface_few() {
    assert_eq!(catalog().len(), 5);
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
fn discover_map_schema_is_strict_bounded_and_preserves_the_legacy_form() {
    let descriptor = catalog()
        .iter()
        .find(|entry| entry.tool() == NativeTool::Discover)
        .unwrap();
    let schema = &descriptor.spec().input_schema;
    let validator = jsonschema::validator_for(schema).unwrap();
    let revision = format!("r:{}", "a".repeat(64));
    let legacy = serde_json::json!({
        "expected_revision": revision,
        "terms": ["Config"],
        "roots": [],
        "languages": ["rust"],
        "cursor": null
    });
    assert!(validator.is_valid(&legacy));
    assert_eq!(schema["oneOf"].as_array().unwrap().len(), 2);
    let legacy_schema = &schema["oneOf"][0];
    assert_eq!(
        legacy_schema["properties"],
        serde_json::json!({
            "cursor": {"type": ["object", "null"]},
            "expected_revision": {"pattern": "^r:[0-9a-f]{64}$", "type": "string"},
            "languages": {"items": {"type": "string"}, "maxItems": 32, "type": "array"},
            "roots": {"items": {"maxLength": 4096, "minLength": 1, "type": "string"}, "maxItems": 32, "type": "array"},
            "terms": {"items": {"maxLength": 256, "minLength": 1, "type": "string"}, "maxItems": 32, "type": "array"}
        })
    );

    let map = serde_json::json!({
        "expected_revision": revision,
        "map": {
            "taskTerms": ["Config"],
            "exactIdentifiers": ["b".repeat(64)],
            "stackFrames": [{"path": "src/lib.rs", "symbol": "Config", "line": 1}],
            "recentlyReadPaths": ["src/lib.rs"],
            "currentEditPaths": ["src/main.rs"],
            "pathPrefixes": ["src"],
            "languages": ["rust"],
            "historyPaths": ["src/lib.rs"],
            "blamePaths": ["src/lib.rs"],
            "relationships": ["contains", "contained_by", "semantic_definition", "imports", "tests", "changed_with"],
            "expansionSeeds": [],
            "graphSeeds": ["c".repeat(64)],
            "expandPaths": ["src/lib.rs"],
            "expandSymbols": ["Config"],
            "expandPackages": ["kit"],
            "expandTests": ["map_graph"],
            "scoreBand": {"min": 0, "max": 18446744073709551615_u64},
            "purpose": "neighborhood",
            "budgets": {"items": 200, "estimatedTokens": 16384, "hops": 4, "degree": 64, "resultBytes": 61440},
            "cursor": null
        }
    });
    assert!(validator.is_valid(&map));
    for purpose in ["dependencies", "dependents"] {
        let mut directional = map.clone();
        directional["map"]["purpose"] = serde_json::json!(purpose);
        assert!(!validator.is_valid(&directional));
    }
    for invalid in [
        serde_json::json!({"unexpected": true}),
        serde_json::json!({"budgets": {"items": 201}}),
        serde_json::json!({"budgets": {"estimatedTokens": 16385}}),
        serde_json::json!({"budgets": {"hops": 5}}),
        serde_json::json!({"budgets": {"degree": 65}}),
        serde_json::json!({"budgets": {"resultBytes": 61441}}),
        serde_json::json!({"cursor": {}}),
        serde_json::json!({"expandPaths": vec!["src/lib.rs"; 129]}),
        serde_json::json!({"expandPaths": ["/src/lib.rs"]}),
        serde_json::json!({"expandPaths": ["C:/src/lib.rs"]}),
        serde_json::json!({"expandPaths": ["src\\lib.rs"]}),
        serde_json::json!({"expandPaths": ["."]}),
        serde_json::json!({"expandPaths": [".."]}),
        serde_json::json!({"expandPaths": ["src/../lib.rs"]}),
        serde_json::json!({"expandPaths": ["src/line\nfeed.rs"]}),
        serde_json::json!({"expandSymbols": [""]}),
        serde_json::json!({"scoreBand": {"min": 0}}),
        serde_json::json!({"scoreBand": {"min": 0, "max": 1, "extra": true}}),
    ] {
        let mut request = serde_json::json!({
            "expected_revision": revision,
            "map": {}
        });
        request["map"] = invalid;
        assert!(!validator.is_valid(&request), "{request}");
    }
    assert!(!validator.is_valid(&serde_json::json!({
        "expected_revision": revision,
        "terms": [],
        "roots": [],
        "languages": [],
        "map": {}
    })));
    assert!(!validator.is_valid(&serde_json::json!({
        "expected_revision": revision,
        "map": {},
        "terms": ["ignored"]
    })));
    let map_properties = &schema["oneOf"][1]["properties"]["map"]["properties"];
    assert_eq!(map_properties["relationships"]["maxItems"], 16);
    for relationship in [
        "defines",
        "imports",
        "exports",
        "references",
        "calls",
        "implements",
        "inherits",
        "overrides",
        "tests",
        "changed_with",
    ] {
        assert!(
            map_properties["relationships"]["items"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == relationship)
        );
    }
    for selector in [
        "graphSeeds",
        "expandPackages",
        "expandTests",
        "historyPaths",
        "blamePaths",
    ] {
        assert_eq!(map_properties[selector]["maxItems"], 128);
        assert_eq!(map_properties[selector]["uniqueItems"], true);
    }
    assert!(
        map_properties["expandPaths"]["items"]["description"]
            .as_str()
            .unwrap()
            .contains("4096-byte UTF-8 limit")
    );
    assert!(
        map_properties["expandSymbols"]["description"]
            .as_str()
            .unwrap()
            .contains("256-byte UTF-8 limit")
    );
}

#[test]
fn expand_paths_schema_matches_portable_paths_and_openapi() {
    let discover = catalog()
        .iter()
        .find(|entry| entry.tool() == NativeTool::Discover)
        .unwrap();
    let schema = &discover.spec().input_schema["oneOf"][1]["properties"]["map"]["properties"]["expandPaths"]
        ["items"];
    let validator = jsonschema::validator_for(schema).unwrap();

    for invalid in [
        "",
        "/src/lib.rs",
        "C:/src/lib.rs",
        "src\\lib.rs",
        "src//lib.rs",
        "src/",
        ".",
        "..",
        "src/./lib.rs",
        "src/../lib.rs",
        "src/file.",
        "src/file ",
        "src/line\nfeed.rs",
        "src/control\u{0085}.rs",
        "src/file?.rs",
        "src/file*.rs",
        "src/file\".rs",
        "src/file<.rs",
        "src/file>.rs",
        "src/file|.rs",
        "src/file:.rs",
        "CON",
        "prn.txt",
        "src/AuX.rs",
        "nul.log",
        "COM1",
        "com9.rs",
        "LPT1",
        "lpt9.txt",
        "CONIN$",
        "conout$.log",
        "COM¹.txt",
        "lpt³.log",
    ] {
        assert!(
            RootRelativePath::parse(invalid, 4096).is_err(),
            "{invalid:?}"
        );
        assert!(
            !validator.is_valid(&serde_json::json!(invalid)),
            "{invalid:?}"
        );
    }
    for prefix in ["COM", "com", "LPT", "lpt"] {
        for number in 1..=9 {
            let invalid = format!("src/{prefix}{number}.txt");
            assert!(RootRelativePath::parse(&invalid, 4096).is_err());
            assert!(!validator.is_valid(&serde_json::json!(invalid)));
        }
    }
    for valid in [
        "README.md",
        "src/lib.rs",
        ".gitignore",
        "...file",
        "console",
        "com10",
        "a.b",
        "資料/日本語.rs",
    ] {
        assert!(RootRelativePath::parse(valid, 4096).is_ok(), "{valid:?}");
        assert!(validator.is_valid(&serde_json::json!(valid)), "{valid:?}");
    }
    assert!(!validator.is_valid(&serde_json::json!("a".repeat(4097))));

    let openapi: serde_json::Value = serde_json::to_value(
        serde_yaml::from_str::<serde_yaml::Value>(include_str!("../../docs/api/openapi.yaml"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        schema,
        &openapi["components"]["schemas"]["RepositoryDiscoverInput"]["oneOf"][1]["properties"]["map"]
            ["properties"]["expandPaths"]["items"]
    );
    let map = &openapi["components"]["schemas"]["RepositoryDiscoverInput"]["oneOf"][1]["properties"]
        ["map"]["properties"];
    assert_eq!(schema, &map["historyPaths"]["items"]);
    assert_eq!(schema, &map["blamePaths"]["items"]);
}

#[test]
fn all_native_descriptor_versions_remain_legacy_compatible() {
    for descriptor in catalog() {
        assert_eq!(descriptor.identity().version().as_str(), "1.0.0");
        assert_eq!(descriptor.spec().metadata["kit.native.version"], "1.0.0");
    }
}

#[test]
fn discover_output_schema_is_strict_for_map_mode_and_keeps_legacy_outputs() {
    let discover = catalog()
        .iter()
        .find(|entry| entry.tool() == NativeTool::Discover)
        .unwrap();
    let schema = discover.spec().output_schema.as_ref().unwrap();
    let validator = jsonschema::draft202012::options().build(schema).unwrap();
    assert_eq!(schema["oneOf"].as_array().unwrap().len(), 2);
    assert_eq!(
        schema["oneOf"][1]["properties"]["data"]["$ref"],
        "#/components/schemas/RepositoryDiscoverMapOutput"
    );
    assert!(validator.is_valid(&serde_json::json!({
        "version": 1, "data": {"items": []}, "artifacts": [], "truncated": false
    })));
    assert!(!validator.is_valid(&serde_json::json!({
        "version": 1,
        "data": {"mode": "map", "semanticEvidenceAvailable": false},
        "artifacts": [],
        "truncated": false
    })));
}

#[test]
fn discover_implementation_digest_identifies_the_graph_map_capable_implementation() {
    let discover = catalog()
        .iter()
        .find(|descriptor| descriptor.tool() == NativeTool::Discover)
        .unwrap();
    let old = Digest::of(DigestAlgorithm::Blake3, b"kit-native-discover-1.0.0");
    let map_capable = Digest::of(
        DigestAlgorithm::Blake3,
        b"kit-native-discover-map-graph-history-blame-1.0.0",
    );
    assert_ne!(discover.identity().implementation_digest(), old);
    assert_eq!(discover.identity().implementation_digest(), map_capable);
    assert_eq!(discover.identity().version().as_str(), "1.0.0");
}

#[test]
fn native_search_and_edit_schemas_reject_cross_form_hybrids() {
    let schema = |tool| {
        &catalog()
            .iter()
            .find(|entry| entry.tool() == tool)
            .unwrap()
            .spec()
            .input_schema
    };
    let search = jsonschema::validator_for(schema(NativeTool::Search)).unwrap();
    let revision = format!("r:{}", "a".repeat(64));
    let lexical = serde_json::json!({
        "expected_revision": revision,
        "text": "needle",
        "mode": "content",
        "path_prefixes": [],
        "languages": []
    });
    assert!(search.is_valid(&lexical));
    assert!(!search.is_valid(&serde_json::json!({
        "expected_revision": revision,
        "text": "needle",
        "mode": "content",
        "rewrite": "replacement",
        "path_prefixes": [],
        "languages": []
    })));
    assert!(!search.is_valid(&serde_json::json!({
        "expected_revision": revision,
        "text": "Some($A)",
        "mode": "structural",
        "cursor": {},
        "path_prefixes": [],
        "languages": ["rust"]
    })));

    let edit = jsonschema::validator_for(schema(NativeTool::Edit)).unwrap();
    let token = format!("kitsp1_{}", "b".repeat(64));
    assert!(edit.is_valid(&serde_json::json!({"preview_token": token})));
    assert!(edit.is_valid(&serde_json::json!({
        "version": 2,
        "operations": []
    })));
    assert!(edit.is_valid(&serde_json::json!({
        "version": 2,
        "operations": [{
            "op": "edit",
            "path": "src/lib.rs",
            "hunks": [{
                "context_before": ["fn main() {"],
                "old": ["    old();"],
                "new": ["    new();"],
                "context_after": ["}"]
            }]
        }, {
            "op": "add_file",
            "path": "new.txt",
            "content": "text\n",
            "executable": false
        }, {
            "op": "delete_file",
            "path": "gone.txt"
        }]
    })));
    // DR-0008: the v1 revision/digest/byte-range form is gone.
    assert!(!edit.is_valid(&serde_json::json!({
        "version": 1,
        "expected_revision": revision,
        "operations": []
    })));
    assert!(!edit.is_valid(&serde_json::json!({
        "version": 2,
        "expected_revision": revision,
        "operations": []
    })));
    // Hunk lines are single lines.
    assert!(!edit.is_valid(&serde_json::json!({
        "version": 2,
        "operations": [{
            "op": "edit",
            "path": "src/lib.rs",
            "hunks": [{
                "context_before": [],
                "old": ["a\nb"],
                "new": ["c"],
                "context_after": []
            }]
        }]
    })));
    assert!(!edit.is_valid(&serde_json::json!({"preview_token": "kitsp1_bad"})));
    assert!(!edit.is_valid(&serde_json::json!({})));
    assert!(!edit.is_valid(&serde_json::json!({
        "preview_token": token,
        "version": 2,
        "operations": []
    })));
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
fn native_graph_and_map_memory_envelope_uses_real_reservations() {
    const TOTAL_MEMORY: usize = 320 * 1024 * 1024;
    const MAP_MEMORY: usize = 64 * 1024 * 1024;
    const GRAPH_MEMORY: usize = TOTAL_MEMORY - MAP_MEMORY;

    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).unwrap();
    let parent = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "kit-native-memory-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ));
    let root = parent.join("workspace");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"native-memory\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn first() {}\n").unwrap();
    let workspace = ManagedWorkspace::open_with_options(
        &root,
        RevisionOptions {
            max_entries: 1_000,
            max_name_bytes: 1024 * 1024,
            max_bytes: 16 * 1024 * 1024,
            max_memory_bytes: 32 * 1024 * 1024,
            max_depth: 64,
            max_scan_time: Duration::from_secs(5),
            max_scan_attempts: 2,
            watcher_interval: Duration::from_millis(5),
            reconciliation_interval: Duration::from_secs(60),
            metadata_path: Some(parent.join("revision.state")),
        },
    )
    .unwrap();
    let revision = workspace.current_revision().unwrap().id();
    let index = MetadataIndex::build(&workspace, revision, &IndexOptions::default()).unwrap();
    let graph_options = GraphOptions {
        max_staging_bytes: GRAPH_MEMORY,
        max_cache_bytes: GraphOptions::default()
            .max_cache_bytes
            .min(GRAPH_MEMORY / 2),
        ..GraphOptions::default()
    };
    let mut provider = StructureGraphProvider::new();
    provider
        .refresh(&workspace, &index, &graph_options, &[], &[])
        .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn second() {}\n").unwrap();
    let revision = workspace.current_revision().unwrap().id();
    let index = MetadataIndex::build(&workspace, revision, &IndexOptions::default()).unwrap();
    provider
        .refresh(&workspace, &index, &graph_options, &[], &[])
        .unwrap();
    assert!(provider.metrics().peak_staging_bytes() <= GRAPH_MEMORY);

    let request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            packages: vec!["native-memory".to_owned()],
            relationships: vec![RelationshipKind::Contains],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    let graph = provider.graph().unwrap();
    let mut low = 1;
    let mut high = MAP_MEMORY;
    while low < high {
        let middle = low + (high - low) / 2;
        match build_repository_map_with_structure(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits {
                max_memory_bytes: middle,
                ..MapLimits::default()
            },
            None,
            Some(graph),
        ) {
            Ok(_) => high = middle,
            Err(MapError::BoundExceeded(MapBound::Memory)) => low = middle + 1,
            Err(error) => panic!("unexpected native map memory result: {error:?}"),
        }
    }
    assert!(matches!(
        build_repository_map_with_structure(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits {
                max_memory_bytes: low - 1,
                ..MapLimits::default()
            },
            None,
            Some(graph),
        ),
        Err(MapError::BoundExceeded(MapBound::Memory))
    ));
    let retained = graph
        .logical_bytes()
        .checked_add(provider.cache_usage().logical_bytes())
        .unwrap();
    assert!(retained + low <= TOTAL_MEMORY);
    drop(provider);
    drop(workspace);
    fs::remove_dir_all(parent).unwrap();
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
