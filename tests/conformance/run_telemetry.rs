use std::collections::{BTreeMap, BTreeSet};

use kit::{
    agent::{
        accounting::{
            CategoryCost, CostSource, CostTableSnapshot, MoneyMicros, TokenUsageCategory,
            UsageEnvelope,
        },
        context::{
            ContextBlock, ContextItem, ContextLayer, ContextPriority, ProjectionLimits, project,
        },
        prompt::{PromptInput, TaskContract, compile},
    },
    domain::{
        config::{Grant, LayerStack, RunConfigContext, RunConfigSnapshot},
        ids::{PrincipalId, ProjectId, RunId},
        secret::SecretLease,
    },
    telemetry::{
        otel::{CheckResult, RunError, RunOutcome},
        redact::{CaptureBoundary, CaptureRedactor},
        run_envelope::{
            CoreRunObservation, ProviderCacheObservation, ProviderCapture, ProviderModelDescriptor,
            RunCapture, RunEnvelope, RunTelemetryError, SummaryRetentionPolicy, first_divergence,
            sanitize_provider_capture,
        },
    },
};
use serde_json::{Value, json};

const CANARY: &str = "kit-provider-canary+/=42";
const PRIVATE_TEXT: &str = "private scratchpad: inspect every hidden branch";

fn config() -> RunConfigSnapshot {
    let grants = [
        Grant::ModelCall,
        Grant::WorkspaceRead,
        Grant::WorkspaceWrite,
        Grant::ProcessSpawn,
        Grant::NetworkEgress,
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    LayerStack::safe_defaults()
        .materialize(
            RunConfigContext {
                principal_id: PrincipalId::generate().unwrap(),
                project_id: ProjectId::generate().unwrap(),
                run_id: RunId::generate().unwrap(),
            },
            &grants,
        )
        .unwrap()
}

fn prompt(dynamic: &str) -> kit::agent::prompt::CompiledPrompt {
    compile(&PromptInput {
        task: TaskContract {
            goal: dynamic.to_owned(),
            ..TaskContract::default()
        },
        ..PromptInput::default()
    })
    .unwrap()
}

fn context(content: &str) -> kit::agent::context::ContextProjection {
    project(
        &[ContextBlock::new(
            ContextLayer::Task,
            ContextPriority::Requirement,
            format!("source:{content}"),
            "revision-1",
            "requested",
            vec![ContextItem::model(1, content)],
        )],
        Some(1_000),
        ProjectionLimits::default(),
    )
}

fn capture<'a>(
    prompt: &'a kit::agent::prompt::CompiledPrompt,
    context: &'a kit::agent::context::ContextProjection,
    config: &'a RunConfigSnapshot,
) -> RunCapture<'a> {
    RunCapture {
        prompt,
        previous_prompt: None,
        current_tokens: None,
        previous_tokens: None,
        context,
        accounting: None,
        provider_model: ProviderModelDescriptor::default(),
        effective_config: config,
        provider_cache: ProviderCacheObservation::default(),
        core: CoreRunObservation::default(),
        provider_summary: None,
        summary_retention: SummaryRetentionPolicy::Discard,
    }
}

#[test]
fn canonical_envelope_integrates_pinned_inputs_and_is_deterministic() {
    let prompt = prompt("ship telemetry");
    let context = context("evidence");
    let config = config();
    let redactor = CaptureRedactor::new(&[]);
    let mut input = capture(&prompt, &context, &config);
    input.provider_model = ProviderModelDescriptor {
        provider: Some("provider-a".into()),
        provider_version: Some("2026-07".into()),
        model: Some("model-b".into()),
        model_snapshot: Some("snapshot-c".into()),
        feature_version: Some("features-2".into()),
        settings: BTreeMap::from([
            ("max_tokens".into(), json!(128)),
            ("temperature".into(), json!(0)),
        ]),
    };
    input.core = CoreRunObservation {
        outcome: Some(RunOutcome::Succeeded),
        latency_ms: Some(75),
        checks: Some(vec![CheckResult {
            name: "cargo test".into(),
            outcome: Some("passed".into()),
            latency_ms: Some(30),
        }]),
        errors: Some(Vec::new()),
    };
    let envelope = RunEnvelope::capture(input, &redactor).unwrap();

    assert_eq!(envelope.prompt.template_version, prompt.template_version);
    assert_eq!(envelope.prompt.prompt_digest, prompt.full_digest);
    assert_eq!(envelope.prompt.stable_prefix_digest, prompt.stable_digest);
    assert_eq!(
        envelope.effective_config_digest,
        format!("sha256:{}", config.digest_hex())
    );
    assert_eq!(envelope.context.block_count, 1);
    assert_eq!(envelope.context.item_count, 1);
    assert_eq!(
        envelope.to_canonical_json().unwrap(),
        envelope.to_canonical_json().unwrap()
    );
    assert_eq!(envelope.digest().unwrap(), envelope.digest().unwrap());
    assert_eq!(envelope.provider_model.snapshot_digest.len(), 64);
}

