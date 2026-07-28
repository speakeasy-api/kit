use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::{
    agent::{
        accounting::{
            CategoryCost, CostSource, MoneyMicros, USAGE_ENVELOPE_VERSION, UsageEnvelope,
        },
        context::{ContextLayer, ContextPriority, ContextProjection},
        prompt::CompiledPrompt,
    },
    domain::config::RunConfigSnapshot,
    telemetry::{
        otel::{CheckResult, RunError, RunOutcome},
        redact::{CaptureBoundary, CaptureRedactor},
    },
};

pub const RUN_ENVELOPE_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryRetentionPolicy {
    #[default]
    Discard,
    RetainRedacted,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelDescriptor {
    pub provider: Option<String>,
    pub provider_version: Option<String>,
    pub model: Option<String>,
    pub model_snapshot: Option<String>,
    pub feature_version: Option<String>,
    pub settings: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelSnapshot {
    pub provider: Option<String>,
    pub provider_version: Option<String>,
    pub model: Option<String>,
    pub model_snapshot: Option<String>,
    pub feature_version: Option<String>,
    pub settings_digest: String,
    pub snapshot_digest: String,
}

impl ProviderModelSnapshot {
    fn capture(
        descriptor: ProviderModelDescriptor,
        redactor: &CaptureRedactor<'_>,
    ) -> Result<Self, RunTelemetryError> {
        let settings_digest = digest(&canonical_bytes(&descriptor.settings)?);
        let mut snapshot = Self {
            provider: redact_optional(descriptor.provider, redactor),
            provider_version: redact_optional(descriptor.provider_version, redactor),
            model: redact_optional(descriptor.model, redactor),
            model_snapshot: redact_optional(descriptor.model_snapshot, redactor),
            feature_version: redact_optional(descriptor.feature_version, redactor),
            settings_digest,
            snapshot_digest: String::new(),
        };
        snapshot.snapshot_digest = snapshot.expected_digest()?;
        Ok(snapshot)
    }

    fn expected_digest(&self) -> Result<String, RunTelemetryError> {
        Ok(digest(&canonical_bytes(&json!({
            "feature_version": self.feature_version,
            "model": self.model,
            "model_snapshot": self.model_snapshot,
            "provider": self.provider,
            "provider_version": self.provider_version,
            "settings_digest": self.settings_digest,
        }))?))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderCacheObservation {
    cache_key: Option<String>,
    residency: Option<bool>,
    write_tokens: Option<u64>,
    read_tokens: Option<u64>,
    retention_policy: Option<String>,
    time_to_first_token_ms: Option<u64>,
    prefill_duration_ms: Option<u64>,
}

impl ProviderCacheObservation {
    pub fn with_exposed_cache_key(mut self, value: impl Into<String>) -> Self {
        self.cache_key = Some(value.into());
        self
    }

    pub fn with_exposed_residency(mut self, value: bool) -> Self {
        self.residency = Some(value);
        self
    }

    pub fn with_exposed_write_tokens(mut self, value: u64) -> Self {
        self.write_tokens = Some(value);
        self
    }

    pub fn with_exposed_read_tokens(mut self, value: u64) -> Self {
        self.read_tokens = Some(value);
        self
    }

    pub fn with_retention_policy(mut self, value: impl Into<String>) -> Self {
        self.retention_policy = Some(value.into());
        self
    }

    pub fn with_exposed_time_to_first_token_ms(mut self, value: u64) -> Self {
        self.time_to_first_token_ms = Some(value);
        self
    }

    pub fn with_exposed_prefill_duration_ms(mut self, value: u64) -> Self {
        self.prefill_duration_ms = Some(value);
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSnapshot {
    pub template_version: String,
    pub prompt_digest: String,
    pub stable_prefix_digest: String,
    pub first_dynamic_byte: u64,
    pub first_divergence_byte: Option<u64>,
    pub first_divergence_token: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrefixDivergence {
    pub byte: Option<u64>,
    pub token: Option<u64>,
}

pub fn first_divergence(
    current: &[u8],
    previous: &[u8],
    current_tokens: Option<&[u32]>,
    previous_tokens: Option<&[u32]>,
) -> PrefixDivergence {
    PrefixDivergence {
        byte: differing_index(current, previous),
        token: current_tokens
            .zip(previous_tokens)
            .and_then(|(current, previous)| differing_index(current, previous)),
    }
}

fn differing_index<T: Eq>(current: &[T], previous: &[T]) -> Option<u64> {
    current
        .iter()
        .zip(previous)
        .position(|(current, previous)| current != previous)
        .or_else(|| (current.len() != previous.len()).then_some(current.len().min(previous.len())))
        .map(|index| u64::try_from(index).expect("slice indexes fit in u64"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSnapshot {
    pub projection_digest: String,
    pub block_count: u64,
    pub item_count: u64,
    pub token_budget: u64,
    pub estimated_tokens: u64,
    pub used_fallback_budget: bool,
}

impl ContextSnapshot {
    fn capture(projection: &ContextProjection) -> Result<Self, RunTelemetryError> {
        let blocks = projection
            .blocks
            .iter()
            .map(|block| {
                json!({
                    "items": block.items.iter().map(|item| json!({
                        "artifact_handle": item.artifact_handle,
                        "preview": item.preview,
                        "sequence": item.sequence,
                    })).collect::<Vec<_>>(),
                    "layer": context_layer(block.layer),
                    "priority": context_priority(block.priority),
                    "provenance": {
                        "estimated_tokens": block.provenance.estimated_tokens,
                        "relevance_score": block.provenance.relevance_score,
                        "retrieval_reason": block.provenance.retrieval_reason,
                        "revision": block.provenance.revision,
                        "source_handle": block.provenance.source_handle,
                    },
                })
            })
            .collect::<Vec<_>>();
        let item_count = projection.blocks.iter().try_fold(0_u64, |total, block| {
            total
                .checked_add(
                    u64::try_from(block.items.len())
                        .map_err(|_| RunTelemetryError::Overflow("context item count"))?,
                )
                .ok_or(RunTelemetryError::Overflow("context item count"))
        })?;
        Ok(Self {
            projection_digest: digest(&canonical_bytes(&blocks)?),
            block_count: u64::try_from(projection.blocks.len())
                .map_err(|_| RunTelemetryError::Overflow("context block count"))?,
            item_count,
            token_budget: u64::try_from(projection.token_budget)
                .map_err(|_| RunTelemetryError::Overflow("context token budget"))?,
            estimated_tokens: u64::try_from(projection.estimated_tokens)
                .map_err(|_| RunTelemetryError::Overflow("context estimated tokens"))?,
            used_fallback_budget: projection.used_fallback_budget,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheSnapshot {
    pub uncached_input_tokens: Option<u64>,
    pub uncached_input_tokens_estimate: Option<u64>,
    pub write_tokens: Option<u64>,
    pub write_tokens_estimate: Option<u64>,
    pub read_tokens: Option<u64>,
    pub read_tokens_estimate: Option<u64>,
    pub provider_cache_key: Option<String>,
    pub provider_residency: Option<bool>,
    pub retention_policy: Option<String>,
    pub time_to_first_token_ms: Option<u64>,
    pub prefill_duration_ms: Option<u64>,
}

impl CacheSnapshot {
    fn capture(
        usage: Option<&UsageEnvelope>,
        observation: ProviderCacheObservation,
        redactor: &CaptureRedactor<'_>,
    ) -> Result<Self, RunTelemetryError> {
        let accounting_write = usage.and_then(|usage| usage.categories.cache_write.billed_tokens);
        let accounting_read = usage.and_then(|usage| usage.categories.cache_read.billed_tokens);
        reconcile_cache("write", accounting_write, observation.write_tokens)?;
        reconcile_cache("read", accounting_read, observation.read_tokens)?;
        Ok(Self {
            uncached_input_tokens: usage
                .and_then(|usage| usage.categories.uncached_input.billed_tokens),
            uncached_input_tokens_estimate: usage
                .and_then(|usage| usage.categories.uncached_input.logical_tokens),
            write_tokens: observation.write_tokens.or(accounting_write),
            write_tokens_estimate: usage
                .and_then(|usage| usage.categories.cache_write.logical_tokens),
            read_tokens: observation.read_tokens.or(accounting_read),
            read_tokens_estimate: usage
                .and_then(|usage| usage.categories.cache_read.logical_tokens),
            provider_cache_key: redact_optional(observation.cache_key, redactor),
            provider_residency: observation.residency,
            retention_policy: redact_optional(observation.retention_policy, redactor),
            time_to_first_token_ms: observation.time_to_first_token_ms,
            prefill_duration_ms: observation.prefill_duration_ms,
        })
    }
}

fn reconcile_cache(
    category: &'static str,
    accounting: Option<u64>,
    provider: Option<u64>,
) -> Result<(), RunTelemetryError> {
    if let (Some(accounting), Some(provider)) = (accounting, provider)
        && accounting != provider
    {
        return Err(RunTelemetryError::CacheAccountingMismatch {
            category,
            accounting,
            provider,
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    ProviderReported,
    CategoryTotal,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CostSnapshot {
    pub effective: Option<MoneyMicros>,
    pub basis: Option<CostBasis>,
    pub provider_reported: Option<MoneyMicros>,
    pub category_total: Option<MoneyMicros>,
}

impl CostSnapshot {
    fn capture(usage: Option<&UsageEnvelope>) -> Result<Option<Self>, RunTelemetryError> {
        let Some(usage) = usage else {
            return Ok(None);
        };
        let provider_reported = usage.provider_cost.as_ref().map(|cost| cost.amount.clone());
        let category_total = category_total(usage)?;
        let (effective, basis) = if let Some(cost) = &provider_reported {
            (Some(cost.clone()), Some(CostBasis::ProviderReported))
        } else if let Some(cost) = &category_total {
            (Some(cost.clone()), Some(CostBasis::CategoryTotal))
        } else {
            (None, None)
        };
        if effective.is_none() {
            return Ok(None);
        }
        Ok(Some(Self {
            effective,
            basis,
            provider_reported,
            category_total,
        }))
    }
}

fn category_total(usage: &UsageEnvelope) -> Result<Option<MoneyMicros>, RunTelemetryError> {
    let categories = &usage.categories;
    let sampled = [
        (
            categories.uncached_input.samples,
            categories.uncached_input.cost.as_ref(),
        ),
        (
            categories.cache_write.samples,
            categories.cache_write.cost.as_ref(),
        ),
        (
            categories.cache_read.samples,
            categories.cache_read.cost.as_ref(),
        ),
        (
            categories.visible_output.samples,
            categories.visible_output.cost.as_ref(),
        ),
        (
            categories.reasoning.samples,
            categories.reasoning.cost.as_ref(),
        ),
        (categories.tool.samples, categories.tool.cost.as_ref()),
        (categories.compute.samples, categories.compute.cost.as_ref()),
        (
            categories.failed_speculation.samples,
            categories.failed_speculation.cost.as_ref(),
        ),
    ];
    let mut total: Option<MoneyMicros> = None;
    for (_, cost) in sampled.iter().filter(|(samples, _)| *samples != 0) {
        let Some(cost) = cost else {
            return Ok(None);
        };
        total = Some(match total {
            None => cost.amount.clone(),
            Some(total) => add_money(&total, &cost.amount)?,
        });
    }
    Ok(total)
}

fn add_money(left: &MoneyMicros, right: &MoneyMicros) -> Result<MoneyMicros, RunTelemetryError> {
    if left.currency != right.currency {
        return Err(RunTelemetryError::CostCurrencyMismatch {
            left: left.currency.clone(),
            right: right.currency.clone(),
        });
    }
    Ok(MoneyMicros {
        currency: left.currency.clone(),
        micros: left
            .micros
            .checked_add(right.micros)
            .ok_or(RunTelemetryError::Overflow("category cost"))?,
    })
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreRunObservation {
    pub outcome: Option<RunOutcome>,
    pub latency_ms: Option<u64>,
    pub checks: Option<Vec<CheckResult>>,
    pub errors: Option<Vec<RunError>>,
}

pub struct RunCapture<'a> {
    pub prompt: &'a CompiledPrompt,
    pub previous_prompt: Option<&'a [u8]>,
    pub current_tokens: Option<&'a [u32]>,
    pub previous_tokens: Option<&'a [u32]>,
    pub context: &'a ContextProjection,
    pub accounting: Option<&'a UsageEnvelope>,
    pub provider_model: ProviderModelDescriptor,
    pub effective_config: &'a RunConfigSnapshot,
    pub provider_cache: ProviderCacheObservation,
    pub core: CoreRunObservation,
    pub provider_summary: Option<&'a str>,
    pub summary_retention: SummaryRetentionPolicy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunEnvelope {
    pub schema_version: u16,
    pub prompt: PromptSnapshot,
    pub context: ContextSnapshot,
    pub provider_model: ProviderModelSnapshot,
    pub effective_config_digest: String,
    pub cache: CacheSnapshot,
    pub outcome: Option<RunOutcome>,
    pub latency_ms: Option<u64>,
    pub usage: Option<UsageEnvelope>,
    pub cost: Option<CostSnapshot>,
    pub checks: Option<Vec<CheckResult>>,
    pub errors: Option<Vec<RunError>>,
    pub summary_retention: SummaryRetentionPolicy,
    pub provider_summary: Option<String>,
}

impl RunEnvelope {
    pub fn capture(
        capture: RunCapture<'_>,
        redactor: &CaptureRedactor<'_>,
    ) -> Result<Self, RunTelemetryError> {
        let divergence =
            capture
                .previous_prompt
                .map_or_else(PrefixDivergence::default, |previous| {
                    first_divergence(
                        &capture.prompt.bytes,
                        previous,
                        capture.current_tokens,
                        capture.previous_tokens,
                    )
                });
        let mut checks = capture.core.checks;
        if let Some(checks) = &mut checks {
            for check in checks {
                check.name = redactor.redact_text(CaptureBoundary::Trace, &check.name);
                check.outcome = check
                    .outcome
                    .take()
                    .map(|value| redactor.redact_text(CaptureBoundary::Trace, &value));
            }
        }
        let mut errors = capture.core.errors;
        if let Some(errors) = &mut errors {
            for error in errors {
                error.class = redact_optional(error.class.take(), redactor);
                error.code = redact_optional(error.code.take(), redactor);
                error.message = redact_optional(error.message.take(), redactor);
            }
        }
        let provider_summary = match capture.summary_retention {
            SummaryRetentionPolicy::Discard => None,
            SummaryRetentionPolicy::RetainRedacted => capture
                .provider_summary
                .filter(|summary| !summary.is_empty())
                .map(|summary| redactor.redact_text(CaptureBoundary::Trace, summary)),
        };
        let usage = capture
            .accounting
            .cloned()
            .map(|usage| sanitize_usage(usage, redactor));
        let envelope = Self {
            schema_version: RUN_ENVELOPE_SCHEMA_VERSION,
            prompt: PromptSnapshot {
                template_version: capture.prompt.template_version.to_owned(),
                prompt_digest: capture.prompt.full_digest.clone(),
                stable_prefix_digest: capture.prompt.stable_digest.clone(),
                first_dynamic_byte: u64::try_from(capture.prompt.first_dynamic_offset)
                    .map_err(|_| RunTelemetryError::Overflow("prompt dynamic offset"))?,
                first_divergence_byte: divergence.byte,
                first_divergence_token: divergence.token,
            },
            context: ContextSnapshot::capture(capture.context)?,
            provider_model: ProviderModelSnapshot::capture(capture.provider_model, redactor)?,
            effective_config_digest: format!("sha256:{}", capture.effective_config.digest_hex()),
            cache: CacheSnapshot::capture(usage.as_ref(), capture.provider_cache, redactor)?,
            outcome: capture.core.outcome,
            latency_ms: capture.core.latency_ms,
            cost: CostSnapshot::capture(usage.as_ref())?,
            usage,
            checks,
            errors,
            summary_retention: capture.summary_retention,
            provider_summary,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    pub fn validate(&self) -> Result<(), RunTelemetryError> {
        if self.schema_version != RUN_ENVELOPE_SCHEMA_VERSION {
            return Err(RunTelemetryError::InvalidEnvelope(
                "unsupported schema version",
            ));
        }
        for digest in [
            &self.prompt.prompt_digest,
            &self.prompt.stable_prefix_digest,
            &self.context.projection_digest,
            &self.provider_model.settings_digest,
            &self.provider_model.snapshot_digest,
        ] {
            if !valid_digest(digest, false) {
                return Err(RunTelemetryError::InvalidEnvelope("invalid digest"));
            }
        }
        if !valid_digest(&self.effective_config_digest, true) {
            return Err(RunTelemetryError::InvalidEnvelope(
                "invalid effective config digest",
            ));
        }
        if self.provider_model.snapshot_digest != self.provider_model.expected_digest()? {
            return Err(RunTelemetryError::InvalidEnvelope(
                "invalid provider/model snapshot digest",
            ));
        }
        if self
            .usage
            .as_ref()
            .is_some_and(|usage| usage.schema_version != USAGE_ENVELOPE_VERSION)
        {
            return Err(RunTelemetryError::InvalidEnvelope(
                "unsupported usage schema version",
            ));
        }
        if self.summary_retention == SummaryRetentionPolicy::Discard
            && self.provider_summary.is_some()
        {
            return Err(RunTelemetryError::InvalidEnvelope(
                "provider summary lacks retention policy",
            ));
        }
        let expected_cost = CostSnapshot::capture(self.usage.as_ref())?;
        if self.cost != expected_cost {
            return Err(RunTelemetryError::InvalidEnvelope(
                "cost does not reconcile with usage",
            ));
        }
        reconcile_cache(
            "write",
            self.usage
                .as_ref()
                .and_then(|usage| usage.categories.cache_write.billed_tokens),
            self.cache.write_tokens,
        )?;
        reconcile_cache(
            "read",
            self.usage
                .as_ref()
                .and_then(|usage| usage.categories.cache_read.billed_tokens),
            self.cache.read_tokens,
        )?;
        Ok(())
    }

    pub fn to_canonical_json(&self) -> Result<Vec<u8>, RunTelemetryError> {
        self.validate()?;
        canonical_bytes(self)
    }

    pub fn digest(&self) -> Result<String, RunTelemetryError> {
        Ok(digest(&self.to_canonical_json()?))
    }
}

pub struct ProviderCapture<'a> {
    pub headers: &'a BTreeMap<String, String>,
    pub errors: &'a [String],
    pub streamed_chunks: &'a [String],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SanitizedProviderCapture {
    pub headers: BTreeMap<String, String>,
    pub errors: Vec<String>,
    pub streamed_chunks: Vec<String>,
}

pub fn sanitize_provider_capture(
    capture: ProviderCapture<'_>,
    boundary: CaptureBoundary,
    redactor: &CaptureRedactor<'_>,
) -> SanitizedProviderCapture {
    SanitizedProviderCapture {
        headers: capture
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    redactor.redact_text(boundary, name),
                    redactor.redact_header(boundary, name, value),
                )
            })
            .collect(),
        errors: capture
            .errors
            .iter()
            .map(|value| redactor.redact_text(boundary, value))
            .collect(),
        streamed_chunks: capture
            .streamed_chunks
            .iter()
            .map(|value| redactor.redact_text(boundary, value))
            .collect(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunTelemetryError {
    Serialization(String),
    Overflow(&'static str),
    CacheAccountingMismatch {
        category: &'static str,
        accounting: u64,
        provider: u64,
    },
    CostCurrencyMismatch {
        left: String,
        right: String,
    },
    InvalidEnvelope(&'static str),
}

impl fmt::Display for RunTelemetryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialization(error) => write!(formatter, "run telemetry serialization: {error}"),
            Self::Overflow(field) => write!(formatter, "run telemetry {field} overflowed"),
            Self::CacheAccountingMismatch {
                category,
                accounting,
                provider,
            } => write!(
                formatter,
                "provider cache {category} tokens {provider} do not reconcile with accounting {accounting}"
            ),
            Self::CostCurrencyMismatch { left, right } => {
                write!(
                    formatter,
                    "cannot reconcile run costs in {left} and {right}"
                )
            }
            Self::InvalidEnvelope(reason) => write!(formatter, "invalid run envelope: {reason}"),
        }
    }
}

impl std::error::Error for RunTelemetryError {}

fn redact_optional(value: Option<String>, redactor: &CaptureRedactor<'_>) -> Option<String> {
    value.map(|value| redactor.redact_text(CaptureBoundary::Trace, &value))
}

fn sanitize_usage(mut usage: UsageEnvelope, redactor: &CaptureRedactor<'_>) -> UsageEnvelope {
    for cost in [
        usage.categories.uncached_input.cost.as_mut(),
        usage.categories.cache_write.cost.as_mut(),
        usage.categories.cache_read.cost.as_mut(),
        usage.categories.visible_output.cost.as_mut(),
        usage.categories.reasoning.cost.as_mut(),
        usage.categories.tool.cost.as_mut(),
        usage.categories.compute.cost.as_mut(),
        usage.categories.failed_speculation.cost.as_mut(),
        usage.provider_cost.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        sanitize_cost(cost, redactor);
    }
    if let Some(table) = &mut usage.cost_table {
        for value in [
            &mut table.version,
            &mut table.provider,
            &mut table.model,
            &mut table.snapshot,
            &mut table.currency,
        ] {
            *value = redactor.redact_text(CaptureBoundary::Trace, value);
        }
    }
    usage
}

fn sanitize_cost(cost: &mut CategoryCost, redactor: &CaptureRedactor<'_>) {
    cost.amount.currency = redactor.redact_text(CaptureBoundary::Trace, &cost.amount.currency);
    if let CostSource::CostTable { version, snapshot } = &mut cost.source {
        *version = redactor.redact_text(CaptureBoundary::Trace, version);
        *snapshot = redactor.redact_text(CaptureBoundary::Trace, snapshot);
    }
}

fn context_layer(layer: ContextLayer) -> &'static str {
    match layer {
        ContextLayer::CanonicalPrompt => "canonical_prompt",
        ContextLayer::Repository => "repository",
        ContextLayer::Checkpoint => "checkpoint",
        ContextLayer::Task => "task",
        ContextLayer::RecentTranscript => "recent_transcript",
        ContextLayer::RetrievedEvidence => "retrieved_evidence",
        ContextLayer::ToolResultDelta => "tool_result_delta",
    }
}

fn context_priority(priority: ContextPriority) -> &'static str {
    match priority {
        ContextPriority::Requirement => "requirement",
        ContextPriority::ActiveFailure => "active_failure",
        ContextPriority::ChangedFile => "changed_file",
        ContextPriority::UnresolvedDecision => "unresolved_decision",
        ContextPriority::Current => "current",
        ContextPriority::OldRawToolOutput => "old_raw_tool_output",
    }
}

fn valid_digest(value: &str, prefixed: bool) -> bool {
    let value = if prefixed {
        let Some(value) = value.strip_prefix("sha256:") else {
            return false;
        };
        value
    } else {
        value
    };
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn digest(bytes: &[u8]) -> String {
    let bytes: [u8; 32] = Sha256::digest(bytes).into();
    let mut value = String::with_capacity(64);
    for byte in bytes {
        use fmt::Write as _;
        write!(value, "{byte:02x}").expect("writing to a string cannot fail");
    }
    value
}

fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>, RunTelemetryError> {
    let value = serde_json::to_value(value)
        .map_err(|error| RunTelemetryError::Serialization(error.to_string()))?;
    let mut output = Vec::new();
    write_canonical(&value, &mut output)?;
    Ok(output)
}

fn write_canonical(value: &Value, output: &mut Vec<u8>) -> Result<(), RunTelemetryError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => output.extend_from_slice(
            serde_json::to_string(value)
                .map_err(|error| RunTelemetryError::Serialization(error.to_string()))?
                .as_bytes(),
        ),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical(&Value::String(key.clone()), output)?;
                output.push(b':');
                write_canonical(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}
