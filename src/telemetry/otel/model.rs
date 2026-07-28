use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::telemetry::redact::{CaptureBoundary, CaptureRedactor, StructuredValue};

pub const ADAPTER_VERSION: u16 = 1;
pub const RFC_SPAN_NAMES: [&str; 9] = [
    "api.command",
    "run.attempt",
    "model.call",
    "tool.call",
    "nested.tool.call",
    "process.exec",
    "child.run",
    "verification.check",
    "compaction.checkpoint",
];

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AttributeValue {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Array(Vec<AttributeValue>),
    Object(BTreeMap<String, AttributeValue>),
}

impl AttributeValue {
    fn redact(&mut self, boundary: CaptureBoundary, redactor: &CaptureRedactor<'_>) {
        *self = Self::from_structured(redactor.redact_value(boundary, &self.to_structured()));
    }

    fn to_structured(&self) -> StructuredValue {
        match self {
            Self::Null => StructuredValue::Null,
            Self::Bool(value) => StructuredValue::Bool(*value),
            Self::I64(value) => StructuredValue::I64(*value),
            Self::U64(value) => StructuredValue::U64(*value),
            Self::F64(value) => StructuredValue::F64(*value),
            Self::String(value) => StructuredValue::String(value.clone()),
            Self::Array(values) => {
                StructuredValue::Array(values.iter().map(Self::to_structured).collect())
            }
            Self::Object(fields) => StructuredValue::Object(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), value.to_structured()))
                    .collect(),
            ),
        }
    }

    fn from_structured(value: StructuredValue) -> Self {
        match value {
            StructuredValue::Null => Self::Null,
            StructuredValue::Bool(value) => Self::Bool(value),
            StructuredValue::I64(value) => Self::I64(value),
            StructuredValue::U64(value) => Self::U64(value),
            StructuredValue::F64(value) => Self::F64(value),
            StructuredValue::String(value) => Self::String(value),
            StructuredValue::Array(values) => {
                Self::Array(values.into_iter().map(Self::from_structured).collect())
            }
            StructuredValue::Object(fields) => Self::Object(
                fields
                    .into_iter()
                    .map(|(name, value)| (name, Self::from_structured(value)))
                    .collect(),
            ),
        }
    }
}

pub(crate) fn redact_attributes(
    attributes: &mut BTreeMap<String, AttributeValue>,
    boundary: CaptureBoundary,
    redactor: &CaptureRedactor<'_>,
) {
    let mut value = AttributeValue::Object(std::mem::take(attributes));
    value.redact(boundary, redactor);
    if let AttributeValue::Object(safe) = value {
        *attributes = safe;
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum SpanName {
    #[serde(rename = "api.command")]
    ApiCommand,
    #[serde(rename = "run.attempt")]
    RunAttempt,
    #[serde(rename = "model.call")]
    ModelCall,
    #[serde(rename = "tool.call")]
    ToolCall,
    #[serde(rename = "nested.tool.call")]
    NestedToolCall,
    #[serde(rename = "process.exec")]
    ProcessExec,
    #[serde(rename = "child.run")]
    ChildRun,
    #[serde(rename = "verification.check")]
    VerificationCheck,
    #[serde(rename = "compaction.checkpoint")]
    CompactionCheckpoint,
}

impl SpanName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiCommand => "api.command",
            Self::RunAttempt => "run.attempt",
            Self::ModelCall => "model.call",
            Self::ToolCall => "tool.call",
            Self::NestedToolCall => "nested.tool.call",
            Self::ProcessExec => "process.exec",
            Self::ChildRun => "child.run",
            Self::VerificationCheck => "verification.check",
            Self::CompactionCheckpoint => "compaction.checkpoint",
        }
    }

    fn accepts_parent(self, parent: Option<Self>) -> bool {
        match self {
            Self::ApiCommand => parent.is_none(),
            Self::RunAttempt => parent == Some(Self::ApiCommand),
            Self::ModelCall
            | Self::ToolCall
            | Self::ChildRun
            | Self::VerificationCheck
            | Self::CompactionCheckpoint => parent == Some(Self::RunAttempt),
            Self::NestedToolCall | Self::ProcessExec => parent == Some(Self::ToolCall),
        }
    }
}

impl std::fmt::Display for SpanName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Internal,
    Server,
    Client,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "code", content = "description", rename_all = "snake_case")]
