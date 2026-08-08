use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use kit::{
    api::service::CommandObservation,
    domain::events::TraceId,
    domain::secret::{SecretCustody, SecretLease},
    telemetry::otel::{
        ADAPTER_VERSION, Adapter, AeadProvider, AttributeValue, DropPolicy, EncryptionKeyHandle,
        EnqueueOutcome, ExportBatch, ExportError, Exporter, LogRecord, LogSeverity, Metric,
        MetricName, MetricValue, RFC_SPAN_NAMES, Resource, RetainedTelemetry, RetentionError,
        RetentionHook, RetentionPolicy, RetentionSink, RunEnvelope, RunError, RuntimeRetention,
        Span, SpanEvent, SpanKind, SpanName, SpanStatus, TelemetryItem, TelemetryReadinessPolicy,
        TelemetryRuntime,
    },
};

const TRACE_ID: &str = "00000000000000000000000000000001";
const CANARY: &str = "kit-otel-canary+/=42";

fn span(id: u64, name: SpanName, parent: Option<u64>) -> Span {
    Span {
        trace_id: TRACE_ID.to_owned(),
        span_id: format!("{id:016x}"),
        parent_span_id: parent.map(|parent| format!("{parent:016x}")),
        span_name: name,
        kind: SpanKind::Internal,
        start_unix_nanos: id,
        end_unix_nanos: id + 1,
        attributes: BTreeMap::new(),
        events: Vec::new(),
        status: SpanStatus::Ok,
    }
}

fn rfc_spans() -> Vec<Span> {
    vec![
        span(1, SpanName::ApiCommand, None),
        span(2, SpanName::RunAttempt, Some(1)),
        span(3, SpanName::ModelCall, Some(2)),
        span(4, SpanName::ToolCall, Some(2)),
        span(5, SpanName::NestedToolCall, Some(4)),
        span(6, SpanName::ProcessExec, Some(4)),
        span(7, SpanName::ChildRun, Some(2)),
        span(8, SpanName::VerificationCheck, Some(2)),
        span(9, SpanName::CompactionCheckpoint, Some(2)),
    ]
}

#[test]
fn exported_span_names_equal_the_rfc_set_and_parentage_is_enforced() {
    let mut batch = ExportBatch::empty(Resource::default());
    batch.spans = rfc_spans();
    batch.validate().unwrap();
    assert_eq!(
        batch.spans.iter().map(Span::name).collect::<BTreeSet<_>>(),
        RFC_SPAN_NAMES.into_iter().collect()
    );

    batch.spans[5].parent_span_id = Some("0000000000000002".to_owned());
    assert!(
        batch
            .to_canonical_json()
            .unwrap_err()
            .to_string()
            .contains("invalid parent")
    );

    let mut batch = ExportBatch::empty(Resource::default());
    batch.spans = rfc_spans();
    batch
        .spans
        .push(span(10, SpanName::NestedToolCall, Some(5)));
    assert!(batch.validate().unwrap_err().contains("invalid parent"));
}

#[test]
fn canonical_export_validates_ids_and_times_and_is_deterministic() {
    let mut batch = ExportBatch::empty(Resource {
        attributes: BTreeMap::from([
            ("z".to_owned(), AttributeValue::U64(1)),
            ("a".to_owned(), AttributeValue::U64(2)),
        ]),
    });
    batch.spans = rfc_spans();
    assert_eq!(
        batch.to_canonical_json().unwrap(),
        batch.to_canonical_json().unwrap()
    );

    batch.spans[0].trace_id = "not-a-trace-id".to_owned();
    assert!(
        batch
            .to_canonical_json()
            .unwrap_err()
            .to_string()
            .contains("invalid trace id")
    );

    batch.spans = rfc_spans();
    batch.spans[0].end_unix_nanos = 0;
    assert!(
        batch
            .to_canonical_json()
            .unwrap_err()
            .to_string()
            .contains("ends before it starts")
    );
}

