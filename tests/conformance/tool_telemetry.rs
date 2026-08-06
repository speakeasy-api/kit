use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use sha2::{Digest, Sha256};

use kit::{
    api::service::AttemptDriverClaim,
    domain::{
        events::{TraceId, UtcDateTime},
        ids::{AttemptId, PrincipalId, ProjectId, RunId},
        lifecycle::{AttemptOwnership, FencingToken},
    },
    telemetry::{
        otel::{
            DropPolicy, DurableLocalExporter, EncryptedLearningFrame, ExportBatch, ExportError,
            Exporter, Metric, MetricName, MetricValue, Resource, TelemetryReadinessPolicy,
            TelemetryRuntime,
        },
        tool_learning::{
            self, AnalysisSignal, CausalResult, CausalUnavailable, DownstreamGrade,
            DownstreamGradeRecord, ErrorClass, ErrorCode, ErrorStage, ExperimentArm, FrozenFactors,
            LearningCandidate, LearningCapabilityKind, LearningCommon, LearningOperation,
            LearningStatus, LearningSurface, PointerDomain, PreparedLearningCapture,
            PreregisteredExperiment, ProjectPointerHasher, RetryClass, ToolLearningAnalyzer,
            ToolLearningEvent,
        },
    },
    test_support,
};

struct Fixture {
    root: std::path::PathBuf,
    store: kit::store::sqlite::append::SqliteStore,
    run: RunId,
    owner: AttemptOwnership,
    claim: AttemptDriverClaim,
    project: ProjectId,
    hasher: ProjectPointerHasher,
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn canonical_call_identity_includes_the_complete_request_digest() {
    let fixture = Fixture::new("call-request-digest");
    let first = PreparedLearningCapture::new(
        fixture.hasher.clone(),
        fixture.run,
        "turn-1",
        1,
        "tools_invoke",
        "provider-call-1",
        br#"{"value":1}"#,
        LearningSurface::Generic,
        b"capability",
        b"schema",
        Some(b"binding"),
        b"source",
        LearningCapabilityKind::Tool,
    )
    .unwrap();
    let second = PreparedLearningCapture::new(
        fixture.hasher.clone(),
        fixture.run,
        "turn-1",
        1,
        "tools_invoke",
        "provider-call-1",
        br#"{"value":2}"#,
        LearningSurface::Generic,
        b"capability",
        b"schema",
        Some(b"binding"),
        b"source",
        LearningCapabilityKind::Tool,
    )
    .unwrap();
    assert_ne!(first.call_pointer(), second.call_pointer());
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-tool-learning-{name}-{}",
            kit::domain::ids::EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let mut store = test_support::open_sqlite_store(root.join("events.sqlite3")).unwrap();
        let principal = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
        let project = ProjectId::parse("project_00000000000000000000000001").unwrap();
        let run = RunId::parse("run_00000000000000000000000001").unwrap();
        let owner = AttemptOwnership::new(
            AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
            principal,
            FencingToken::new(1),
        );
        let claim = store
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: run,
                attempt_id: owner.attempt_id,
                principal_id: principal,
                fence: owner.fencing_token,
                lease_version: 1,
                expires_at_unix_micros: 0,
            })
            .unwrap();
        Self {
            root,
            store,
            run,
            owner,
            claim,
            project,
            hasher: ProjectPointerHasher::new(project, &[11; 32]),
        }
    }

    fn common(
        &self,
        ordinal: u64,
        operation: LearningOperation,
        surface: LearningSurface,
        key: &str,
    ) -> LearningCommon {
        LearningCommon::new(
            &self.hasher,
            self.run,
            ordinal,
            operation,
            surface,
            key.as_bytes(),
            None,
            None,
            None,
        )
    }

    fn append(&mut self, event: &ToolLearningEvent) {
        tool_learning::append(
            &mut self.store,
            self.owner,
            self.claim,
            &self.hasher,
            UtcDateTime::parse("2026-08-05T12:00:00Z").unwrap(),
            TraceId::parse("tool-learning-test").unwrap(),
            event,
        )
        .unwrap();
    }

    fn terminalize_scheduler_run(&self) {
        rusqlite::Connection::open(self.root.join("events.sqlite3"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS store_clock (
                     singleton INTEGER PRIMARY KEY CHECK (singleton=1),
                     unix_micros INTEGER NOT NULL
                 );
                 INSERT OR IGNORE INTO store_clock VALUES (1,0);",
            )
            .unwrap();
        let scheduler =
            kit::runtime::scheduler::DurableScheduler::open(self.root.join("events.sqlite3"))
                .unwrap();
        scheduler
            .register_run(self.run, self.owner.principal_id, "tool-learning-stats")
            .unwrap();
        scheduler.admit_run(self.run).unwrap();
        scheduler.finish_run(self.run, false).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn six_events(fixture: &Fixture) -> Vec<ToolLearningEvent> {
    let call = fixture
        .hasher
        .pointer(PointerDomain::Call, b"provider-call-1");
    vec![
        ToolLearningEvent::Opportunity {
            common: fixture.common(
                1,
                LearningOperation::Projection,
                LearningSurface::Discovery,
                "opportunity-1",
            ),
            offered: 4,
            eager: 3,
            deferred: 0,
            generic_available: true,
            projection: fixture
                .hasher
                .pointer(PointerDomain::Schema, b"final-provider-projection"),
            candidates: Vec::new(),
            detail_artifact: None,
        },
        ToolLearningEvent::Search {
            common: fixture.common(
                2,
                LearningOperation::Search,
                LearningSurface::Discovery,
                "search-1",
            ),
            query: fixture
                .hasher
                .pointer(PointerDomain::Query, b"database lookup"),
            status: LearningStatus::Succeeded,
            result_count: 1,
            detail_artifact: None,
        },
        ToolLearningEvent::Inspection {
            common: fixture.common(
                3,
                LearningOperation::Inspect,
                LearningSurface::Discovery,
                "inspection-1",
            ),
            handle: fixture
                .hasher
                .pointer(PointerDomain::Handle, b"authorized-handle"),
            status: LearningStatus::Succeeded,
        },
        ToolLearningEvent::Call {
            common: fixture.common(
                4,
                LearningOperation::Invoke,
                LearningSurface::Generic,
                "call-1",
            ),
            call: call.clone(),
            binding: Some(fixture.hasher.pointer(PointerDomain::Binding, b"binding-1")),
            source: Some(fixture.hasher.pointer(PointerDomain::Source, b"source-1")),
            kind: Some(LearningCapabilityKind::Tool),
            sequence: Some(
                fixture
                    .hasher
                    .pointer(PointerDomain::Sequence, b"sequence-1"),
            ),
            sequence_order: Some(1),
            kernel_intent: Some(
                fixture
                    .hasher
                    .pointer(PointerDomain::KernelEvent, b"intent-1"),
            ),
        },
        ToolLearningEvent::Error {
            common: fixture.common(
                5,
                LearningOperation::Invoke,
                LearningSurface::Generic,
                "error-1",
            ),
            call: call.clone(),
            stage: ErrorStage::Transport,
            class: ErrorClass::Transport,
            code: ErrorCode::Timeout,
            field: None,
            retry: RetryClass::Safe,
            dispatched: true,
            known: true,
        },
        ToolLearningEvent::Outcome {
            common: fixture.common(
                6,
                LearningOperation::Invoke,
                LearningSurface::Generic,
                "outcome-1",
            ),
            call,
            status: LearningStatus::Failed,
            dispatched: true,
            known: true,
            cost_microusd: Some(1),
            kernel_outcome: Some(
                fixture
                    .hasher
                    .pointer(PointerDomain::KernelEvent, b"outcome-1"),
            ),
        },
    ]
}

#[test]
fn tool_learning_stream_has_all_six_closed_classes_and_one_terminal_outcome() {
    let mut fixture = Fixture::new("six-classes");
    let events = six_events(&fixture);
    for event in &events {
        fixture.append(event);
        fixture.append(event);
    }
    let recovered = tool_learning::records(&fixture.store, fixture.run, &fixture.hasher).unwrap();
    assert_eq!(recovered.len(), 6);
    assert_eq!(
        recovered
            .iter()
            .map(ToolLearningEvent::class_name)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "opportunity",
            "search",
            "inspection",
            "call",
            "error",
            "outcome",
        ])
    );
    assert_eq!(
        recovered
            .iter()
            .filter(|event| matches!(event, ToolLearningEvent::Outcome { .. }))
            .count(),
        1
    );
}

