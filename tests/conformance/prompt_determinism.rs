#[path = "../../src/agent/prompt/mod.rs"]
mod prompt;

use std::collections::{BTreeMap, BTreeSet};

use prompt::{
    CompileError, DeletionDisposition, MODULE_COUNT, ModelVariant, ModuleKind, PromptInput,
    Stability, TaskContract, ToolDefinition, VariantEvaluation, compile,
};
use serde_json::json;

fn input(order: usize) -> PromptInput {
    let mut repository_instructions = BTreeMap::new();
    let mut evidence = BTreeMap::new();
    let mut continuation = BTreeMap::new();
    let mut budget = BTreeMap::new();
    for (key, value) in [
        ("z-last", "preserve public API"),
        ("a-first", "use Rust 2024"),
    ]
    .into_iter()
    .cycle()
    .skip(order % 2)
    .take(2)
    {
        repository_instructions.insert(key.into(), value.into());
    }
    for (key, value) in [("ev-b", "second"), ("ev-a", "first")]
        .into_iter()
        .cycle()
        .skip(order % 2)
        .take(2)
    {
        evidence.insert(key.into(), value.into());
    }
    for (key, value) in [("run_id", "run-dynamic-42"), ("cursor", "9")]
        .into_iter()
        .cycle()
        .skip(order % 2)
        .take(2)
    {
        continuation.insert(key.into(), value.into());
    }
    for (key, value) in [("tool_calls", 20), ("tokens", 8_000)]
        .into_iter()
        .cycle()
        .skip(order % 2)
        .take(2)
    {
        budget.insert(key.into(), value);
    }

    let mut tools = vec![
        ToolDefinition {
            name: "write".into(),
            description: "write a file".into(),
            input_schema: json!({"required": ["path"], "properties": {"text": "string", "path": "string"}}),
        },
        ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: json!({"properties": {"offset": "integer", "path": "string"}, "required": ["path"]}),
        },
    ];
    tools.rotate_left(order % 2);

    PromptInput {
        tools,
        repository_instructions,
        task: TaskContract {
            goal: "compile a deterministic prompt".into(),
            explicit_requirements: vec!["emit canonical bytes".into()],
            inferred_acceptance_criteria: vec!["same input has one digest".into()],
            scope: vec!["src/agent/prompt".into()],
            protected_areas: vec!["Cargo.toml".into()],
            available_verification: vec!["prompt_determinism".into()],
            risk_class: "low".into(),
            resource_budget: budget,
        },
        retrieved_evidence: evidence,
        continuation_state: continuation,
        model_variant: None,
        experiment: None,
    }
}

#[test]
fn equivalent_inputs_compile_to_one_digest() {
    let outputs = (0..1_000)
        .map(|iteration| compile(&input(iteration)).unwrap())
        .collect::<Vec<_>>();
    let digests = outputs
        .iter()
        .map(|output| output.full_digest.as_str())
        .collect::<BTreeSet<_>>();

    assert_eq!(digests.len(), 1);
    assert!(
        outputs
            .windows(2)
            .all(|pair| pair[0].bytes == pair[1].bytes)
    );
}

#[test]
fn stable_prefix_precedes_dynamic_values_and_reports_both_digests() {
    let output = compile(&input(0)).unwrap();
    let stable = std::str::from_utf8(&output.bytes[..output.first_dynamic_offset]).unwrap();
    let dynamic = std::str::from_utf8(&output.bytes[output.first_dynamic_offset..]).unwrap();

    assert_eq!(output.modules.len(), MODULE_COUNT);
    assert_eq!(output.modules[0].kind, ModuleKind::ImmutableSafetyAuthority);
    assert_eq!(output.modules[1].kind, ModuleKind::ConciseOperatingBehavior);
    assert_eq!(output.modules[2].kind, ModuleKind::CodingTestingQuality);
    assert_eq!(output.modules[3].kind, ModuleKind::ToolRouting);
    assert_eq!(output.modules[4].kind, ModuleKind::RepositoryInstructions);
    assert_eq!(
        output.modules[5].kind,
        ModuleKind::TaskRequirementsAcceptance
    );
    assert_eq!(
        output.modules[6].kind,
        ModuleKind::RetrievedEvidenceContinuation
    );
    assert!(
        output.modules[..5]
            .iter()
            .all(|module| module.stability == Stability::Stable)
    );
    assert!(
        output.modules[5..]
            .iter()
            .all(|module| module.stability == Stability::Dynamic)
    );
    assert!(!stable.contains("run_id"));
    assert!(!stable.contains("run-dynamic-42"));
    assert!(!stable.contains("timestamp"));
    assert!(dynamic.contains("run-dynamic-42"));
    assert_eq!(
        output.stable_digest,
        blake3::hash(stable.as_bytes()).to_hex().to_string()
    );
    assert_eq!(
        output.full_digest,
        blake3::hash(&output.bytes).to_hex().to_string()
    );
}

#[test]
fn contract_marks_inferences_and_policy_metadata_is_versioned() {
    let output = compile(&input(0)).unwrap();
    let text = output.text();

    assert!(text.contains(r#""inferred":true"#));
    assert!(text.contains(r#""protected_areas":["Cargo.toml"]"#));
    assert!(text.contains(r#""resource_budget":{"tokens":8000,"tool_calls":20}"#));
    assert!(
        output
            .modules
            .iter()
            .all(|module| !module.version.is_empty())
    );
    assert!(
        output
            .modules
            .iter()
            .all(|module| !module.rationale.source.is_empty())
    );
    assert!(
        output
            .modules
            .iter()
            .all(|module| !module.rationale.evidence.is_empty())
    );
    assert_eq!(
        output.modules[0].deletion_disposition,
        DeletionDisposition::ProtectedRequirement
    );
    assert!(
        output
            .deletion_policy
            .retention_rule
            .contains("value remains measurable")
    );
}

#[test]
fn model_variants_require_protected_policy_evidence() {
    let baseline = compile(&input(0)).unwrap();
    let mut variant_input = input(0);
    variant_input.model_variant = Some(ModelVariant {
        id: "measured-model".into(),
        version: "1".into(),
        additional_operating_rules: vec!["Prefer concise tool results.".into()],
        evaluation: VariantEvaluation {
            evidence_id: "".into(),
            security_not_weakened: true,
            authority_not_weakened: true,
            workspace_safety_not_weakened: true,
        },
    });

    assert!(matches!(
        compile(&variant_input),
        Err(CompileError::UnevaluatedModelVariant { .. })
    ));

    variant_input
        .model_variant
        .as_mut()
        .unwrap()
        .evaluation
        .evidence_id = "ablation-42".into();
    let variant = compile(&variant_input).unwrap();
    assert_eq!(baseline.modules[0].digest, variant.modules[0].digest);
}