#[test]
fn metric_schemas_have_only_fixed_bounded_labels() {
    let names = [
        MetricName::RunCount,
        MetricName::RunLatency,
        MetricName::ModelTokens,
        MetricName::ToolCalls,
        MetricName::RunCost,
        MetricName::CacheTokens,
        MetricName::VerificationChecks,
        MetricName::Errors,
    ];
    let forbidden = ["run_id", "path", "prompt", "command"];
    for name in names {
        assert!(
            name.label_schema()
                .iter()
                .all(|label| !forbidden.contains(label))
        );
    }
    assert!(
        Metric::new(
            MetricName::RunCount,
            "1",
            MetricValue::Counter { value: 1 },
            BTreeMap::from([("run_id".to_owned(), "run-1".to_owned())]),
            1,
        )
        .is_err()
    );
    assert!(
        Metric::new(
            MetricName::RunCount,
            "1",
            MetricValue::Counter { value: 1 },
            BTreeMap::from([("outcome".to_owned(), "user-defined".to_owned())]),
            1,
        )
        .is_err()
    );
}

#[derive(Default)]
struct CaptureExporter(Vec<ExportBatch>);

impl Exporter for CaptureExporter {
    fn export(&mut self, batch: &ExportBatch) -> Result<(), ExportError> {
        self.0.push(batch.clone());
        Ok(())
    }
}

#[test]
fn canary_is_absent_from_every_export_signal() {
    let secrets = [SecretLease::new(CANARY.as_bytes().to_vec())];
    let resource = Resource {
        attributes: BTreeMap::from([(
            "service.name".to_owned(),
            AttributeValue::String(format!("kit-{CANARY}")),
        )]),
    };
    let mut adapter = Adapter::new(resource, &secrets, 4, DropPolicy::DropNewest).unwrap();
    let mut trace = span(1, SpanName::ApiCommand, None);
    trace.attributes.insert(
        "error.message".to_owned(),
        AttributeValue::String(format!("provider said {CANARY}")),
    );
    trace.events.push(SpanEvent {
        name: format!("event-{CANARY}"),
        timestamp_unix_nanos: 1,
        attributes: BTreeMap::new(),
    });
    trace.status = SpanStatus::Error(Some(CANARY.to_owned()));
    adapter.enqueue(TelemetryItem::Span(trace)).unwrap();
    adapter
        .enqueue(TelemetryItem::Log(LogRecord {
            timestamp_unix_nanos: 1,
            severity: LogSeverity::Error,
            body: AttributeValue::String(format!("failed: {CANARY}")),
            attributes: BTreeMap::from([(
                "password".to_owned(),
                AttributeValue::String(CANARY.to_owned()),
            )]),
            trace_id: None,
            span_id: None,
        }))
        .unwrap();
    adapter
        .enqueue(TelemetryItem::RunEnvelope(RunEnvelope {
            errors: Some(vec![RunError {
                class: Some("model".to_owned()),
                code: Some("provider_error".to_owned()),
                message: Some(CANARY.to_owned()),
            }]),
            ..RunEnvelope::default()
        }))
        .unwrap();
    adapter
        .enqueue(TelemetryItem::Metric(
            Metric::new(
                MetricName::RunCount,
                format!("1-{CANARY}"),
                MetricValue::Counter { value: 1 },
                BTreeMap::from([("outcome".to_owned(), "failed".to_owned())]),
                1,
            )
            .unwrap(),
        ))
        .unwrap();

    let mut exporter = CaptureExporter::default();
    assert_eq!(adapter.flush(&mut exporter).unwrap(), 4);
    let corpus = String::from_utf8(exporter.0[0].to_canonical_json().unwrap()).unwrap();
    assert_eq!(corpus.matches(CANARY).count(), 0, "{corpus}");
}