#[test]
fn continuation_cannot_append_a_second_call_projection_for_the_same_call() {
    let mut fixture = Fixture::new("one-call-projection");
    let events = six_events(&fixture);
    for event in &events[..4] {
        fixture.append(event);
    }
    let ToolLearningEvent::Call {
        common,
        call,
        binding,
        source,
        kind,
        sequence,
        sequence_order,
        ..
    } = &events[3]
    else {
        unreachable!()
    };
    let duplicate = ToolLearningEvent::Call {
        common: LearningCommon::new(
            &fixture.hasher,
            fixture.run,
            common.ordinal + 1,
            common.operation,
            common.surface,
            b"continuation-link",
            common.request.clone(),
            common.capability.clone(),
            common.schema.clone(),
        ),
        call: call.clone(),
        binding: binding.clone(),
        source: source.clone(),
        kind: *kind,
        sequence: sequence.clone(),
        sequence_order: *sequence_order,
        kernel_intent: Some(
            fixture
                .hasher
                .pointer(PointerDomain::KernelEvent, b"later-intent"),
        ),
    };
    assert!(
        tool_learning::append(
            &mut fixture.store,
            fixture.owner,
            fixture.claim,
            &fixture.hasher,
            UtcDateTime::parse("2026-08-05T12:00:01Z").unwrap(),
            TraceId::parse("one-call-projection").unwrap(),
            &duplicate,
        )
        .is_err()
    );
    assert_eq!(
        tool_learning::records(&fixture.store, fixture.run, &fixture.hasher)
            .unwrap()
            .iter()
            .filter(|event| matches!(event, ToolLearningEvent::Call { .. }))
            .count(),
        1
    );
}

#[test]
fn schema_is_deterministic_bounded_and_contains_no_raw_or_encoded_canaries() {
    let fixture = Fixture::new("schema");
    let raw = "RAW-CANARY-prompt-query-description-schema-args-output-url-error-reasoning-secret";
    let encoded = "UkFXLUNBTkFSWS1wcm9tcHQtcXVlcnk=";
    let event = ToolLearningEvent::Search {
        common: fixture.common(
            1,
            LearningOperation::Search,
            LearningSurface::Discovery,
            "golden-search",
        ),
        query: fixture
            .hasher
            .pointer(PointerDomain::Query, format!("{raw}:{encoded}").as_bytes()),
        status: LearningStatus::Succeeded,
        result_count: 1,
        detail_artifact: Some(fixture.hasher.pointer(
            PointerDomain::Artifact,
            format!("{encoded}:{raw}").as_bytes(),
        )),
    };
    let first = serde_json::to_vec(&event).unwrap();
    let second = serde_json::to_vec(&event).unwrap();
    assert_eq!(first, second);
    let wire = String::from_utf8(first).unwrap();
    assert!(wire.starts_with("{\"event_class\":\"search\",\"common\":{"));
    assert!(wire.contains("\"format\":\"tool_learning.v1\""));
    assert!(!wire.contains(raw));
    assert!(!wire.contains(encoded));
    event.validate().unwrap();
}

#[test]
fn append_rejects_forged_and_cross_project_pointers() {
    let mut fixture = Fixture::new("pointer-forgery");
    let other = ProjectPointerHasher::new(ProjectId::generate().unwrap(), &[11; 32]);
    let event = ToolLearningEvent::Search {
        common: LearningCommon::new(
            &other,
            fixture.run,
            1,
            LearningOperation::Search,
            LearningSurface::Discovery,
            b"cross-project",
            None,
            None,
            None,
        ),
        query: other.pointer(PointerDomain::Query, b"query"),
        status: LearningStatus::Succeeded,
        result_count: 0,
        detail_artifact: None,
    };
    assert!(matches!(
        tool_learning::append(
            &mut fixture.store,
            fixture.owner,
            fixture.claim,
            &fixture.hasher,
            UtcDateTime::parse("2026-08-05T12:00:00Z").unwrap(),
            TraceId::parse("pointer-forgery").unwrap(),
            &event,
        ),
        Err(tool_learning::ToolLearningError::InvalidPointer)
    ));

    let valid = fixture.hasher.pointer(PointerDomain::Query, b"query");
    let mut forged = valid.as_str().as_bytes().to_vec();
    let last = forged.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    let forged: tool_learning::LearningPointer =
        serde_json::from_str(&format!("\"{}\"", String::from_utf8(forged).unwrap())).unwrap();
    assert!(
        fixture
            .hasher
            .validate(&forged, PointerDomain::Query)
            .is_err()
    );
    assert!(
        fixture
            .hasher
            .validate(&valid, PointerDomain::Schema)
            .is_err()
    );
}

#[test]
fn complete_record_mac_rejects_status_count_class_and_pointer_substitution() {
    for (name, index, mutate) in [
        (
            "status",
            1,
            (|value: &mut serde_json::Value, _: &Fixture| {
                value["status"] = serde_json::json!("failed");
            }) as fn(&mut serde_json::Value, &Fixture),
        ),
        ("count", 0, |value: &mut serde_json::Value, _: &Fixture| {
            value["offered"] = serde_json::json!(3);
        }),
        ("class", 4, |value: &mut serde_json::Value, _: &Fixture| {
            value["class"] = serde_json::json!("auth");
        }),
        (
            "pointer",
            4,
            |value: &mut serde_json::Value, fixture: &Fixture| {
                value["common"]["request"] = serde_json::json!(
                    fixture
                        .hasher
                        .pointer(PointerDomain::Request, b"substituted")
                );
            },
        ),
    ] {
        let mut fixture = Fixture::new(name);
        for event in six_events(&fixture) {
            fixture.append(&event);
        }
        let stored = tool_learning::records(&fixture.store, fixture.run, &fixture.hasher).unwrap();
        let mut value = serde_json::to_value(&stored[index]).unwrap();
        mutate(&mut value, &fixture);
        let connection = rusqlite::Connection::open(fixture.root.join("events.sqlite3")).unwrap();
        connection
            .execute(
                "UPDATE events SET payload=?1 WHERE payload=?2",
                rusqlite::params![
                    serde_json::to_vec(&value).unwrap(),
                    serde_json::to_vec(&stored[index]).unwrap(),
                ],
            )
            .unwrap();
        assert!(tool_learning::records(&fixture.store, fixture.run, &fixture.hasher).is_err());
    }
}

