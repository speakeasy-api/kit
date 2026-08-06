use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use crate::{
    api::service::{
        ArtifactService, CapabilityService, Command, CommandObservation, LeaseService, Scheduler,
        ServiceError,
    },
    domain::{
        ids::{PrincipalId, ProjectId, RunId, ThreadId},
        secret::SecretLease,
    },
    store::sqlite::idempotency::IdempotencyKey,
    telemetry::otel::{
        Adapter, AdapterError, AeadProvider, AttributeValue, DropPolicy, EncryptionKeyHandle,
        EnqueueOutcome, ExportError, Exporter, LogRecord, LogSeverity, Metric, MetricName,
        MetricValue, Resource, RetentionError, RetentionHook, RetentionPolicy, RetentionSink,
        RunEnvelope, Span, SpanKind, SpanName, SpanStatus, TelemetryItem,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryReadinessPolicy {
    BestEffort,
    Required,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryRetentionStatus {
    DisabledByPolicy,
    Encrypted,
}

pub enum RuntimeRetention<'a> {
    DisabledByPolicy,
    EncryptedExporter,
    Encrypted {
        hook: RetentionHook<'a>,
        sink: Box<dyn RetentionSink + Send + 'a>,
    },
}

impl<'a> RuntimeRetention<'a> {
    pub fn encrypted(
        mut policy: RetentionPolicy,
        aead: &'a (dyn AeadProvider + Sync),
        key: &'a EncryptionKeyHandle,
        sink: impl RetentionSink + Send + 'a,
    ) -> Self {
        policy.encryption_required = true;
        Self::Encrypted {
            hook: RetentionHook::new(policy, Some(aead), Some(key)),
            sink: Box::new(sink),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelemetryHealth {
    pub ready: bool,
    pub learning_ready: bool,
    pub learning_healthy: bool,
    pub exporter_healthy: bool,
    pub retention_healthy: bool,
    pub queue_healthy: bool,
    pub shutting_down: bool,
    pub queued: usize,
    pub dropped: u64,
    pub retention_status: TelemetryRetentionStatus,
    pub last_error: Option<String>,
    pub learning_last_error: Option<String>,
}

#[derive(Debug)]
pub enum TelemetryRuntimeError {
    Adapter(AdapterError),
    Export(ExportError),
    Retention(RetentionError),
    LearningSinkUnavailable,
}

impl fmt::Display for TelemetryRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adapter(error) => error.fmt(formatter),
            Self::Export(error) => write!(formatter, "telemetry export failed: {error}"),
            Self::Retention(error) => error.fmt(formatter),
            Self::LearningSinkUnavailable => formatter
                .write_str("tool-learning telemetry requires an encrypted access-controlled sink"),
        }
    }
}

impl std::error::Error for TelemetryRuntimeError {}

struct RuntimeState<'a> {
    adapter: Adapter<'a>,
    exporter: Box<dyn Exporter + Send + 'a>,
    retention: RuntimeRetention<'a>,
    exporter_healthy: bool,
    retention_healthy: bool,
    queue_healthy: bool,
    learning_healthy: bool,
    shutting_down: bool,
    last_error: Option<String>,
    learning_last_error: Option<String>,
    next_span: u64,
}

pub struct TelemetryRuntime<'a> {
    state: Mutex<RuntimeState<'a>>,
    readiness_policy: TelemetryReadinessPolicy,
}

impl<'a> TelemetryRuntime<'a> {
    pub fn new(
        resource: Resource,
        secrets: &'a [SecretLease],
        capacity: usize,
        drop_policy: DropPolicy,
        exporter: impl Exporter + Send + 'a,
        retention: RuntimeRetention<'a>,
        readiness_policy: TelemetryReadinessPolicy,
    ) -> Result<Self, TelemetryRuntimeError> {
        Ok(Self {
            state: Mutex::new(RuntimeState {
                adapter: Adapter::new(resource, secrets, capacity, drop_policy)
                    .map_err(TelemetryRuntimeError::Adapter)?,
                exporter: Box::new(exporter),
                retention,
                exporter_healthy: true,
                retention_healthy: true,
                queue_healthy: true,
                learning_healthy: true,
                shutting_down: false,
                last_error: None,
                learning_last_error: None,
                next_span: 1,
            }),
            readiness_policy,
        })
    }