#[test]
fn divergence_reports_the_exact_zero_based_byte_and_token() {
    let current = "cache-prefix-🦀-current".as_bytes();
    let previous = "cache-prefix-🦀-prior".as_bytes();
    let byte = current
        .iter()
        .zip(previous)
        .position(|(left, right)| left != right)
        .unwrap() as u64;
    let divergence = first_divergence(
        current,
        previous,
        Some(&[10, 11, 12, 99]),
        Some(&[10, 11, 13, 99]),
    );
    assert_eq!(divergence.byte, Some(byte));
    assert_eq!(divergence.token, Some(2));
    assert_eq!(first_divergence(b"same", b"same", None, None).byte, None);
    assert_eq!(
        first_divergence(b"same+", b"same", None, None).byte,
        Some(4)
    );

    let current_prompt = prompt("current");
    let prior = prompt("prior");
    let context = context("evidence");
    let config = config();
    let redactor = CaptureRedactor::new(&[]);
    let expected = first_divergence(
        &current_prompt.bytes,
        &prior.bytes,
        Some(&[1, 4]),
        Some(&[1, 5]),
    );
    let mut input = capture(&current_prompt, &context, &config);
    input.previous_prompt = Some(&prior.bytes);
    input.current_tokens = Some(&[1, 4]);
    input.previous_tokens = Some(&[1, 5]);
    let envelope = RunEnvelope::capture(input, &redactor).unwrap();
    assert_eq!(envelope.prompt.first_divergence_byte, expected.byte);
    assert_eq!(envelope.prompt.first_divergence_token, Some(1));
}

#[test]
fn unavailable_provider_values_are_explicit_nulls() {
    let prompt = prompt("nulls");
    let context = context("evidence");
    let config = config();
    let redactor = CaptureRedactor::new(&[]);
    let value = serde_json::to_value(
        RunEnvelope::capture(capture(&prompt, &context, &config), &redactor).unwrap(),
    )
    .unwrap();

    for path in [
        "/provider_model/provider",
        "/provider_model/provider_version",
        "/provider_model/model",
        "/provider_model/model_snapshot",
        "/provider_model/feature_version",
        "/prompt/first_divergence_byte",
        "/prompt/first_divergence_token",
        "/cache/uncached_input_tokens",
        "/cache/write_tokens",
        "/cache/read_tokens",
        "/cache/provider_cache_key",
        "/cache/provider_residency",
        "/cache/time_to_first_token_ms",
        "/cache/prefill_duration_ms",
        "/outcome",
        "/latency_ms",
        "/usage",
        "/cost",
        "/checks",
        "/errors",
        "/provider_summary",
    ] {
        assert!(
            value.pointer(path).unwrap().is_null(),
            "{path} was not null"
        );
    }

    let usage = UsageEnvelope::default();
    let mut input = capture(&prompt, &context, &config);
    input.accounting = Some(&usage);
    let partial = serde_json::to_value(RunEnvelope::capture(input, &redactor).unwrap()).unwrap();
    assert!(partial["cost"].is_null());
}

#[test]
fn provider_cache_and_accounting_reconcile_without_inventing_values() {
    let prompt = prompt("accounting");
    let context = context("evidence");
    let config = config();
    let redactor = CaptureRedactor::new(&[]);
    let mut usage = UsageEnvelope::default();
    usage.categories.uncached_input = tokens(1, Some(20), Some(20), Some(40));
    usage.categories.cache_write = tokens(1, Some(3), Some(3), Some(6));
    usage.categories.cache_read = tokens(1, Some(7), Some(7), Some(7));
    usage.categories.visible_output = tokens(1, Some(5), Some(5), Some(15));
    usage.provider_cost = Some(CategoryCost {
        amount: MoneyMicros::new("USD", 68).unwrap(),
        source: CostSource::ProviderReported,
    });
    usage.provider_cost_samples = 1;

    let mut input = capture(&prompt, &context, &config);
    input.accounting = Some(&usage);
    input.provider_cache = ProviderCacheObservation::default()
        .with_exposed_cache_key("cache-1")
        .with_exposed_residency(true)
        .with_exposed_write_tokens(3)
        .with_exposed_read_tokens(7)
        .with_retention_policy("short")
        .with_exposed_time_to_first_token_ms(12)
        .with_exposed_prefill_duration_ms(9);
    let envelope = RunEnvelope::capture(input, &redactor).unwrap();
    assert_eq!(envelope.usage, Some(usage.clone()));
    assert_eq!(envelope.cache.uncached_input_tokens, Some(20));
    assert_eq!(envelope.cache.write_tokens, Some(3));
    assert_eq!(envelope.cache.read_tokens, Some(7));
    assert_eq!(envelope.cost.unwrap().effective.unwrap().micros, 68);

    let mut mismatch = capture(&prompt, &context, &config);
    mismatch.accounting = Some(&usage);
    mismatch.provider_cache = ProviderCacheObservation::default().with_exposed_read_tokens(8);
    assert!(matches!(
        RunEnvelope::capture(mismatch, &redactor),
        Err(RunTelemetryError::CacheAccountingMismatch {
            category: "read",
            ..
        })
    ));
}

