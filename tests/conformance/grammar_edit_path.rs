#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    collections::{BTreeSet, VecDeque},
    future::Future,
    pin::Pin,
};

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, ToolCallPart, TurnCancellation,
};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    SessionConfig, StructuredOutputCapability, TurnRequest,
};
use kit::{
    agent::adapters::grammar_edit::{
        EditOutputMode, EditPathTrace, GRAMMAR_EDIT_INTENT_METADATA, GRAMMAR_EDIT_OUTCOME_METADATA,
        GrammarEditContext, GrammarEditIntentEvidence, GrammarEditLimits, GrammarEditModelAdapter,
    },
    domain::{
        config::{
            ConfigLayer, GRAMMAR_EDIT_EXPERIMENT_VERSION, GrammarEditExperiment, Grant, LayerStack,
            RunConfigContext, UnsupportedGrammarEditPolicy,
        },
        ids::{PrincipalId, ProjectId, RunId},
    },
    workspace::edit::{
        ir::{EditLimits, RevisionToken},
        normalize::{ModelEditFormat, NormalizationContext, normalize_with_trace},
    },
};
use serde_json::json;

fn config(
    enabled: bool,
    policy: UnsupportedGrammarEditPolicy,
) -> kit::domain::config::RunConfigSnapshot {
    let authority = BTreeSet::from([Grant::ModelCall, Grant::WorkspaceWrite]);
    let mut run = ConfigLayer::empty();
    run.grammar_edit = Some(GrammarEditExperiment {
        version: GRAMMAR_EDIT_EXPERIMENT_VERSION,
        enabled,
        unsupported_provider: policy,
    });
    let mut layers = LayerStack::safe_defaults();
    layers.run = Some(run);
    layers
        .materialize(
            RunConfigContext {
                principal_id: PrincipalId::generate().unwrap(),
                project_id: ProjectId::generate().unwrap(),
                run_id: RunId::generate().unwrap(),
            },
            &authority,
        )
        .unwrap()
}

fn revision() -> RevisionToken {
    RevisionToken::parse(format!("r:{}", "1".repeat(64))).unwrap()
}

fn grammar_context() -> GrammarEditContext {
    let root = std::env::temp_dir().join(format!(
        "kit-grammar-context-{}",
        RunId::generate().unwrap()
    ));
    std::fs::create_dir_all(&root).unwrap();
    GrammarEditContext::open(root.canonicalize().unwrap(), EditLimits::default()).unwrap()
}

fn grammar_context_at(root: &std::path::Path) -> GrammarEditContext {
    GrammarEditContext::open(root, EditLimits::default()).unwrap()
}

fn structured_value() -> serde_json::Value {
    json!({
        "version": 1,
        "expected_revision": revision().to_string(),
        "operations": [{
            "op": "add_file",
            "path": "new.txt",
            "content": {"encoding": "utf8", "newline": "lf", "text": "new", "final_newline": true},
            "executable": false
        }]
    })
}

#[derive(Clone)]
struct FixtureAdapter {
    supported: bool,
    result: ModelTurnResult,
}

struct FixtureSession {
    capability: Option<StructuredOutputCapability>,
    result: Option<ModelTurnResult>,
}

struct FixtureTurn(VecDeque<ModelTurnEvent>);

impl ModelAdapter for FixtureAdapter {
    type Session = FixtureSession;

    fn start_session<'a, 'b>(
        &'a self,
        _config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'b>>
    where
        'a: 'b,
        Self: 'b,
    {
        Box::pin(async move {
            Ok(FixtureSession {
                capability: self.supported.then(|| {
                    StructuredOutputCapability::new("fixture.output-format.v1", true, 64 * 1024)
                        .unwrap()
                }),
                result: Some(self.result.clone()),
            })
        })
    }

    fn provider_name(&self) -> Option<&str> {
        Some("fixture")
    }
}

impl ModelSession for FixtureSession {
    type Turn = FixtureTurn;