#[test]
fn encrypted_export_ack_survives_restart_and_frame_eviction_without_duplicates() {
    let mut fixture = Fixture::new("encrypted");
    let events = six_events(&fixture);
    for event in &events {
        fixture.append(event);
    }
    let path = fixture.root.join("telemetry.otel.enc");
    let key = [7_u8; 32];
    let runtime = TelemetryRuntime::encrypted_local(
        Resource::default(),
        &[],
        16,
        DropPolicy::DropNewest,
        DurableLocalExporter::open(&path, &key, 32 * 1024).unwrap(),
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    assert_eq!(
        runtime
            .export_learning_outbox(&mut fixture.store, &fixture.hasher)
            .unwrap(),
        6
    );
    assert_eq!(
        runtime
            .export_learning_outbox(&mut fixture.store, &fixture.hasher)
            .unwrap(),
        0
    );
    drop(runtime);
    let mut exporter = DurableLocalExporter::open(&path, &key, 32 * 1024).unwrap();
    let frames = exporter.read_learning_frames().unwrap();
    assert_eq!(frames.len(), 6);
    for frame in &frames {
        exporter.export_encrypted_learning(frame).unwrap();
    }
    for index in 0..400 {
        exporter
            .export_encrypted_learning(&EncryptedLearningFrame {
                frame_id: format!("filler-{index}"),
                ciphertext: vec![9; 128],
            })
            .unwrap();
    }
    let original_ids = frames
        .iter()
        .map(|frame| frame.frame_id.clone())
        .collect::<BTreeSet<_>>();
    drop(exporter);
    let mut exporter = DurableLocalExporter::open(&path, &key, 32 * 1024).unwrap();
    for frame in &frames {
        exporter.export_encrypted_learning(frame).unwrap();
    }
    let retained = exporter
        .read_learning_frames()
        .unwrap()
        .into_iter()
        .map(|frame| frame.frame_id)
        .collect::<Vec<_>>();
    assert_eq!(
        retained.iter().collect::<BTreeSet<_>>().len(),
        retained.len()
    );
    assert!(retained.len() <= 256);
    assert!(
        retained
            .iter()
            .all(|frame_id| !original_ids.contains(frame_id))
    );
    let encrypted = std::fs::read(format!("{}.learning.sqlite3", path.display())).unwrap();
    assert!(
        !encrypted
            .windows(b"tool_learning.v1".len())
            .any(|window| { window == b"tool_learning.v1" })
    );
}

#[test]
fn encrypted_export_drains_every_page_before_reporting_learning_healthy() {
    let mut fixture = Fixture::new("encrypted-pages");
    for index in 0..300_u64 {
        let event = ToolLearningEvent::Search {
            common: fixture.common(
                index + 1,
                LearningOperation::Search,
                LearningSurface::Discovery,
                &format!("search-{index}"),
            ),
            query: fixture
                .hasher
                .pointer(PointerDomain::Query, format!("query-{index}").as_bytes()),
            status: LearningStatus::Succeeded,
            result_count: 0,
            detail_artifact: None,
        };
        fixture.append(&event);
    }
    let runtime = TelemetryRuntime::encrypted_local(
        Resource::default(),
        &[],
        8,
        DropPolicy::DropNewest,
        DurableLocalExporter::open(
            fixture.root.join("paged.otel.enc"),
            &[23; 32],
            2 * 1024 * 1024,
        )
        .unwrap(),
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    assert_eq!(
        runtime
            .export_learning_outbox(&mut fixture.store, &fixture.hasher)
            .unwrap(),
        300
    );
    assert!(
        fixture
            .store
            .pending_learning_outbox(
                fixture
                    .hasher
                    .pointer(
                        PointerDomain::Project,
                        fixture.project.to_string().as_bytes(),
                    )
                    .as_str(),
                1,
            )
            .unwrap()
            .is_empty()
    );
    assert!(runtime.health().learning_healthy);
}

#[test]
fn learning_health_requires_every_durable_queue_to_be_drained() {
    for queue in ["outbox", "reconciliation", "snapshot", "terminal-overlay"] {
        let mut fixture = Fixture::new(queue);
        let project = fixture.hasher.pointer(
            PointerDomain::Project,
            fixture.project.to_string().as_bytes(),
        );
        if queue == "outbox" {
            fixture.append(&ToolLearningEvent::Search {
                common: fixture.common(
                    1,
                    LearningOperation::Search,
                    LearningSurface::Discovery,
                    "stranded-outbox",
                ),
                query: fixture.hasher.pointer(PointerDomain::Query, b"stranded"),
                status: LearningStatus::Succeeded,
                result_count: 0,
                detail_artifact: None,
            });
        }
        fixture.terminalize_scheduler_run();
        let connection = rusqlite::Connection::open(fixture.root.join("events.sqlite3")).unwrap();
        match queue {
            "reconciliation" => {
                connection.execute(
                    "INSERT INTO tool_learning_reconciliation
                     (operation_id,project,first_position,last_position,row_count,payload_bytes,required,created_at)
                     VALUES ('stranded',?1,NULL,NULL,1,1,1,0)",
                    [project.as_str()],
                ).unwrap();
            }
            "snapshot" => {
                connection.execute(
                    "INSERT INTO catalog_stats_snapshots
                     (project,raw_run_id,generation,overlay_digest,row_count,payload_bytes,frame_id,status)
                     VALUES (?1,?2,1,'stranded',1,1,'stranded','pending')",
                    rusqlite::params![project.as_str(), fixture.run.to_string()],
                ).unwrap();
            }
            "terminal-overlay" => {
                connection.execute(
                    "INSERT INTO catalog_stats_overlay
                     (project,run_id,raw_run_id,binding,attempts,succeeded,failed,cancelled,outcome_unknown,source_event)
                     VALUES (?1,'run-pointer',?2,'binding',1,1,0,0,0,'event')",
                    rusqlite::params![project.as_str(), fixture.run.to_string()],
                ).unwrap();
            }
            _ => {}
        }
        drop(connection);
        let runtime = TelemetryRuntime::encrypted_local(
            Resource::default(),
            &[],
            8,
            DropPolicy::DropNewest,
            DurableLocalExporter::open(
                fixture.root.join(format!("{queue}.otel.enc")),
                &[25; 32],
                1024 * 1024,
            )
            .unwrap(),
            TelemetryReadinessPolicy::Required,
        )
        .unwrap();
        if queue == "outbox" {
            runtime
                .export_catalog_stats_snapshot(&mut fixture.store, &fixture.hasher, fixture.run)
                .unwrap();
        } else {
            assert_eq!(
                runtime
                    .export_learning_outbox(&mut fixture.store, &fixture.hasher)
                    .unwrap(),
                0
            );
        }
        assert!(!runtime.health().learning_healthy, "queue {queue}");
        assert!(!runtime.health().learning_ready, "queue {queue}");
    }
}

#[test]
fn oversized_ciphertext_is_rejected_without_durable_insert() {
    let root = std::env::temp_dir().join(format!(
        "kit-learning-oversized-frame-{}",
        kit::domain::ids::EventId::generate().unwrap()
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("telemetry.otel.enc");
    let mut exporter = DurableLocalExporter::open(&path, &[24; 32], 128).unwrap();
    assert!(
        exporter
            .export_encrypted_learning(&EncryptedLearningFrame {
                frame_id: "oversized".to_owned(),
                ciphertext: vec![7; 129],
            })
            .is_err()
    );
    assert!(exporter.read_learning_frames().unwrap().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn learning_metric_has_only_closed_low_cardinality_dimensions() {
    Metric::new(
        MetricName::ToolLearningEvents,
        "1",
        MetricValue::Counter { value: 1 },
        BTreeMap::from([
            ("event_class".to_owned(), "call".to_owned()),
            ("operation".to_owned(), "invoke".to_owned()),
            ("status".to_owned(), "succeeded".to_owned()),
        ]),
        1,
    )
    .unwrap();
    assert!(
        Metric::new(
            MetricName::ToolLearningEvents,
            "1",
            MetricValue::Counter { value: 1 },
            BTreeMap::from([
                ("event_class".to_owned(), "call".to_owned()),
                ("operation".to_owned(), "tenant-tool-name".to_owned()),
                ("status".to_owned(), "succeeded".to_owned()),
            ]),
            1,
        )
        .is_err()
    );
}

#[test]
fn operational_stats_update_in_a_durable_non_binding_per_run_overlay() {
    let mut fixture = Fixture::new("durable-stats");
    for event in six_events(&fixture) {
        fixture.append(&event);
    }
    let run = fixture
        .hasher
        .pointer(PointerDomain::Run, fixture.run.to_string().as_bytes());
    let stats = fixture.store.catalog_stats(run.as_str()).unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].attempts, 1);
    assert_eq!(stats[0].failed, 1);
    assert_eq!(stats[0].succeeded, 0);
}

#[test]
fn stats_reconciliation_resnapshots_late_terminal_outcomes() {
    let mut fixture = Fixture::new("terminal-stats");
    for event in six_events(&fixture) {
        fixture.append(&event);
    }
    let runtime = TelemetryRuntime::encrypted_local(
        Resource::default(),
        &[],
        32,
        DropPolicy::DropNewest,
        DurableLocalExporter::open(
            fixture.root.join("terminal-stats.otel.enc"),
            &[31; 32],
            1024 * 1024,
        )
        .unwrap(),
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    runtime
        .reconcile_learning_backlog(&mut fixture.store, &fixture.hasher)
        .unwrap();
    let run = fixture
        .hasher
        .pointer(PointerDomain::Run, fixture.run.to_string().as_bytes());
    assert_eq!(fixture.store.catalog_stats(run.as_str()).unwrap().len(), 1);

    fixture.terminalize_scheduler_run();
    runtime
        .reconcile_learning_backlog(&mut fixture.store, &fixture.hasher)
        .unwrap();
    assert!(
        fixture
            .store
            .catalog_stats(run.as_str())
            .unwrap()
            .is_empty()
    );

    let call = fixture.hasher.pointer(PointerDomain::Call, b"late-call");
    let binding = fixture.hasher.pointer(PointerDomain::Binding, b"binding-1");
    let start = tool_learning::next_ordinal(&fixture.store, fixture.run).unwrap();
    fixture.append(&ToolLearningEvent::Call {
        common: fixture.common(
            start,
            LearningOperation::Invoke,
            LearningSurface::Generic,
            "late-call",
        ),
        call: call.clone(),
        binding: Some(binding),
        source: None,
        kind: Some(LearningCapabilityKind::Tool),
        sequence: None,
        sequence_order: None,
        kernel_intent: None,
    });
    fixture.append(&ToolLearningEvent::Outcome {
        common: fixture.common(
            start + 1,
            LearningOperation::Invoke,
            LearningSurface::Generic,
            "late-outcome",
        ),
        call,
        status: LearningStatus::Succeeded,
        dispatched: true,
        known: true,
        cost_microusd: Some(1),
        kernel_outcome: None,
    });
    assert_eq!(
        fixture.store.catalog_stats(run.as_str()).unwrap()[0].succeeded,
        1
    );
    runtime
        .reconcile_learning_backlog(&mut fixture.store, &fixture.hasher)
        .unwrap();
    assert!(
        fixture
            .store
            .catalog_stats(run.as_str())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn terminal_run_without_catalog_outcomes_is_a_healthy_no_op() {
    struct RejectUnexpectedExport;
    impl Exporter for RejectUnexpectedExport {
        fn export(&mut self, _: &ExportBatch) -> Result<(), ExportError> {
            Ok(())
        }

        fn export_encrypted_learning(
            &mut self,
            _: &EncryptedLearningFrame,
        ) -> Result<(), ExportError> {
            Err(ExportError("empty snapshot was exported".to_owned()))
        }
    }

    let mut fixture = Fixture::new("empty-terminal-stats");
    fixture.terminalize_scheduler_run();
    let runtime = TelemetryRuntime::encrypted_local(
        Resource::default(),
        &[],
        8,
        DropPolicy::DropNewest,
        RejectUnexpectedExport,
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();

    runtime
        .export_catalog_stats_snapshot(&mut fixture.store, &fixture.hasher, fixture.run)
        .unwrap();
    let health = runtime.health();
    assert!(health.learning_healthy);
    assert!(health.learning_ready);
    assert!(health.ready);
}

#[test]
fn failed_snapshot_export_retries_the_detached_frame_once() {
    struct FailOnce {
        failed: Arc<AtomicBool>,
        frames: Arc<Mutex<Vec<String>>>,
    }
    impl Exporter for FailOnce {
        fn export(&mut self, _: &ExportBatch) -> Result<(), ExportError> {
            Ok(())
        }

        fn export_encrypted_learning(
            &mut self,
            frame: &EncryptedLearningFrame,
        ) -> Result<(), ExportError> {
            self.frames.lock().unwrap().push(frame.frame_id.clone());
            if !self.failed.swap(true, Ordering::AcqRel) {
                return Err(ExportError("sink offline".to_owned()));
            }
            Ok(())
        }
    }

    let mut fixture = Fixture::new("snapshot-export-retry");
    for event in six_events(&fixture) {
        fixture.append(&event);
    }
    fixture.terminalize_scheduler_run();
    let failed = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(Mutex::new(Vec::new()));
    let runtime = TelemetryRuntime::encrypted_local(
        Resource::default(),
        &[],
        8,
        DropPolicy::DropNewest,
        FailOnce {
            failed: Arc::clone(&failed),
            frames: Arc::clone(&frames),
        },
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    let run = fixture
        .hasher
        .pointer(PointerDomain::Run, fixture.run.to_string().as_bytes());

    assert!(
        runtime
            .export_catalog_stats_snapshot(&mut fixture.store, &fixture.hasher, fixture.run)
            .is_err()
    );
    assert!(
        fixture
            .store
            .catalog_stats(run.as_str())
            .unwrap()
            .is_empty()
    );
    runtime
        .export_catalog_stats_snapshot(&mut fixture.store, &fixture.hasher, fixture.run)
        .unwrap();
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], frames[1]);
    drop(frames);
    assert_eq!(
        runtime
            .export_learning_outbox(&mut fixture.store, &fixture.hasher)
            .unwrap(),
        6
    );
    assert!(runtime.health().learning_healthy);
}

#[test]
fn busy_startup_learning_claim_blocks_required_but_not_best_effort_admission() {
    for (name, policy, admission_ready) in [
        ("required", TelemetryReadinessPolicy::Required, false),
        ("best-effort", TelemetryReadinessPolicy::BestEffort, true),
    ] {
        let mut fixture = Fixture::new(name);
        let event = six_events(&fixture).remove(0);
        fixture.append(&event);
        let project = fixture.hasher.pointer(
            PointerDomain::Project,
            fixture.project.to_string().as_bytes(),
        );
        rusqlite::Connection::open(fixture.root.join("events.sqlite3"))
            .unwrap()
            .execute(
                "INSERT INTO tool_learning_export_claims (project,token,claimed_at)
                 VALUES (?1,'other-owner',9223372036854775807)",
                [project.as_str()],
            )
            .unwrap();
        let runtime = TelemetryRuntime::encrypted_local(
            Resource::default(),
            &[],
            8,
            DropPolicy::DropNewest,
            DurableLocalExporter::open(
                fixture.root.join(format!("{name}.otel.enc")),
                &[32; 32],
                1024 * 1024,
            )
            .unwrap(),
            policy,
        )
        .unwrap();
        assert_eq!(
            runtime
                .reconcile_learning_backlog(&mut fixture.store, &fixture.hasher)
                .unwrap(),
            0
        );
        assert!(!runtime.health().learning_ready);
        assert_eq!(runtime.learning_admission_ready(), admission_ready);
    }
}

#[test]
fn catalog_stats_capacity_backpressures_without_deleting_unexported_rows() {
    let mut fixture = Fixture::new("durable-stats-capacity");
    let database = fixture.root.join("events.sqlite3");
    let mut connection = rusqlite::Connection::open(&database).unwrap();
    let transaction = connection.transaction().unwrap();
    let project = fixture.hasher.pointer(
        PointerDomain::Project,
        fixture.project.to_string().as_bytes(),
    );
    for index in 0..10_000 {
        transaction.execute(
            "INSERT INTO catalog_stats_overlay
             (project,run_id,binding,attempts,succeeded,failed,cancelled,outcome_unknown,source_event)
             VALUES (?1,?2,?3,1,0,1,0,0,?4)",
            rusqlite::params![
                project.as_str(),
                format!("retained-run-{index}"),
                format!("retained-binding-{index}"),
                format!("retained-event-{index}"),
            ],
        )
        .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);

    let events = six_events(&fixture);
    for event in &events[..5] {
        fixture.append(event);
    }
    assert!(
        tool_learning::append(
            &mut fixture.store,
            fixture.owner,
            fixture.claim,
            &fixture.hasher,
            UtcDateTime::parse("2026-08-05T12:00:01Z").unwrap(),
            TraceId::parse("stats-capacity").unwrap(),
            &events[5],
        )
        .is_err()
    );
    let connection = rusqlite::Connection::open(database).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM catalog_stats_overlay", [], |row| {
            row.get(0)
        })
        .unwrap();
    let first: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM catalog_stats_overlay WHERE binding='retained-binding-0')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 10_000);
    assert!(first);
}

#[test]
fn learning_failures_have_independent_sticky_health_and_stats_authentication() {
    struct OrdinaryOnly;
    impl Exporter for OrdinaryOnly {
        fn export(&mut self, _: &ExportBatch) -> Result<(), ExportError> {
            Ok(())
        }
    }

    let mut fixture = Fixture::new("learning-health");
    for event in six_events(&fixture) {
        fixture.append(&event);
    }
    let runtime = TelemetryRuntime::encrypted_local(
        Resource::default(),
        &[],
        16,
        DropPolicy::DropNewest,
        OrdinaryOnly,
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    assert!(
        runtime
            .export_learning_outbox(&mut fixture.store, &fixture.hasher)
            .is_err()
    );
    assert!(!runtime.health().learning_healthy);
    assert!(runtime.health().exporter_healthy);
    assert!(!runtime.health().ready);
    assert_eq!(runtime.flush().unwrap(), 0);
    assert!(!runtime.health().learning_healthy);

    let run = fixture
        .hasher
        .pointer(PointerDomain::Run, fixture.run.to_string().as_bytes());
    let connection = rusqlite::Connection::open(fixture.root.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE catalog_stats_overlay SET attempts=attempts+1 WHERE run_id=?1",
            [run.as_str()],
        )
        .unwrap();
    drop(connection);
    assert!(
        runtime
            .export_catalog_stats_snapshot(&mut fixture.store, &fixture.hasher, fixture.run)
            .is_err()
    );
}

fn analyzer_case() -> (
    Fixture,
    ToolLearningAnalyzer,
    Vec<ToolLearningEvent>,
    Vec<PreregisteredExperiment>,
    Vec<DownstreamGradeRecord>,
) {
    let fixture = Fixture::new("analyzer");
    let analyzer = ToolLearningAnalyzer::new(128);
    let mut events = Vec::new();
    let mut experiments = Vec::new();
    let mut grades = Vec::new();
    let common = |run, ordinal, operation, key: &str, capability, schema| {
        LearningCommon::new(
            &fixture.hasher,
            run,
            ordinal,
            operation,
            LearningSurface::Generic,
            key.as_bytes(),
            None,
            Some(capability),
            Some(schema),
        )
    };
    for signal in [
        AnalysisSignal::PoorDescription,
        AnalysisSignal::HarmfulDecoy,
        AnalysisSignal::MisunderstoodField,
        AnalysisSignal::ValuableSequence,
    ] {
        let name = format!("{signal:?}");
        let experiment = fixture
            .hasher
            .pointer(PointerDomain::Experiment, name.as_bytes());
        let capability = fixture
            .hasher
            .pointer(PointerDomain::Capability, name.as_bytes());
        let schema = fixture
            .hasher
            .pointer(PointerDomain::Schema, name.as_bytes());
        let sequence_capability = fixture.hasher.pointer(
            PointerDomain::Capability,
            format!("{name}:second").as_bytes(),
        );
        let sequence_schema = fixture
            .hasher
            .pointer(PointerDomain::Schema, format!("{name}:second").as_bytes());
        let direct_run = RunId::generate().unwrap();
        let competing_run = RunId::generate().unwrap();
        let direct_pointer = fixture
            .hasher
            .pointer(PointerDomain::Run, direct_run.to_string().as_bytes());
        let competing_pointer = fixture
            .hasher
            .pointer(PointerDomain::Run, competing_run.to_string().as_bytes());
        let expected_sequence = || {
            if signal == AnalysisSignal::ValuableSequence {
                (1..=2)
                    .map(|order| tool_learning::PreregisteredSequenceStep {
                        capability: if order == 1 {
                            capability.clone()
                        } else {
                            sequence_capability.clone()
                        },
                        schema: if order == 1 {
                            schema.clone()
                        } else {
                            sequence_schema.clone()
                        },
                        surface: LearningSurface::Generic,
                        ordinal: order,
                    })
                    .collect()
            } else {
                Vec::new()
            }
        };
        experiments.extend([
            PreregisteredExperiment {
                experiment: experiment.clone(),
                run: direct_pointer.clone(),
                arm: ExperimentArm::Direct,
                capability: capability.clone(),
                schema: schema.clone(),
                surface: LearningSurface::Generic,
                authorized: true,
                offered: true,
                description_only: signal == AnalysisSignal::PoorDescription,
                frozen_factors: FrozenFactors {
                    canonical_actual_config_digest: format!(
                        "sha256:{}",
                        hex_digest(format!("{name}:frozen").as_bytes())
                    ),
                    arm_config: fixture.hasher.pointer(
                        PointerDomain::Artifact,
                        format!("{name}:direct-config").as_bytes(),
                    ),
                    receipt: fixture.hasher.pointer(
                        PointerDomain::Artifact,
                        format!("{name}:direct-receipt").as_bytes(),
                    ),
                    declaration_artifact: fixture
                        .hasher
                        .pointer(PointerDomain::Artifact, format!("{name}:frozen").as_bytes()),
                },
                expected_sequence: expected_sequence(),
            },
            PreregisteredExperiment {
                experiment: experiment.clone(),
                run: competing_pointer.clone(),
                arm: ExperimentArm::Competing,
                capability: capability.clone(),
                schema: schema.clone(),
                surface: LearningSurface::Generic,
                authorized: true,
                offered: true,
                description_only: signal == AnalysisSignal::PoorDescription,
                frozen_factors: FrozenFactors {
                    canonical_actual_config_digest: format!(
                        "sha256:{}",
                        hex_digest(format!("{name}:frozen").as_bytes())
                    ),
                    arm_config: fixture.hasher.pointer(
                        PointerDomain::Artifact,
                        format!("{name}:competing-config").as_bytes(),
                    ),
                    receipt: fixture.hasher.pointer(
                        PointerDomain::Artifact,
                        format!("{name}:competing-receipt").as_bytes(),
                    ),
                    declaration_artifact: fixture
                        .hasher
                        .pointer(PointerDomain::Artifact, format!("{name}:frozen").as_bytes()),
                },
                expected_sequence: expected_sequence(),
            },
        ]);
        grades.extend([
            DownstreamGradeRecord {
                experiment: experiment.clone(),
                run: direct_pointer,
                grade: match signal {
                    AnalysisSignal::HarmfulDecoy => DownstreamGrade::Harmful,
                    AnalysisSignal::MisunderstoodField => DownstreamGrade::Failed,
                    _ => DownstreamGrade::Passed,
                },
                cost_microusd: 20,
                latency_ms: 20,
                receipt: fixture.hasher.pointer(
                    PointerDomain::Artifact,
                    format!("{name}:direct-receipt").as_bytes(),
                ),
                harm_receipt: (signal == AnalysisSignal::HarmfulDecoy)
                    .then_some(tool_learning::HarmReceiptKind::Security),
                sequence: (signal == AnalysisSignal::ValuableSequence).then_some(
                    tool_learning::SequenceObservation {
                        cost_microusd: 20,
                        latency_ms: 20,
                    },
                ),
            },
            DownstreamGradeRecord {
                experiment,
                run: competing_pointer,
                grade: DownstreamGrade::Passed,
                cost_microusd: if signal == AnalysisSignal::ValuableSequence {
                    10
                } else {
                    20
                },
                latency_ms: if signal == AnalysisSignal::ValuableSequence {
                    10
                } else {
                    20
                },
                receipt: fixture.hasher.pointer(
                    PointerDomain::Artifact,
                    format!("{name}:competing-receipt").as_bytes(),
                ),
                harm_receipt: None,
                sequence: (signal == AnalysisSignal::ValuableSequence).then_some(
                    tool_learning::SequenceObservation {
                        cost_microusd: 10,
                        latency_ms: 10,
                    },
                ),
            },
        ]);
        for run in [direct_run, competing_run] {
            events.push(ToolLearningEvent::Opportunity {
                common: common(
                    run,
                    1,
                    LearningOperation::Projection,
                    &format!("{name}:opportunity:{run}"),
                    capability.clone(),
                    schema.clone(),
                ),
                offered: 1,
                eager: 0,
                deferred: 0,
                generic_available: true,
                projection: schema.clone(),
                candidates: vec![LearningCandidate {
                    capability: capability.clone(),
                    schema: schema.clone(),
                    surface: LearningSurface::Generic,
                    authorized: true,
                    offered: true,
                }],
                detail_artifact: None,
            });
        }
        let direct_call = fixture
            .hasher
            .pointer(PointerDomain::Call, format!("{name}:direct").as_bytes());
        let competing_call = fixture
            .hasher
            .pointer(PointerDomain::Call, format!("{name}:competing").as_bytes());
        match signal {
            AnalysisSignal::PoorDescription => {
                events.push(ToolLearningEvent::Opportunity {
                    common: common(
                        direct_run,
                        1,
                        LearningOperation::Projection,
                        &name,
                        capability.clone(),
                        schema.clone(),
                    ),
                    offered: 1,
                    eager: 1,
                    deferred: 0,
                    generic_available: true,
                    projection: schema.clone(),
                    candidates: vec![LearningCandidate {
                        capability: capability.clone(),
                        schema: schema.clone(),
                        surface: LearningSurface::Generic,
                        authorized: true,
                        offered: true,
                    }],
                    detail_artifact: None,
                });
                events.push(ToolLearningEvent::Call {
                    common: common(
                        competing_run,
                        1,
                        LearningOperation::Invoke,
                        &name,
                        capability,
                        schema,
                    ),
                    call: competing_call,
                    binding: None,
                    source: None,
                    kind: Some(LearningCapabilityKind::Tool),
                    sequence: None,
                    sequence_order: None,
                    kernel_intent: None,
                });
            }
            AnalysisSignal::HarmfulDecoy | AnalysisSignal::MisunderstoodField => {
                events.push(ToolLearningEvent::Call {
                    common: common(
                        direct_run,
                        1,
                        LearningOperation::Invoke,
                        &name,
                        capability.clone(),
                        schema.clone(),
                    ),
                    call: direct_call.clone(),
                    binding: None,
                    source: None,
                    kind: Some(LearningCapabilityKind::Tool),
                    sequence: None,
                    sequence_order: None,
                    kernel_intent: None,
                });
                if signal == AnalysisSignal::MisunderstoodField {
                    events.push(ToolLearningEvent::Error {
                        common: common(
                            direct_run,
                            2,
                            LearningOperation::Invoke,
                            &format!("{name}:error"),
                            capability.clone(),
                            schema.clone(),
                        ),
                        call: direct_call,
                        stage: ErrorStage::SchemaValidation,
                        class: ErrorClass::Input,
                        code: ErrorCode::InvalidSchema,
                        field: Some(
                            fixture
                                .hasher
                                .pointer(PointerDomain::Field, name.as_bytes()),
                        ),
                        retry: RetryClass::Never,
                        dispatched: false,
                        known: true,
                    });
                    for attempt in 2..=3 {
                        let call = fixture.hasher.pointer(
                            PointerDomain::Call,
                            format!("{name}:misunderstood:{attempt}").as_bytes(),
                        );
                        events.push(ToolLearningEvent::Call {
                            common: common(
                                direct_run,
                                attempt * 2 - 1,
                                LearningOperation::Invoke,
                                &format!("{name}:call:{attempt}"),
                                capability.clone(),
                                schema.clone(),
                            ),
                            call: call.clone(),
                            binding: None,
                            source: None,
                            kind: Some(LearningCapabilityKind::Tool),
                            sequence: None,
                            sequence_order: None,
                            kernel_intent: None,
                        });
                        events.push(ToolLearningEvent::Error {
                            common: common(
                                direct_run,
                                attempt * 2,
                                LearningOperation::Invoke,
                                &format!("{name}:error:{attempt}"),
                                capability.clone(),
                                schema.clone(),
                            ),
                            call,
                            stage: ErrorStage::SchemaValidation,
                            class: ErrorClass::Input,
                            code: ErrorCode::InvalidSchema,
                            field: Some(
                                fixture
                                    .hasher
                                    .pointer(PointerDomain::Field, name.as_bytes()),
                            ),
                            retry: RetryClass::Never,
                            dispatched: false,
                            known: true,
                        });
                    }
                }
            }
            AnalysisSignal::ValuableSequence => {
                for run in [direct_run, competing_run] {
                    for order in 1..=2 {
                        let selected_capability = if order == 1 {
                            capability.clone()
                        } else {
                            sequence_capability.clone()
                        };
                        let selected_schema = if order == 1 {
                            schema.clone()
                        } else {
                            sequence_schema.clone()
                        };
                        let call = fixture.hasher.pointer(
                            PointerDomain::Call,
                            format!("{name}:{run}:sequence:{order}").as_bytes(),
                        );
                        events.push(ToolLearningEvent::Call {
                            common: common(
                                run,
                                order * 2 - 1,
                                LearningOperation::Invoke,
                                &format!("{name}:{run}:call:{order}"),
                                selected_capability.clone(),
                                selected_schema.clone(),
                            ),
                            call: call.clone(),
                            binding: None,
                            source: None,
                            kind: Some(LearningCapabilityKind::Tool),
                            sequence: Some(fixture.hasher.pointer(
                                PointerDomain::Sequence,
                                format!("{name}:{run}").as_bytes(),
                            )),
                            sequence_order: Some(order as u16),
                            kernel_intent: None,
                        });
                        events.push(ToolLearningEvent::Outcome {
                            common: common(
                                run,
                                order * 2,
                                LearningOperation::Invoke,
                                &format!("{name}:{run}:outcome:{order}"),
                                selected_capability,
                                selected_schema,
                            ),
                            call,
                            status: LearningStatus::Succeeded,
                            dispatched: true,
                            known: true,
                            cost_microusd: Some(10),
                            kernel_outcome: None,
                        });
                    }
                }
            }
        }
    }
    (fixture, analyzer, events, experiments, grades)
}

fn findings(
    case: &(
        Fixture,
        ToolLearningAnalyzer,
        Vec<ToolLearningEvent>,
        Vec<PreregisteredExperiment>,
        Vec<DownstreamGradeRecord>,
    ),
) -> BTreeSet<tool_learning::AnalysisFinding> {
    let CausalResult::Available { findings, .. } = case.1.analyze(&case.2, &case.3, &case.4) else {
        panic!("complete raw records were unavailable");
    };
    findings
}

#[test]
fn analyzer_derives_four_findings_from_raw_records_and_refuses_missing_arms() {
    let case = analyzer_case();
    let all = findings(&case);
    assert_eq!(
        all.into_iter()
            .map(|finding| finding.signal)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            AnalysisSignal::PoorDescription,
            AnalysisSignal::HarmfulDecoy,
            AnalysisSignal::MisunderstoodField,
            AnalysisSignal::ValuableSequence,
        ])
    );

    let missing_arm = case.3[0].clone();
    assert_eq!(
        case.1.analyze(&case.2, &[missing_arm], &case.4),
        CausalResult::Unavailable(CausalUnavailable::MissingArm)
    );
}

#[test]
fn kit_cap_828_description_routing_requires_both_preregistered_arms_and_candidates() {
    let case = analyzer_case();
    let poor = findings(&case)
        .into_iter()
        .find(|finding| finding.signal == AnalysisSignal::PoorDescription)
        .unwrap();
    assert_eq!(poor.surface, LearningSurface::Generic);
    assert!(
        case.3
            .iter()
            .filter(|experiment| experiment.description_only)
            .all(|experiment| {
                case.2.iter().any(|event| {
                    matches!(event,
            ToolLearningEvent::Opportunity { candidates, .. }
            if candidates.iter().any(|candidate|
                candidate.capability == experiment.capability
                    && candidate.schema == experiment.schema
                    && candidate.surface == experiment.surface
                    && candidate.authorized
                    && candidate.offered))
                })
            })
    );
    assert_eq!(
        case.1.analyze(&case.2, &[], &case.4),
        CausalResult::Unavailable(CausalUnavailable::MissingPreregistration)
    );
}

#[test]
fn kit_cap_832_poor_description_has_exact_subject_and_rejects_other_factor_changes() {
    let mut case = analyzer_case();
    let poor = findings(&case)
        .into_iter()
        .find(|finding| finding.signal == AnalysisSignal::PoorDescription)
        .unwrap();
    let expected = case
        .0
        .hasher
        .pointer(PointerDomain::Capability, b"PoorDescription");
    assert_eq!(poor.capability, expected);
    assert_eq!(
        poor.schema,
        case.0
            .hasher
            .pointer(PointerDomain::Schema, b"PoorDescription")
    );
    assert_eq!((poor.field, poor.sequence), (None, None));
    case.3
        .iter_mut()
        .find(|experiment| {
            experiment.capability == expected && experiment.arm == ExperimentArm::Direct
        })
        .unwrap()
        .frozen_factors
        .canonical_actual_config_digest = format!("sha256:{}", "f".repeat(64));
    let CausalResult::Available { findings, .. } = case.1.analyze(&case.2, &case.3, &case.4) else {
        panic!("factor mismatch must remain an available empty analysis");
    };
    assert!(
        !findings
            .iter()
            .any(|finding| finding.signal == AnalysisSignal::PoorDescription)
    );
}

#[test]
fn kit_cap_833_harmful_decoy_has_exact_subject_and_requires_verified_grade() {
    let mut case = analyzer_case();
    let harmful = findings(&case)
        .into_iter()
        .find(|finding| finding.signal == AnalysisSignal::HarmfulDecoy)
        .unwrap();
    assert_eq!(
        harmful.capability,
        case.0
            .hasher
            .pointer(PointerDomain::Capability, b"HarmfulDecoy")
    );
    assert_eq!((harmful.field, harmful.sequence), (None, None));
    case.4.retain(|grade| {
        grade.experiment
            != case
                .0
                .hasher
                .pointer(PointerDomain::Experiment, b"HarmfulDecoy")
    });
    assert_eq!(
        case.1.analyze(&case.2, &case.3, &case.4),
        CausalResult::Unavailable(CausalUnavailable::MissingDownstreamGrade)
    );
}

#[test]
fn kit_cap_834_misunderstood_field_has_exact_subject_and_linkage() {
    let case = analyzer_case();
    let misunderstood = findings(&case)
        .into_iter()
        .find(|finding| finding.signal == AnalysisSignal::MisunderstoodField)
        .unwrap();
    assert_eq!(
        misunderstood.capability,
        case.0
            .hasher
            .pointer(PointerDomain::Capability, b"MisunderstoodField")
    );
    assert_eq!(
        misunderstood.field,
        Some(
            case.0
                .hasher
                .pointer(PointerDomain::Field, b"MisunderstoodField")
        )
    );
    assert_eq!(misunderstood.sequence, None);
    assert_eq!(
        case.1.analyze_linked(None, Some(&case.3), Some(&case.4)),
        CausalResult::Unavailable(CausalUnavailable::MissingLinkage)
    );
}

#[test]
fn kit_cap_835_valuable_sequence_has_exact_subject_and_enforces_bounds() {
    let mut case = analyzer_case();
    let valuable = findings(&case)
        .into_iter()
        .find(|finding| finding.signal == AnalysisSignal::ValuableSequence)
        .unwrap();
    assert_eq!(
        valuable.capability,
        case.0
            .hasher
            .pointer(PointerDomain::Capability, b"ValuableSequence")
    );
    assert_eq!(valuable.field, None);
    assert!(
        valuable
            .sequence
            .is_some_and(|sequence| { matches!(sequence.domain(), Ok(PointerDomain::Sequence)) })
    );
    assert_eq!(
        ToolLearningAnalyzer::new(0).analyze(&case.2, &case.3, &case.4),
        CausalResult::Unavailable(CausalUnavailable::BoundExceeded)
    );

    let competing_run = case
        .3
        .iter()
        .find(|experiment| {
            experiment.capability
                == case
                    .0
                    .hasher
                    .pointer(PointerDomain::Capability, b"ValuableSequence")
                && experiment.arm == ExperimentArm::Competing
        })
        .unwrap()
        .run
        .clone();
    case.2
        .iter_mut()
        .find_map(|event| match event {
            ToolLearningEvent::Call {
                common,
                sequence_order: Some(2),
                ..
            } if common.run == competing_run => Some(common),
            _ => None,
        })
        .unwrap()
        .schema = Some(
        case.0
            .hasher
            .pointer(PointerDomain::Schema, b"wrong-step-schema"),
    );
    assert!(
        !findings(&case)
            .iter()
            .any(|finding| finding.signal == AnalysisSignal::ValuableSequence)
    );
}

#[test]
fn valuable_sequence_requires_distinct_capabilities_and_exact_observed_order() {
    let mut repeated = analyzer_case();
    let valuable_capability = repeated
        .0
        .hasher
        .pointer(PointerDomain::Capability, b"ValuableSequence");
    let competing_run = repeated
        .3
        .iter_mut()
        .find(|experiment| {
            experiment.capability == valuable_capability
                && experiment.arm == ExperimentArm::Competing
        })
        .map(|experiment| {
            experiment.expected_sequence[1].capability = valuable_capability.clone();
            experiment.run.clone()
        })
        .unwrap();
    repeated
        .2
        .iter_mut()
        .find_map(|event| match event {
            ToolLearningEvent::Call {
                common,
                sequence_order: Some(2),
                ..
            } if common.run == competing_run => Some(common),
            _ => None,
        })
        .unwrap()
        .capability = Some(valuable_capability);
    assert!(
        !findings(&repeated)
            .iter()
            .any(|finding| finding.signal == AnalysisSignal::ValuableSequence)
    );

    let mut reordered = analyzer_case();
    let competing_run = reordered
        .3
        .iter()
        .find(|experiment| {
            experiment.capability
                == reordered
                    .0
                    .hasher
                    .pointer(PointerDomain::Capability, b"ValuableSequence")
                && experiment.arm == ExperimentArm::Competing
        })
        .unwrap()
        .run
        .clone();
    for event in &mut reordered.2 {
        if let ToolLearningEvent::Call {
            common,
            sequence_order: Some(order),
            ..
        } = event
            && common.run == competing_run
        {
            *order = 3 - *order;
        }
    }
    assert!(
        !findings(&reordered)
            .iter()
            .any(|finding| finding.signal == AnalysisSignal::ValuableSequence)
    );
}

#[test]
fn valuable_sequence_uses_relative_call_order_and_latency_only_improvement() {
    let mut case = analyzer_case();
    for event in &mut case.2 {
        if let ToolLearningEvent::Call {
            sequence_order: Some(order),
            ..
        } = event
        {
            *order += 3;
        }
    }
    let first_calls = case
        .2
        .iter()
        .filter(|event| {
            matches!(
                event,
                ToolLearningEvent::Call {
                    sequence_order: Some(4),
                    ..
                }
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    for first_call in first_calls {
        for (order, operation) in [
            LearningOperation::Search,
            LearningOperation::Inspect,
            LearningOperation::Bind,
        ]
        .into_iter()
        .enumerate()
        {
            let ToolLearningEvent::Call {
                mut common,
                binding,
                source,
                kind,
                sequence,
                kernel_intent,
                ..
            } = first_call.clone()
            else {
                unreachable!()
            };
            common.operation = operation;
            common.event_id = case.0.hasher.pointer(
                PointerDomain::Event,
                format!("deferred:{operation:?}:{}", common.run.as_str()).as_bytes(),
            );
            case.2.push(ToolLearningEvent::Call {
                call: case.0.hasher.pointer(
                    PointerDomain::Call,
                    format!("deferred:{operation:?}:{}", common.run.as_str()).as_bytes(),
                ),
                common,
                binding,
                source,
                kind,
                sequence,
                sequence_order: Some(order as u16 + 1),
                kernel_intent,
            });
        }
    }
    let experiment = case
        .0
        .hasher
        .pointer(PointerDomain::Experiment, b"ValuableSequence");
    let competing_run = case
        .3
        .iter()
        .find(|candidate| {
            candidate.experiment == experiment && candidate.arm == ExperimentArm::Competing
        })
        .unwrap()
        .run
        .clone();
    case.4
        .iter_mut()
        .find(|grade| grade.experiment == experiment && grade.run == competing_run)
        .unwrap()
        .sequence
        .as_mut()
        .unwrap()
        .cost_microusd = 20;
    assert!(
        findings(&case)
            .iter()
            .any(|finding| finding.signal == AnalysisSignal::ValuableSequence)
    );
}