    pub fn local(
        resource: Resource,
        secrets: &'a [SecretLease],
        capacity: usize,
        drop_policy: DropPolicy,
        exporter: impl Exporter + Send + 'a,
        readiness_policy: TelemetryReadinessPolicy,
    ) -> Result<Self, TelemetryRuntimeError> {
        Self::new(
            resource,
            secrets,
            capacity,
            drop_policy,
            exporter,
            RuntimeRetention::DisabledByPolicy,
            readiness_policy,
        )
    }

    pub fn encrypted_local(
        resource: Resource,
        secrets: &'a [SecretLease],
        capacity: usize,
        drop_policy: DropPolicy,
        exporter: impl Exporter + Send + 'a,
        readiness_policy: TelemetryReadinessPolicy,
    ) -> Result<Self, TelemetryRuntimeError> {
        Self::new(
            resource,
            secrets,
            capacity,
            drop_policy,
            exporter,
            RuntimeRetention::EncryptedExporter,
            readiness_policy,
        )
    }

    pub fn emit(&self, item: TelemetryItem) -> Result<EnqueueOutcome, TelemetryRuntimeError> {
        let mut state = self.lock();
        match state.adapter.enqueue(item) {
            Ok(outcome) => {
                if outcome != EnqueueOutcome::Accepted {
                    state.queue_healthy = false;
                    state.last_error = Some("telemetry queue capacity exceeded".to_owned());
                }
                Ok(outcome)
            }
            Err(error) => {
                state.queue_healthy = false;
                state.last_error = Some(error.to_string());
                Err(TelemetryRuntimeError::Adapter(error))
            }
        }
    }

    pub fn emit_span(&self, span: Span) -> Result<EnqueueOutcome, TelemetryRuntimeError> {
        self.emit(TelemetryItem::Span(span))
    }

    pub fn emit_run_envelope(
        &self,
        envelope: RunEnvelope,
    ) -> Result<EnqueueOutcome, TelemetryRuntimeError> {
        self.emit(TelemetryItem::RunEnvelope(envelope))
    }

    pub fn emit_canonical_run_envelope(
        &self,
        envelope: crate::telemetry::run_envelope::RunEnvelope,
    ) -> Result<EnqueueOutcome, TelemetryRuntimeError> {
        self.emit(TelemetryItem::RunEnvelope(RunEnvelope {
            outcome: envelope.outcome,
            latency_ms: envelope.latency_ms,
            errors: envelope.errors.clone(),
            canonical: Some(Box::new(envelope)),
            ..RunEnvelope::default()
        }))
    }

    pub fn export_learning_outbox(
        &self,
        store: &mut crate::store::sqlite::append::SqliteStore,
        hasher: &crate::telemetry::tool_learning::ProjectPointerHasher,
    ) -> Result<usize, TelemetryRuntimeError> {
        let result = self.export_learning_outbox_claim(store, hasher);
        match result {
            Ok(Some(exported)) => {
                self.record_learning_result(
                    store,
                    hasher,
                    &Ok::<_, TelemetryRuntimeError>(exported),
                );
                Ok(exported)
            }
            Ok(None) => {
                self.mark_learning_failure("durable learning export is owned by another worker");
                Ok(0)
            }
            Err(error) => {
                self.mark_learning_failure(error.to_string());
                Err(error)
            }
        }
    }