    fn begin_turn<'a, 'b>(
        &'a mut self,
        request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'b>>
    where
        'a: 'b,
        Self: 'b,
    {
        let mut result = self.result.take().unwrap();
        if let Some(structured) = &request.structured_output {
            for part in result
                .output_items
                .iter_mut()
                .flat_map(|item| &mut item.parts)
            {
                if let Part::Structured(value) = part {
                    value.schema = Some(structured.schema().clone());
                    if let Some(expected) =
                        structured.schema()["properties"]["expected_revision"]["const"].as_str()
                    {
                        value.value["expected_revision"] = json!(expected);
                    }
                }
            }
            result.metadata.insert(
                "agentkit.structured_output".to_owned(),
                serde_json::to_value(agentkit_loop::StructuredOutputEvidence {
                    name: structured.name().to_owned(),
                    version: structured.version(),
                    strict: structured.strict(),
                    schema_digest: structured.schema_digest().to_owned(),
                    session_id: request.session_id.to_string(),
                    turn_id: request.turn_id.to_string(),
                    honored: true,
                    error: None,
                })
                .unwrap(),
            );
        }
        Box::pin(async move {
            Ok(FixtureTurn(VecDeque::from([ModelTurnEvent::Finished(
                result,
            )])))
        })
    }

    fn model_name(&self) -> Option<&str> {
        Some("fixture-model")
    }

    fn structured_output_capability(&self) -> Option<&StructuredOutputCapability> {
        self.capability.as_ref()
    }
}

impl ModelTurn for FixtureTurn {
    fn next_event<'a, 'b>(
        &'a mut self,
        _cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ModelTurnEvent>, LoopError>> + Send + 'b>>
    where
        'a: 'b,
        Self: 'b,
    {
        let event = self.0.pop_front();
        Box::pin(async move { Ok(event) })
    }
}

fn request() -> TurnRequest {
    TurnRequest {
        session_id: agentkit_core::SessionId::new("session"),
        turn_id: agentkit_core::TurnId::new("turn"),
        transcript: vec![Item::text(ItemKind::User, "edit")],
        available_tools: Vec::new(),
        cache: None,
        structured_output: None,
        metadata: MetadataMap::new(),
    }
}

fn result(part: Part, reason: FinishReason) -> ModelTurnResult {
    ModelTurnResult {
        finish_reason: reason,
        output_items: vec![Item::new(ItemKind::Assistant, vec![part])],
        usage: None,
        metadata: MetadataMap::new(),
        model: Some("fixture-model".to_owned()),
        response_id: Some("response".to_owned()),
    }
}

#[test]
fn experiment_defaults_off_and_release_activation_is_erased() {
    assert!(!GrammarEditExperiment::default().enabled);
    #[cfg(not(debug_assertions))]
    {
        let mut layers = LayerStack::safe_defaults();
        let mut run = ConfigLayer::empty();
        run.grammar_edit = Some(GrammarEditExperiment {
            version: GRAMMAR_EDIT_EXPERIMENT_VERSION,
            enabled: true,
            unsupported_provider: UnsupportedGrammarEditPolicy::Fail,
        });
        layers.run = Some(run);
        assert!(matches!(
            layers.materialize(
                RunConfigContext {
                    principal_id: PrincipalId::generate().unwrap(),
                    project_id: ProjectId::generate().unwrap(),
                    run_id: RunId::generate().unwrap(),
                },
                &BTreeSet::from([Grant::ModelCall])
            ),
            Err(kit::domain::config::ConfigError::GrammarEditReleaseDisabled)
        ));
    }
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn concrete_session_capability_selects_constraint_or_explicit_fallback() {
    let model = FixtureAdapter {
        supported: true,
        result: result(
            Part::structured(structured_value()),
            FinishReason::Completed,
        ),
    };
    let adapter = GrammarEditModelAdapter::new(
        model,
        config(true, UnsupportedGrammarEditPolicy::Fail),
        GrammarEditLimits::default(),
        Some(grammar_context()),
    );
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    let mut projected = request();
    session.prepare_turn(&mut projected).unwrap();
    let structured = projected.structured_output.as_ref().unwrap();
    assert_eq!(structured.name(), "kit.edit-ir-input");
    assert_eq!(structured.version(), 1);
    assert!(structured.strict());
    assert!(structured.schema_digest().starts_with("sha256:"));
    let evidence: GrammarEditIntentEvidence =
        serde_json::from_value(projected.metadata[GRAMMAR_EDIT_INTENT_METADATA].clone()).unwrap();
    assert_eq!(evidence.selected_mode, EditOutputMode::Constrained);
    assert_eq!(
        evidence.capability_version.as_deref(),
        Some("fixture.output-format.v1")
    );

    let unsupported = FixtureAdapter {
        supported: false,
        result: result(Part::text("ordinary"), FinishReason::Completed),
    };
    let adapter = GrammarEditModelAdapter::new(
        unsupported,
        config(true, UnsupportedGrammarEditPolicy::OrdinaryOutput),
        GrammarEditLimits::default(),
        Some(grammar_context()),
    );
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    let mut projected = request();
    session.prepare_turn(&mut projected).unwrap();
    assert!(projected.structured_output.is_none());
    let Part::Text(contract) = &projected.transcript[0].parts[0] else {
        panic!("ordinary edit contract is not text");
    };
    assert!(
        contract
            .text
            .contains("Return exactly one JSON edit object")
    );
    assert!(contract.text.contains("expected_revision"));
    assert!(contract.text.contains("additionalProperties"));
    let evidence: GrammarEditIntentEvidence =
        serde_json::from_value(projected.metadata[GRAMMAR_EDIT_INTENT_METADATA].clone()).unwrap();
    assert_eq!(
        evidence.fallback_reason.as_deref(),
        Some("unsupported_provider_model")
    );
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn terminal_result_is_privately_classified_and_evidenced() {
    let adapter = GrammarEditModelAdapter::new(
        FixtureAdapter {
            supported: true,
            result: result(
                Part::structured(structured_value()),
                FinishReason::Completed,
            ),
        },
        config(true, UnsupportedGrammarEditPolicy::Fail),
        GrammarEditLimits::default(),
        Some(grammar_context()),
    );
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    let mut request = request();
    session.prepare_turn(&mut request).unwrap();
    let mut turn = session.begin_turn(request, None).await.unwrap();
    let ModelTurnEvent::Finished(result) = turn.next_event(None).await.unwrap().unwrap() else {
        panic!("missing terminal result");
    };
    assert_eq!(
        result.metadata[GRAMMAR_EDIT_OUTCOME_METADATA]["honored"],
        true
    );
    assert_eq!(
        result.metadata[GRAMMAR_EDIT_OUTCOME_METADATA]["structured"],
        true
    );
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn refusal_partial_tool_mix_and_unhonored_constraints_are_exhaustive() {
    let cases = [
        (
            result(Part::structured(structured_value()), FinishReason::Blocked),
            "refusal",
        ),
        (
            result(
                Part::structured(structured_value()),
                FinishReason::MaxTokens,
            ),
            "partial_stream",
        ),
        (
            result(
                Part::ToolCall(ToolCallPart::new("call", "tool", json!({}))),
                FinishReason::ToolCall,
            ),
            "tool_mix",
        ),
        (
            result(Part::text("not structured"), FinishReason::Completed),
            "constraint_not_honored",
        ),
        (
            result(
                Part::structured(structured_value()),
                FinishReason::Cancelled,
            ),
            "cancelled",
        ),
        (
            result(Part::structured(structured_value()), FinishReason::Error),
            "provider_error",
        ),
        (
            result(
                Part::structured(structured_value()),
                FinishReason::Other("pause_turn".to_owned()),
            ),
            "other_finish",
        ),
    ];
    for (provider_result, expected) in cases {
        let adapter = GrammarEditModelAdapter::new(
            FixtureAdapter {
                supported: true,
                result: provider_result,
            },
            config(true, UnsupportedGrammarEditPolicy::Fail),
            GrammarEditLimits::default(),
            Some(grammar_context()),
        );
        let mut session = adapter
            .start_session(SessionConfig::new("session"))
            .await
            .unwrap();
        let mut request = request();
        session.prepare_turn(&mut request).unwrap();
        let mut turn = session.begin_turn(request, None).await.unwrap();
        let ModelTurnEvent::Finished(result) = turn.next_event(None).await.unwrap().unwrap() else {
            panic!("missing terminal result");
        };
        assert_eq!(
            result.metadata[GRAMMAR_EDIT_OUTCOME_METADATA]["result"],
            expected
        );
    }
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn workspace_revision_change_during_model_is_typed_rejection() {
    let root = std::env::temp_dir().join(format!(
        "kit-grammar-revision-change-{}",
        RunId::generate().unwrap()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let context = grammar_context_at(&root);
    let adapter = GrammarEditModelAdapter::new(
        FixtureAdapter {
            supported: true,
            result: result(
                Part::structured(structured_value()),
                FinishReason::Completed,
            ),
        },
        config(true, UnsupportedGrammarEditPolicy::Fail),
        GrammarEditLimits::default(),
        Some(context),
    );
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    let mut request = request();
    session.prepare_turn(&mut request).unwrap();
    let mut turn = session.begin_turn(request, None).await.unwrap();
    std::fs::write(root.join("concurrent.txt"), "changed").unwrap();
    let ModelTurnEvent::Finished(result) = turn.next_event(None).await.unwrap().unwrap() else {
        panic!("missing terminal result");
    };
    assert_eq!(
        result.metadata[GRAMMAR_EDIT_OUTCOME_METADATA]["result"],
        "revision_changed"
    );
}

#[test]
fn schema_and_semantic_decoder_cover_unicode_paths_ranges_and_all_variants() {
    let context = NormalizationContext::new(revision(), EditLimits::default());
    let digest = format!("blake3:{}", "a".repeat(64));
    let value = json!({
        "version": 1,
        "expected_revision": revision().to_string(),
        "operations": [
            {"op":"add_file","path":"unicodé.txt","content":{"encoding":"utf8","newline":"lf","text":"é\nx","final_newline":false},"executable":false},
            {"op":"delete_file","path":"gone.txt","base_digest":digest},
            {"op":"move_file","from":"from.txt","to":"to.txt","base_digest":digest},
            {"op":"replace_range","path":"edit.txt","base_digest":digest,"range":{"start":0,"end":0},"expected":{"encoding":"utf8","newline":"lf","text":"","final_newline":false},"replacement":{"encoding":"utf8","newline":"crlf","text":"x\ny","final_newline":true},"executable":"preserve"}
        ]
    });
    let bytes = serde_json::to_vec(&value).unwrap();
    normalize_with_trace(ModelEditFormat::StructuredJson, &bytes, &context, &mut ()).unwrap();

    for path in ["a\r.txt", "NUL", "../x", "a\\b", "a\n.txt"] {
        let mut invalid = structured_value();
        invalid["operations"][0]["path"] = json!(path);
        assert!(
            normalize_with_trace(
                ModelEditFormat::StructuredJson,
                &serde_json::to_vec(&invalid).unwrap(),
                &context,
                &mut ()
            )
            .is_err()
        );
    }

    let tiny = EditLimits {
        max_content_bytes: 1,
        ..EditLimits::default()
    };
    let tiny_context = NormalizationContext::new(revision(), tiny);
    let mut unicode = structured_value();
    unicode["operations"][0]["content"]["text"] = json!("é");
    assert!(
        normalize_with_trace(
            ModelEditFormat::StructuredJson,
            &serde_json::to_vec(&unicode).unwrap(),
            &tiny_context,
            &mut ()
        )
        .is_err()
    );

    let escaped = serde_json::to_string(&unicode)
        .unwrap()
        .replace('é', "\\u00e9");
    assert!(
        normalize_with_trace(
            ModelEditFormat::StructuredJson,
            escaped.as_bytes(),
            &tiny_context,
            &mut ()
        )
        .is_err()
    );

    let three_operations = EditLimits {
        max_operations: 3,
        ..EditLimits::default()
    };
    assert!(
        normalize_with_trace(
            ModelEditFormat::StructuredJson,
            &bytes,
            &NormalizationContext::new(revision(), three_operations),
            &mut ()
        )
        .is_err()
    );

    let mut negative = structured_value();
    negative["operations"][0] = json!({
        "op":"replace_range",
        "path":"edit.txt",
        "base_digest":format!("blake3:{}", "a".repeat(64)),
        "range":{"start":-1,"end":0},
        "expected":{"encoding":"utf8","newline":"lf","text":"","final_newline":false},
        "replacement":{"encoding":"utf8","newline":"lf","text":"","final_newline":false},
        "executable":"preserve"
    });
    assert!(
        normalize_with_trace(
            ModelEditFormat::StructuredJson,
            &serde_json::to_vec(&negative).unwrap(),
            &context,
            &mut ()
        )
        .is_err()
    );
}

#[test]
fn semantically_equal_ordinary_and_constrained_inputs_share_normalization_trace() {
    let context = NormalizationContext::new(revision(), EditLimits::default());
    let structured = serde_json::to_vec(&structured_value()).unwrap();
    let diff = b"--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+new\n";
    let mut constrained_trace = EditPathTrace::default();
    let constrained = normalize_with_trace(
        ModelEditFormat::StructuredJson,
        &structured,
        &context,
        &mut constrained_trace,
    )
    .unwrap();
    let mut ordinary_trace = EditPathTrace::default();
    let ordinary = normalize_with_trace(
        ModelEditFormat::UnifiedDiff,
        diff,
        &context,
        &mut ordinary_trace,
    )
    .unwrap();
    assert_eq!(constrained, ordinary);
    assert_eq!(constrained_trace, ordinary_trace);
}