#[test]
fn ordered_otel_state_redacts_secrets_split_across_keys_values_and_events() {
    let secrets = [SecretLease::new("split-secret-event-tail")];
    let mut adapter =
        Adapter::new(Resource::default(), &secrets, 1, DropPolicy::DropNewest).unwrap();
    let mut trace = span(1, SpanName::ApiCommand, None);
    trace.attributes.insert(
        "split-secret-".to_owned(),
        AttributeValue::String("event-".to_owned()),
    );
    trace.events.push(SpanEvent {
        name: "tail".to_owned(),
        timestamp_unix_nanos: 1,
        attributes: BTreeMap::from([(
            "valid.key".to_owned(),
            AttributeValue::String("public".to_owned()),
        )]),
    });

    adapter.enqueue(TelemetryItem::Span(trace)).unwrap();
    let mut exporter = CaptureExporter::default();
    adapter.flush(&mut exporter).unwrap();
    let exported = serde_json::to_string(&exporter.0[0]).unwrap();
    assert!(!exported.contains("split-secret-event-tail"));
    assert!(exported.contains("redacted.0"));
    assert!(exported.contains("[REDACTED]"));
}

#[test]
fn complete_projected_batch_rejects_reconstruction_across_queued_nested_fields() {
    let secret = "fragmentattribute-event-status-fragmentqueued";
    let secrets = [SecretLease::new(secret)];
    let mut adapter =
        Adapter::new(Resource::default(), &secrets, 4, DropPolicy::DropNewest).unwrap();
    let mut first = span(1, SpanName::ApiCommand, None);
    first.attributes.insert(
        "fragment".to_owned(),
        AttributeValue::String("attribute-".to_owned()),
    );
    first.events.push(SpanEvent {
        name: "event-".to_owned(),
        timestamp_unix_nanos: 1,
        attributes: BTreeMap::new(),
    });
    first.status = SpanStatus::Error(Some("status-".to_owned()));
    assert_eq!(
        adapter.enqueue(TelemetryItem::Span(first)).unwrap(),
        EnqueueOutcome::Accepted
    );

    let mut second = span(2, SpanName::RunAttempt, Some(1));
    second.attributes.insert(
        "fragment".to_owned(),
        AttributeValue::String("queued".to_owned()),
    );
    let error = adapter.enqueue(TelemetryItem::Span(second)).unwrap_err();
    assert!(error.to_string().contains("reconstructs active secret"));
    assert_eq!(adapter.queued(), 1);
}

#[test]
fn queue_is_bounded_and_export_failure_preserves_records() {
    let mut adapter = Adapter::new(Resource::default(), &[], 2, DropPolicy::DropNewest).unwrap();
    assert_eq!(
        adapter
            .enqueue(TelemetryItem::RunEnvelope(RunEnvelope::default()))
            .unwrap(),
        EnqueueOutcome::Accepted
    );
    assert_eq!(
        adapter
            .enqueue(TelemetryItem::RunEnvelope(RunEnvelope::default()))
            .unwrap(),
        EnqueueOutcome::Accepted
    );
    assert_eq!(
        adapter
            .enqueue(TelemetryItem::RunEnvelope(RunEnvelope::default()))
            .unwrap(),
        EnqueueOutcome::DroppedNewest
    );
    assert_eq!(adapter.queued(), 2);
    assert_eq!(adapter.dropped(), 1);

    struct Failing;
    impl Exporter for Failing {
        fn export(&mut self, _: &ExportBatch) -> Result<(), ExportError> {
            Err(ExportError("offline".to_owned()))
        }
    }
    assert!(adapter.flush(&mut Failing).is_err());
    assert_eq!(adapter.queued(), 2);
}

#[test]
fn unavailable_run_envelope_fields_are_explicit_nulls() {
    let value = serde_json::to_value(RunEnvelope::default()).unwrap();
    assert_eq!(value["schema_version"], ADAPTER_VERSION);
    for field in [
        "outcome",
        "latency_ms",
        "model_usage",
        "tool_usage",
        "cost_microusd",
        "cache",
        "checks",
        "errors",
    ] {
        assert!(value[field].is_null(), "{field} was not null");
    }
}