pub enum SpanStatus {
    Unset,
    Ok,
    Error(Option<String>),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SpanEvent {
    pub name: String,
    pub timestamp_unix_nanos: u64,
    pub attributes: BTreeMap<String, AttributeValue>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Span {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    #[serde(rename = "name")]
    pub span_name: SpanName,
    pub kind: SpanKind,
    pub start_unix_nanos: u64,
    pub end_unix_nanos: u64,
    pub attributes: BTreeMap<String, AttributeValue>,
    pub events: Vec<SpanEvent>,
    pub status: SpanStatus,
}

impl Span {
    pub fn name(&self) -> &'static str {
        self.span_name.as_str()
    }

    fn redact(&mut self, redactor: &CaptureRedactor<'_>) {
        redact_attributes(&mut self.attributes, CaptureBoundary::Trace, redactor);
        for event in &mut self.events {
            event.name = redactor.redact_text(CaptureBoundary::Trace, &event.name);
            redact_attributes(&mut event.attributes, CaptureBoundary::Trace, redactor);
        }
        if let SpanStatus::Error(Some(description)) = &mut self.status {
            *description = redactor.redact_text(CaptureBoundary::Trace, description);
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Resource {
    pub attributes: BTreeMap<String, AttributeValue>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum MetricName {
    #[serde(rename = "kit.run.count")]
    RunCount,
    #[serde(rename = "kit.run.latency")]
    RunLatency,
    #[serde(rename = "kit.model.tokens")]
    ModelTokens,
    #[serde(rename = "kit.tool.calls")]
    ToolCalls,
    #[serde(rename = "kit.run.cost")]
    RunCost,
    #[serde(rename = "kit.cache.tokens")]
    CacheTokens,
    #[serde(rename = "kit.verification.checks")]
    VerificationChecks,
    #[serde(rename = "kit.errors")]
    Errors,
}

impl MetricName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunCount => "kit.run.count",
            Self::RunLatency => "kit.run.latency",
            Self::ModelTokens => "kit.model.tokens",
            Self::ToolCalls => "kit.tool.calls",
            Self::RunCost => "kit.run.cost",
            Self::CacheTokens => "kit.cache.tokens",
            Self::VerificationChecks => "kit.verification.checks",
            Self::Errors => "kit.errors",
        }
    }

    pub const fn label_schema(self) -> &'static [&'static str] {
        match self {
            Self::RunCount | Self::RunLatency => &["outcome"],
            Self::ModelTokens => &["token_type"],
            Self::ToolCalls => &["tool_result"],
            Self::RunCost => &["cost_type"],
            Self::CacheTokens => &["cache_category"],
            Self::VerificationChecks => &["check_result"],
            Self::Errors => &["error_class"],
        }
    }

    fn accepts(self, key: &str, value: &str) -> bool {
        match (self, key) {
            (Self::RunCount | Self::RunLatency, "outcome") => matches!(
                value,
                "succeeded" | "failed" | "cancelled" | "interrupted" | "outcome_unknown"
            ),
            (Self::ModelTokens, "token_type") => matches!(
                value,
                "input" | "output" | "reasoning" | "cache_read" | "cache_write"
            ),
            (Self::ToolCalls, "tool_result") => {
                matches!(value, "succeeded" | "failed" | "cancelled" | "unknown")
            }
            (Self::RunCost, "cost_type") => {
                matches!(value, "model" | "tool" | "compute" | "total")
            }
            (Self::CacheTokens, "cache_category") => {
                matches!(value, "read" | "write" | "uncached")
            }
            (Self::VerificationChecks, "check_result") => {
                matches!(value, "passed" | "failed" | "skipped" | "unavailable")
            }
            (Self::Errors, "error_class") => matches!(
                value,
                "model" | "tool" | "verification" | "system" | "policy" | "unknown"
            ),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MetricValue {
    Counter { value: u64 },
    Gauge { value: f64 },
    Histogram { count: u64, sum: f64 },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Metric {
    #[serde(rename = "name")]
    pub metric_name: MetricName,
    pub unit: String,
    pub value: MetricValue,
    pub labels: BTreeMap<String, String>,
    pub timestamp_unix_nanos: u64,
}

impl Metric {
    pub fn new(
        metric_name: MetricName,
        unit: impl Into<String>,
        value: MetricValue,
        labels: BTreeMap<String, String>,
        timestamp_unix_nanos: u64,
    ) -> Result<Self, MetricError> {
        let metric = Self {
            metric_name,
            unit: unit.into(),
            value,
            labels,
            timestamp_unix_nanos,
        };
        metric.validate()?;
        Ok(metric)
    }

    pub fn name(&self) -> &'static str {
        self.metric_name.as_str()
    }

    pub fn validate(&self) -> Result<(), MetricError> {
        let expected: BTreeSet<_> = self.metric_name.label_schema().iter().copied().collect();
        let actual: BTreeSet<_> = self.labels.keys().map(String::as_str).collect();
        if actual != expected {
            return Err(MetricError::LabelSchema {
                metric: self.metric_name,
                expected: expected.into_iter().collect(),
                actual: actual.into_iter().map(str::to_owned).collect(),
            });
        }
        for (key, value) in &self.labels {
            if !self.metric_name.accepts(key, value) {
                return Err(MetricError::UnboundedLabelValue {
                    metric: self.metric_name,
                    key: key.clone(),
                    value: value.clone(),
                });
            }
        }
        match self.value {
            MetricValue::Gauge { value } if !value.is_finite() => Err(MetricError::NonFinite),
            MetricValue::Histogram { sum, .. } if !sum.is_finite() => Err(MetricError::NonFinite),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricError {
    LabelSchema {
        metric: MetricName,
        expected: Vec<&'static str>,
        actual: Vec<String>,
    },
    UnboundedLabelValue {
        metric: MetricName,
        key: String,
        value: String,
    },
    NonFinite,
}

impl std::fmt::Display for MetricError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LabelSchema {
                metric,
                expected,
                actual,
            } => write!(
                formatter,
                "{} labels must be {expected:?}, got {actual:?}",
                metric.as_str()
            ),
            Self::UnboundedLabelValue { metric, key, .. } => {
                write!(formatter, "{} has unbounded label {key}", metric.as_str())
            }
            Self::NonFinite => formatter.write_str("metric values must be finite"),
        }
    }
}

impl std::error::Error for MetricError {}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelUsage {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub calls: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolUsage {
    pub calls: Option<u64>,
    pub succeeded: Option<u64>,
    pub failed: Option<u64>,
    pub latency_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CacheUsage {
    pub uncached_input_tokens: Option<u64>,
    pub read_tokens: Option<u64>,
    pub write_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub outcome: Option<String>,
    pub latency_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunError {
    pub class: Option<String>,
    pub code: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunEnvelope {
    pub schema_version: u16,
    pub outcome: Option<RunOutcome>,
    pub latency_ms: Option<u64>,
    pub model_usage: Option<ModelUsage>,
    pub tool_usage: Option<ToolUsage>,
    pub cost_microusd: Option<u64>,
    pub cache: Option<CacheUsage>,
    pub checks: Option<Vec<CheckResult>>,
    pub errors: Option<Vec<RunError>>,
    #[serde(default)]
    pub canonical: Option<Box<crate::telemetry::run_envelope::RunEnvelope>>,
}

impl Default for RunEnvelope {
    fn default() -> Self {
        Self {
            schema_version: ADAPTER_VERSION,
            outcome: None,
            latency_ms: None,
            model_usage: None,
            tool_usage: None,
            cost_microusd: None,
            cache: None,
            checks: None,
            errors: None,
            canonical: None,
        }
    }
}

impl RunEnvelope {
    fn redact(&mut self, redactor: &CaptureRedactor<'_>) {
        if let Some(usage) = &mut self.model_usage {
            for value in [&mut usage.provider, &mut usage.model]
                .into_iter()
                .flatten()
            {
                *value = redactor.redact_text(CaptureBoundary::Trace, value);
            }
        }
        if let Some(checks) = &mut self.checks {
            for check in checks {
                check.name = redactor.redact_text(CaptureBoundary::Trace, &check.name);
                if let Some(outcome) = &mut check.outcome {
                    *outcome = redactor.redact_text(CaptureBoundary::Trace, outcome);
                }
            }
        }
        if let Some(errors) = &mut self.errors {
            for error in errors {
                for value in [&mut error.class, &mut error.code, &mut error.message]
                    .into_iter()
                    .flatten()
                {
                    *value = redactor.redact_text(CaptureBoundary::Trace, value);
                }
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LogRecord {
    pub timestamp_unix_nanos: u64,
    pub severity: LogSeverity,
    pub body: AttributeValue,
    pub attributes: BTreeMap<String, AttributeValue>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSeverity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogRecord {
    fn redact(&mut self, redactor: &CaptureRedactor<'_>) {
        self.body.redact(CaptureBoundary::Log, redactor);
        redact_attributes(&mut self.attributes, CaptureBoundary::Log, redactor);
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "signal", content = "record", rename_all = "snake_case")]
pub enum TelemetryItem {
    Span(Span),
    Metric(Metric),
    Log(LogRecord),
    RunEnvelope(RunEnvelope),
}

impl TelemetryItem {
    pub(crate) fn redact(&mut self, redactor: &CaptureRedactor<'_>) {
        match self {
            Self::Span(span) => span.redact(redactor),
            Self::Metric(metric) => {
                metric.unit = redactor.redact_text(CaptureBoundary::Trace, &metric.unit);
            }
            Self::Log(log) => log.redact(redactor),
            Self::RunEnvelope(envelope) => envelope.redact(redactor),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), MetricError> {
        match self {
            Self::Metric(metric) => metric.validate(),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExportBatch {
    pub schema_version: u16,
    pub resource: Resource,
    pub spans: Vec<Span>,
    pub metrics: Vec<Metric>,
    pub logs: Vec<LogRecord>,
    pub run_envelopes: Vec<RunEnvelope>,
}

impl ExportBatch {
    pub fn empty(resource: Resource) -> Self {
        Self {
            schema_version: ADAPTER_VERSION,
            resource,
            spans: Vec::new(),
            metrics: Vec::new(),
            logs: Vec::new(),
            run_envelopes: Vec::new(),
        }
    }

    pub fn from_items(resource: Resource, items: impl IntoIterator<Item = TelemetryItem>) -> Self {
        let mut batch = Self::empty(resource);
        for item in items {
            match item {
                TelemetryItem::Span(span) => batch.spans.push(span),
                TelemetryItem::Metric(metric) => batch.metrics.push(metric),
                TelemetryItem::Log(log) => batch.logs.push(log),
                TelemetryItem::RunEnvelope(envelope) => batch.run_envelopes.push(envelope),
            }
        }
        batch
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != ADAPTER_VERSION {
            return Err(format!(
                "unsupported telemetry schema version {}",
                self.schema_version
            ));
        }
        for metric in &self.metrics {
            metric.validate().map_err(|error| error.to_string())?;
        }
        for envelope in &self.run_envelopes {
            if envelope.schema_version != ADAPTER_VERSION {
                return Err(format!(
                    "unsupported run envelope schema version {}",
                    envelope.schema_version
                ));
            }
        }
        let spans: BTreeMap<_, _> = self
            .spans
            .iter()
            .map(|span| (span.span_id.as_str(), span))
            .collect();
        if spans.len() != self.spans.len() {
            return Err("duplicate span id".to_owned());
        }
        for span in &self.spans {
            if !valid_otel_id(&span.trace_id, 32) {
                return Err(format!("span {} has invalid trace id", span.span_id));
            }
            if !valid_otel_id(&span.span_id, 16) {
                return Err(format!("invalid span id {}", span.span_id));
            }
            if span.end_unix_nanos < span.start_unix_nanos {
                return Err(format!("span {} ends before it starts", span.span_id));
            }
            let parent = match &span.parent_span_id {
                Some(id) => {
                    let parent = spans
                        .get(id.as_str())
                        .ok_or_else(|| format!("span {} has missing parent {id}", span.span_id))?;
                    if parent.trace_id != span.trace_id {
                        return Err(format!("span {} parent has another trace", span.span_id));
                    }
                    Some(parent.span_name)
                }
                None => None,
            };
            if !span.span_name.accepts_parent(parent) {
                return Err(format!(
                    "{} has invalid parent {:?}",
                    span.span_name, parent
                ));
            }
        }
        for log in &self.logs {
            if log
                .trace_id
                .as_deref()
                .is_some_and(|id| !valid_otel_id(id, 32))
            {
                return Err("log has invalid trace id".to_owned());
            }
            if log
                .span_id
                .as_deref()
                .is_some_and(|id| !valid_otel_id(id, 16))
            {
                return Err("log has invalid span id".to_owned());
            }
        }
        Ok(())
    }

    pub fn to_canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        self.validate()
            .map_err(<serde_json::Error as serde::ser::Error>::custom)?;
        serde_json::to_vec(self)
    }
}

fn valid_otel_id(value: &str, length: usize) -> bool {
    value.len() == length
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value.bytes().any(|byte| byte != b'0')
}
