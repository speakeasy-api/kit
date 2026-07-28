use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::{self, Write},
    path::Path,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    api::auth::contract::AuthenticatedPrincipal,
    domain::config::Grant,
    domain::secret::SecretLease,
    executor::check::{CHECK_EXECUTOR_CONTRACT_VERSION, CheckProcessEvidence},
    store::artifacts::{
        ArtifactClass, ArtifactMetadata, ArtifactReference, ArtifactRetention, ArtifactStore,
    },
    telemetry::redact::{CaptureBoundary, CaptureRedactor},
    verify::profiles::{
        CheckLaunchStatus, CheckResultStatus, VerificationBoundary, VerificationCheckResult,
        VerificationError, VerificationObservation, VerificationObserver, VerificationReceipt,
        VerificationResult,
    },
};

pub const FEEDBACK_SCHEMA_VERSION: u16 = 1;
pub const DIAGNOSTIC_SCHEMA_VERSION: u16 = 1;
pub const CHECK_EVENT_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticAdapter {
    NormalizedJsonLinesV1,
    RustcJsonV1,
}

impl DiagnosticAdapter {
    const fn version(self) -> u16 {
        1
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Note,
    Help,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

impl DiagnosticRange {
    fn valid(self) -> bool {
        self.start_line > 0
            && self.start_column > 0
            && self.end_line >= self.start_line
            && self.end_column > 0
            && (self.end_line != self.start_line || self.end_column >= self.start_column)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedDiagnostic {
    pub schema_version: u16,
    pub check_id: String,
    pub path: String,
    pub range: DiagnosticRange,
    pub code: Option<String>,
    pub message: String,
    pub message_digest: String,
    pub severity: DiagnosticSeverity,
    pub tool: String,
}

impl NormalizedDiagnostic {
    fn identity(&self) -> DiagnosticIdentity {
        DiagnosticIdentity {
            check_id: self.check_id.clone(),
            path: self.path.clone(),
            range: self.range,
            code: self.code.clone(),
            message_digest: self.message_digest.clone(),
            severity: self.severity,
            tool: self.tool.clone(),
        }
    }

    fn location_identity(&self) -> DiagnosticLocationIdentity {
        DiagnosticLocationIdentity {
            check_id: self.check_id.clone(),
            path: self.path.clone(),
            range: self.range,
            code: self.code.clone(),
            tool: self.tool.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DiagnosticIdentity {
    check_id: String,
    path: String,
    range: DiagnosticRange,
    code: Option<String>,
    message_digest: String,
    severity: DiagnosticSeverity,
    tool: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DiagnosticLocationIdentity {
    check_id: String,
    path: String,
    range: DiagnosticRange,
    code: Option<String>,
    tool: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticDeltaKind {
    New,
    Resolved,
    Persisting,
    Changed,
    Observed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticDelta {
    pub kind: DiagnosticDeltaKind,
    pub before: Option<NormalizedDiagnostic>,
    pub after: Option<NormalizedDiagnostic>,
    pub changed_line: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineUnavailableReason {
    Missing,
    NotCapturedBeforeEdit,
    CheckPlanMismatch,
    ExecutorVersionMismatch,
    ToolVersionMismatch,
    ConfigMismatch,
    AdapterVersionMismatch,
    ArtifactUnavailable,
    ParseBudgetExceeded,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum BaselineAvailability {
    Available,
    Unavailable(BaselineUnavailableReason),
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckFingerprint {
    pub check_id: String,
    pub class: String,
    pub required: bool,
    pub executor_version: u16,
    pub adapter: DiagnosticAdapter,
    pub adapter_version: u16,
    pub image_digest: Option<String>,
    pub tool_digest: Option<String>,
    pub config_digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticSet {
    pub revision: String,
    pub checks: Vec<CheckFingerprint>,
    pub diagnostics: Vec<NormalizedDiagnostic>,
    pub malformed_records: u64,
    pub oversized_records: u64,
    pub sanitized_input_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackLimits {
    pub max_input_bytes: usize,
    pub max_diagnostics: usize,
    pub max_path_bytes: usize,
    pub max_message_bytes: usize,
    pub max_record_bytes: usize,
    pub max_report_bytes: usize,
    pub max_feedback_bytes: usize,
    pub max_log_references: usize,
    pub max_memory_bytes: usize,
    pub max_mapping_bytes: usize,
    pub max_pending_record_bytes: usize,
    pub max_pending_records: usize,
    pub max_pending_bytes: usize,
    pub max_adapters: usize,
    pub max_pending_events: usize,
    pub max_operation_time: Duration,
}

impl Default for FeedbackLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 4 * 1024 * 1024,
            max_diagnostics: 4096,
            max_path_bytes: 512,
            max_message_bytes: 4096,
            max_record_bytes: 16 * 1024,
            max_report_bytes: 8 * 1024 * 1024,
            max_feedback_bytes: 32 * 1024,
            max_log_references: 128,
            max_memory_bytes: 16 * 1024 * 1024,
            max_mapping_bytes: 1024 * 1024,
            max_pending_record_bytes: 8 * 1024 * 1024,
            max_pending_records: 128,
            max_pending_bytes: 16 * 1024 * 1024,
            max_adapters: 128,
            max_pending_events: 384,
            max_operation_time: Duration::from_secs(10),
        }
    }
}

impl FeedbackLimits {
    fn validate(&self) -> Result<(), FeedbackError> {
        if self.max_input_bytes == 0
            || self.max_path_bytes == 0
            || self.max_message_bytes == 0
            || self.max_record_bytes == 0
            || self.max_report_bytes == 0
            || self.max_feedback_bytes < 512
            || self.max_log_references == 0
            || self.max_memory_bytes == 0
            || self.max_mapping_bytes == 0
            || self.max_pending_record_bytes == 0
            || self.max_pending_records == 0
            || self.max_pending_bytes == 0
            || self.max_adapters == 0
            || self.max_pending_events == 0
            || self.max_operation_time.is_zero()
        {
            return Err(FeedbackError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PathMove {
    before: String,
    after: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LineMove {
    before_path: String,
    after_path: String,
    before_start: u32,
    after_start: u32,
    line_count: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ChangedLineRange {
    path: String,
    start_line: u32,
    end_line: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditMapping {
    paths: Vec<PathMove>,
    lines: Vec<LineMove>,
    changed_lines: Vec<ChangedLineRange>,
}

impl EditMapping {
    pub(crate) fn from_effects<F>(
        effects: &[crate::workspace::edit::validate::PlannedEffect],
        mut actual_after: F,
    ) -> Result<Self, FeedbackError>
    where
        F: FnMut(
            &crate::workspace::edit::ir::RootRelativePath,
        ) -> Result<Option<Vec<u8>>, FeedbackError>,
    {
        let mut mapping = Self::default();
        for effect in effects {
            use crate::workspace::edit::validate::PlannedEffect;
            match effect {
                PlannedEffect::Add { path, .. } => {
                    let after = actual_after(path)?.ok_or(FeedbackError::InvalidMapping)?;
                    mapping
                        .changed_lines
                        .push(changed_file(path.as_str(), &after)?);
                }
                PlannedEffect::Delete { path, .. } => {
                    mapping.changed_lines.push(ChangedLineRange {
                        path: path.as_str().to_owned(),
                        start_line: 1,
                        end_line: u32::MAX,
                    });
                }
                PlannedEffect::Move { from, to, .. } => mapping.paths.push(PathMove {
                    before: from.as_str().to_owned(),
                    after: to.as_str().to_owned(),
                }),
                PlannedEffect::Replace { path, before, .. } => {
                    let before = before.content().ok_or(FeedbackError::InvalidMapping)?;
                    let after = actual_after(path)?.ok_or(FeedbackError::InvalidMapping)?;
                    let after = after.as_slice();
                    if before == after {
                        continue;
                    }
                    let before_count = line_count(before)?;
                    let after_count = line_count(after)?;
                    let prefix = before
                        .split_inclusive(|byte| *byte == b'\n')
                        .zip(after.split_inclusive(|byte| *byte == b'\n'))
                        .take_while(|(before, after)| before == after)
                        .count();
                    let max_suffix = before_count.min(after_count).saturating_sub(prefix);
                    let suffix = common_suffix_lines(before, after, max_suffix);
                    let changed_start = u32::try_from(prefix)
                        .ok()
                        .and_then(|line| line.checked_add(1))
                        .ok_or(FeedbackError::InvalidMapping)?;
                    let changed_end = u32::try_from(after_count.saturating_sub(suffix))
                        .map_err(|_| FeedbackError::InvalidMapping)?
                        .max(changed_start);
                    mapping.changed_lines.push(ChangedLineRange {
                        path: path.as_str().to_owned(),
                        start_line: changed_start,
                        end_line: changed_end,
                    });
                    if suffix > 0 {
                        mapping.lines.push(LineMove {
                            before_path: path.as_str().to_owned(),
                            after_path: path.as_str().to_owned(),
                            before_start: u32::try_from(before_count - suffix + 1)
                                .map_err(|_| FeedbackError::InvalidMapping)?,
                            after_start: u32::try_from(after_count - suffix + 1)
                                .map_err(|_| FeedbackError::InvalidMapping)?,
                            line_count: u32::try_from(suffix)
                                .map_err(|_| FeedbackError::InvalidMapping)?,
                        });
                    }
                }
            }
        }
        validate_mapping(&mapping)?;
        Ok(mapping)
    }

    pub(crate) fn digest(&self) -> String {
        digest(&serde_json::to_vec(self).expect("edit mapping serialization cannot fail"))
    }
}

fn line_count(bytes: &[u8]) -> Result<usize, FeedbackError> {
    let count = bytes.split_inclusive(|byte| *byte == b'\n').count();
    u32::try_from(count)
        .map(|_| count)
        .map_err(|_| FeedbackError::InvalidMapping)
}

fn common_suffix_lines(before: &[u8], after: &[u8], max: usize) -> usize {
    let mut before_end = before.len();
    let mut after_end = after.len();
    let mut count = 0;
    while count < max {
        let before_start = before[..before_end.saturating_sub(1)]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let after_start = after[..after_end.saturating_sub(1)]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        if before[before_start..before_end] != after[after_start..after_end] {
            break;
        }
        count += 1;
        before_end = before_start;
        after_end = after_start;
    }
    count
}

fn changed_file(path: &str, bytes: &[u8]) -> Result<ChangedLineRange, FeedbackError> {
    Ok(ChangedLineRange {
        path: path.to_owned(),
        start_line: 1,
        end_line: u32::try_from(bytes.split_inclusive(|byte| *byte == b'\n').count())
            .map_err(|_| FeedbackError::InvalidMapping)?
            .max(1),
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Comparison {
    pub baseline: BaselineAvailability,
    pub deltas: Vec<DiagnosticDelta>,
}

pub fn parse_diagnostics(
    adapter: DiagnosticAdapter,
    check_id: &str,
    tool: &str,
    sanitized: &[u8],
    limits: &FeedbackLimits,
) -> Result<DiagnosticSet, FeedbackError> {
    limits.validate()?;
    if sanitized.len() > limits.max_input_bytes {
        return Err(FeedbackError::BudgetExceeded);
    }
    validate_component(check_id, 128)?;
    validate_component(tool, 128)?;
    let mut diagnostics = Vec::new();
    let mut malformed_records = 0_u64;
    let mut oversized_records = 0_u64;
    let deadline = deadline(limits)?;
    for line in sanitized.split(|byte| *byte == b'\n') {
        check_deadline(deadline)?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if line.len() > limits.max_record_bytes {
            oversized_records = oversized_records
                .checked_add(1)
                .ok_or(FeedbackError::BudgetExceeded)?;
            continue;
        }
        match parse_diagnostic_line(adapter, check_id, tool, line, limits) {
            Some(diagnostic) if diagnostics.len() < limits.max_diagnostics => {
                diagnostics.push(diagnostic);
            }
            Some(_) => return Err(FeedbackError::DiagnosticLimitExceeded),
            None => {
                malformed_records = malformed_records
                    .checked_add(1)
                    .ok_or(FeedbackError::BudgetExceeded)?;
            }
        }
    }
    diagnostics.sort_by_key(NormalizedDiagnostic::identity);
    Ok(DiagnosticSet {
        revision: String::new(),
        checks: vec![CheckFingerprint {
            check_id: check_id.to_owned(),
            class: "diagnostics".to_owned(),
            required: false,
            executor_version: CHECK_EXECUTOR_CONTRACT_VERSION,
            adapter,
            adapter_version: adapter.version(),
            image_digest: Some(digest(b"pure-diagnostic-adapter")),
            tool_digest: Some(digest(tool.as_bytes())),
            config_digest: Some(digest(format!("{adapter:?}").as_bytes())),
        }],
        diagnostics,
        malformed_records,
        oversized_records,
        sanitized_input_bytes: sanitized.len() as u64,
    })
}

#[derive(Deserialize)]
struct NormalizedWire {
    schema_version: u16,
    path: String,
    range: DiagnosticRange,
    code: Option<String>,
    message: String,
    severity: DiagnosticSeverity,
    tool: Option<String>,
}

fn parse_diagnostic_line(
    adapter: DiagnosticAdapter,
    check_id: &str,
    tool: &str,
    line: &[u8],
    limits: &FeedbackLimits,
) -> Option<NormalizedDiagnostic> {
    let wire = match adapter {
        DiagnosticAdapter::NormalizedJsonLinesV1 => {
            let wire: NormalizedWire = serde_json::from_slice(line).ok()?;
            if wire.schema_version != DIAGNOSTIC_SCHEMA_VERSION
                || wire.tool.as_deref().is_some_and(|value| value != tool)
            {
                return None;
            }
            wire
        }
        DiagnosticAdapter::RustcJsonV1 => rustc_wire(line, tool)?,
    };
    if !valid_path(&wire.path, limits.max_path_bytes)
        || !wire.range.valid()
        || wire.message.is_empty()
        || wire.message.len() > limits.max_message_bytes
        || wire
            .code
            .as_ref()
            .is_some_and(|code| !valid_component(code, 128))
    {
        return None;
    }
    Some(NormalizedDiagnostic {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        check_id: check_id.to_owned(),
        path: wire.path,
        range: wire.range,
        code: wire.code,
        message_digest: digest(wire.message.as_bytes()),
        message: wire.message,
        severity: wire.severity,
        tool: tool.to_owned(),
    })
}

fn rustc_wire(line: &[u8], tool: &str) -> Option<NormalizedWire> {
    let value: Value = serde_json::from_slice(line).ok()?;
    if value.get("reason")?.as_str()? != "compiler-message" {
        return None;
    }
    let message = value.get("message")?;
    let spans = message.get("spans")?.as_array()?;
    let span = spans
        .iter()
        .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true))
        .or_else(|| spans.first())?;
    let severity = match message.get("level")?.as_str()? {
        "error" | "failure-note" => DiagnosticSeverity::Error,
        "warning" => DiagnosticSeverity::Warning,
        "note" => DiagnosticSeverity::Note,
        "help" => DiagnosticSeverity::Help,
        _ => return None,
    };
    Some(NormalizedWire {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        path: span.get("file_name")?.as_str()?.to_owned(),
        range: DiagnosticRange {
            start_line: u32::try_from(span.get("line_start")?.as_u64()?).ok()?,
            start_column: u32::try_from(span.get("column_start")?.as_u64()?).ok()?,
            end_line: u32::try_from(span.get("line_end")?.as_u64()?).ok()?,
            end_column: u32::try_from(span.get("column_end")?.as_u64()?).ok()?,
        },
        code: message
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        message: message.get("message")?.as_str()?.to_owned(),
        severity,
        tool: Some(tool.to_owned()),
    })
}

pub fn compare_diagnostics(
    baseline: Option<&DiagnosticSet>,
    current: &DiagnosticSet,
) -> Result<Comparison, FeedbackError> {
    compare_mapped_diagnostics(baseline, current, &EditMapping::default())
}

fn compare_mapped_diagnostics(
    baseline: Option<&DiagnosticSet>,
    current: &DiagnosticSet,
    mapping: &EditMapping,
) -> Result<Comparison, FeedbackError> {
    validate_mapping(mapping)?;
    let availability = baseline.map_or(
        BaselineAvailability::Unavailable(BaselineUnavailableReason::Missing),
        |baseline| baseline_compatibility(baseline, current),
    );
    if availability != BaselineAvailability::Available {
        return Ok(Comparison {
            baseline: availability,
            deltas: current
                .diagnostics
                .iter()
                .cloned()
                .map(|after| DiagnosticDelta {
                    changed_line: changed_line(&after, mapping),
                    kind: DiagnosticDeltaKind::Observed,
                    before: None,
                    after: Some(after),
                })
                .collect(),
        });
    }
    let baseline = baseline.expect("available baseline exists");
    let before = baseline
        .diagnostics
        .iter()
        .cloned()
        .map(|diagnostic| map_diagnostic(diagnostic, mapping))
        .collect::<Vec<_>>();
    let mut matched_before = vec![false; before.len()];
    let mut matched_after = vec![false; current.diagnostics.len()];
    let mut exact = BTreeMap::<DiagnosticIdentity, Vec<usize>>::new();
    for (index, diagnostic) in before.iter().enumerate() {
        exact.entry(diagnostic.identity()).or_default().push(index);
    }
    let mut deltas = Vec::new();
    for (after_index, after) in current.diagnostics.iter().enumerate() {
        if let Some(indices) = exact.get_mut(&after.identity())
            && let Some(before_index) = indices.pop()
        {
            matched_before[before_index] = true;
            matched_after[after_index] = true;
            deltas.push(DiagnosticDelta {
                kind: DiagnosticDeltaKind::Persisting,
                before: Some(before[before_index].clone()),
                after: Some(after.clone()),
                changed_line: changed_line(after, mapping),
            });
        }
    }
    let mut locations = BTreeMap::<DiagnosticLocationIdentity, Vec<usize>>::new();
    for (index, diagnostic) in before.iter().enumerate() {
        if !matched_before[index] {
            locations
                .entry(diagnostic.location_identity())
                .or_default()
                .push(index);
        }
    }
    for (after_index, after) in current.diagnostics.iter().enumerate() {
        if matched_after[after_index] {
            continue;
        }
        if let Some(indices) = locations.get_mut(&after.location_identity())
            && let Some(before_index) = indices.pop()
        {
            matched_before[before_index] = true;
            matched_after[after_index] = true;
            deltas.push(DiagnosticDelta {
                kind: DiagnosticDeltaKind::Changed,
                before: Some(before[before_index].clone()),
                after: Some(after.clone()),
                changed_line: changed_line(after, mapping),
            });
        }
    }
    deltas.extend(
        current
            .diagnostics
            .iter()
            .enumerate()
            .filter(|(index, _)| !matched_after[*index])
            .map(|(_, after)| DiagnosticDelta {
                kind: DiagnosticDeltaKind::New,
                before: None,
                after: Some(after.clone()),
                changed_line: changed_line(after, mapping),
            }),
    );
    deltas.extend(
        before
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !matched_before[*index])
            .map(|(_, before)| DiagnosticDelta {
                kind: DiagnosticDeltaKind::Resolved,
                changed_line: changed_line(&before, mapping),
                before: Some(before),
                after: None,
            }),
    );
    deltas.sort_by_key(delta_rank);
    Ok(Comparison {
        baseline: BaselineAvailability::Available,
        deltas,
    })
}

fn baseline_compatibility(before: &DiagnosticSet, after: &DiagnosticSet) -> BaselineAvailability {
    if before.revision.is_empty() || before.revision == after.revision {
        return BaselineAvailability::Unavailable(BaselineUnavailableReason::NotCapturedBeforeEdit);
    }
    if before.checks.len() != after.checks.len()
        || before
            .checks
            .iter()
            .zip(&after.checks)
            .any(|(before, after)| {
                before.check_id != after.check_id
                    || before.class != after.class
                    || before.required != after.required
            })
    {
        return BaselineAvailability::Unavailable(BaselineUnavailableReason::CheckPlanMismatch);
    }
    for (before, after) in before.checks.iter().zip(&after.checks) {
        if before.tool_digest.is_none()
            || before.config_digest.is_none()
            || after.tool_digest.is_none()
            || after.config_digest.is_none()
        {
            return BaselineAvailability::Unavailable(
                BaselineUnavailableReason::ArtifactUnavailable,
            );
        }
        if before.executor_version != after.executor_version {
            return BaselineAvailability::Unavailable(
                BaselineUnavailableReason::ExecutorVersionMismatch,
            );
        }
        if before.adapter != after.adapter || before.adapter_version != after.adapter_version {
            return BaselineAvailability::Unavailable(
                BaselineUnavailableReason::AdapterVersionMismatch,
            );
        }
        if before.image_digest != after.image_digest || before.tool_digest != after.tool_digest {
            return BaselineAvailability::Unavailable(
                BaselineUnavailableReason::ToolVersionMismatch,
            );
        }
        if before.config_digest != after.config_digest {
            return BaselineAvailability::Unavailable(BaselineUnavailableReason::ConfigMismatch);
        }
    }
    BaselineAvailability::Available
}

fn map_diagnostic(
    mut diagnostic: NormalizedDiagnostic,
    mapping: &EditMapping,
) -> NormalizedDiagnostic {
    let original_path = diagnostic.path.clone();
    if let Some(lines) = mapping.lines.iter().find(|lines| {
        lines.before_path == original_path
            && diagnostic.range.start_line >= lines.before_start
            && diagnostic.range.end_line <= lines.before_start + (lines.line_count - 1)
    }) {
        let shift = |line: u32| lines.after_start + (line - lines.before_start);
        diagnostic.path.clone_from(&lines.after_path);
        diagnostic.range.start_line = shift(diagnostic.range.start_line);
        diagnostic.range.end_line = shift(diagnostic.range.end_line);
    } else if let Some(path) = mapping
        .paths
        .iter()
        .find(|path| path.before == original_path)
    {
        diagnostic.path.clone_from(&path.after);
    }
    diagnostic
}

fn changed_line(diagnostic: &NormalizedDiagnostic, mapping: &EditMapping) -> bool {
    mapping.changed_lines.iter().any(|changed| {
        changed.path == diagnostic.path
            && diagnostic.range.start_line <= changed.end_line
            && diagnostic.range.end_line >= changed.start_line
    })
}

fn delta_rank(delta: &DiagnosticDelta) -> (u8, u8, bool, String, u32, String) {
    let diagnostic = delta
        .after
        .as_ref()
        .or(delta.before.as_ref())
        .expect("delta diagnostic");
    let kind = match delta.kind {
        DiagnosticDeltaKind::New => 0,
        DiagnosticDeltaKind::Changed => 1,
        DiagnosticDeltaKind::Observed => 2,
        DiagnosticDeltaKind::Persisting => 3,
        DiagnosticDeltaKind::Resolved => 4,
    };
    let severity = match diagnostic.severity {
        DiagnosticSeverity::Error => 0,
        DiagnosticSeverity::Warning => 1,
        DiagnosticSeverity::Note => 2,
        DiagnosticSeverity::Help => 3,
    };
    (
        kind,
        severity,
        !delta.changed_line,
        diagnostic.path.clone(),
        diagnostic.range.start_line,
        diagnostic.message_digest.clone(),
    )
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpaqueArtifactRef {
    pub reference: String,
    pub length: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredFailure {
    pub check_id: String,
    pub status: String,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FeedbackItem {
    RequiredFailure(RequiredFailure),
    Diagnostic(Box<DiagnosticDelta>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackCounts {
    pub input_bytes: u64,
    pub baseline_diagnostics: u64,
    pub current_diagnostics: u64,
    pub result_count: u64,
    pub included_results: u64,
    pub omitted_results: u64,
    pub serialized_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackPayload {
    pub schema_version: u16,
    pub baseline: BaselineAvailability,
    pub counts: FeedbackCounts,
    pub truncated: bool,
    pub items: Vec<FeedbackItem>,
    pub diagnostic_report: OpaqueArtifactRef,
    pub full_logs: Vec<OpaqueArtifactRef>,
}

impl FeedbackPayload {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("feedback serialization cannot fail")
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_feedback(
    comparison: &Comparison,
    baseline_diagnostics: usize,
    current_diagnostics: usize,
    input_bytes: u64,
    required_failures: &[RequiredFailure],
    diagnostic_report: OpaqueArtifactRef,
    full_logs: Vec<OpaqueArtifactRef>,
    max_bytes: usize,
) -> Result<FeedbackPayload, FeedbackError> {
    if max_bytes < 512 || full_logs.len() > 128 {
        return Err(FeedbackError::InvalidLimits);
    }
    validate_artifact_ref(&diagnostic_report)?;
    for reference in &full_logs {
        validate_artifact_ref(reference)?;
    }
    for failure in required_failures {
        validate_component(&failure.check_id, 128)?;
        validate_component(&failure.status, 64)?;
    }
    let required_count = required_failures.len();
    let mut all = required_failures
        .iter()
        .cloned()
        .map(FeedbackItem::RequiredFailure)
        .chain(
            comparison
                .deltas
                .iter()
                .cloned()
                .map(Box::new)
                .map(FeedbackItem::Diagnostic),
        )
        .collect::<Vec<_>>();
    let total = all.len();
    loop {
        let included = all.len();
        let mut payload = FeedbackPayload {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            baseline: comparison.baseline.clone(),
            counts: FeedbackCounts {
                input_bytes,
                baseline_diagnostics: baseline_diagnostics as u64,
                current_diagnostics: current_diagnostics as u64,
                result_count: total as u64,
                included_results: included as u64,
                omitted_results: (total - included) as u64,
                serialized_bytes: 0,
            },
            truncated: included != total,
            items: all.clone(),
            diagnostic_report: diagnostic_report.clone(),
            full_logs: full_logs.clone(),
        };
        stabilize_size(&mut payload);
        if payload.canonical_bytes().len() <= max_bytes {
            return Ok(payload);
        }
        if all.len() == required_count {
            return Err(FeedbackError::BudgetExceeded);
        }
        all.pop();
    }
}

fn stabilize_size(payload: &mut FeedbackPayload) {
    loop {
        let size = payload.canonical_bytes().len() as u64;
        if payload.counts.serialized_bytes == size {
            break;
        }
        payload.counts.serialized_bytes = size;
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticReport {
    pub schema_version: u16,
    pub baseline: BaselineAvailability,
    pub baseline_set: Option<DiagnosticSet>,
    pub current_set: DiagnosticSet,
    pub deltas: Vec<DiagnosticDelta>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckEventKind {
    Started,
    Progress,
    Completed,
    Failure,
}

impl CheckEventKind {
    fn event_type(&self) -> &'static str {
        match self {
            Self::Started => "check.started",
            Self::Progress => "check.progress",
            Self::Completed => "check.completed",
            Self::Failure => "check.failure",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckEvent {
    pub schema_version: u16,
    pub cursor: u64,
    pub event_id: String,
    pub event_type: String,
    pub principal_id: String,
    pub project_id: String,
    pub workspace_id: String,
    pub run_id: String,
    pub edit_digest: String,
    pub feedback_operation_id: String,
    pub base_revision: String,
    pub staged_state_digest: String,
    pub successor_revision: Option<String>,
    pub verification_plan_digest: String,
    pub verification_result_digest: String,
    pub fence: u64,
    pub check_id: String,
    pub status: String,
    pub diagnostic_count: u64,
    pub artifacts: Vec<OpaqueArtifactRef>,
}

#[derive(Clone, Debug)]
pub struct FeedbackAuthority {
    principal_id: String,
    project_id: String,
    workspace_id: String,
    run_id: String,
    edit_digest: String,
    fence: u64,
    binding: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingFeedbackAuthority {
    principal_id: String,
    project_id: String,
    workspace_id: String,
    run_id: String,
    edit_digest: String,
    fence: u64,
    binding: String,
}

impl From<&FeedbackAuthority> for PendingFeedbackAuthority {
    fn from(authority: &FeedbackAuthority) -> Self {
        Self {
            principal_id: authority.principal_id.clone(),
            project_id: authority.project_id.clone(),
            workspace_id: authority.workspace_id.clone(),
            run_id: authority.run_id.clone(),
            edit_digest: authority.edit_digest.clone(),
            fence: authority.fence,
            binding: authority.binding.clone(),
        }
    }
}

impl PendingFeedbackAuthority {
    fn resolve(&self) -> Result<FeedbackAuthority, FeedbackError> {
        let binding = digest(
            format!(
                "{}\0{}\0{}\0{}\0{}\0{}",
                self.principal_id,
                self.project_id,
                self.workspace_id,
                self.run_id,
                self.edit_digest,
                self.fence
            )
            .as_bytes(),
        );
        if binding != self.binding || !valid_digest(&self.edit_digest) || self.fence == 0 {
            return Err(FeedbackError::EventConflict);
        }
        Ok(FeedbackAuthority {
            principal_id: self.principal_id.clone(),
            project_id: self.project_id.clone(),
            workspace_id: self.workspace_id.clone(),
            run_id: self.run_id.clone(),
            edit_digest: self.edit_digest.clone(),
            fence: self.fence,
            binding,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "expires_at", rename_all = "snake_case")]
enum PendingRetention {
    UntilUnixMicros(i64),
    Forever,
}

impl From<ArtifactRetention> for PendingRetention {
    fn from(retention: ArtifactRetention) -> Self {
        match retention {
            ArtifactRetention::UntilUnixMicros(value) => Self::UntilUnixMicros(value),
            ArtifactRetention::Forever => Self::Forever,
        }
    }
}

impl PendingRetention {
    fn resolve(&self) -> ArtifactRetention {
        match self {
            Self::UntilUnixMicros(value) => ArtifactRetention::UntilUnixMicros(*value),
            Self::Forever => ArtifactRetention::Forever,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct RecoveredVerificationResult {
    plan_digest: String,
    binding_digest: String,
    provenance: String,
    checks: Vec<VerificationCheckResult>,
    evidence_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingFeedbackRecord {
    schema_version: u16,
    authority: PendingFeedbackAuthority,
    operation_id: String,
    base_revision: String,
    staged_state_digest: String,
    verification_receipt: VerificationReceipt,
    baseline_report: Option<OpaqueArtifactRef>,
    baseline_compatibility: BaselineAvailability,
    mapping_bytes: Vec<u8>,
    mapping_digest: String,
    adapters: BTreeMap<String, DiagnosticAdapter>,
    adapters_digest: String,
    limits: FeedbackLimits,
    retention: PendingRetention,
    stored_at_unix_micros: i64,
    report_artifact: OpaqueArtifactRef,
    payload_artifact: OpaqueArtifactRef,
    expected_events: Vec<PendingCheckEvent>,
}

impl PendingFeedbackRecord {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, FeedbackError> {
        serde_json::to_vec(self).map_err(serialization_error)
    }
}

impl FeedbackAuthority {
    #[allow(dead_code)]
    pub(crate) fn issue(
        authenticated: &AuthenticatedPrincipal,
        workspace_id: impl Into<String>,
        run_id: impl Into<String>,
        edit_digest: impl Into<String>,
        fence: u64,
    ) -> Result<Self, FeedbackError> {
        let grants = authenticated.grant_snapshot();
        if !grants.grants().contains(&Grant::WorkspaceWrite)
            && !grants.grants().contains(&Grant::VerificationTargeted)
        {
            return Err(FeedbackError::AuthorityRequired);
        }
        let principal_id = grants.principal_id().to_string();
        let project_id = grants.project_id().to_string();
        let workspace_id = workspace_id.into();
        let run_id = run_id.into();
        let edit_digest = edit_digest.into();
        validate_component(&workspace_id, 128)?;
        validate_component(&run_id, 128)?;
        if !valid_digest(&edit_digest) || fence == 0 {
            return Err(FeedbackError::AuthorityRequired);
        }
        let binding = digest(
            format!(
                "{principal_id}\0{project_id}\0{workspace_id}\0{run_id}\0{edit_digest}\0{fence}"
            )
            .as_bytes(),
        );
        Ok(Self {
            principal_id,
            project_id,
            workspace_id,
            run_id,
            edit_digest,
            fence,
            binding,
        })
    }
}

pub struct FeedbackEventStore {
    connection: Connection,
}

impl FeedbackEventStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, FeedbackError> {
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(store_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(store_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(store_error)?;
        connection
            .busy_timeout(Duration::from_millis(100))
            .map_err(store_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS feedback_feeds (
                    authority_binding TEXT PRIMARY KEY,
                    next_cursor INTEGER NOT NULL CHECK (next_cursor > 0)
                 );
                 CREATE TABLE IF NOT EXISTS check_events (
                    authority_binding TEXT NOT NULL,
                    cursor INTEGER NOT NULL CHECK (cursor > 0),
                    event_id TEXT NOT NULL UNIQUE,
                    payload_digest TEXT NOT NULL,
                    principal_id TEXT NOT NULL,
                    project_id TEXT NOT NULL,
                    workspace_id TEXT NOT NULL,
                    payload BLOB NOT NULL,
                    PRIMARY KEY (authority_binding, cursor)
                 );
                 CREATE TABLE IF NOT EXISTS check_lifecycle (
                     authority_binding TEXT NOT NULL,
                     base_revision TEXT NOT NULL,
                     staged_state_digest TEXT NOT NULL,
                     plan_digest TEXT NOT NULL,
                     check_id TEXT NOT NULL,
                     state TEXT NOT NULL CHECK (state IN ('started', 'progress', 'completed', 'failure')),
                     ordinal INTEGER NOT NULL CHECK (ordinal BETWEEN 1 AND 3),
                     transition_digest TEXT NOT NULL,
                     result_digest TEXT,
                     PRIMARY KEY (authority_binding, base_revision, staged_state_digest, plan_digest, check_id)
                 );
                 CREATE TABLE IF NOT EXISTS check_boundaries (
                     authority_binding TEXT NOT NULL,
                     base_revision TEXT NOT NULL,
                     staged_state_digest TEXT NOT NULL,
                     plan_digest TEXT NOT NULL,
                     check_id TEXT NOT NULL,
                     ordinal INTEGER NOT NULL CHECK (ordinal BETWEEN 1 AND 3),
                     transition_digest TEXT NOT NULL,
                     PRIMARY KEY (
                         authority_binding, base_revision, staged_state_digest,
                          plan_digest, check_id, ordinal
                     )
                 );
                 CREATE TABLE IF NOT EXISTS feedback_operations (
                     operation_id TEXT PRIMARY KEY,
                     authority_binding TEXT NOT NULL,
                     edit_digest TEXT NOT NULL,
                     base_revision TEXT NOT NULL,
                     staged_state_digest TEXT NOT NULL,
                     verification_result_digest TEXT NOT NULL,
                     report_reference TEXT,
                     report_length INTEGER,
                     payload_reference TEXT,
                     payload_length INTEGER,
                     successor_revision TEXT
                 );
                 CREATE TABLE IF NOT EXISTS baseline_captures (
                     authority_binding TEXT NOT NULL,
                     edit_digest TEXT NOT NULL,
                     base_revision TEXT NOT NULL,
                     selected_plan_digest TEXT NOT NULL,
                     report_reference TEXT NOT NULL,
                     report_length INTEGER NOT NULL,
                     PRIMARY KEY (
                         authority_binding, edit_digest, base_revision, selected_plan_digest
                     )
                  );
                   CREATE TABLE IF NOT EXISTS pending_feedback (
                       operation_id TEXT PRIMARY KEY,
                      authority_binding TEXT NOT NULL,
                      principal_id TEXT NOT NULL,
                      project_id TEXT NOT NULL,
                      workspace_id TEXT NOT NULL,
                      record_digest TEXT NOT NULL,
                       record BLOB NOT NULL,
                       state TEXT NOT NULL CHECK (state IN ('pending', 'complete'))
                   );
                   CREATE TABLE IF NOT EXISTS pending_feedback_quarantine (
                       pending_rowid INTEGER PRIMARY KEY,
                       reason TEXT NOT NULL
                   );",
            )
            .map_err(store_error)?;
        transaction.commit().map_err(store_error)?;
        Ok(Self { connection })
    }

    fn append(
        &mut self,
        authority: &FeedbackAuthority,
        event: PendingCheckEvent,
        secrets: &[SecretLease],
    ) -> Result<CheckEvent, FeedbackError> {
        let event_type = event.kind.event_type().to_owned();
        let identity = serde_json::to_vec(&(
            &authority.binding,
            &event.operation_id,
            &event_type,
            &event.base_revision,
            &event.staged_state_digest,
            &event.plan_digest,
            &event.check_id,
        ))
        .map_err(serialization_error)?;
        let event_id = digest(&identity);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let mut stored = CheckEvent {
            schema_version: CHECK_EVENT_SCHEMA_VERSION,
            cursor: 0,
            event_id: event_id.clone(),
            event_type: event_type.clone(),
            principal_id: authority.principal_id.clone(),
            project_id: authority.project_id.clone(),
            workspace_id: authority.workspace_id.clone(),
            run_id: authority.run_id.clone(),
            edit_digest: authority.edit_digest.clone(),
            feedback_operation_id: event.operation_id,
            base_revision: event.base_revision,
            staged_state_digest: event.staged_state_digest.clone(),
            successor_revision: None,
            verification_plan_digest: event.plan_digest,
            verification_result_digest: event.result_digest,
            fence: authority.fence,
            check_id: event.check_id,
            status: event.status,
            diagnostic_count: event.diagnostic_count,
            artifacts: event.artifacts,
        };
        let semantic_bytes = serde_json::to_vec(&stored).map_err(serialization_error)?;
        ensure_redacted(&semantic_bytes, secrets)?;
        if let Some((cursor, existing_digest, persisted)) = transaction
            .query_row(
                "SELECT cursor, payload_digest, payload FROM check_events WHERE event_id = ?1",
                [&stored.event_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(store_error)?
        {
            stored.cursor = u64::try_from(cursor).map_err(|_| FeedbackError::EventConflict)?;
            let expected = serde_json::to_vec(&stored).map_err(serialization_error)?;
            if existing_digest != digest(&expected) || persisted != expected {
                return Err(FeedbackError::EventConflict);
            }
            transaction.commit().map_err(store_error)?;
            return Ok(stored);
        }

        apply_lifecycle_transition(&transaction, authority, &event_type, &stored)?;
        let cursor = reserve_cursor(&transaction, authority)?;
        stored.cursor = u64::try_from(cursor).map_err(|_| FeedbackError::EventConflict)?;
        let bytes = serde_json::to_vec(&stored).map_err(serialization_error)?;
        ensure_redacted(&bytes, secrets)?;
        let payload_digest = digest(&bytes);
        transaction
            .execute(
                "INSERT INTO check_events
                 (authority_binding, cursor, event_id, payload_digest, principal_id, project_id,
                  workspace_id, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    authority.binding,
                    cursor,
                    event_id,
                    payload_digest,
                    authority.principal_id,
                    authority.project_id,
                    authority.workspace_id,
                    bytes
                ],
            )
            .map_err(store_error)?;
        transaction.commit().map_err(store_error)?;
        Ok(stored)
    }

    fn record_boundary(
        &mut self,
        authority: &FeedbackAuthority,
        base_revision: &str,
        staged_state_digest: &str,
        observation: VerificationObservation<'_>,
    ) -> Result<u64, FeedbackError> {
        validate_component(base_revision, 128)?;
        if !valid_digest(staged_state_digest) {
            return Err(FeedbackError::InvalidLifecycle);
        }
        validate_component(observation.check_id, 128)?;
        if !valid_digest(observation.plan_digest) {
            return Err(FeedbackError::InvalidLifecycle);
        }
        let next = match observation.boundary {
            VerificationBoundary::Started => "started",
            VerificationBoundary::Progress => "progress",
            VerificationBoundary::Completed => "completed",
            VerificationBoundary::Failure => "failure",
        };
        let ordinal = match observation.boundary {
            VerificationBoundary::Started => 1_i64,
            VerificationBoundary::Progress => 2,
            VerificationBoundary::Completed | VerificationBoundary::Failure => 3,
        };
        let transition_digest = digest(
            &serde_json::to_vec(&(
                &authority.binding,
                base_revision,
                staged_state_digest,
                observation.plan_digest,
                observation.check_id,
                ordinal,
                next,
                observation.status.map(status_name),
            ))
            .map_err(serialization_error)?,
        );
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        if let Some(existing_digest) = transaction
            .query_row(
                "SELECT transition_digest FROM check_boundaries
                 WHERE authority_binding = ?1 AND base_revision = ?2
                   AND staged_state_digest = ?3 AND plan_digest = ?4
                   AND check_id = ?5 AND ordinal = ?6",
                params![
                    authority.binding,
                    base_revision,
                    staged_state_digest,
                    observation.plan_digest,
                    observation.check_id,
                    ordinal
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(store_error)?
        {
            if existing_digest != transition_digest {
                return Err(FeedbackError::EventConflict);
            }
            transaction.commit().map_err(store_error)?;
            return Ok(ordinal as u64);
        }

        let previous = transaction
            .query_row(
                "SELECT state, ordinal FROM check_lifecycle
                 WHERE authority_binding = ?1 AND base_revision = ?2
                   AND staged_state_digest = ?3 AND plan_digest = ?4 AND check_id = ?5",
                params![
                    authority.binding,
                    base_revision,
                    staged_state_digest,
                    observation.plan_digest,
                    observation.check_id
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(store_error)?;
        if !matches!(
            (
                ordinal,
                previous
                    .as_ref()
                    .map(|(state, ordinal)| (state.as_str(), *ordinal))
            ),
            (1, None) | (2, Some(("started", 1))) | (3, Some(("progress", 2)))
        ) {
            return Err(FeedbackError::InvalidLifecycle);
        }
        transaction
            .execute(
                "INSERT INTO check_boundaries
                 (authority_binding, base_revision, staged_state_digest, plan_digest,
                   check_id, ordinal, transition_digest)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    authority.binding,
                    base_revision,
                    staged_state_digest,
                    observation.plan_digest,
                    observation.check_id,
                    ordinal,
                    transition_digest
                ],
            )
            .map_err(store_error)?;
        if previous.is_none() {
            transaction
                .execute(
                    "INSERT INTO check_lifecycle
                 (authority_binding, base_revision, staged_state_digest, plan_digest, check_id,
                   state, ordinal, transition_digest, result_digest)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
                    params![
                        authority.binding,
                        base_revision,
                        staged_state_digest,
                        observation.plan_digest,
                        observation.check_id,
                        next,
                        ordinal,
                        transition_digest
                    ],
                )
                .map_err(store_error)?;
        } else {
            transaction
                .execute(
                    "UPDATE check_lifecycle
                 SET state = ?6, ordinal = ?7, transition_digest = ?8
                 WHERE authority_binding = ?1 AND base_revision = ?2
                   AND staged_state_digest = ?3 AND plan_digest = ?4 AND check_id = ?5",
                    params![
                        authority.binding,
                        base_revision,
                        staged_state_digest,
                        observation.plan_digest,
                        observation.check_id,
                        next,
                        ordinal,
                        transition_digest
                    ],
                )
                .map_err(store_error)?;
        }
        transaction.commit().map_err(store_error)?;
        Ok(ordinal as u64)
    }

    fn validate_result_lifecycle(
        &self,
        authority: &FeedbackAuthority,
        base_revision: &str,
        staged_state_digest: &str,
        plan_digest: &str,
        checks: &[VerificationCheckResult],
    ) -> Result<(), FeedbackError> {
        for check in checks {
            let state = self
                .connection
                .query_row(
                    "SELECT state FROM check_lifecycle
                     WHERE authority_binding = ?1 AND base_revision = ?2
                       AND staged_state_digest = ?3 AND plan_digest = ?4 AND check_id = ?5",
                    params![
                        authority.binding,
                        base_revision,
                        staged_state_digest,
                        plan_digest,
                        check.check_id
                    ],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(store_error)?;
            let expected = if check.status == CheckResultStatus::Pass {
                "completed"
            } else {
                "failure"
            };
            if state.as_deref() != Some(expected) {
                return Err(FeedbackError::PendingLifecycle);
            }
        }
        Ok(())
    }

    fn register_operation(
        &mut self,
        authority: &FeedbackAuthority,
        operation_id: &str,
        base_revision: &str,
        staged_state_digest: &str,
        result_digest: &str,
    ) -> Result<(), FeedbackError> {
        if !valid_digest(operation_id)
            || !valid_digest(staged_state_digest)
            || !valid_digest(result_digest)
        {
            return Err(FeedbackError::EventConflict);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO feedback_operations
                 (operation_id, authority_binding, edit_digest, base_revision,
                  staged_state_digest, verification_result_digest)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    operation_id,
                    authority.binding,
                    authority.edit_digest,
                    base_revision,
                    staged_state_digest,
                    result_digest
                ],
            )
            .map_err(store_error)?;
        let stored = transaction
            .query_row(
                "SELECT authority_binding, edit_digest, base_revision,
                        staged_state_digest, verification_result_digest
                 FROM feedback_operations WHERE operation_id = ?1",
                [operation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(store_error)?;
        if stored
            != (
                authority.binding.clone(),
                authority.edit_digest.clone(),
                base_revision.to_owned(),
                staged_state_digest.to_owned(),
                result_digest.to_owned(),
            )
        {
            return Err(FeedbackError::EventConflict);
        }
        transaction.commit().map_err(store_error)
    }

    fn register_pending(
        &mut self,
        record: &PendingFeedbackRecord,
        secrets: &[SecretLease],
        deadline: Instant,
        budget: &mut FeedbackBudget,
    ) -> Result<(), FeedbackError> {
        ensure_pending_redacted(&record.mapping_bytes, secrets)?;
        validate_pending_limits(record)?;
        let canonical_len = canonical_len(record, deadline)?;
        if canonical_len > record.limits.max_pending_record_bytes {
            return Err(FeedbackError::BudgetExceeded);
        }
        budget.consume_pending_record(canonical_len, &record.limits)?;
        let mut bytes = Vec::with_capacity(canonical_len);
        serde_json::to_writer(&mut bytes, record).map_err(serialization_error)?;
        if bytes.len() != canonical_len {
            return Err(FeedbackError::EventConflict);
        }
        ensure_pending_redacted(&bytes, secrets)?;
        check_deadline(deadline)?;
        validate_pending_record(record, deadline)?;
        let record_digest = digest(&bytes);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO feedback_operations
                 (operation_id, authority_binding, edit_digest, base_revision,
                  staged_state_digest, verification_result_digest)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    record.operation_id,
                    record.authority.binding,
                    record.authority.edit_digest,
                    record.base_revision,
                    record.staged_state_digest,
                    record.verification_receipt.result_digest
                ],
            )
            .map_err(store_error)?;
        let operation = transaction
            .query_row(
                "SELECT authority_binding, edit_digest, base_revision,
                        staged_state_digest, verification_result_digest
                 FROM feedback_operations WHERE operation_id = ?1",
                [&record.operation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(store_error)?;
        if operation
            != (
                record.authority.binding.clone(),
                record.authority.edit_digest.clone(),
                record.base_revision.clone(),
                record.staged_state_digest.clone(),
                record.verification_receipt.result_digest.clone(),
            )
        {
            return Err(FeedbackError::EventConflict);
        }
        transaction
            .execute(
                "INSERT OR IGNORE INTO pending_feedback
                 (operation_id, authority_binding, principal_id, project_id, workspace_id,
                  record_digest, record, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending')",
                params![
                    record.operation_id,
                    record.authority.binding,
                    record.authority.principal_id,
                    record.authority.project_id,
                    record.authority.workspace_id,
                    record_digest,
                    bytes
                ],
            )
            .map_err(store_error)?;
        let stored = transaction
            .query_row(
                "SELECT authority_binding, record_digest,
                        CASE WHEN length(record) <= ?2 THEN record END
                 FROM pending_feedback WHERE operation_id = ?1",
                params![record.operation_id, canonical_len],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .map_err(store_error)?;
        if stored != (record.authority.binding.clone(), record_digest, Some(bytes)) {
            return Err(FeedbackError::EventConflict);
        }
        transaction.commit().map_err(store_error)
    }

    fn pending_recovery_size(
        &self,
        principal_id: &str,
        project_id: &str,
        workspace_id: &str,
    ) -> Result<(usize, usize), FeedbackError> {
        let (count, bytes) = self
            .connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(record)), 0)
                 FROM pending_feedback
                 WHERE state = 'pending' AND principal_id = ?1 AND project_id = ?2
                   AND workspace_id = ?3
                   AND rowid NOT IN (SELECT pending_rowid FROM pending_feedback_quarantine)",
                params![principal_id, project_id, workspace_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(store_error)?;
        Ok((
            usize::try_from(count).map_err(|_| FeedbackError::BudgetExceeded)?,
            usize::try_from(bytes).map_err(|_| FeedbackError::BudgetExceeded)?,
        ))
    }

    fn next_pending_record(
        &mut self,
        principal_id: &str,
        project_id: &str,
        workspace_id: &str,
        limits: &FeedbackLimits,
        secrets: &[SecretLease],
        deadline: Instant,
    ) -> Result<Option<PendingFeedbackRecord>, FeedbackError> {
        check_deadline(deadline)?;
        let row = self
            .connection
            .query_row(
                "SELECT rowid,
                        CASE WHEN length(record_digest) <= 128 THEN record_digest END,
                        length(record),
                        CASE WHEN length(record) <= MIN(?4, ?5) THEN record END
                 FROM pending_feedback
                 WHERE state = 'pending' AND principal_id = ?1 AND project_id = ?2
                   AND workspace_id = ?3
                   AND rowid NOT IN (SELECT pending_rowid FROM pending_feedback_quarantine)
                 ORDER BY operation_id LIMIT 1",
                params![
                    principal_id,
                    project_id,
                    workspace_id,
                    limits.max_pending_record_bytes,
                    limits.max_memory_bytes
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(store_error)?;
        let Some((rowid, record_digest, length, bytes)) = row else {
            return Ok(None);
        };
        let length = usize::try_from(length).map_err(|_| FeedbackError::BudgetExceeded)?;
        let decoded = record_digest
            .zip(bytes)
            .filter(|(_, bytes)| bytes.len() == length)
            .ok_or(FeedbackError::EventConflict)
            .and_then(|(record_digest, bytes)| {
                if length > limits.max_memory_bytes {
                    return Err(FeedbackError::BudgetExceeded);
                }
                check_deadline(deadline)?;
                decode_pending_record(&record_digest, &bytes, secrets, deadline)
            });
        let record = match decoded {
            Ok(record) => record,
            Err(error) => {
                self.quarantine_pending(rowid, &error.to_string())?;
                return Ok(None);
            }
        };
        if record.authority.principal_id != principal_id
            || record.authority.project_id != project_id
            || record.authority.workspace_id != workspace_id
        {
            self.quarantine_pending(rowid, "pending authority scope mismatch")?;
            return Ok(None);
        }
        Ok(Some(record))
    }

    fn quarantine_pending(&mut self, rowid: i64, reason: &str) -> Result<(), FeedbackError> {
        self.connection
            .execute(
                "INSERT OR IGNORE INTO pending_feedback_quarantine (pending_rowid, reason)
                 VALUES (?1, ?2)",
                params![rowid, reason],
            )
            .map(|_| ())
            .map_err(store_error)
    }

    fn complete_pending(&mut self, operation_id: &str) -> Result<(), FeedbackError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        if transaction
            .execute(
                "UPDATE pending_feedback SET state = 'complete' WHERE operation_id = ?1",
                [operation_id],
            )
            .map_err(store_error)?
            != 1
        {
            return Err(FeedbackError::EventConflict);
        }
        transaction.commit().map_err(store_error)
    }

    fn record_operation_artifact(
        &mut self,
        operation_id: &str,
        kind: &str,
        artifact: &OpaqueArtifactRef,
    ) -> Result<(), FeedbackError> {
        let (reference_column, length_column) = match kind {
            "report" => ("report_reference", "report_length"),
            "payload" => ("payload_reference", "payload_length"),
            _ => return Err(FeedbackError::EventConflict),
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let sql = format!(
            "UPDATE feedback_operations
             SET {reference_column} = COALESCE({reference_column}, ?2),
                 {length_column} = COALESCE({length_column}, ?3)
             WHERE operation_id = ?1"
        );
        if transaction
            .execute(
                &sql,
                params![operation_id, artifact.reference, artifact.length],
            )
            .map_err(store_error)?
            != 1
        {
            return Err(FeedbackError::EventConflict);
        }
        let sql = format!(
            "SELECT {reference_column}, {length_column} FROM feedback_operations WHERE operation_id = ?1"
        );
        let stored = transaction
            .query_row(&sql, [operation_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })
            .map_err(store_error)?;
        if stored != (artifact.reference.clone(), artifact.length) {
            return Err(FeedbackError::EventConflict);
        }
        transaction.commit().map_err(store_error)
    }

    fn record_baseline(
        &mut self,
        authority: &FeedbackAuthority,
        context: &crate::workspace::edit::validate::EditOperationContext,
        report: &OpaqueArtifactRef,
    ) -> Result<(), FeedbackError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO baseline_captures
                 (authority_binding, edit_digest, base_revision, selected_plan_digest,
                  report_reference, report_length)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    authority.binding,
                    authority.edit_digest,
                    context.base_revision(),
                    context.selected_plan_digest(),
                    report.reference,
                    report.length
                ],
            )
            .map_err(store_error)?;
        let stored = transaction
            .query_row(
                "SELECT report_reference, report_length FROM baseline_captures
                 WHERE authority_binding = ?1 AND edit_digest = ?2
                   AND base_revision = ?3 AND selected_plan_digest = ?4",
                params![
                    authority.binding,
                    authority.edit_digest,
                    context.base_revision(),
                    context.selected_plan_digest()
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
            )
            .map_err(store_error)?;
        if stored != (report.reference.clone(), report.length) {
            return Err(FeedbackError::EventConflict);
        }
        transaction.commit().map_err(store_error)
    }

    fn baseline_artifact(
        &self,
        authority: &FeedbackAuthority,
        context: &crate::workspace::edit::validate::EditOperationContext,
    ) -> Result<Option<OpaqueArtifactRef>, FeedbackError> {
        self.connection
            .query_row(
                "SELECT report_reference, report_length FROM baseline_captures
                 WHERE authority_binding = ?1 AND edit_digest = ?2
                   AND base_revision = ?3 AND selected_plan_digest = ?4",
                params![
                    authority.binding,
                    authority.edit_digest,
                    context.base_revision(),
                    context.selected_plan_digest()
                ],
                |row| {
                    Ok(OpaqueArtifactRef {
                        reference: row.get(0)?,
                        length: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(store_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn attach_successor(
        &mut self,
        authority: &FeedbackAuthority,
        operation_id: &str,
        result_digest: &str,
        successor_revision: &str,
        secrets: &[SecretLease],
        limits: &FeedbackLimits,
        deadline: Instant,
    ) -> Result<CheckEvent, FeedbackError> {
        validate_component(successor_revision, 128)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let existing_successor = transaction
            .query_row(
                "SELECT successor_revision FROM feedback_operations
                 WHERE operation_id = ?1 AND authority_binding = ?2
                   AND verification_result_digest = ?3",
                params![operation_id, authority.binding, result_digest],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(store_error)?
            .ok_or(FeedbackError::EventConflict)?;
        if existing_successor
            .as_deref()
            .is_some_and(|value| value != successor_revision)
        {
            return Err(FeedbackError::EventConflict);
        }
        let (record_digest, record_bytes) = transaction
            .query_row(
                "SELECT record_digest,
                        CASE WHEN length(record) <= MIN(?2, ?3) THEN record END
                 FROM pending_feedback WHERE operation_id = ?1",
                params![
                    operation_id,
                    limits.max_pending_record_bytes,
                    limits.max_memory_bytes
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
            )
            .map_err(store_error)?;
        let record_bytes = record_bytes.ok_or(FeedbackError::BudgetExceeded)?;
        let record = decode_pending_record(&record_digest, &record_bytes, secrets, deadline)?;
        if record.authority.binding != authority.binding
            || record.verification_receipt.result_digest != result_digest
        {
            return Err(FeedbackError::EventConflict);
        }
        let event_type = "feedback.successor_attached".to_owned();
        let event_id = digest(
            &serde_json::to_vec(&(
                &authority.binding,
                operation_id,
                &event_type,
                successor_revision,
            ))
            .map_err(serialization_error)?,
        );
        let mut event = CheckEvent {
            schema_version: CHECK_EVENT_SCHEMA_VERSION,
            cursor: 0,
            event_id: event_id.clone(),
            event_type,
            principal_id: authority.principal_id.clone(),
            project_id: authority.project_id.clone(),
            workspace_id: authority.workspace_id.clone(),
            run_id: authority.run_id.clone(),
            edit_digest: authority.edit_digest.clone(),
            feedback_operation_id: operation_id.to_owned(),
            base_revision: record.base_revision,
            staged_state_digest: record.staged_state_digest,
            successor_revision: Some(successor_revision.to_owned()),
            verification_plan_digest: record.verification_receipt.plan_digest,
            verification_result_digest: result_digest.to_owned(),
            fence: authority.fence,
            check_id: String::new(),
            status: "attached".to_owned(),
            diagnostic_count: 0,
            artifacts: Vec::new(),
        };
        if let Some((cursor, payload_digest, payload)) = transaction
            .query_row(
                "SELECT cursor, payload_digest, payload FROM check_events WHERE event_id = ?1",
                [&event_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(store_error)?
        {
            event.cursor = u64::try_from(cursor).map_err(|_| FeedbackError::EventConflict)?;
            let expected = serde_json::to_vec(&event).map_err(serialization_error)?;
            if payload_digest != digest(&expected) || payload != expected {
                return Err(FeedbackError::EventConflict);
            }
            transaction.commit().map_err(store_error)?;
            return Ok(event);
        }
        let cursor = reserve_cursor(&transaction, authority)?;
        event.cursor = u64::try_from(cursor).map_err(|_| FeedbackError::EventConflict)?;
        let payload = serde_json::to_vec(&event).map_err(serialization_error)?;
        let payload_digest = digest(&payload);
        transaction
            .execute(
                "UPDATE feedback_operations SET successor_revision = ?2 WHERE operation_id = ?1",
                params![operation_id, successor_revision],
            )
            .map_err(store_error)?;
        transaction
            .execute(
                "INSERT INTO check_events
                 (authority_binding, cursor, event_id, payload_digest, principal_id, project_id,
                  workspace_id, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    authority.binding,
                    cursor,
                    event_id,
                    payload_digest,
                    authority.principal_id,
                    authority.project_id,
                    authority.workspace_id,
                    payload
                ],
            )
            .map_err(store_error)?;
        transaction.commit().map_err(store_error)?;
        Ok(event)
    }

    pub fn events(
        &self,
        authenticated: &AuthenticatedPrincipal,
        authority: &FeedbackAuthority,
        artifacts: &ArtifactStore,
        after: u64,
    ) -> Result<Vec<CheckEvent>, FeedbackError> {
        let grants = authenticated.grant_snapshot();
        if !grants.grants().contains(&Grant::WorkspaceRead)
            || authority.principal_id != grants.principal_id().to_string()
            || authority.project_id != grants.project_id().to_string()
        {
            return Err(FeedbackError::AuthorityRequired);
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT cursor, payload FROM check_events
                 WHERE authority_binding = ?1 AND principal_id = ?2 AND project_id = ?3
                   AND workspace_id = ?4 AND cursor > ?5
                 ORDER BY cursor",
            )
            .map_err(store_error)?;
        let rows = statement
            .query_map(
                params![
                    authority.binding,
                    authority.principal_id,
                    authority.project_id,
                    authority.workspace_id,
                    after
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .map_err(store_error)?;
        rows.map(|row| {
            let (cursor, bytes) = row.map_err(store_error)?;
            let event: CheckEvent = serde_json::from_slice(&bytes).map_err(serialization_error)?;
            if event.cursor != u64::try_from(cursor).map_err(|_| FeedbackError::EventConflict)? {
                return Err(FeedbackError::EventConflict);
            }
            for opaque in &event.artifacts {
                let reference = ArtifactReference::parse(&opaque.reference)
                    .map_err(|_| FeedbackError::UnauthenticatedArtifact)?;
                let artifact = artifacts
                    .resolve_reference(authenticated, reference)
                    .map_err(|_| FeedbackError::UnauthenticatedArtifact)?;
                if artifact.manifest().size != opaque.length
                    || artifact.manifest().class != ArtifactClass::Report
                {
                    return Err(FeedbackError::UnauthenticatedArtifact);
                }
            }
            Ok(event)
        })
        .collect()
    }
}

fn decode_pending_record(
    record_digest: &str,
    bytes: &[u8],
    secrets: &[SecretLease],
    deadline: Instant,
) -> Result<PendingFeedbackRecord, FeedbackError> {
    check_deadline(deadline)?;
    ensure_pending_redacted(bytes, secrets)?;
    check_deadline(deadline)?;
    if digest(bytes) != record_digest {
        return Err(FeedbackError::EventConflict);
    }
    let record: PendingFeedbackRecord =
        serde_json::from_slice(bytes).map_err(serialization_error)?;
    if !canonical_eq(&record, bytes, deadline)? {
        return Err(FeedbackError::EventConflict);
    }
    ensure_pending_redacted(&record.mapping_bytes, secrets)?;
    validate_pending_record(&record, deadline)?;
    Ok(record)
}

fn canonical_len<T: Serialize>(value: &T, deadline: Instant) -> Result<usize, FeedbackError> {
    let mut writer = CountingWriter { bytes: 0, deadline };
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        if error.io_error_kind() == Some(io::ErrorKind::TimedOut) {
            FeedbackError::BudgetExceeded
        } else {
            serialization_error(error)
        }
    })?;
    Ok(writer.bytes)
}

struct CountingWriter {
    bytes: usize,
    deadline: Instant,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() > self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "feedback serialization deadline exceeded",
            ));
        }
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("feedback serialization length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn canonical_eq<T: Serialize>(
    value: &T,
    expected: &[u8],
    deadline: Instant,
) -> Result<bool, FeedbackError> {
    let mut writer = EqualityWriter {
        expected,
        offset: 0,
        equal: true,
        deadline,
    };
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        if error.io_error_kind() == Some(io::ErrorKind::TimedOut) {
            FeedbackError::BudgetExceeded
        } else {
            serialization_error(error)
        }
    })?;
    Ok(writer.equal && writer.offset == expected.len())
}

struct EqualityWriter<'a> {
    expected: &'a [u8],
    offset: usize,
    equal: bool,
    deadline: Instant,
}

impl Write for EqualityWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() > self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "feedback canonical comparison deadline exceeded",
            ));
        }
        let end = self.offset.saturating_add(bytes.len());
        self.equal &= self.expected.get(self.offset..end) == Some(bytes);
        self.offset = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn reserve_cursor(
    transaction: &rusqlite::Transaction<'_>,
    authority: &FeedbackAuthority,
) -> Result<i64, FeedbackError> {
    transaction
        .execute(
            "INSERT OR IGNORE INTO feedback_feeds (authority_binding, next_cursor) VALUES (?1, 1)",
            [&authority.binding],
        )
        .map_err(store_error)?;
    let cursor = transaction
        .query_row(
            "SELECT next_cursor FROM feedback_feeds WHERE authority_binding = ?1",
            [&authority.binding],
            |row| row.get(0),
        )
        .map_err(store_error)?;
    transaction
        .execute(
            "UPDATE feedback_feeds SET next_cursor = next_cursor + 1 WHERE authority_binding = ?1",
            [&authority.binding],
        )
        .map_err(store_error)?;
    Ok(cursor)
}

pub struct FeedbackVerificationObserver<'a> {
    events: &'a mut FeedbackEventStore,
    authority: &'a FeedbackAuthority,
    base_revision: String,
    staged_state_digest: String,
}

impl<'a> FeedbackVerificationObserver<'a> {
    pub fn new(
        events: &'a mut FeedbackEventStore,
        authority: &'a FeedbackAuthority,
        staged: &crate::workspace::edit::stage::StagedEdit<'_>,
    ) -> Self {
        Self::from_context(
            events,
            authority,
            staged.operation_context(),
            staged.state_digest(),
        )
    }

    pub(crate) fn from_context(
        events: &'a mut FeedbackEventStore,
        authority: &'a FeedbackAuthority,
        context: &crate::workspace::edit::validate::EditOperationContext,
        staged_state_digest: &str,
    ) -> Self {
        Self {
            events,
            authority,
            base_revision: context.base_revision().to_owned(),
            staged_state_digest: staged_state_digest.to_owned(),
        }
    }
}

impl VerificationObserver for FeedbackVerificationObserver<'_> {
    fn observe(
        &mut self,
        observation: VerificationObservation<'_>,
    ) -> Result<(), VerificationError> {
        self.events
            .record_boundary(
                self.authority,
                &self.base_revision,
                &self.staged_state_digest,
                observation,
            )
            .map(|_| ())
            .map_err(|error| VerificationError::Observer(error.to_string()))
    }
}

fn apply_lifecycle_transition(
    transaction: &rusqlite::Transaction<'_>,
    authority: &FeedbackAuthority,
    event_type: &str,
    event: &CheckEvent,
) -> Result<(), FeedbackError> {
    let state = transaction
        .query_row(
            "SELECT state FROM check_lifecycle
             WHERE authority_binding = ?1 AND base_revision = ?2
               AND staged_state_digest = ?3 AND plan_digest = ?4 AND check_id = ?5",
            params![
                authority.binding,
                event.base_revision,
                event.staged_state_digest,
                event.verification_plan_digest,
                event.check_id
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(store_error)?;
    let next = match (event_type, state.as_deref()) {
        ("check.started", None) => "started",
        ("check.progress", Some("started" | "progress")) => "progress",
        ("check.completed", Some("started" | "progress")) => "completed",
        ("check.failure", Some("started" | "progress")) => "failure",
        ("check.started" | "check.progress", Some("completed" | "failure")) => return Ok(()),
        ("check.completed", Some("completed")) | ("check.failure", Some("failure")) => {
            if let Some(result_digest) = transaction
                .query_row(
                    "SELECT result_digest FROM check_lifecycle
                     WHERE authority_binding = ?1 AND base_revision = ?2
                       AND staged_state_digest = ?3 AND plan_digest = ?4 AND check_id = ?5",
                    params![
                        authority.binding,
                        event.base_revision,
                        event.staged_state_digest,
                        event.verification_plan_digest,
                        event.check_id
                    ],
                    |row| row.get::<_, Option<String>>(0),
                )
                .map_err(store_error)?
                && result_digest != event.verification_result_digest
            {
                return Err(FeedbackError::EventConflict);
            }
            transaction
                .execute(
                    "UPDATE check_lifecycle SET result_digest = ?6
                     WHERE authority_binding = ?1 AND base_revision = ?2
                       AND staged_state_digest = ?3 AND plan_digest = ?4 AND check_id = ?5",
                    params![
                        authority.binding,
                        event.base_revision,
                        event.staged_state_digest,
                        event.verification_plan_digest,
                        event.check_id,
                        event.verification_result_digest
                    ],
                )
                .map_err(store_error)?;
            return Ok(());
        }
        _ => return Err(FeedbackError::InvalidLifecycle),
    };
    if state.is_none() {
        transaction
            .execute(
                "INSERT INTO check_lifecycle
                  (authority_binding, base_revision, staged_state_digest, plan_digest, check_id,
                   state, ordinal, transition_digest, result_digest)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, NULL)",
                params![
                    authority.binding,
                    event.base_revision,
                    event.staged_state_digest,
                    event.verification_plan_digest,
                    event.check_id,
                    next,
                    digest(event.event_id.as_bytes())
                ],
            )
            .map_err(store_error)?;
    } else {
        let result_digest = matches!(next, "completed" | "failure")
            .then_some(event.verification_result_digest.as_str());
        transaction
            .execute(
                "UPDATE check_lifecycle SET state = ?6, result_digest = ?7
                 WHERE authority_binding = ?1 AND base_revision = ?2
                   AND staged_state_digest = ?3 AND plan_digest = ?4 AND check_id = ?5",
                params![
                    authority.binding,
                    event.base_revision,
                    event.staged_state_digest,
                    event.verification_plan_digest,
                    event.check_id,
                    next,
                    result_digest
                ],
            )
            .map_err(store_error)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingCheckEvent {
    kind: CheckEventKind,
    operation_id: String,
    base_revision: String,
    staged_state_digest: String,
    plan_digest: String,
    result_digest: String,
    check_id: String,
    status: String,
    diagnostic_count: u64,
    artifacts: Vec<OpaqueArtifactRef>,
}

pub struct FeedbackPipeline<'a> {
    artifacts: &'a ArtifactStore,
    events: &'a mut FeedbackEventStore,
    authenticated: &'a AuthenticatedPrincipal,
    workspace_id: String,
    retention: ArtifactRetention,
    stored_at_unix_micros: i64,
    secrets: &'a [SecretLease],
    limits: FeedbackLimits,
}

pub struct FeedbackOutput {
    pub feedback_operation_id: String,
    pub payload: FeedbackPayload,
    pub payload_artifact: OpaqueArtifactRef,
    pub report_artifact: OpaqueArtifactRef,
    pub events: Vec<CheckEvent>,
}

pub struct BaselineCapture {
    set: DiagnosticSet,
    context: crate::workspace::edit::validate::EditOperationContext,
    authority_binding: String,
    completed_cursor: u64,
    usage: FeedbackBudget,
    elapsed: Duration,
    pub report_artifact: OpaqueArtifactRef,
}

#[derive(Clone, Debug, Default)]
struct FeedbackBudget {
    input_bytes: usize,
    diagnostics: usize,
    memory_bytes: usize,
    pending_records: usize,
    pending_bytes: usize,
}

impl FeedbackBudget {
    fn consume_input(
        &mut self,
        bytes: usize,
        limits: &FeedbackLimits,
    ) -> Result<(), FeedbackError> {
        self.input_bytes = self
            .input_bytes
            .checked_add(bytes)
            .ok_or(FeedbackError::BudgetExceeded)?;
        self.consume_memory(bytes, limits)?;
        if self.input_bytes > limits.max_input_bytes {
            return Err(FeedbackError::BudgetExceeded);
        }
        Ok(())
    }

    fn consume_diagnostics(
        &mut self,
        count: usize,
        memory: usize,
        limits: &FeedbackLimits,
    ) -> Result<(), FeedbackError> {
        self.diagnostics = self
            .diagnostics
            .checked_add(count)
            .ok_or(FeedbackError::DiagnosticLimitExceeded)?;
        if self.diagnostics > limits.max_diagnostics {
            return Err(FeedbackError::DiagnosticLimitExceeded);
        }
        self.consume_memory(memory, limits)
    }

    fn consume_memory(
        &mut self,
        bytes: usize,
        limits: &FeedbackLimits,
    ) -> Result<(), FeedbackError> {
        self.memory_bytes = self
            .memory_bytes
            .checked_add(bytes)
            .ok_or(FeedbackError::BudgetExceeded)?;
        if self.memory_bytes > limits.max_memory_bytes {
            return Err(FeedbackError::BudgetExceeded);
        }
        Ok(())
    }

    fn consume_pending_bytes(
        &mut self,
        bytes: usize,
        limits: &FeedbackLimits,
    ) -> Result<(), FeedbackError> {
        self.pending_bytes = self
            .pending_bytes
            .checked_add(bytes)
            .ok_or(FeedbackError::BudgetExceeded)?;
        if self.pending_bytes > limits.max_pending_bytes {
            return Err(FeedbackError::BudgetExceeded);
        }
        self.consume_memory(bytes, limits)
    }

    fn consume_pending_record(
        &mut self,
        bytes: usize,
        limits: &FeedbackLimits,
    ) -> Result<(), FeedbackError> {
        self.pending_records = self
            .pending_records
            .checked_add(1)
            .ok_or(FeedbackError::BudgetExceeded)?;
        if self.pending_records > limits.max_pending_records {
            return Err(FeedbackError::BudgetExceeded);
        }
        self.consume_pending_bytes(bytes, limits)
    }

    fn reserve_pending_recovery(
        &mut self,
        records: usize,
        bytes: usize,
        limits: &FeedbackLimits,
    ) -> Result<(), FeedbackError> {
        self.pending_records = records;
        self.pending_bytes = bytes;
        if records > limits.max_pending_records || bytes > limits.max_pending_bytes {
            return Err(FeedbackError::BudgetExceeded);
        }
        Ok(())
    }
}

impl BaselineCapture {
    pub fn revision(&self) -> &str {
        &self.set.revision
    }

    pub fn diagnostic_count(&self) -> usize {
        self.set.diagnostics.len()
    }
}

impl<'a> FeedbackPipeline<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        artifacts: &'a ArtifactStore,
        events: &'a mut FeedbackEventStore,
        authenticated: &'a AuthenticatedPrincipal,
        workspace_id: impl Into<String>,
        retention: ArtifactRetention,
        stored_at_unix_micros: i64,
        secrets: &'a [SecretLease],
        limits: FeedbackLimits,
    ) -> Result<Self, FeedbackError> {
        limits.validate()?;
        let workspace_id = workspace_id.into();
        validate_component(&workspace_id, 128)?;
        if !authenticated
            .grant_snapshot()
            .grants()
            .iter()
            .any(|grant| matches!(grant, Grant::WorkspaceWrite | Grant::VerificationTargeted))
        {
            return Err(FeedbackError::AuthorityRequired);
        }
        Ok(Self {
            artifacts,
            events,
            authenticated,
            workspace_id,
            retention,
            stored_at_unix_micros,
            secrets,
            limits,
        })
    }

    pub fn process(
        &mut self,
        authority: &FeedbackAuthority,
        baseline: Option<&BaselineCapture>,
        outcome: &crate::workspace::edit::stage::VerificationOutcome<'_>,
        adapters: &BTreeMap<String, DiagnosticAdapter>,
    ) -> Result<FeedbackOutput, FeedbackError> {
        self.process_result(
            authority,
            baseline,
            outcome.operation_context(),
            outcome.staged_state_digest(),
            outcome.verification(),
            outcome.staged().feedback_mapping(),
            adapters,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn process_result(
        &mut self,
        authority: &FeedbackAuthority,
        baseline: Option<&BaselineCapture>,
        context: &crate::workspace::edit::validate::EditOperationContext,
        staged_state_digest: &str,
        result: &VerificationResult,
        mapping: &EditMapping,
        adapters: &BTreeMap<String, DiagnosticAdapter>,
    ) -> Result<FeedbackOutput, FeedbackError> {
        self.authenticate_authority(authority)?;
        let base_revision = context.base_revision();
        let operation_id = feedback_operation_id(
            authority,
            context.selected_plan_digest(),
            staged_state_digest,
            result,
        )?;
        let baseline = baseline.filter(|baseline| {
            baseline.authority_binding == authority.binding
                && baseline.completed_cursor > 0
                && baseline.context == *context
                && baseline.set.revision == base_revision
        });
        let recovered_baseline = if baseline.is_none() {
            self.recover_baseline(authority, context)?
        } else {
            None
        };
        let baseline = baseline.or(recovered_baseline.as_ref());
        let spent = baseline.map_or(Duration::ZERO, |baseline| baseline.elapsed);
        let deadline = deadline_after(&self.limits, spent)?;
        self.authenticate_result(result)?;
        let capture = baseline;
        let baseline = capture.map(|baseline| &baseline.set);
        let mut usage = capture
            .map(|baseline| baseline.usage.clone())
            .unwrap_or_default();
        let (current, logs) = self.collect_result(
            staged_state_digest,
            result.checks(),
            adapters,
            &self.limits,
            deadline,
            &mut usage,
        )?;
        let mapping_len = canonical_len(mapping, deadline)?;
        if mapping_len > self.limits.max_mapping_bytes {
            return Err(FeedbackError::BudgetExceeded);
        }
        usage.consume_pending_bytes(mapping_len, &self.limits)?;
        let mut mapping_bytes = Vec::with_capacity(mapping_len);
        serde_json::to_writer(&mut mapping_bytes, mapping).map_err(serialization_error)?;
        if mapping_bytes.len() != mapping_len {
            return Err(FeedbackError::EventConflict);
        }
        ensure_pending_redacted(&mapping_bytes, self.secrets)?;
        validate_mapping_bounded(mapping, self.limits.max_path_bytes)?;
        let comparison = compare_mapped_diagnostics(baseline, &current, mapping)?;
        let report = DiagnosticReport {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            baseline: comparison.baseline.clone(),
            baseline_set: baseline.cloned(),
            current_set: current.clone(),
            deltas: comparison.deltas.clone(),
        };
        let report_bytes = self.canonical_redacted(&report)?;
        if report_bytes.len() > self.limits.max_report_bytes {
            return Err(FeedbackError::BudgetExceeded);
        }
        let report_artifact = target_artifact(&operation_id, "report", report_bytes.len());
        let required_failures = result
            .checks()
            .iter()
            .filter(|check| check.required && check.status != CheckResultStatus::Pass)
            .map(|check| RequiredFailure {
                check_id: check.check_id.clone(),
                status: status_name(check.status).to_owned(),
                exit_code: check.exit_code,
            })
            .collect::<Vec<_>>();
        let input_bytes = current
            .sanitized_input_bytes
            .checked_add(baseline.map_or(0, |baseline| baseline.sanitized_input_bytes))
            .ok_or(FeedbackError::BudgetExceeded)?;
        let payload = render_feedback(
            &comparison,
            baseline.map_or(0, |baseline| baseline.diagnostics.len()),
            current.diagnostics.len(),
            input_bytes,
            &required_failures,
            report_artifact.clone(),
            logs.clone(),
            self.limits.max_feedback_bytes,
        )?;
        let payload_bytes = self.canonical_redacted(&payload)?;
        let payload_artifact = target_artifact(&operation_id, "payload", payload_bytes.len());
        validate_adapters(adapters, &self.limits)?;
        let adapters_bytes = serde_json::to_vec(adapters).map_err(serialization_error)?;
        let expected_event_count = result
            .checks()
            .len()
            .checked_mul(3)
            .ok_or(FeedbackError::BudgetExceeded)?;
        if expected_event_count > self.limits.max_pending_events {
            return Err(FeedbackError::BudgetExceeded);
        }
        let record = PendingFeedbackRecord {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            authority: authority.into(),
            operation_id: operation_id.clone(),
            base_revision: base_revision.to_owned(),
            staged_state_digest: staged_state_digest.to_owned(),
            verification_receipt: result.receipt(),
            baseline_report: capture.map(|capture| capture.report_artifact.clone()),
            baseline_compatibility: comparison.baseline,
            mapping_digest: digest(&mapping_bytes),
            mapping_bytes,
            adapters: adapters.clone(),
            adapters_digest: digest(&adapters_bytes),
            limits: self.limits.clone(),
            retention: self.retention.into(),
            stored_at_unix_micros: self.stored_at_unix_micros,
            report_artifact: report_artifact.clone(),
            payload_artifact: payload_artifact.clone(),
            expected_events: pending_result_events(
                &operation_id,
                base_revision,
                staged_state_digest,
                result.plan_digest(),
                result.result_digest(),
                result.checks(),
                &current,
                &report_artifact,
                &payload_artifact,
            ),
        };
        self.events
            .register_pending(&record, self.secrets, deadline, &mut usage)?;
        feedback_crashpoint("pending_record");
        feedback_crashpoint("result_artifact");
        check_deadline(deadline)?;
        self.complete_pending_record(&record, deadline)
    }

    pub fn recover_pending(&mut self) -> Result<Vec<FeedbackOutput>, FeedbackError> {
        let grants = self.authenticated.grant_snapshot();
        let principal = self.authenticated.principal_id().to_string();
        let project = grants.project_id().to_string();
        let deadline = deadline(&self.limits)?;
        let (record_count, record_bytes) =
            self.events
                .pending_recovery_size(&principal, &project, &self.workspace_id)?;
        let mut budget = FeedbackBudget::default();
        budget.reserve_pending_recovery(record_count, record_bytes, &self.limits)?;
        let mut outputs = Vec::with_capacity(record_count);
        for _ in 0..record_count {
            check_deadline(deadline)?;
            let Some(record) = self.events.next_pending_record(
                &principal,
                &project,
                &self.workspace_id,
                &self.limits,
                self.secrets,
                deadline,
            )?
            else {
                continue;
            };
            outputs.push(self.complete_pending_record(&record, deadline)?);
        }
        Ok(outputs)
    }

    fn complete_pending_record(
        &mut self,
        record: &PendingFeedbackRecord,
        operation_deadline: Instant,
    ) -> Result<FeedbackOutput, FeedbackError> {
        check_deadline(operation_deadline)?;
        if record.schema_version != FEEDBACK_SCHEMA_VERSION
            || !valid_digest(&record.operation_id)
            || !valid_digest(&record.staged_state_digest)
            || digest(&record.mapping_bytes) != record.mapping_digest
            || digest(&serde_json::to_vec(&record.adapters).map_err(serialization_error)?)
                != record.adapters_digest
        {
            return Err(FeedbackError::EventConflict);
        }
        record.limits.validate()?;
        let authority = record.authority.resolve()?;
        self.authenticate_authority(&authority)?;
        let principal = self.authenticated.principal_id().to_string();
        let project = self.authenticated.grant_snapshot().project_id().to_string();
        record
            .verification_receipt
            .validate_artifacts(self.artifacts, &principal, &project)
            .map_err(|_| FeedbackError::UnauthenticatedResult)?;
        let result_bytes = self.open_authenticated(
            &record.verification_receipt.result_artifact.reference,
            usize::try_from(record.verification_receipt.result_artifact.length)
                .map_err(|_| FeedbackError::BudgetExceeded)?,
            ArtifactClass::Report,
        )?;
        let result: RecoveredVerificationResult =
            serde_json::from_slice(&result_bytes).map_err(serialization_error)?;
        if result.plan_digest != record.verification_receipt.plan_digest
            || result.binding_digest != record.verification_receipt.binding_digest
            || result.provenance != record.verification_receipt.provenance
            || result.evidence_digest != record.verification_receipt.evidence_digest
            || digest(&result_bytes) != record.verification_receipt.result_digest
        {
            return Err(FeedbackError::UnauthenticatedResult);
        }
        ensure_pending_redacted(&record.mapping_bytes, self.secrets)?;
        let mapping: EditMapping =
            serde_json::from_slice(&record.mapping_bytes).map_err(serialization_error)?;
        if serde_json::to_vec(&mapping).map_err(serialization_error)? != record.mapping_bytes {
            return Err(FeedbackError::InvalidMapping);
        }
        validate_mapping_bounded(&mapping, record.limits.max_path_bytes)?;
        let baseline_report = record
            .baseline_report
            .as_ref()
            .map(|artifact| self.open_feedback_artifact::<DiagnosticReport>(artifact))
            .transpose()?;
        let baseline = baseline_report
            .as_ref()
            .and_then(|report| report.baseline_set.as_ref());
        if baseline.is_some_and(|set| set.revision != record.base_revision) {
            return Err(FeedbackError::EventConflict);
        }
        let deadline = deadline(&record.limits)?.min(operation_deadline);
        let mut usage = FeedbackBudget::default();
        if let Some(set) = baseline {
            usage.consume_input(
                usize::try_from(set.sanitized_input_bytes)
                    .map_err(|_| FeedbackError::BudgetExceeded)?,
                &record.limits,
            )?;
            usage.consume_diagnostics(
                set.diagnostics.len(),
                serde_json::to_vec(&set.diagnostics)
                    .map_err(serialization_error)?
                    .len(),
                &record.limits,
            )?;
        }
        let (current, logs) = self.collect_result(
            &record.staged_state_digest,
            &result.checks,
            &record.adapters,
            &record.limits,
            deadline,
            &mut usage,
        )?;
        let comparison = compare_mapped_diagnostics(baseline, &current, &mapping)?;
        if comparison.baseline != record.baseline_compatibility {
            return Err(FeedbackError::EventConflict);
        }
        let report = DiagnosticReport {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            baseline: comparison.baseline.clone(),
            baseline_set: baseline.cloned(),
            current_set: current.clone(),
            deltas: comparison.deltas.clone(),
        };
        let report_bytes = self.canonical_redacted(&report)?;
        if report_bytes.len() > record.limits.max_report_bytes
            || target_artifact(&record.operation_id, "report", report_bytes.len())
                != record.report_artifact
        {
            return Err(FeedbackError::EventConflict);
        }
        let report_artifact = self.persist_with_metadata(
            &report_bytes,
            &record.operation_id,
            "report",
            record.retention.resolve(),
            record.stored_at_unix_micros,
            deadline,
        )?;
        feedback_crashpoint("report_artifact");
        self.events
            .record_operation_artifact(&record.operation_id, "report", &report_artifact)?;
        feedback_crashpoint("report_reference");
        let required_failures = result
            .checks
            .iter()
            .filter(|check| check.required && check.status != CheckResultStatus::Pass)
            .map(|check| RequiredFailure {
                check_id: check.check_id.clone(),
                status: status_name(check.status).to_owned(),
                exit_code: check.exit_code,
            })
            .collect::<Vec<_>>();
        let input_bytes = current
            .sanitized_input_bytes
            .checked_add(baseline.map_or(0, |set| set.sanitized_input_bytes))
            .ok_or(FeedbackError::BudgetExceeded)?;
        let payload = render_feedback(
            &comparison,
            baseline.map_or(0, |set| set.diagnostics.len()),
            current.diagnostics.len(),
            input_bytes,
            &required_failures,
            report_artifact.clone(),
            logs,
            record.limits.max_feedback_bytes,
        )?;
        let payload_bytes = self.canonical_redacted(&payload)?;
        if target_artifact(&record.operation_id, "payload", payload_bytes.len())
            != record.payload_artifact
        {
            return Err(FeedbackError::EventConflict);
        }
        let payload_artifact = self.persist_with_metadata(
            &payload_bytes,
            &record.operation_id,
            "payload",
            record.retention.resolve(),
            record.stored_at_unix_micros,
            deadline,
        )?;
        feedback_crashpoint("payload_artifact");
        self.events.record_operation_artifact(
            &record.operation_id,
            "payload",
            &payload_artifact,
        )?;
        feedback_crashpoint("payload_reference");
        let expected = pending_result_events(
            &record.operation_id,
            &record.base_revision,
            &record.staged_state_digest,
            &result.plan_digest,
            &record.verification_receipt.result_digest,
            &result.checks,
            &current,
            &report_artifact,
            &payload_artifact,
        );
        if expected != record.expected_events {
            return Err(FeedbackError::EventConflict);
        }
        self.reconcile_result_lifecycle(
            &authority,
            &record.base_revision,
            &record.staged_state_digest,
            &result.plan_digest,
            &result.checks,
        )?;
        self.events.validate_result_lifecycle(
            &authority,
            &record.base_revision,
            &record.staged_state_digest,
            &result.plan_digest,
            &result.checks,
        )?;
        let mut events = Vec::with_capacity(expected.len());
        for event in expected {
            check_deadline(deadline)?;
            let point = match event.kind {
                CheckEventKind::Started => "event.started",
                CheckEventKind::Progress => "event.progress",
                CheckEventKind::Completed => "event.completed",
                CheckEventKind::Failure => "event.failure",
            };
            events.push(self.events.append(&authority, event, self.secrets)?);
            feedback_crashpoint(point);
        }
        self.events.complete_pending(&record.operation_id)?;
        feedback_crashpoint("pending_complete");
        Ok(FeedbackOutput {
            feedback_operation_id: record.operation_id.clone(),
            payload,
            payload_artifact,
            report_artifact,
            events,
        })
    }

    pub fn capture_baseline(
        &mut self,
        authority: &FeedbackAuthority,
        context: &crate::workspace::edit::validate::EditOperationContext,
        result: &VerificationResult,
        adapters: &BTreeMap<String, DiagnosticAdapter>,
    ) -> Result<BaselineCapture, FeedbackError> {
        self.authenticate_authority(authority)?;
        let started = Instant::now();
        let deadline = deadline(&self.limits)?;
        self.authenticate_result(result)?;
        let mut usage = FeedbackBudget::default();
        let (set, _) = self.collect_result(
            context.base_revision(),
            result.checks(),
            adapters,
            &self.limits,
            deadline,
            &mut usage,
        )?;
        let report = DiagnosticReport {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            baseline: BaselineAvailability::Available,
            baseline_set: Some(set.clone()),
            current_set: set.clone(),
            deltas: Vec::new(),
        };
        let bytes = self.canonical_redacted(&report)?;
        if bytes.len() > self.limits.max_report_bytes {
            return Err(FeedbackError::BudgetExceeded);
        }
        let baseline_operation_id = digest(
            &serde_json::to_vec(&(
                "baseline",
                &authority.binding,
                &authority.edit_digest,
                context.base_revision(),
                context.selected_plan_digest(),
                result.result_digest(),
            ))
            .map_err(serialization_error)?,
        );
        self.events.register_operation(
            authority,
            &baseline_operation_id,
            context.base_revision(),
            context.base_workspace_digest(),
            result.result_digest(),
        )?;
        let report_artifact =
            self.persist(&bytes, &baseline_operation_id, "baseline-report", deadline)?;
        self.events.record_operation_artifact(
            &baseline_operation_id,
            "report",
            &report_artifact,
        )?;
        self.events
            .record_baseline(authority, context, &report_artifact)?;
        Ok(BaselineCapture {
            set,
            context: context.clone(),
            authority_binding: authority.binding.clone(),
            completed_cursor: 1,
            usage,
            elapsed: started.elapsed(),
            report_artifact,
        })
    }

    fn authenticate_authority(&self, authority: &FeedbackAuthority) -> Result<(), FeedbackError> {
        let grants = self.authenticated.grant_snapshot();
        if authority.principal_id != grants.principal_id().to_string()
            || authority.project_id != grants.project_id().to_string()
            || authority.workspace_id != self.workspace_id
        {
            return Err(FeedbackError::AuthorityRequired);
        }
        Ok(())
    }

    fn authenticate_result(&self, result: &VerificationResult) -> Result<(), FeedbackError> {
        if !result.verify_digest() {
            return Err(FeedbackError::UnauthenticatedResult);
        }
        result
            .receipt()
            .validate_artifacts(
                self.artifacts,
                &self.authenticated.principal_id().to_string(),
                &self.authenticated.grant_snapshot().project_id().to_string(),
            )
            .map_err(|_| FeedbackError::UnauthenticatedResult)
    }

    fn recover_baseline(
        &self,
        authority: &FeedbackAuthority,
        context: &crate::workspace::edit::validate::EditOperationContext,
    ) -> Result<Option<BaselineCapture>, FeedbackError> {
        let Some(report_artifact) = self.events.baseline_artifact(authority, context)? else {
            return Ok(None);
        };
        let report: DiagnosticReport = self.open_feedback_artifact(&report_artifact)?;
        let set = report
            .baseline_set
            .filter(|set| set.revision == context.base_revision())
            .ok_or(FeedbackError::EventConflict)?;
        let diagnostic_memory = serde_json::to_vec(&set.diagnostics)
            .map_err(serialization_error)?
            .len();
        Ok(Some(BaselineCapture {
            usage: FeedbackBudget {
                input_bytes: usize::try_from(set.sanitized_input_bytes)
                    .map_err(|_| FeedbackError::BudgetExceeded)?,
                diagnostics: set.diagnostics.len(),
                memory_bytes: diagnostic_memory,
                ..FeedbackBudget::default()
            },
            set,
            context: context.clone(),
            authority_binding: authority.binding.clone(),
            completed_cursor: 1,
            elapsed: Duration::ZERO,
            report_artifact,
        }))
    }

    fn open_feedback_artifact<T: for<'de> Deserialize<'de>>(
        &self,
        opaque: &OpaqueArtifactRef,
    ) -> Result<T, FeedbackError> {
        let length = usize::try_from(opaque.length).map_err(|_| FeedbackError::BudgetExceeded)?;
        let bytes = self.open_authenticated(&opaque.reference, length, ArtifactClass::Report)?;
        serde_json::from_slice(&bytes).map_err(serialization_error)
    }

    pub fn attach_materialization(
        &mut self,
        authority: &FeedbackAuthority,
        output: &FeedbackOutput,
        materialized: &crate::workspace::edit::recovery::MaterializedEdit,
    ) -> Result<(), FeedbackError> {
        self.authenticate_authority(authority)?;
        self.ensure_redacted(materialized.revision().id().to_string().as_bytes())?;
        let deadline = deadline(&self.limits)?;
        self.events.attach_successor(
            authority,
            &output.feedback_operation_id,
            &materialized.verification_receipt().result_digest,
            &materialized.revision().id().to_string(),
            self.secrets,
            &self.limits,
            deadline,
        )?;
        Ok(())
    }

    fn reconcile_result_lifecycle(
        &mut self,
        authority: &FeedbackAuthority,
        base_revision: &str,
        staged_state_digest: &str,
        plan_digest: &str,
        checks: &[VerificationCheckResult],
    ) -> Result<(), FeedbackError> {
        for check in checks {
            for (boundary, status) in [
                (VerificationBoundary::Started, None),
                (VerificationBoundary::Progress, None),
                (
                    if check.status == CheckResultStatus::Pass {
                        VerificationBoundary::Completed
                    } else {
                        VerificationBoundary::Failure
                    },
                    Some(check.status),
                ),
            ] {
                self.events.record_boundary(
                    authority,
                    base_revision,
                    staged_state_digest,
                    VerificationObservation {
                        plan_digest,
                        check_id: &check.check_id,
                        boundary,
                        status,
                    },
                )?;
                feedback_crashpoint(match boundary {
                    VerificationBoundary::Started => "lifecycle.started",
                    VerificationBoundary::Progress => "lifecycle.progress",
                    VerificationBoundary::Completed => "lifecycle.completed",
                    VerificationBoundary::Failure => "lifecycle.failure",
                });
            }
        }
        Ok(())
    }

    fn collect_result(
        &self,
        revision: &str,
        checks: &[VerificationCheckResult],
        adapters: &BTreeMap<String, DiagnosticAdapter>,
        limits: &FeedbackLimits,
        deadline: Instant,
        budget: &mut FeedbackBudget,
    ) -> Result<(DiagnosticSet, Vec<OpaqueArtifactRef>), FeedbackError> {
        validate_component(revision, 128)?;
        let mut set = DiagnosticSet {
            revision: revision.to_owned(),
            checks: Vec::new(),
            diagnostics: Vec::new(),
            malformed_records: 0,
            oversized_records: 0,
            sanitized_input_bytes: 0,
        };
        let mut logs = Vec::new();
        for check in checks {
            check_deadline(deadline)?;
            validate_component(&check.check_id, 128)?;
            let adapter = adapters
                .get(&check.check_id)
                .copied()
                .ok_or(FeedbackError::UntrustedAdapter)?;
            let process = self.process_evidence(check, deadline)?;
            set.checks.push(CheckFingerprint {
                check_id: check.check_id.clone(),
                class: check_class_name(check.class).to_owned(),
                required: check.required,
                executor_version: CHECK_EXECUTOR_CONTRACT_VERSION,
                adapter,
                adapter_version: adapter.version(),
                image_digest: process.as_ref().map(|process| process.image_digest.clone()),
                tool_digest: process.as_ref().map(|process| process.tool_digest.clone()),
                config_digest: process
                    .as_ref()
                    .map(|process| process.config_digest.clone()),
            });
            let tool = process.as_ref().map_or(check.check_id.as_str(), |process| {
                process.tool_digest.as_str()
            });
            for (reference, length) in [
                (&check.stdout_artifact, check.stdout_length),
                (&check.stderr_artifact, check.stderr_length),
            ] {
                let (Some(reference), Some(length)) = (reference, length) else {
                    continue;
                };
                if logs.len() >= limits.max_log_references {
                    return Err(FeedbackError::BudgetExceeded);
                }
                let length = usize::try_from(length).map_err(|_| FeedbackError::BudgetExceeded)?;
                budget.consume_input(length, limits)?;
                let bytes = self.open_authenticated(reference, length, ArtifactClass::Log)?;
                let sanitized =
                    CaptureRedactor::new(self.secrets).sanitize(CaptureBoundary::Artifact, &bytes);
                let sanitized = sanitized
                    .bytes()
                    .map_err(|_| FeedbackError::UnsanitizedArtifact)?;
                if sanitized != bytes {
                    return Err(FeedbackError::UnsanitizedArtifact);
                }
                let mut parsed = parse_diagnostics(
                    adapter,
                    &check.check_id,
                    tool,
                    sanitized,
                    &FeedbackLimits {
                        max_input_bytes: length.max(1),
                        max_diagnostics: limits.max_diagnostics.saturating_sub(budget.diagnostics),
                        ..limits.clone()
                    },
                )?;
                let diagnostic_memory = serde_json::to_vec(&parsed.diagnostics)
                    .map_err(serialization_error)?
                    .len();
                budget.consume_diagnostics(parsed.diagnostics.len(), diagnostic_memory, limits)?;
                set.diagnostics.append(&mut parsed.diagnostics);
                set.malformed_records = set
                    .malformed_records
                    .checked_add(parsed.malformed_records)
                    .ok_or(FeedbackError::BudgetExceeded)?;
                set.oversized_records = set
                    .oversized_records
                    .checked_add(parsed.oversized_records)
                    .ok_or(FeedbackError::BudgetExceeded)?;
                set.sanitized_input_bytes = set
                    .sanitized_input_bytes
                    .checked_add(length as u64)
                    .ok_or(FeedbackError::BudgetExceeded)?;
                logs.push(OpaqueArtifactRef {
                    reference: reference.clone(),
                    length: length as u64,
                });
            }
        }
        set.checks.sort();
        set.diagnostics.sort_by_key(NormalizedDiagnostic::identity);
        Ok((set, logs))
    }

    fn process_evidence(
        &self,
        check: &crate::verify::profiles::VerificationCheckResult,
        deadline: Instant,
    ) -> Result<Option<CheckProcessEvidence>, FeedbackError> {
        if check.launch == CheckLaunchStatus::NotStarted {
            return Ok(None);
        }
        check_deadline(deadline)?;
        let reference = check
            .process_artifact
            .as_deref()
            .ok_or(FeedbackError::UnauthenticatedResult)?;
        let length = usize::try_from(
            check
                .process_artifact_length
                .ok_or(FeedbackError::UnauthenticatedResult)?,
        )
        .map_err(|_| FeedbackError::BudgetExceeded)?;
        if length > 64 * 1024 {
            return Err(FeedbackError::BudgetExceeded);
        }
        let bytes = self.open_authenticated(reference, length, ArtifactClass::Report)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(serialization_error)
    }

    fn open_authenticated(
        &self,
        reference: &str,
        length: usize,
        class: ArtifactClass,
    ) -> Result<Vec<u8>, FeedbackError> {
        let reference = ArtifactReference::parse(reference)
            .map_err(|_| FeedbackError::UnauthenticatedArtifact)?;
        let artifact = self
            .artifacts
            .open_reference(reference)
            .map_err(|_| FeedbackError::UnauthenticatedArtifact)?;
        if artifact.manifest().size != length as u64
            || artifact.manifest().class != class
            || artifact.manifest().principal != self.authenticated.principal_id().to_string()
            || artifact.manifest().project
                != self.authenticated.grant_snapshot().project_id().to_string()
        {
            return Err(FeedbackError::UnauthenticatedArtifact);
        }
        self.artifacts
            .open_bytes_bounded(artifact.digest(), length)
            .map_err(|_| FeedbackError::UnauthenticatedArtifact)
    }

    fn persist(
        &self,
        bytes: &[u8],
        operation_id: &str,
        kind: &str,
        deadline: Instant,
    ) -> Result<OpaqueArtifactRef, FeedbackError> {
        self.persist_with_metadata(
            bytes,
            operation_id,
            kind,
            self.retention,
            self.stored_at_unix_micros,
            deadline,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_with_metadata(
        &self,
        bytes: &[u8],
        operation_id: &str,
        kind: &str,
        retention: ArtifactRetention,
        stored_at_unix_micros: i64,
        deadline: Instant,
    ) -> Result<OpaqueArtifactRef, FeedbackError> {
        check_deadline(deadline)?;
        self.ensure_redacted(bytes)?;
        let principal = self.authenticated.principal_id().to_string();
        let project = self.authenticated.grant_snapshot().project_id().to_string();
        let metadata = ArtifactMetadata::new(
            "application/json",
            ArtifactClass::Report,
            &principal,
            &project,
            retention,
            stored_at_unix_micros,
        )
        .map_err(artifact_error)?;
        let reference = ArtifactReference::derive(
            b"kit-feedback-artifact-reference-v1",
            format!("{operation_id}\0{kind}").as_bytes(),
        );
        match self.artifacts.open_reference(reference) {
            Ok(existing) => {
                let manifest = existing.manifest();
                let stored = self
                    .artifacts
                    .open_bytes_bounded(existing.digest(), bytes.len())
                    .map_err(artifact_error)?;
                if stored != bytes
                    || manifest.size != bytes.len() as u64
                    || manifest.media_type != metadata.media_type
                    || manifest.class != metadata.class
                    || manifest.principal != metadata.principal
                    || manifest.project != metadata.project
                    || manifest.retention != metadata.retention
                    || manifest.stored_at_unix_micros != metadata.stored_at_unix_micros
                {
                    return Err(FeedbackError::EventConflict);
                }
                return Ok(OpaqueArtifactRef {
                    reference: reference.to_string(),
                    length: manifest.size,
                });
            }
            Err(crate::store::artifacts::ArtifactError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(artifact_error(error)),
        }
        let artifact = self
            .artifacts
            .stage_chunks_with_reference_before([bytes], bytes.len(), metadata, reference, deadline)
            .and_then(|staged| staged.promote_pending_before(deadline))
            .and_then(|pending| pending.commit_before(deadline))
            .map_err(artifact_error)?;
        Ok(OpaqueArtifactRef {
            reference: artifact.reference().to_string(),
            length: artifact.manifest().size,
        })
    }

    fn canonical_redacted<T: Serialize>(&self, value: &T) -> Result<Vec<u8>, FeedbackError> {
        let bytes = serde_json::to_vec(value).map_err(serialization_error)?;
        self.ensure_redacted(&bytes)?;
        Ok(bytes)
    }

    fn ensure_redacted(&self, bytes: &[u8]) -> Result<(), FeedbackError> {
        ensure_redacted(bytes, self.secrets)
    }
}

fn ensure_redacted(bytes: &[u8], secrets: &[SecretLease]) -> Result<(), FeedbackError> {
    let sanitized = CaptureRedactor::new(secrets).sanitize(CaptureBoundary::Artifact, bytes);
    let sanitized = sanitized
        .bytes()
        .map_err(|_| FeedbackError::UnsanitizedArtifact)?;
    if sanitized == bytes {
        Ok(())
    } else {
        Err(FeedbackError::UnsanitizedArtifact)
    }
}

fn ensure_pending_redacted(bytes: &[u8], secrets: &[SecretLease]) -> Result<(), FeedbackError> {
    let sanitized = CaptureRedactor::new(secrets).sanitize(CaptureBoundary::Artifact, bytes);
    match sanitized.bytes() {
        Ok(sanitized) if sanitized == bytes => Ok(()),
        _ => Err(FeedbackError::SecretDetected),
    }
}

fn target_artifact(operation_id: &str, kind: &str, length: usize) -> OpaqueArtifactRef {
    OpaqueArtifactRef {
        reference: ArtifactReference::derive(
            b"kit-feedback-artifact-reference-v1",
            format!("{operation_id}\0{kind}").as_bytes(),
        )
        .to_string(),
        length: length as u64,
    }
}

#[cfg(any(test, debug_assertions))]
fn feedback_crashpoint(point: &str) {
    if std::env::var("KIT_FEEDBACK_CRASH_POINT").as_deref() == Ok(point) {
        #[cfg(unix)]
        unsafe {
            libc::_exit(86);
        }
        #[cfg(not(unix))]
        std::process::exit(86);
    }
}

#[cfg(not(any(test, debug_assertions)))]
fn feedback_crashpoint(_: &str) {}

#[allow(clippy::too_many_arguments)]
fn pending_result_events(
    operation_id: &str,
    base_revision: &str,
    staged_state_digest: &str,
    plan_digest: &str,
    result_digest: &str,
    checks: &[VerificationCheckResult],
    diagnostics: &DiagnosticSet,
    report: &OpaqueArtifactRef,
    payload: &OpaqueArtifactRef,
) -> Vec<PendingCheckEvent> {
    let mut events = Vec::with_capacity(checks.len() * 3);
    for check in checks {
        let diagnostic_count = diagnostics
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.check_id == check.check_id)
            .count() as u64;
        let terminal = if check.status == CheckResultStatus::Pass {
            CheckEventKind::Completed
        } else {
            CheckEventKind::Failure
        };
        for kind in [CheckEventKind::Started, CheckEventKind::Progress, terminal] {
            events.push(PendingCheckEvent {
                kind,
                operation_id: operation_id.to_owned(),
                base_revision: base_revision.to_owned(),
                staged_state_digest: staged_state_digest.to_owned(),
                plan_digest: plan_digest.to_owned(),
                result_digest: result_digest.to_owned(),
                check_id: check.check_id.clone(),
                status: status_name(check.status).to_owned(),
                diagnostic_count,
                artifacts: vec![report.clone(), payload.clone()],
            });
        }
    }
    events
}

fn status_name(status: CheckResultStatus) -> &'static str {
    match status {
        CheckResultStatus::Pass => "pass",
        CheckResultStatus::Nonzero => "nonzero",
        CheckResultStatus::Unavailable => "unavailable",
        CheckResultStatus::Timeout => "timeout",
        CheckResultStatus::Cancelled => "cancelled",
        CheckResultStatus::Rejected => "rejected",
        CheckResultStatus::NotQuiescent => "not_quiescent",
        CheckResultStatus::ProtocolFailure => "protocol_failure",
    }
}

fn check_class_name(class: crate::verify::profiles::CheckClass) -> &'static str {
    match class {
        crate::verify::profiles::CheckClass::Syntax => "syntax",
        crate::verify::profiles::CheckClass::Diagnostics => "diagnostics",
        crate::verify::profiles::CheckClass::Typecheck => "typecheck",
        crate::verify::profiles::CheckClass::Targeted => "targeted",
        crate::verify::profiles::CheckClass::Full => "full",
    }
}

fn validate_mapping(mapping: &EditMapping) -> Result<(), FeedbackError> {
    validate_mapping_bounded(mapping, FeedbackLimits::default().max_path_bytes)
}

fn validate_mapping_bounded(
    mapping: &EditMapping,
    max_path_bytes: usize,
) -> Result<(), FeedbackError> {
    let mut before = BTreeSet::new();
    let mut after = BTreeSet::new();
    for path in &mapping.paths {
        if !valid_path(&path.before, max_path_bytes)
            || !valid_path(&path.after, max_path_bytes)
            || !before.insert(&path.before)
            || !after.insert(&path.after)
        {
            return Err(FeedbackError::InvalidMapping);
        }
    }
    let mut before_ranges = BTreeMap::<&str, Vec<(u32, u32)>>::new();
    let mut after_ranges = BTreeMap::<&str, Vec<(u32, u32)>>::new();
    for lines in &mapping.lines {
        let before_end = lines
            .line_count
            .checked_sub(1)
            .and_then(|count| lines.before_start.checked_add(count));
        let after_end = lines
            .line_count
            .checked_sub(1)
            .and_then(|count| lines.after_start.checked_add(count));
        if !valid_path(&lines.before_path, max_path_bytes)
            || !valid_path(&lines.after_path, max_path_bytes)
            || lines.before_start == 0
            || lines.after_start == 0
            || lines.line_count == 0
            || before_end.is_none()
            || after_end.is_none()
        {
            return Err(FeedbackError::InvalidMapping);
        }
        before_ranges
            .entry(&lines.before_path)
            .or_default()
            .push((lines.before_start, before_end.expect("checked above")));
        after_ranges
            .entry(&lines.after_path)
            .or_default()
            .push((lines.after_start, after_end.expect("checked above")));
    }
    if before_ranges
        .values_mut()
        .chain(after_ranges.values_mut())
        .any(|ranges| {
            ranges.sort_unstable();
            ranges.windows(2).any(|pair| pair[0].1 >= pair[1].0)
        })
    {
        return Err(FeedbackError::InvalidMapping);
    }
    let mut changed_ranges = BTreeMap::<&str, Vec<(u32, u32)>>::new();
    for changed in &mapping.changed_lines {
        if !valid_path(&changed.path, max_path_bytes)
            || changed.start_line == 0
            || changed.end_line < changed.start_line
        {
            return Err(FeedbackError::InvalidMapping);
        }
        changed_ranges
            .entry(&changed.path)
            .or_default()
            .push((changed.start_line, changed.end_line));
    }
    if changed_ranges.values_mut().any(|ranges| {
        ranges.sort_unstable();
        ranges.windows(2).any(|pair| pair[0].1 >= pair[1].0)
    }) {
        return Err(FeedbackError::InvalidMapping);
    }
    Ok(())
}

fn validate_adapters(
    adapters: &BTreeMap<String, DiagnosticAdapter>,
    limits: &FeedbackLimits,
) -> Result<(), FeedbackError> {
    if adapters.len() > limits.max_adapters {
        return Err(FeedbackError::BudgetExceeded);
    }
    for check_id in adapters.keys() {
        validate_component(check_id, 128)?;
    }
    Ok(())
}

fn validate_pending_limits(record: &PendingFeedbackRecord) -> Result<(), FeedbackError> {
    record.limits.validate()?;
    if record.mapping_bytes.len() > record.limits.max_mapping_bytes
        || record.adapters.len() > record.limits.max_adapters
        || record.expected_events.len() > record.limits.max_pending_events
    {
        return Err(FeedbackError::BudgetExceeded);
    }
    Ok(())
}

fn validate_pending_record(
    record: &PendingFeedbackRecord,
    deadline: Instant,
) -> Result<(), FeedbackError> {
    validate_pending_limits(record)?;
    validate_adapters(&record.adapters, &record.limits)?;
    validate_component(&record.authority.principal_id, 128)?;
    validate_component(&record.authority.project_id, 128)?;
    validate_component(&record.authority.workspace_id, 128)?;
    validate_component(&record.authority.run_id, 128)?;
    validate_component(&record.base_revision, 128)?;
    if !valid_digest(&record.operation_id)
        || !valid_digest(&record.staged_state_digest)
        || !valid_digest(&record.mapping_digest)
        || !valid_digest(&record.adapters_digest)
    {
        return Err(FeedbackError::EventConflict);
    }
    if record
        .verification_receipt
        .stdout_artifacts
        .len()
        .checked_add(record.verification_receipt.stderr_artifacts.len())
        .is_none_or(|count| count > record.limits.max_log_references)
        || record.verification_receipt.process_artifacts.len() > record.limits.max_adapters
        || record.verification_receipt.selected_check_count > record.limits.max_adapters
    {
        return Err(FeedbackError::BudgetExceeded);
    }
    if digest(&record.mapping_bytes) != record.mapping_digest
        || digest(&serde_json::to_vec(&record.adapters).map_err(serialization_error)?)
            != record.adapters_digest
    {
        return Err(FeedbackError::EventConflict);
    }
    let mapping: EditMapping =
        serde_json::from_slice(&record.mapping_bytes).map_err(serialization_error)?;
    if !canonical_eq(&mapping, &record.mapping_bytes, deadline)? {
        return Err(FeedbackError::InvalidMapping);
    }
    validate_mapping_bounded(&mapping, record.limits.max_path_bytes)?;
    record.authority.resolve()?;
    for event in &record.expected_events {
        validate_component(&event.check_id, 128)?;
        validate_component(&event.status, 64)?;
        validate_component(&event.base_revision, 128)?;
        if event.artifacts.len() > 2 {
            return Err(FeedbackError::BudgetExceeded);
        }
        for artifact in &event.artifacts {
            validate_artifact_ref(artifact)?;
        }
    }
    Ok(())
}

fn validate_artifact_ref(reference: &OpaqueArtifactRef) -> Result<(), FeedbackError> {
    ArtifactReference::parse(&reference.reference)
        .map(|_| ())
        .map_err(|_| FeedbackError::UnauthenticatedArtifact)
}

fn validate_component(value: &str, max: usize) -> Result<(), FeedbackError> {
    if valid_component(value, max) {
        Ok(())
    } else {
        Err(FeedbackError::InvalidDiagnostic)
    }
}

fn valid_component(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
}

fn valid_path(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.is_ascii()
        && !value.starts_with('/')
        && !value.contains(['\\', ':'])
        && value
            .split('/')
            .all(|part| !part.is_empty() && !matches!(part, "." | ".."))
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("blake3:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn feedback_operation_id(
    authority: &FeedbackAuthority,
    selected_plan_digest: &str,
    staged_state_digest: &str,
    result: &VerificationResult,
) -> Result<String, FeedbackError> {
    Ok(digest(
        &serde_json::to_vec(&(
            "kit-feedback-operation-v1",
            &authority.binding,
            &authority.edit_digest,
            selected_plan_digest,
            staged_state_digest,
            result.result_digest(),
            result.provenance(),
        ))
        .map_err(serialization_error)?,
    ))
}

fn deadline(limits: &FeedbackLimits) -> Result<Instant, FeedbackError> {
    Instant::now()
        .checked_add(limits.max_operation_time)
        .ok_or(FeedbackError::BudgetExceeded)
}

fn deadline_after(limits: &FeedbackLimits, spent: Duration) -> Result<Instant, FeedbackError> {
    let remaining = limits
        .max_operation_time
        .checked_sub(spent)
        .ok_or(FeedbackError::BudgetExceeded)?;
    Instant::now()
        .checked_add(remaining)
        .ok_or(FeedbackError::BudgetExceeded)
}

fn check_deadline(deadline: Instant) -> Result<(), FeedbackError> {
    if Instant::now() > deadline {
        Err(FeedbackError::BudgetExceeded)
    } else {
        Ok(())
    }
}

fn store_error(error: rusqlite::Error) -> FeedbackError {
    FeedbackError::EventPersistence(error.to_string())
}

fn artifact_error(error: crate::store::artifacts::ArtifactError) -> FeedbackError {
    FeedbackError::ArtifactPersistence(error.to_string())
}

fn serialization_error(error: serde_json::Error) -> FeedbackError {
    FeedbackError::Serialization(error.to_string())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeedbackError {
    InvalidLimits,
    InvalidDiagnostic,
    InvalidMapping,
    BudgetExceeded,
    DiagnosticLimitExceeded,
    UntrustedAdapter,
    SecretDetected,
    UnsanitizedArtifact,
    UnauthenticatedArtifact,
    UnauthenticatedResult,
    AuthorityRequired,
    EventConflict,
    InvalidLifecycle,
    PendingLifecycle,
    Serialization(String),
    ArtifactPersistence(String),
    EventPersistence(String),
}

impl fmt::Display for FeedbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("invalid feedback limits"),
            Self::InvalidDiagnostic => formatter.write_str("invalid normalized diagnostic"),
            Self::InvalidMapping => formatter.write_str("invalid edit path or line mapping"),
            Self::BudgetExceeded => formatter.write_str("feedback operation exceeded its budget"),
            Self::DiagnosticLimitExceeded => {
                formatter.write_str("diagnostic count exceeded its bound")
            }
            Self::UntrustedAdapter => formatter.write_str("no trusted diagnostic adapter selected"),
            Self::SecretDetected => {
                formatter.write_str("secret detected in canonical pending feedback")
            }
            Self::UnsanitizedArtifact => {
                formatter.write_str("check artifact did not cross the redaction boundary")
            }
            Self::UnauthenticatedArtifact => {
                formatter.write_str("feedback artifact ownership or metadata is invalid")
            }
            Self::UnauthenticatedResult => {
                formatter.write_str("verification result or receipt is unauthenticated")
            }
            Self::AuthorityRequired => formatter.write_str("feedback event authority is required"),
            Self::EventConflict => formatter.write_str("check event idempotency conflict"),
            Self::InvalidLifecycle => formatter.write_str("invalid durable check lifecycle"),
            Self::PendingLifecycle => {
                formatter.write_str("verification lifecycle is pending canonical reconciliation")
            }
            Self::Serialization(error) => {
                write!(formatter, "feedback serialization failed: {error}")
            }
            Self::ArtifactPersistence(error) => {
                write!(formatter, "feedback artifact persistence failed: {error}")
            }
            Self::EventPersistence(error) => {
                write!(formatter, "feedback event persistence failed: {error}")
            }
        }
    }
}

impl std::error::Error for FeedbackError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
        domain::{
            config::Grant,
            ids::{PrincipalId, ProjectId},
        },
    };

    fn line(path: &str, line: u32, message: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "path": path,
            "range": {"start_line": line, "start_column": 1, "end_line": line, "end_column": 2},
            "code": "E1",
            "message": message,
            "severity": "error",
            "tool": "tool"
        }))
        .unwrap()
    }

    fn set(revision: &str, bytes: &[u8]) -> DiagnosticSet {
        let mut set = parse_diagnostics(
            DiagnosticAdapter::NormalizedJsonLinesV1,
            "check",
            "tool",
            bytes,
            &FeedbackLimits::default(),
        )
        .unwrap();
        set.revision = revision.to_owned();
        set
    }

    fn pending_record(authenticated: &AuthenticatedPrincipal) -> PendingFeedbackRecord {
        let authority = FeedbackAuthority::issue(
            authenticated,
            "workspace-pending",
            "run-pending",
            digest(b"pending-edit"),
            1,
        )
        .unwrap();
        let mapping_bytes = serde_json::to_vec(&EditMapping::default()).unwrap();
        let adapters = BTreeMap::new();
        let operation_id = digest(b"pending-operation");
        PendingFeedbackRecord {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            authority: (&authority).into(),
            operation_id: operation_id.clone(),
            base_revision: "revision-pending".into(),
            staged_state_digest: digest(b"pending-stage"),
            verification_receipt: VerificationReceipt {
                version: crate::verify::profiles::VERIFICATION_PROFILE_VERSION,
                schema_digest: digest(b"schema"),
                plan_digest: digest(b"plan"),
                binding_digest: digest(b"binding"),
                provenance: digest(b"provenance"),
                result_digest: digest(b"result"),
                evidence_digest: digest(b"evidence"),
                experiment_identity: crate::domain::config::GRAMMAR_EDIT_EXPERIMENT_ID.to_owned(),
                experiment_digest: format!("sha256:{}", "0".repeat(64)),
                model_outcome: None,
                result_artifact: crate::verify::profiles::VerificationArtifactReference {
                    reference: format!("artifact-ref:{}", "1".repeat(64)),
                    length: 1,
                },
                stdout_artifacts: Vec::new(),
                stderr_artifacts: Vec::new(),
                process_artifacts: Vec::new(),
                selected_check_count: 0,
                process_artifact_count: 0,
            },
            baseline_report: None,
            baseline_compatibility: BaselineAvailability::Unavailable(
                BaselineUnavailableReason::Missing,
            ),
            mapping_digest: digest(&mapping_bytes),
            mapping_bytes,
            adapters_digest: digest(&serde_json::to_vec(&adapters).unwrap()),
            adapters,
            limits: FeedbackLimits::default(),
            retention: PendingRetention::Forever,
            stored_at_unix_micros: 1,
            report_artifact: target_artifact(&operation_id, "report", 1),
            payload_artifact: target_artifact(&operation_id, "payload", 1),
            expected_events: Vec::new(),
        }
    }

    fn rebind(authority: &mut PendingFeedbackAuthority) {
        authority.binding = digest(
            format!(
                "{}\0{}\0{}\0{}\0{}\0{}",
                authority.principal_id,
                authority.project_id,
                authority.workspace_id,
                authority.run_id,
                authority.edit_digest,
                authority.fence
            )
            .as_bytes(),
        );
    }

    #[test]
    fn red_baseline_maps_moves_and_never_invents_causality() {
        let before = set("r1", &line("src/old.rs", 4, "old"));
        let after = set("r2", &line("src/new.rs", 14, "new"));
        let mapping = EditMapping {
            lines: vec![LineMove {
                before_path: "src/old.rs".into(),
                after_path: "src/new.rs".into(),
                before_start: 4,
                after_start: 14,
                line_count: 1,
            }],
            changed_lines: vec![ChangedLineRange {
                path: "src/new.rs".into(),
                start_line: 14,
                end_line: 14,
            }],
            ..EditMapping::default()
        };
        let compared = compare_mapped_diagnostics(Some(&before), &after, &mapping).unwrap();
        assert_eq!(compared.deltas[0].kind, DiagnosticDeltaKind::Changed);
        assert!(compared.deltas[0].changed_line);

        let unavailable = compare_mapped_diagnostics(None, &after, &mapping).unwrap();
        assert_eq!(unavailable.deltas[0].kind, DiagnosticDeltaKind::Observed);
    }

    #[test]
    fn event_store_replays_ids_and_preserves_cursor_order_across_restart() {
        let root = std::env::temp_dir().join(format!(
            "kit-feedback-events-{}-{}",
            std::process::id(),
            blake3::hash(b"event-test").to_hex()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let path = root.join("events.sqlite3");
        let grants = GrantSnapshot::new(
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        );
        let authenticated = AuthenticatedPrincipal::from_grants(grants);
        let authority = FeedbackAuthority::issue(
            &authenticated,
            "workspace_0000000000000000000000",
            "run_00000000000000000000000000",
            digest(b"edit"),
            7,
        )
        .unwrap();
        let pending = |kind| PendingCheckEvent {
            kind,
            operation_id: digest(b"operation"),
            base_revision: "r1".into(),
            staged_state_digest: digest(b"stage"),
            plan_digest: digest(b"plan"),
            result_digest: digest(b"result"),
            check_id: "check".into(),
            status: "pass".into(),
            diagnostic_count: 0,
            artifacts: Vec::new(),
        };
        let mut initial = FeedbackEventStore::open(&path).unwrap();
        let first = initial
            .append(&authority, pending(CheckEventKind::Started), &[])
            .unwrap();
        for kind in [CheckEventKind::Progress, CheckEventKind::Completed] {
            initial.append(&authority, pending(kind), &[]).unwrap();
        }
        drop(initial);
        let mut restarted = FeedbackEventStore::open(&path).unwrap();
        let replay = restarted
            .append(&authority, pending(CheckEventKind::Started), &[])
            .unwrap();
        assert_eq!(first, replay);
        let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
        let events = restarted
            .events(&authenticated, &authority, &artifacts, 0)
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().map(|event| event.cursor).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["check.started", "check.progress", "check.completed"]
        );
        let mut conflict = pending(CheckEventKind::Started);
        conflict.status = "nonzero".into();
        assert_eq!(
            restarted.append(&authority, conflict, &[]),
            Err(FeedbackError::EventConflict)
        );
        let mut leaked = pending(CheckEventKind::Started);
        leaked.check_id = "leak-check".into();
        leaked.status = "EVENT_CANARY".into();
        assert_eq!(
            restarted.append(
                &authority,
                leaked,
                &[SecretLease::new(b"EVENT_CANARY".to_vec())],
            ),
            Err(FeedbackError::UnsanitizedArtifact)
        );
        let mut next = pending(CheckEventKind::Started);
        next.check_id = "other-check".into();
        assert_eq!(restarted.append(&authority, next, &[]).unwrap().cursor, 4);
        let schema: String = restarted
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'check_events'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!schema.contains("AUTOINCREMENT"));

        let other_grants = GrantSnapshot::new(
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        );
        let other = AuthenticatedPrincipal::from_grants(other_grants);
        assert_eq!(
            restarted.events(&other, &authority, &artifacts, 0),
            Err(FeedbackError::AuthorityRequired)
        );
        for entry in std::fs::read_dir(&root).unwrap().map(Result::unwrap) {
            if entry.file_type().unwrap().is_file() {
                assert!(
                    !std::fs::read(entry.path())
                        .unwrap()
                        .windows(b"EVENT_CANARY".len())
                        .any(|window| window == b"EVENT_CANARY")
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_canaries_are_rejected_before_db_or_wal_persistence() {
        let root = std::env::temp_dir().join(format!(
            "kit-feedback-pending-canary-{}-{}",
            std::process::id(),
            blake3::hash(b"pending-canary-test").to_hex()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let path = root.join("events.sqlite3");
        let grants = GrantSnapshot::new(
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        );
        let authenticated = AuthenticatedPrincipal::from_grants(grants);
        let mut store = FeedbackEventStore::open(&path).unwrap();
        let secret = b"top+/secret";
        let leases = [
            SecretLease::new(secret.to_vec()),
            SecretLease::new(vec![0, 255, 1, 2, 3, 128]),
        ];
        let base = pending_record(&authenticated);
        let mut records = Vec::new();

        let mut mapping = base.clone();
        mapping.mapping_bytes = serde_json::to_vec(&EditMapping {
            paths: vec![PathMove {
                before: "src/top+/secret.rs".into(),
                after: "src/new.rs".into(),
            }],
            ..EditMapping::default()
        })
        .unwrap();
        mapping.mapping_digest = digest(&mapping.mapping_bytes);
        records.push(mapping);

        let mut authority = base.clone();
        authority.authority.run_id = "run-top%2B%2Fsecret".into();
        rebind(&mut authority.authority);
        records.push(authority);

        let mut adapter = base.clone();
        adapter.adapters.insert(
            "dG9wKy9zZWNyZXQ=".into(),
            DiagnosticAdapter::NormalizedJsonLinesV1,
        );
        adapter.adapters_digest = digest(&serde_json::to_vec(&adapter.adapters).unwrap());
        records.push(adapter);

        let mut receipt = base.clone();
        receipt.verification_receipt.provenance = "prefix-top+/secret-suffix".into();
        records.push(receipt);

        let mut event = base.clone();
        event.expected_events.push(PendingCheckEvent {
            kind: CheckEventKind::Started,
            operation_id: event.operation_id.clone(),
            base_revision: event.base_revision.clone(),
            staged_state_digest: event.staged_state_digest.clone(),
            plan_digest: digest(b"event-plan"),
            result_digest: digest(b"event-result"),
            check_id: "top+/secret".into(),
            status: "pass".into(),
            diagnostic_count: 0,
            artifacts: Vec::new(),
        });
        records.push(event);

        let mut binary = base;
        binary.mapping_bytes = vec![0, 255, 1, 2, 3, 128];
        binary.mapping_digest = digest(&binary.mapping_bytes);
        records.push(binary);

        for record in &records {
            assert_eq!(
                store.register_pending(
                    record,
                    &leases,
                    Instant::now() + Duration::from_secs(1),
                    &mut FeedbackBudget::default(),
                ),
                Err(FeedbackError::SecretDetected)
            );
        }
        let pending: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM pending_feedback", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending, 0);
        for entry in std::fs::read_dir(&root).unwrap().map(Result::unwrap) {
            if entry.file_type().unwrap().is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                for canary in [
                    secret.as_slice(),
                    b"top%2B%2Fsecret".as_slice(),
                    b"dG9wKy9zZWNyZXQ=".as_slice(),
                    [0, 255, 1, 2, 3, 128].as_slice(),
                ] {
                    assert!(!bytes.windows(canary.len()).any(|window| window == canary));
                }
            }
        }
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_and_secret_pending_rows_are_quarantined_without_events() {
        let root = std::env::temp_dir().join(format!(
            "kit-feedback-pending-quarantine-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let grants = GrantSnapshot::new(
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        );
        let authenticated = AuthenticatedPrincipal::from_grants(grants);
        let principal = authenticated.principal_id().to_string();
        let project = authenticated.grant_snapshot().project_id().to_string();
        let mut store = FeedbackEventStore::open(&root).unwrap();
        for (index, bytes) in [b"SECRET_PENDING".as_slice(), b"not-json".as_slice()]
            .into_iter()
            .enumerate()
        {
            store
                .connection
                .execute(
                    "INSERT INTO pending_feedback
                     (operation_id, authority_binding, principal_id, project_id, workspace_id,
                      record_digest, record, state)
                     VALUES (?1, 'binding', ?2, ?3, 'workspace', ?4, ?5, 'pending')",
                    params![
                        digest(&[index as u8]),
                        principal,
                        project,
                        digest(bytes),
                        bytes
                    ],
                )
                .unwrap();
        }
        let limits = FeedbackLimits::default();
        let secrets = [SecretLease::new(b"SECRET_PENDING".to_vec())];
        for _ in 0..2 {
            assert!(
                store
                    .next_pending_record(
                        &principal,
                        &project,
                        "workspace",
                        &limits,
                        &secrets,
                        Instant::now() + Duration::from_secs(1),
                    )
                    .unwrap()
                    .is_none()
            );
        }
        let quarantined: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM pending_feedback_quarantine",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let events: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM check_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(quarantined, 2);
        assert_eq!(events, 0);
        drop(store);
        std::fs::remove_file(root).unwrap();
    }

    #[test]
    fn pending_limits_accept_exact_values_and_reject_one_over() {
        let mut limits = FeedbackLimits {
            max_pending_records: 2,
            max_pending_bytes: 10,
            max_memory_bytes: 10,
            ..FeedbackLimits::default()
        };
        let mut exact = FeedbackBudget::default();
        assert_eq!(exact.reserve_pending_recovery(2, 10, &limits), Ok(()));
        assert_eq!(
            FeedbackBudget::default().reserve_pending_recovery(3, 10, &limits),
            Err(FeedbackError::BudgetExceeded)
        );
        assert_eq!(
            FeedbackBudget::default().reserve_pending_recovery(2, 11, &limits),
            Err(FeedbackError::BudgetExceeded)
        );
        assert_eq!(
            FeedbackBudget::default().consume_pending_record(10, &limits),
            Ok(())
        );
        assert_eq!(
            FeedbackBudget::default().consume_pending_record(11, &limits),
            Err(FeedbackError::BudgetExceeded)
        );

        let mapping = EditMapping {
            paths: vec![PathMove {
                before: "a".repeat(8),
                after: "b".into(),
            }],
            ..EditMapping::default()
        };
        assert_eq!(validate_mapping_bounded(&mapping, 8), Ok(()));
        assert_eq!(
            validate_mapping_bounded(&mapping, 7),
            Err(FeedbackError::InvalidMapping)
        );

        let adapters = BTreeMap::from([
            ("a".into(), DiagnosticAdapter::NormalizedJsonLinesV1),
            ("b".into(), DiagnosticAdapter::RustcJsonV1),
        ]);
        limits.max_adapters = 2;
        assert_eq!(validate_adapters(&adapters, &limits), Ok(()));
        limits.max_adapters = 1;
        assert_eq!(
            validate_adapters(&adapters, &limits),
            Err(FeedbackError::BudgetExceeded)
        );

        let grants = GrantSnapshot::new(
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        );
        let authenticated = AuthenticatedPrincipal::from_grants(grants);
        let mut record = pending_record(&authenticated);
        let event = PendingCheckEvent {
            kind: CheckEventKind::Started,
            operation_id: record.operation_id.clone(),
            base_revision: record.base_revision.clone(),
            staged_state_digest: record.staged_state_digest.clone(),
            plan_digest: digest(b"limit-plan"),
            result_digest: digest(b"limit-result"),
            check_id: "check".into(),
            status: "pass".into(),
            diagnostic_count: 0,
            artifacts: Vec::new(),
        };
        record.expected_events.push(event.clone());
        record.limits.max_pending_events = 1;
        assert_eq!(validate_pending_limits(&record), Ok(()));
        record.expected_events.push(event);
        assert_eq!(
            validate_pending_limits(&record),
            Err(FeedbackError::BudgetExceeded)
        );
    }

    #[test]
    fn restart_preserves_pending_observer_lifecycle_without_events() {
        let root = std::env::temp_dir().join(format!(
            "kit-feedback-pending-{}-{}",
            std::process::id(),
            blake3::hash(b"pending-test").to_hex()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let path = root.join("events.sqlite3");
        let grants = GrantSnapshot::new(
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        );
        let authenticated = AuthenticatedPrincipal::from_grants(grants);
        let authority = FeedbackAuthority::issue(
            &authenticated,
            "workspace_0000000000000000000000",
            "run_00000000000000000000000000",
            digest(b"pending-edit"),
            8,
        )
        .unwrap();
        let plan = digest(b"pending-plan");
        let stage = digest(b"pending-stage");
        let mut store = FeedbackEventStore::open(&path).unwrap();
        {
            let mut cursors = Vec::new();
            for boundary in [
                VerificationBoundary::Started,
                VerificationBoundary::Progress,
            ] {
                let cursor = store
                    .record_boundary(
                        &authority,
                        "revision-pending",
                        &stage,
                        VerificationObservation {
                            plan_digest: &plan,
                            check_id: "pending-check",
                            boundary,
                            status: None,
                        },
                    )
                    .unwrap();
                assert_eq!(
                    store
                        .record_boundary(
                            &authority,
                            "revision-pending",
                            &stage,
                            VerificationObservation {
                                plan_digest: &plan,
                                check_id: "pending-check",
                                boundary,
                                status: None,
                            },
                        )
                        .unwrap(),
                    cursor
                );
                cursors.push(cursor);
            }
            assert_eq!(cursors, [1, 2]);
            assert_eq!(
                store.record_boundary(
                    &authority,
                    "revision-pending",
                    &stage,
                    VerificationObservation {
                        plan_digest: &plan,
                        check_id: "pending-check",
                        boundary: VerificationBoundary::Progress,
                        status: Some(CheckResultStatus::Pass),
                    },
                ),
                Err(FeedbackError::EventConflict)
            );
        }
        drop(store);
        let restarted = FeedbackEventStore::open(&path).unwrap();
        let state: String = restarted
            .connection
            .query_row(
                "SELECT state FROM check_lifecycle WHERE authority_binding = ?1",
                [&authority.binding],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "progress");
        let boundaries: i64 = restarted
            .connection
            .query_row(
                "SELECT COUNT(*) FROM check_boundaries WHERE authority_binding = ?1",
                [&authority.binding],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(boundaries, 2);
        let public_feeds: i64 = restarted
            .connection
            .query_row(
                "SELECT COUNT(*) FROM feedback_feeds WHERE authority_binding = ?1",
                [&authority.binding],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(public_feeds, 0);
        let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
        assert!(
            restarted
                .events(&authenticated, &authority, &artifacts, 0)
                .unwrap()
                .is_empty()
        );
        drop(restarted);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zero_diagnostic_budget_accepts_only_non_diagnostics() {
        let limits = FeedbackLimits {
            max_diagnostics: 0,
            ..FeedbackLimits::default()
        };
        let malformed = parse_diagnostics(
            DiagnosticAdapter::NormalizedJsonLinesV1,
            "check",
            "tool",
            b"not-json\n",
            &limits,
        )
        .unwrap();
        assert_eq!(malformed.malformed_records, 1);
        assert!(malformed.diagnostics.is_empty());
        assert_eq!(
            parse_diagnostics(
                DiagnosticAdapter::NormalizedJsonLinesV1,
                "check",
                "tool",
                &line("src/lib.rs", 1, "valid"),
                &limits,
            ),
            Err(FeedbackError::DiagnosticLimitExceeded)
        );
    }

    #[test]
    fn mapping_rejects_duplicate_overlapping_and_overflowing_ranges() {
        let duplicate = EditMapping {
            paths: vec![
                PathMove {
                    before: "a".into(),
                    after: "b".into(),
                },
                PathMove {
                    before: "a".into(),
                    after: "c".into(),
                },
            ],
            ..EditMapping::default()
        };
        assert_eq!(
            validate_mapping(&duplicate),
            Err(FeedbackError::InvalidMapping)
        );

        let invalid_lines = |before_start, after_start, line_count| EditMapping {
            lines: vec![LineMove {
                before_path: "a".into(),
                after_path: "b".into(),
                before_start,
                after_start,
                line_count,
            }],
            ..EditMapping::default()
        };
        assert_eq!(
            validate_mapping(&invalid_lines(u32::MAX, 1, 2)),
            Err(FeedbackError::InvalidMapping)
        );
        let overlap = EditMapping {
            lines: vec![
                LineMove {
                    before_path: "a".into(),
                    after_path: "b".into(),
                    before_start: 1,
                    after_start: 1,
                    line_count: 2,
                },
                LineMove {
                    before_path: "a".into(),
                    after_path: "c".into(),
                    before_start: 2,
                    after_start: 3,
                    line_count: 2,
                },
            ],
            ..EditMapping::default()
        };
        assert_eq!(
            validate_mapping(&overlap),
            Err(FeedbackError::InvalidMapping)
        );
    }
}