struct FakeAead {
    calls: AtomicU64,
    fail: bool,
}

impl AeadProvider for FakeAead {
    fn seal(
        &self,
        key: &EncryptionKeyHandle,
        plaintext: &[u8],
        associated_data: &[u8],
    ) -> Result<Vec<u8>, String> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        assert_eq!(key.identifier(), "local-key");
        assert!(associated_data.starts_with(b"kit.telemetry.v1;retain_until="));
        assert!(!plaintext.is_empty());
        if self.fail {
            Err("seal failed".to_owned())
        } else {
            Ok(b"ciphertext".to_vec())
        }
    }
}

#[derive(Default)]
struct CaptureSink(Option<RetainedTelemetry>);

impl RetentionSink for CaptureSink {
    fn persist(&mut self, record: RetainedTelemetry) -> Result<(), String> {
        self.0 = Some(record);
        Ok(())
    }
}

fn sensitive_policy() -> RetentionPolicy {
    RetentionPolicy {
        retain_until_unix_nanos: 42,
        sensitive: true,
        encryption_required: true,
    }
}

#[test]
fn sensitive_retention_invokes_injected_aead_and_records_declared_retention() {
    let aead = FakeAead {
        calls: AtomicU64::new(0),
        fail: false,
    };
    let key = EncryptionKeyHandle::new("local-key").unwrap();
    let hook = RetentionHook::new(sensitive_policy(), Some(&aead), Some(&key));
    let mut sink = CaptureSink::default();
    hook.retain(&ExportBatch::empty(Resource::default()), &mut sink)
        .unwrap();
    assert_eq!(aead.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        sink.0,
        Some(RetainedTelemetry {
            schema_version: ADAPTER_VERSION,
            retain_until_unix_nanos: 42,
            encrypted: true,
            bytes: b"ciphertext".to_vec(),
        })
    );
}

#[test]
fn required_encryption_fails_closed_when_missing_or_failing() {
    let batch = ExportBatch::empty(Resource::default());
    let mut sink = CaptureSink::default();
    assert_eq!(
        RetentionHook::new(sensitive_policy(), None, None).retain(&batch, &mut sink),
        Err(RetentionError::EncryptionUnavailable)
    );
    assert!(sink.0.is_none());

    let aead = FakeAead {
        calls: AtomicU64::new(0),
        fail: true,
    };
    let key = EncryptionKeyHandle::new("local-key").unwrap();
    assert!(matches!(
        RetentionHook::new(sensitive_policy(), Some(&aead), Some(&key)).retain(&batch, &mut sink),
        Err(RetentionError::Encryption(_))
    ));
    assert_eq!(aead.calls.load(Ordering::Acquire), 1);
    assert!(sink.0.is_none());
}

struct RuntimeExporter {
    fail: Arc<AtomicBool>,
    batches: Arc<Mutex<Vec<ExportBatch>>>,
}

impl Exporter for RuntimeExporter {
    fn export(&mut self, batch: &ExportBatch) -> Result<(), ExportError> {
        if self.fail.load(Ordering::Acquire) {
            Err(ExportError("offline".to_owned()))
        } else {
            self.batches.lock().unwrap().push(batch.clone());
            Ok(())
        }
    }
}

fn runtime_exporter() -> (
    RuntimeExporter,
    Arc<AtomicBool>,
    Arc<Mutex<Vec<ExportBatch>>>,
) {
    let fail = Arc::new(AtomicBool::new(false));
    let batches = Arc::new(Mutex::new(Vec::new()));
    (
        RuntimeExporter {
            fail: fail.clone(),
            batches: batches.clone(),
        },
        fail,
        batches,
    )
}