fn tokens(
    samples: u64,
    logical_tokens: Option<u64>,
    billed_tokens: Option<u64>,
    cost: Option<u64>,
) -> TokenUsageCategory {
    TokenUsageCategory {
        samples,
        logical_tokens,
        billed_tokens,
        cost: cost.map(|micros| CategoryCost {
            amount: MoneyMicros::new("USD", micros).unwrap(),
            source: CostSource::CostTable {
                version: "1".into(),
                snapshot: "rates-1".into(),
            },
        }),
    }
}

#[test]
fn provider_canaries_are_absent_on_every_capture_boundary() {
    let secrets = [SecretLease::new(CANARY.as_bytes().to_vec())];
    let redactor = CaptureRedactor::new(&secrets);
    let headers = BTreeMap::from([
        ("authorization".into(), format!("Bearer {CANARY}")),
        ("x-request-id".into(), format!("request-{CANARY}")),
    ]);
    let errors = vec![format!("provider failed with {CANARY}")];
    let streamed_chunks = vec![format!("visible chunk {CANARY}")];
    for boundary in [
        CaptureBoundary::Event,
        CaptureBoundary::Artifact,
        CaptureBoundary::Log,
        CaptureBoundary::Trace,
    ] {
        let sanitized = sanitize_provider_capture(
            ProviderCapture {
                headers: &headers,
                errors: &errors,
                streamed_chunks: &streamed_chunks,
            },
            boundary,
            &redactor,
        );
        let corpus = serde_json::to_string(&sanitized).unwrap();
        assert_eq!(corpus.matches(CANARY).count(), 0, "{boundary:?}: {corpus}");
    }

    let prompt = prompt(CANARY);
    let context = context(CANARY);
    let config = config();
    let mut input = capture(&prompt, &context, &config);
    input.provider_model.provider = Some(format!("provider-{CANARY}"));
    input.provider_cache =
        ProviderCacheObservation::default().with_exposed_cache_key(format!("cache-{CANARY}"));
    let usage = UsageEnvelope {
        cost_table: Some(CostTableSnapshot {
            version: format!("version-{CANARY}"),
            provider: format!("provider-{CANARY}"),
            model: format!("model-{CANARY}"),
            snapshot: format!("snapshot-{CANARY}"),
            currency: "USD".into(),
        }),
        ..UsageEnvelope::default()
    };
    input.accounting = Some(&usage);
    input.core.errors = Some(vec![RunError {
        class: Some("model".into()),
        code: Some("provider_error".into()),
        message: Some(format!("failed: {CANARY}")),
    }]);
    input.provider_summary = Some(CANARY);
    input.summary_retention = SummaryRetentionPolicy::RetainRedacted;
    let corpus = String::from_utf8(
        RunEnvelope::capture(input, &redactor)
            .unwrap()
            .to_canonical_json()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(corpus.matches(CANARY).count(), 0, "{corpus}");
}

#[test]
fn private_reasoning_has_no_schema_path_and_summary_requires_retention() {
    let prompt = prompt("summary policy");
    let context = context("evidence");
    let config = config();
    let redactor = CaptureRedactor::new(&[]);

    let mut discarded = capture(&prompt, &context, &config);
    discarded.provider_summary = Some("concise provider summary");
    let discarded = RunEnvelope::capture(discarded, &redactor).unwrap();
    assert_eq!(discarded.provider_summary, None);

    let mut retained = capture(&prompt, &context, &config);
    retained.provider_summary = Some("concise provider summary");
    retained.summary_retention = SummaryRetentionPolicy::RetainRedacted;
    let value = serde_json::to_value(RunEnvelope::capture(retained, &redactor).unwrap()).unwrap();
    assert_eq!(value["provider_summary"], "concise provider summary");

    let corpus = serde_json::to_string(&value).unwrap().to_ascii_lowercase();
    assert!(!corpus.contains(&PRIVATE_TEXT.to_ascii_lowercase()));
    scan_forbidden_reasoning_fields(&value);
}

fn scan_forbidden_reasoning_fields(value: &Value) {
    match value {
        Value::Object(fields) => {
            for (name, value) in fields {
                assert!(
                    ![
                        "chain_of_thought",
                        "hidden_reasoning",
                        "reasoning_content",
                        "private_scratchpad",
                        "thinking",
                    ]
                    .contains(&name.as_str()),
                    "forbidden telemetry field {name}"
                );
                scan_forbidden_reasoning_fields(value);
            }
        }
        Value::Array(values) => values.iter().for_each(scan_forbidden_reasoning_fields),
        _ => {}
    }
}