    fn export_learning_outbox_claim(
        &self,
        store: &mut crate::store::sqlite::append::SqliteStore,
        hasher: &crate::telemetry::tool_learning::ProjectPointerHasher,
    ) -> Result<Option<usize>, TelemetryRuntimeError> {
        let claim = store
            .claim_learning_export(hasher.project().as_str())
            .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())));
        match claim {
            Ok(Some(token)) => {
                let exported = self.export_learning_outbox_inner(store, hasher);
                let released = store
                    .release_learning_export(hasher.project().as_str(), &token)
                    .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())));
                match (exported, released) {
                    (Err(error), _) | (_, Err(error)) => Err(error),
                    (Ok(count), Ok(())) => Ok(Some(count)),
                }
            }
            Ok(None) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn reconcile_learning_backlog(
        &self,
        store: &mut crate::store::sqlite::append::SqliteStore,
        hasher: &crate::telemetry::tool_learning::ProjectPointerHasher,
    ) -> Result<usize, TelemetryRuntimeError> {
        match self.reconcile_learning_backlog_inner(store, hasher) {
            Ok(Some(exported)) => {
                self.record_learning_result(
                    store,
                    hasher,
                    &Ok::<_, TelemetryRuntimeError>(exported),
                );
                Ok(exported)
            }
            Ok(None) => {
                self.mark_learning_failure("durable learning export is owned by another worker");
                Ok(0)
            }
            Err(error) => {
                self.mark_learning_failure(error.to_string());
                Err(error)
            }
        }
    }

    fn reconcile_learning_backlog_inner(
        &self,
        store: &mut crate::store::sqlite::append::SqliteStore,
        hasher: &crate::telemetry::tool_learning::ProjectPointerHasher,
    ) -> Result<Option<usize>, TelemetryRuntimeError> {
        let projects = store
            .pending_learning_projects()
            .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?;
        if projects
            .iter()
            .any(|project| project != hasher.project().as_str())
            || store
                .pending_catalog_stats_projects()
                .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?
                .iter()
                .any(|project| project != hasher.project().as_str())
        {
            return Err(TelemetryRuntimeError::Export(ExportError(
                "durable learning outbox contains another project authority".to_owned(),
            )));
        }
        let Some(mut exported) = self.export_learning_outbox_claim(store, hasher)? else {
            return Ok(None);
        };
        if !store
            .pending_learning_outbox(hasher.project().as_str(), 1)
            .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?
            .is_empty()
        {
            return Err(TelemetryRuntimeError::Export(ExportError(
                "durable learning outbox remains in progress".to_owned(),
            )));
        }
        loop {
            let reconciled = store
                .reconcile_learning_markers(hasher.project().as_str(), 256)
                .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?;
            if reconciled == 0 {
                if store
                    .has_learning_markers(hasher.project().as_str())
                    .map_err(|error| {
                        TelemetryRuntimeError::Export(ExportError(error.to_string()))
                    })?
                {
                    return Err(TelemetryRuntimeError::Export(ExportError(
                        "durable learning reconciliation remains in progress".to_owned(),
                    )));
                }
                break;
            }
            let Some(count) = self.export_learning_outbox_claim(store, hasher)? else {
                return Ok(None);
            };
            exported = exported.saturating_add(count);
        }
        loop {
            let runs = store
                .pending_catalog_stats_runs(hasher.project().as_str())
                .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?;
            if runs.is_empty() {
                break;
            }
            for run in runs {
                let run = RunId::parse(&run).map_err(|_| {
                    TelemetryRuntimeError::Export(ExportError(
                        "durable catalog statistics have no run authority".to_owned(),
                    ))
                })?;
                self.export_catalog_stats_snapshot(store, hasher, run)?;
                exported = exported.saturating_add(1);
            }
        }
        Ok(Some(exported))
    }

    fn export_learning_outbox_inner(
        &self,
        store: &mut crate::store::sqlite::append::SqliteStore,
        hasher: &crate::telemetry::tool_learning::ProjectPointerHasher,
    ) -> Result<usize, TelemetryRuntimeError> {
        let mut state = self.lock();
        let mut exported = 0;
        loop {
            let pending = store
                .pending_learning_outbox(hasher.project().as_str(), 256)
                .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?;
            if pending.is_empty() {
                break;
            }
            for row in pending {
                let event: crate::telemetry::tool_learning::ToolLearningEvent =
                    serde_json::from_slice(&row.payload).map_err(|error| {
                        TelemetryRuntimeError::Export(ExportError(format!(
                            "invalid durable learning outbox record: {error}"
                        )))
                    })?;
                event.validate_with(hasher).map_err(|error| {
                    TelemetryRuntimeError::Export(ExportError(format!(
                        "unauthenticated durable learning outbox record: {error}"
                    )))
                })?;
                let expected_run = hasher.pointer(
                    crate::telemetry::tool_learning::PointerDomain::Run,
                    row.run_id.as_bytes(),
                );
                if row.project != hasher.project().as_str()
                    || event.common().project != *hasher.project()
                    || event.common().run != expected_run
                    || row.frame_id != event.common().event_id.as_str()
                    || crate::domain::ids::EventId::from_stable_bytes(
                        event.common().event_id.as_str().as_bytes(),
                    ) != row.event_id
                {
                    return Err(TelemetryRuntimeError::Export(ExportError(
                        "learning outbox event linkage mismatch".to_owned(),
                    )));
                }
                let frame = crate::telemetry::otel::EncryptedLearningFrame {
                    frame_id: row.frame_id.clone(),
                    ciphertext: hasher
                        .encrypt_export_frame(&row.frame_id, &row.payload)
                        .map_err(|error| {
                            TelemetryRuntimeError::Export(ExportError(error.to_string()))
                        })?,
                };
                if let Err(error) = state.exporter.export_encrypted_learning(&frame) {
                    return Err(TelemetryRuntimeError::Export(error));
                }
                let labels = BTreeMap::from([
                    ("event_class".to_owned(), event.class_name().to_owned()),
                    (
                        "operation".to_owned(),
                        event.common().operation.as_str().to_owned(),
                    ),
                    (
                        "status".to_owned(),
                        learning_metric_status(&event).to_owned(),
                    ),
                ]);
                if !matches!(
                    Metric::new(
                        MetricName::ToolLearningEvents,
                        "1",
                        MetricValue::Counter { value: 1 },
                        labels,
                        0,
                    )
                    .and_then(|metric| {
                        state
                            .adapter
                            .enqueue(TelemetryItem::Metric(metric))
                            .map_err(|_| crate::telemetry::otel::MetricError::InvalidLearningRecord)
                    }),
                    Ok(EnqueueOutcome::Accepted)
                ) {
                    state.queue_healthy = false;
                    state.last_error =
                        Some("tool-learning metric queue capacity exceeded".to_owned());
                }
                store
                    .acknowledge_learning_outbox(hasher.project().as_str(), &row.frame_id)
                    .map_err(|error| {
                        TelemetryRuntimeError::Export(ExportError(error.to_string()))
                    })?;
                exported += 1;
            }
        }
        Ok(exported)
    }

    pub fn export_catalog_stats_snapshot(
        &self,
        store: &mut crate::store::sqlite::append::SqliteStore,
        hasher: &crate::telemetry::tool_learning::ProjectPointerHasher,
        run_id: RunId,
    ) -> Result<(), TelemetryRuntimeError> {
        let result = self.export_catalog_stats_snapshot_inner(store, hasher, run_id);
        self.record_learning_result(store, hasher, &result);
        result
    }

    fn export_catalog_stats_snapshot_inner(
        &self,
        store: &mut crate::store::sqlite::append::SqliteStore,
        hasher: &crate::telemetry::tool_learning::ProjectPointerHasher,
        run_id: RunId,
    ) -> Result<(), TelemetryRuntimeError> {
        if !store
            .catalog_stats_run_terminal(run_id)
            .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?
        {
            return Err(TelemetryRuntimeError::Export(ExportError(
                "catalog statistics run is not durably terminal".to_owned(),
            )));
        }
        let run = hasher.pointer(
            crate::telemetry::tool_learning::PointerDomain::Run,
            run_id.to_string().as_bytes(),
        );
        loop {
            let Some(snapshot) = store
                .catalog_stats_snapshot(run_id, run.as_str())
                .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?
            else {
                return Ok(());
            };
            let entries = &snapshot.entries;
            let records = crate::telemetry::tool_learning::records(store, run_id, hasher)
                .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?;
            let calls = records
                .iter()
                .filter_map(|record| match record {
                    crate::telemetry::tool_learning::ToolLearningEvent::Call {
                        call,
                        binding: Some(binding),
                        ..
                    } => Some((call, binding)),
                    _ => None,
                })
                .collect::<BTreeMap<_, _>>();
            let outcomes = records
                .iter()
                .filter_map(|record| match record {
                    crate::telemetry::tool_learning::ToolLearningEvent::Outcome {
                        common,
                        call,
                        status,
                        ..
                    } => calls.get(call).map(|binding| (*binding, common, *status)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let mut authenticated = Vec::with_capacity(entries.len());
            for entry in entries {
                let matching = outcomes
                    .iter()
                    .filter(|(binding, _, _)| binding.as_str() == entry.binding)
                    .collect::<Vec<_>>();
                let count = usize::try_from(entry.attempts).map_err(|_| {
                    TelemetryRuntimeError::Export(ExportError(
                        "catalog statistics count is invalid".to_owned(),
                    ))
                })?;
                let Some(last) = matching
                    .iter()
                    .position(|(_, common, _)| common.event_id.as_str() == entry.source_event)
                else {
                    return Err(TelemetryRuntimeError::Export(ExportError(
                        "catalog statistics authentication mismatch".to_owned(),
                    )));
                };
                let end = last + 1;
                let start = end.checked_sub(count).ok_or_else(|| {
                    TelemetryRuntimeError::Export(ExportError(
                        "catalog statistics authentication mismatch".to_owned(),
                    ))
                })?;
                let mut verified = crate::store::sqlite::append::DurableCatalogStats {
                    project: entry.project.clone(),
                    run: entry.run.clone(),
                    binding: entry.binding.clone(),
                    attempts: 0,
                    succeeded: 0,
                    failed: 0,
                    cancelled: 0,
                    outcome_unknown: 0,
                    source_event: String::new(),
                    revision: entry.revision,
                };
                for (_, common, status) in matching.into_iter().skip(start).take(count) {
                    if common.project.as_str() != entry.project || common.run.as_str() != entry.run
                    {
                        return Err(TelemetryRuntimeError::Export(ExportError(
                            "catalog statistics row binding mismatch".to_owned(),
                        )));
                    }
                    verified.attempts += 1;
                    match status {
                        crate::telemetry::tool_learning::LearningStatus::Succeeded => {
                            verified.succeeded += 1
                        }
                        crate::telemetry::tool_learning::LearningStatus::Cancelled => {
                            verified.cancelled += 1
                        }
                        crate::telemetry::tool_learning::LearningStatus::OutcomeUnknown => {
                            verified.outcome_unknown += 1
                        }
                        crate::telemetry::tool_learning::LearningStatus::Failed
                        | crate::telemetry::tool_learning::LearningStatus::Interrupted
                        | crate::telemetry::tool_learning::LearningStatus::Unavailable => {
                            verified.failed += 1
                        }
                    }
                    verified.source_event = common.event_id.as_str().to_owned();
                }
                authenticated.push(verified);
            }
            if *entries != authenticated {
                return Err(TelemetryRuntimeError::Export(ExportError(
                    "catalog statistics authentication mismatch".to_owned(),
                )));
            }
            for entry in entries {
                if entry.project != hasher.project().as_str() || entry.run != run.as_str() {
                    return Err(TelemetryRuntimeError::Export(ExportError(
                        "catalog statistics row binding mismatch".to_owned(),
                    )));
                }
                let binding =
                    crate::telemetry::tool_learning::LearningPointer::parse(entry.binding.clone())
                        .map_err(|error| {
                            TelemetryRuntimeError::Export(ExportError(error.to_string()))
                        })?;
                hasher
                    .validate(
                        &binding,
                        crate::telemetry::tool_learning::PointerDomain::Binding,
                    )
                    .map_err(|error| {
                        TelemetryRuntimeError::Export(ExportError(error.to_string()))
                    })?;
            }
            let frame = crate::telemetry::otel::EncryptedLearningFrame {
                frame_id: snapshot.frame_id.clone(),
                ciphertext: hasher
                    .encrypt_export_frame(&snapshot.frame_id, &snapshot.payload)
                    .map_err(|error| {
                        TelemetryRuntimeError::Export(ExportError(error.to_string()))
                    })?,
            };
            self.lock()
                .exporter
                .export_encrypted_learning(&frame)
                .map_err(TelemetryRuntimeError::Export)?;
            if store
                .acknowledge_catalog_stats(&snapshot)
                .map_err(|error| TelemetryRuntimeError::Export(ExportError(error.to_string())))?
            {
                continue;
            }
        }
    }

    pub fn emit_api_command(&self, observation: CommandObservation<'_>) {
        let mut state = self.lock();
        let sequence = state.next_span;
        state.next_span = state.next_span.saturating_add(2);
        let trace_id = otel_trace_id(&observation.trace_id.to_string());
        let span_id = otel_span_id(&trace_id, observation.operation, sequence);
        let outcome = if observation.succeeded {
            "succeeded"
        } else {
            "failed"
        };
        let is_run = observation.operation.starts_with("run.");
        let emit_run = is_run && state.adapter.remaining_capacity() >= 5;
        if is_run && !emit_run {
            state.queue_healthy = false;
            state.last_error = Some("telemetry queue capacity exceeded".to_owned());
        }
        let span = Span {
            trace_id: trace_id.clone(),
            span_id: span_id.clone(),
            parent_span_id: None,
            span_name: SpanName::ApiCommand,
            kind: SpanKind::Server,
            start_unix_nanos: observation.start_unix_nanos,
            end_unix_nanos: observation.end_unix_nanos.max(observation.start_unix_nanos),
            attributes: BTreeMap::from([
                (
                    "kit.operation".to_owned(),
                    AttributeValue::String(observation.operation.to_owned()),
                ),
                (
                    "kit.command.outcome".to_owned(),
                    AttributeValue::String(outcome.to_owned()),
                ),
            ]),
            events: Vec::new(),
            status: if observation.succeeded {
                SpanStatus::Ok
            } else {
                SpanStatus::Error(None)
            },
        };
        if let Ok(outcome) = state.adapter.enqueue(TelemetryItem::Span(span)) {
            if outcome != EnqueueOutcome::Accepted {
                state.queue_healthy = false;
                state.last_error = Some("telemetry queue capacity exceeded".to_owned());
            }
        } else {
            state.queue_healthy = false;
            state.last_error = Some("api.command telemetry enqueue failed".to_owned());
        }
        if emit_run {
            let run_span = Span {
                trace_id: trace_id.clone(),
                span_id: otel_span_id(&trace_id, "run.attempt", sequence.saturating_add(1)),
                parent_span_id: Some(span_id.clone()),
                span_name: SpanName::RunAttempt,
                kind: SpanKind::Internal,
                start_unix_nanos: observation.start_unix_nanos,
                end_unix_nanos: observation.end_unix_nanos.max(observation.start_unix_nanos),
                attributes: BTreeMap::from([
                    (
                        "kit.operation".to_owned(),
                        AttributeValue::String(observation.operation.to_owned()),
                    ),
                    (
                        "kit.run.outcome".to_owned(),
                        AttributeValue::String(outcome.to_owned()),
                    ),
                ]),
                events: Vec::new(),
                status: if observation.succeeded {
                    SpanStatus::Ok
                } else {
                    SpanStatus::Error(None)
                },
            };
            if let Ok(enqueued) = state.adapter.enqueue(TelemetryItem::Span(run_span)) {
                if enqueued != EnqueueOutcome::Accepted {
                    state.queue_healthy = false;
                    state.last_error = Some("telemetry queue capacity exceeded".to_owned());
                }
            } else {
                state.queue_healthy = false;
                state.last_error = Some("run lifecycle telemetry enqueue failed".to_owned());
            }
            let labels = BTreeMap::from([("outcome".to_owned(), outcome.to_owned())]);
            for metric in [
                Metric::new(
                    MetricName::RunCount,
                    "1",
                    MetricValue::Counter { value: 1 },
                    labels.clone(),
                    observation.end_unix_nanos,
                ),
                Metric::new(
                    MetricName::RunLatency,
                    "ms",
                    MetricValue::Histogram {
                        count: 1,
                        sum: observation
                            .end_unix_nanos
                            .saturating_sub(observation.start_unix_nanos)
                            as f64
                            / 1_000_000.0,
                    },
                    labels,
                    observation.end_unix_nanos,
                ),
            ] {
                match metric {
                    Ok(metric) => enqueue_lifecycle(&mut state, TelemetryItem::Metric(metric)),
                    Err(error) => {
                        state.queue_healthy = false;
                        state.last_error = Some(error.to_string());
                    }
                }
            }
        }
        enqueue_lifecycle(
            &mut state,
            TelemetryItem::Log(LogRecord {
                timestamp_unix_nanos: observation.end_unix_nanos,
                severity: if observation.succeeded {
                    LogSeverity::Info
                } else {
                    LogSeverity::Error
                },
                body: AttributeValue::String("api command completed".to_owned()),
                attributes: BTreeMap::from([
                    (
                        "kit.operation".to_owned(),
                        AttributeValue::String(observation.operation.to_owned()),
                    ),
                    (
                        "kit.command.outcome".to_owned(),
                        AttributeValue::String(outcome.to_owned()),
                    ),
                ]),
                trace_id: Some(trace_id),
                span_id: Some(span_id),
            }),
        );
    }

    pub fn flush(&self) -> Result<usize, TelemetryRuntimeError> {
        let mut state = self.lock();
        let Some(batch) = state.adapter.pending_batch() else {
            return Ok(0);
        };
        let count = state.adapter.queued();
        if let Err(error) = batch.validate() {
            let error = AdapterError::InvalidBatch(error);
            state.queue_healthy = false;
            state.last_error = Some(error.to_string());
            return Err(TelemetryRuntimeError::Adapter(error));
        }
        if let Err(error) = state.exporter.export(&batch) {
            state.exporter_healthy = false;
            state.last_error = Some(error.to_string());
            return Err(TelemetryRuntimeError::Export(error));
        }
        state.exporter_healthy = true;
        if let RuntimeRetention::Encrypted { hook, sink } = &mut state.retention
            && let Err(error) = hook.retain(&batch, sink.as_mut())
        {
            state.retention_healthy = false;
            state.last_error = Some(error.to_string());
            return Err(TelemetryRuntimeError::Retention(error));
        }
        state.retention_healthy = true;
        state.adapter.acknowledge(count);
        state.queue_healthy = true;
        state.last_error = None;
        Ok(count)
    }

    pub fn shutdown(&self) -> Result<usize, TelemetryRuntimeError> {
        let result = self.flush();
        self.lock().shutting_down = true;
        result
    }

    pub fn health(&self) -> TelemetryHealth {
        let state = self.lock();
        let healthy = state.exporter_healthy && state.retention_healthy && state.queue_healthy;
        let retention_status = match state.retention {
            RuntimeRetention::DisabledByPolicy => TelemetryRetentionStatus::DisabledByPolicy,
            RuntimeRetention::EncryptedExporter | RuntimeRetention::Encrypted { .. } => {
                TelemetryRetentionStatus::Encrypted
            }
        };
        TelemetryHealth {
            ready: !state.shutting_down
                && (self.readiness_policy == TelemetryReadinessPolicy::BestEffort
                    || healthy && state.learning_healthy),
            learning_ready: !state.shutting_down && state.learning_healthy,
            learning_healthy: state.learning_healthy,
            exporter_healthy: state.exporter_healthy,
            retention_healthy: state.retention_healthy,
            queue_healthy: state.queue_healthy,
            shutting_down: state.shutting_down,
            queued: state.adapter.queued(),
            dropped: state.adapter.dropped(),
            retention_status,
            last_error: state.last_error.clone(),
            learning_last_error: state.learning_last_error.clone(),
        }
    }

    pub fn learning_admission_ready(&self) -> bool {
        self.readiness_policy == TelemetryReadinessPolicy::BestEffort
            || self.lock().learning_healthy
    }

    pub fn learning_required(&self) -> bool {
        self.readiness_policy == TelemetryReadinessPolicy::Required
    }

    pub(crate) fn mark_learning_failure(&self, error: impl Into<String>) {
        let mut state = self.lock();
        state.learning_healthy = false;
        state.learning_last_error = Some(error.into());
    }

    fn record_learning_result<T>(
        &self,
        store: &crate::store::sqlite::append::SqliteStore,
        hasher: &crate::telemetry::tool_learning::ProjectPointerHasher,
        result: &Result<T, TelemetryRuntimeError>,
    ) {
        let mut state = self.lock();
        match result {
            Ok(_) => match store.learning_backlog_drained(hasher.project().as_str()) {
                Ok(true) => {
                    state.learning_healthy = true;
                    state.learning_last_error = None;
                }
                Ok(false) => {
                    state.learning_healthy = false;
                }
                Err(error) => {
                    state.learning_healthy = false;
                    state.learning_last_error = Some(error.to_string());
                }
            },
            Err(error) => {
                state.learning_healthy = false;
                state.learning_last_error = Some(error.to_string());
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeState<'a>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn learning_metric_status(
    event: &crate::telemetry::tool_learning::ToolLearningEvent,
) -> &'static str {
    use crate::telemetry::tool_learning::{LearningStatus, RetryClass, ToolLearningEvent};

    let status = match event {
        ToolLearningEvent::Search { status, .. }
        | ToolLearningEvent::Inspection { status, .. }
        | ToolLearningEvent::Outcome { status, .. } => *status,
        ToolLearningEvent::Error { retry, .. } => {
            if matches!(
                retry,
                RetryClass::AuthorizationResume | RetryClass::UrlResume
            ) {
                LearningStatus::Interrupted
            } else {
                LearningStatus::Failed
            }
        }
        ToolLearningEvent::Opportunity { .. } | ToolLearningEvent::Call { .. } => {
            LearningStatus::Succeeded
        }
    };
    status.as_str()
}

fn enqueue_lifecycle(state: &mut RuntimeState<'_>, item: TelemetryItem) {
    match state.adapter.enqueue(item) {
        Ok(EnqueueOutcome::Accepted) => {}
        Ok(_) => {
            state.queue_healthy = false;
            state.last_error = Some("telemetry queue capacity exceeded".to_owned());
        }
        Err(error) => {
            state.queue_healthy = false;
            state.last_error = Some(error.to_string());
        }
    }
}

pub struct InstrumentedRuntime<'a, R> {
    inner: R,
    telemetry: Arc<TelemetryRuntime<'a>>,
    flush_on_command: bool,
}

impl<'a, R> InstrumentedRuntime<'a, R> {
    pub fn new(inner: R, telemetry: Arc<TelemetryRuntime<'a>>) -> Self {
        Self {
            inner,
            telemetry,
            flush_on_command: false,
        }
    }

    pub fn flushing(inner: R, telemetry: Arc<TelemetryRuntime<'a>>) -> Self {
        Self {
            inner,
            telemetry,
            flush_on_command: true,
        }
    }

    pub fn telemetry(&self) -> &Arc<TelemetryRuntime<'a>> {
        &self.telemetry
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Scheduler> Scheduler for InstrumentedRuntime<'_, R> {
    fn admit_command(
        &self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) -> Result<(), ServiceError> {
        self.inner
            .admit_command(principal_id, idempotency_key, command)
    }

    fn command_rejected(
        &self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) {
        self.inner
            .command_rejected(principal_id, idempotency_key, command);
    }

    fn command_committed(
        &self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) -> Result<(), ServiceError> {
        self.inner
            .command_committed(principal_id, idempotency_key, command)
    }

    fn command_completed(&self, observation: CommandObservation<'_>) {
        self.inner.command_completed(observation);
        self.telemetry.emit_api_command(observation);
        if self.flush_on_command {
            let _ = self.telemetry.flush();
        }
    }
}

impl<R: CapabilityService> CapabilityService for InstrumentedRuntime<'_, R> {
    fn auth_resolved(&self, run_id: RunId, granted: bool) {
        self.inner.auth_resolved(run_id, granted);
    }

    fn list(&self, project_id: ProjectId) -> Vec<crate::api::service::CapabilityProjection> {
        self.inner.list(project_id)
    }
}

impl<R: LeaseService> LeaseService for InstrumentedRuntime<'_, R> {
    fn deletion_requested(&self, thread_id: ThreadId) {
        self.inner.deletion_requested(thread_id);
    }
}

impl<R: ArtifactService> ArtifactService for InstrumentedRuntime<'_, R> {
    fn commit_verified<T>(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        command: &Command,
        commit: impl FnOnce() -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        self.inner
            .commit_verified(principal_id, project_id, command, commit)
    }

    fn metadata_registered(&self, metadata: &crate::api::service::ArtifactMetadataProjection) {
        self.inner.metadata_registered(metadata);
    }

    fn store_mcp_callback_content(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        run_id: crate::domain::ids::RunId,
        callback_id: crate::domain::ids::McpCallbackId,
        idempotency_key: &crate::store::sqlite::idempotency::IdempotencyKey,
        bytes: &[u8],
        expires_at_unix_micros: i64,
    ) -> Result<crate::domain::mcp_callback::McpCallbackArtifactRef, ServiceError> {
        self.inner.store_mcp_callback_content(
            principal_id,
            project_id,
            run_id,
            callback_id,
            idempotency_key,
            bytes,
            expires_at_unix_micros,
        )
    }

    fn mcp_callback_revision_live(&self, revision: &str) -> bool {
        self.inner.mcp_callback_revision_live(revision)
    }

    fn mcp_callback_content_public(
        &self,
        callback: &crate::domain::mcp_callback::McpCallbackProjection,
        content: &serde_json::Value,
    ) -> bool {
        self.inner.mcp_callback_content_public(callback, content)
    }

    fn with_mcp_callback_revision<T>(
        &self,
        revision: &str,
        commit: impl FnOnce(&str) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        self.inner.with_mcp_callback_revision(revision, commit)
    }
}

fn otel_trace_id(source: &str) -> String {
    if source.len() == 32
        && source.bytes().all(|byte| byte.is_ascii_hexdigit())
        && source.bytes().any(|byte| byte != b'0')
    {
        return source.to_ascii_lowercase();
    }
    let digest = blake3::hash(source.as_bytes());
    digest.as_bytes()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn otel_span_id(trace_id: &str, operation: &str, sequence: u64) -> String {
    let mut input = Vec::with_capacity(trace_id.len() + operation.len() + 8);
    input.extend_from_slice(trace_id.as_bytes());
    input.extend_from_slice(operation.as_bytes());
    input.extend_from_slice(&sequence.to_be_bytes());
    blake3::hash(&input).as_bytes()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