#[test]
fn command_hook_emits_an_api_command_span() {
    let (exporter, _, batches) = runtime_exporter();
    let runtime = TelemetryRuntime::local(
        Resource::default(),
        &[],
        8,
        DropPolicy::DropNewest,
        exporter,
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    let trace_id = TraceId::parse(TRACE_ID).unwrap();
    runtime.emit_api_command(CommandObservation {
        trace_id: &trace_id,
        operation: "run.start",
        start_unix_nanos: 10,
        end_unix_nanos: 20,
        succeeded: true,
    });

    assert_eq!(runtime.flush().unwrap(), 5);
    let batches = batches.lock().unwrap();
    let span = &batches[0].spans[0];
    assert_eq!(span.span_name, SpanName::ApiCommand);
    assert!(span.events.is_empty());
    assert_eq!(
        span.attributes["kit.operation"],
        AttributeValue::String("run.start".to_owned())
    );
    assert_eq!(batches[0].spans[1].span_name, SpanName::RunAttempt);
    assert_eq!(
        batches[0].spans[1].parent_span_id,
        Some(span.span_id.clone())
    );
    assert!(batches[0].run_envelopes.is_empty());
    assert_eq!(batches[0].metrics.len(), 2);
    assert_eq!(batches[0].logs.len(), 1);
}

#[test]
fn runtime_preserves_a_bounded_queue_and_recovers_required_readiness() {
    let (exporter, fail, batches) = runtime_exporter();
    let runtime = TelemetryRuntime::local(
        Resource::default(),
        &[],
        2,
        DropPolicy::DropNewest,
        exporter,
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    assert_eq!(
        runtime.emit_run_envelope(RunEnvelope::default()).unwrap(),
        EnqueueOutcome::Accepted
    );
    assert_eq!(
        runtime.emit_run_envelope(RunEnvelope::default()).unwrap(),
        EnqueueOutcome::Accepted
    );
    assert_eq!(
        runtime.emit_run_envelope(RunEnvelope::default()).unwrap(),
        EnqueueOutcome::DroppedNewest
    );
    fail.store(true, Ordering::Release);
    assert!(runtime.flush().is_err());
    let health = runtime.health();
    assert_eq!(health.queued, 2);
    assert_eq!(health.dropped, 1);
    assert!(!health.exporter_healthy);
    assert!(!health.ready);

    fail.store(false, Ordering::Release);
    assert_eq!(runtime.flush().unwrap(), 2);
    assert!(runtime.health().ready);
    assert_eq!(batches.lock().unwrap()[0].run_envelopes.len(), 2);
}

#[test]
fn pending_batch_failure_updates_required_health_until_the_queue_drains() {
    let custody = SecretCustody::default();
    let (exporter, _, _) = runtime_exporter();
    let runtime = TelemetryRuntime::encrypted_project(
        Resource::default(),
        &custody,
        4,
        DropPolicy::DropNewest,
        exporter,
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    for body in ["pending-", "secret"] {
        runtime
            .emit(TelemetryItem::Log(LogRecord {
                timestamp_unix_nanos: 1,
                severity: LogSeverity::Info,
                body: AttributeValue::String(body.to_owned()),
                attributes: BTreeMap::new(),
                trace_id: None,
                span_id: None,
            }))
            .unwrap();
    }
    custody.register(
        "runtime",
        "test",
        Arc::new(SecretLease::new("pending-secret")),
    );

    assert!(runtime.flush().is_err());
    let failed = runtime.health();
    assert!(!failed.queue_healthy);
    assert!(!failed.ready);
    assert_eq!(failed.queued, 2);

    custody.remove_owner("runtime");
    assert_eq!(runtime.flush().unwrap(), 2);
    let recovered = runtime.health();
    assert!(recovered.queue_healthy);
    assert!(recovered.ready);
    assert_eq!(recovered.queued, 0);
}

struct SyncAead(AtomicU64);

impl AeadProvider for SyncAead {
    fn seal(
        &self,
        key: &EncryptionKeyHandle,
        plaintext: &[u8],
        associated_data: &[u8],
    ) -> Result<Vec<u8>, String> {
        self.0.fetch_add(1, Ordering::AcqRel);
        let mut input = Vec::new();
        input.extend_from_slice(key.identifier().as_bytes());
        input.extend_from_slice(associated_data);
        input.extend_from_slice(plaintext);
        Ok(blake3::hash(&input).as_bytes().to_vec())
    }
}

struct SharedSink {
    records: Arc<Mutex<Vec<RetainedTelemetry>>>,
    fail: Arc<AtomicBool>,
}

impl RetentionSink for SharedSink {
    fn persist(&mut self, record: RetainedTelemetry) -> Result<(), String> {
        if self.fail.load(Ordering::Acquire) {
            return Err("retention unavailable".to_owned());
        }
        self.records.lock().unwrap().push(record);
        Ok(())
    }
}

#[test]
fn retention_failure_marks_required_health_and_preserves_the_queue_for_retry() {
    let (exporter, _, _) = runtime_exporter();
    let retained = Arc::new(Mutex::new(Vec::new()));
    let retention_fail = Arc::new(AtomicBool::new(true));
    let aead = SyncAead(AtomicU64::new(0));
    let key = EncryptionKeyHandle::new("state-root:telemetry-key").unwrap();
    let runtime = TelemetryRuntime::new(
        Resource::default(),
        &[],
        4,
        DropPolicy::DropNewest,
        exporter,
        RuntimeRetention::encrypted(
            sensitive_policy(),
            &aead,
            &key,
            SharedSink {
                records: retained.clone(),
                fail: retention_fail.clone(),
            },
        ),
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    runtime.emit_run_envelope(RunEnvelope::default()).unwrap();

    assert!(runtime.flush().is_err());
    assert_eq!(runtime.health().queued, 1);
    assert!(!runtime.health().retention_healthy);
    assert!(!runtime.health().ready);
    retention_fail.store(false, Ordering::Release);
    assert_eq!(runtime.flush().unwrap(), 1);
    assert_eq!(retained.lock().unwrap().len(), 1);
    assert!(runtime.health().ready);
}

#[test]
fn shutdown_exports_run_hierarchy_and_retains_only_encrypted_bytes() {
    let (exporter, _, batches) = runtime_exporter();
    let retained = Arc::new(Mutex::new(Vec::new()));
    let retention_fail = Arc::new(AtomicBool::new(false));
    let aead = SyncAead(AtomicU64::new(0));
    let key = EncryptionKeyHandle::new("state-root:telemetry-key").unwrap();
    let secrets = [SecretLease::new(CANARY.as_bytes().to_vec())];
    let runtime = TelemetryRuntime::new(
        Resource::default(),
        &secrets,
        16,
        DropPolicy::DropNewest,
        exporter,
        RuntimeRetention::encrypted(
            sensitive_policy(),
            &aead,
            &key,
            SharedSink {
                records: retained.clone(),
                fail: retention_fail,
            },
        ),
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    for span in rfc_spans() {
        runtime.emit_span(span).unwrap();
    }
    runtime
        .emit_run_envelope(RunEnvelope {
            errors: Some(vec![RunError {
                class: Some("model".to_owned()),
                code: Some("provider_error".to_owned()),
                message: Some(CANARY.to_owned()),
            }]),
            ..RunEnvelope::default()
        })
        .unwrap();

    assert_eq!(runtime.shutdown().unwrap(), 10);
    assert_eq!(aead.0.load(Ordering::Acquire), 1);
    let batch = &batches.lock().unwrap()[0];
    assert_eq!(batch.spans.len(), RFC_SPAN_NAMES.len());
    assert_eq!(batch.run_envelopes.len(), 1);
    assert!(batch.spans.iter().all(|span| span.events.is_empty()));
    let retained = retained.lock().unwrap();
    assert!(retained[0].encrypted);
    assert!(!retained[0].bytes.is_empty());
    assert!(
        !retained[0]
            .bytes
            .windows(CANARY.len())
            .any(|bytes| bytes == CANARY.as_bytes())
    );
    assert!(!runtime.health().ready);
}
