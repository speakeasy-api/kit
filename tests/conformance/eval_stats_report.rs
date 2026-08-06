use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use jsonschema::Validator;
use rusqlite::Connection;
use serde_json::{Value, json};

#[path = "../../eval/reports/core/mod.rs"]
mod stats;

use stats::{
    AnalysisStatus, BoundTrialEnvelope, ConformanceLedgerAnchor, CoreMetric, Direction,
    EvidenceSource, FailurePolicy, IntervalMethod, MetricRole, MetricSummary, MetricUnit,
    Preregistration, RegistrationAuthority, StatisticalReport, StatsError, TerminalDurableUsage,
    TerminalErrorEvidence, TrialOutcome, TrialRunConfig,
};

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-stats-{}-{}",
            std::process::id(),
            getrandom::u64().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn plan() -> Preregistration {
    Preregistration::from_json(include_bytes!(
        "../../eval/preregistration/templates/core-paired-v1.json"
    ))
    .unwrap()
}

fn refresh_design(plan: &mut Preregistration) {
    let (experiment, task_set, dataset) = plan.derived_design_digests().unwrap();
    plan.digests.experiment = experiment;
    plan.digests.task_set = task_set;
    plan.digests.dataset = dataset;
}

fn registry() -> (
    TestRoot,
    RegistrationAuthority,
    stats::RegisteredPreregistration,
) {
    let root = TestRoot::new();
    let mut authority = RegistrationAuthority::open_with_anchor(
        &root.0,
        ConformanceLedgerAnchor::source_semantics_fake(),
    )
    .unwrap();
    let registered = authority.register(plan()).unwrap();
    (root, authority, registered)
}

#[derive(Clone, Copy)]
struct Measurement {
    success: bool,
    intervention: bool,
    verified: bool,
    cost_usd: f64,
    latency_ms: u64,
}

struct Evidence {
    harness_report_bytes: Vec<u8>,
    events_bytes: Vec<u8>,
}

fn evidence(
    plan: &Preregistration,
    roster: &stats::RosterEntry,
    run_config: &TrialRunConfig,
    measurement: Measurement,
) -> Evidence {
    let event_high_watermark = run_config.event_start_watermark + 3;
    let events_bytes = serde_json::to_vec(&json!({
        "schema_version": 1,
        "source": "conformance_source_semantics_fake",
        "run_config_digest": run_config.immutable_digest,
        "admission_position": run_config.admission_position,
        "admission_nonce": run_config.admission_nonce,
        "admission_token_digest": run_config.admission_token_digest,
        "scheduler_run_id": run_config.scheduler_run_id,
        "scheduler_consumption_position": run_config.scheduler_consumption_position,
        "scheduler_consumption_digest": run_config.scheduler_consumption_digest,
        "event_high_watermark": event_high_watermark,
        "trial_id": roster.trial_id,
        "pair_id": roster.pair_id,
        "task_id": roster.task_id,
        "dataset_member_id": roster.dataset_member_id,
        "seed": roster.seed,
        "arm": roster.arm,
        "config_digest": roster.config_digest,
        "started_monotonic_millis": 1000,
        "finished_monotonic_millis": 1000 + measurement.latency_ms,
        "intervention": measurement.intervention,
        "exclusion_reason": "",
        "scheduler_events": [{
            "kind": "run_admitted",
            "event_position": run_config.event_start_watermark + 1,
            "admission_token_digest": run_config.admission_token_digest
        }],
        "provider_events": [{
            "kind": "provider_completed",
            "event_position": run_config.event_start_watermark + 2,
            "admission_token_digest": run_config.admission_token_digest
        }],
        "tool_events": [{
            "kind": "tool_completed",
            "event_position": event_high_watermark,
            "admission_token_digest": run_config.admission_token_digest
        }]
    }))
    .unwrap();
    let outcome = if measurement.success {
        "success"
    } else {
        "failure"
    };
    let (expected, actual) = if measurement.verified {
        ("pass", "pass")
    } else {
        ("pass", "fail")
    };
    let mut harness_report = json!({
        "schema_version": 2,
        "harness_version": "m004-core-v2",
        "trial_id": roster.trial_id,
        "manifest_identity_digest": digest('1'),
        "manifest_bytes_digest": digest('2'),
        "task_manifest_digest": roster.task_manifest_digest,
        "grader_manifest_digest": digest('4'),
        "base_tree_digest": digest('5'),
        "patch_digest": digest('6'),
        "final_tree_digest": digest('7'),
        "grader_image_digest": digest('8'),
        "grader_harness_commit": "0123456789abcdef0123456789abcdef01234567",
        "hidden_tests_digest": digest('9'),
        "acceptance_digest": digest('a'),
        "gold_patch_digest": digest('b'),
        "harness_config_digest": plan.digests.harness,
        "toolchain_digest": digest('d'),
        "agent": boundary("agent", '3'),
        "grader": boundary("grader", '8'),
        "agent_result_digest": digest('f'),
        "events_digest": stats::sha256(&events_bytes),
        "logs_digest": digest('1'),
        "artifacts_digest": digest('2'),
        "usage": {
            "turns": { "availability": "measured", "value": 1 },
            "input_tokens": { "availability": "measured", "value": 10 },
            "output_tokens": { "availability": "measured", "value": 10 },
            "cost_microusd": {
                "availability": "measured",
                "value": (measurement.cost_usd * 1_000_000.0) as u64
            },
            "tool_calls": { "availability": "measured", "value": 1 },
            "processes": { "availability": "measured", "value": 1 }
        },
        "outcome": outcome,
        "grade": {
            "schema_version": 1,
            "outcome": outcome,
            "base_tree_digest": digest('5'),
            "patch_digest": digest('6'),
            "final_tree_digest": digest('7'),
            "checks": [{
                "id": "check-1",
                "passed": expected == actual,
                "path": "result.txt",
                "expected": expected,
                "actual": actual
            }],
            "hidden": {
                "verdict": outcome,
                "count": 1,
                "digest": digest('9')
            },
            "diagnostic": if measurement.success { Value::Null } else { json!("grader failure") }
        }
    });
    harness_report["model_digest"] = json!(roster.model_digest);
    harness_report["model_settings_digest"] = json!(roster.model_settings_digest);
    harness_report["config_digest"] = json!(roster.config_digest);
    harness_report["provider_capability_digest"] = json!(roster.provider_capability_digest);
    harness_report["run_config_digest"] = json!(run_config.immutable_digest);
    harness_report["admission_source"] = json!("conformance_source_semantics_fake");
    harness_report["admission_position"] = json!(run_config.admission_position);
    harness_report["admission_nonce"] = json!(run_config.admission_nonce);
    harness_report["admission_token_digest"] = json!(run_config.admission_token_digest);
    harness_report["scheduler_run_id"] = json!(run_config.scheduler_run_id);
    harness_report["scheduler_consumption_position"] =
        json!(run_config.scheduler_consumption_position);
    harness_report["scheduler_consumption_digest"] = json!(run_config.scheduler_consumption_digest);
    harness_report["event_high_watermark"] = json!(event_high_watermark);
    harness_report["task_set_digest"] = json!(plan.digests.task_set);
    harness_report["dataset_digest"] = json!(plan.digests.dataset);
    harness_report["experiment_design_digest"] = json!(plan.digests.experiment);
    harness_report["production_pins_digest"] = json!(plan.execution_environment.digest);
    let harness_report_bytes = serde_json::to_vec(&harness_report).unwrap();
    Evidence {
        harness_report_bytes,
        events_bytes,
    }
}

fn boundary(phase: &str, digest_byte: char) -> Value {
    json!({
        "phase": phase,
        "route": "conformance_fake",
        "image_digest": digest(digest_byte),
        "runtime_identity": "conformance-runtime",
        "helper_identity": "conformance-helper",
        "permitted_profile_digest": digest('4'),
        "survivor_processes": 0,
        "quiescent": true,
        "outcome": { "kind": "success" }
    })
}

fn tamper_event(mut evidence: Evidence, pointer: &str, replacement: Value) -> Evidence {
    let mut events: Value = serde_json::from_slice(&evidence.events_bytes).unwrap();
    *events.pointer_mut(pointer).unwrap() = replacement;
    evidence.events_bytes = serde_json::to_vec(&events).unwrap();
    let mut report: Value = serde_json::from_slice(&evidence.harness_report_bytes).unwrap();
    report["events_digest"] = json!(stats::sha256(&evidence.events_bytes));
    evidence.harness_report_bytes = serde_json::to_vec(&report).unwrap();
    evidence
}

fn production_source(mut evidence: Evidence) -> Evidence {
    let mut events: Value = serde_json::from_slice(&evidence.events_bytes).unwrap();
    events["source"] = json!("production_authenticated");
    for class in ["scheduler_events", "provider_events", "tool_events"] {
        for event in events[class].as_array_mut().unwrap() {
            event["event_digest"] =
                json!(stats::sha256(event["kind"].as_str().unwrap().as_bytes()));
        }
    }
    evidence.events_bytes = serde_json::to_vec(&events).unwrap();
    let mut report: Value = serde_json::from_slice(&evidence.harness_report_bytes).unwrap();
    report["admission_source"] = json!("production_authenticated");
    report["agent"]["route"] = json!("production");
    report["grader"]["route"] = json!("production");
    report["events_digest"] = json!(stats::sha256(&evidence.events_bytes));
    evidence.harness_report_bytes = serde_json::to_vec(&report).unwrap();
    evidence
}

fn run_roster(
    authority: &mut RegistrationAuthority,
    registered: &stats::RegisteredPreregistration,
    measurements: &[Measurement],
) -> Vec<BoundTrialEnvelope> {
    registered
        .preregistration
        .roster
        .iter()
        .zip(measurements)
        .map(|(roster, measurement)| {
            let admission = authority.admit_next(registered).unwrap();
            assert_eq!(admission.trial_id(), roster.trial_id);
            assert_eq!(admission.arm(), roster.arm);
            let run_config = authority
                .consume_for_run_admission(registered, &admission)
                .unwrap();
            let evidence = evidence(
                &registered.preregistration,
                roster,
                &run_config,
                *measurement,
            );
            authority
                .record_harness_trial(
                    registered,
                    &run_config,
                    evidence.harness_report_bytes,
                    evidence.events_bytes,
                )
                .unwrap()
        })
        .collect()
}

fn measurements() -> [Measurement; 6] {
    [
        Measurement {
            success: true,
            intervention: false,
            verified: true,
            cost_usd: 10.0,
            latency_ms: 100,
        },
        Measurement {
            success: true,
            intervention: false,
            verified: true,
            cost_usd: 8.0,
            latency_ms: 90,
        },
        Measurement {
            success: true,
            intervention: false,
            verified: true,
            cost_usd: 9.0,
            latency_ms: 80,
        },
        Measurement {
            success: false,
            intervention: true,
            verified: false,
            cost_usd: 12.0,
            latency_ms: 120,
        },
        Measurement {
            success: true,
            intervention: false,
            verified: true,
            cost_usd: 11.0,
            latency_ms: 110,
        },
        Measurement {
            success: false,
            intervention: true,
            verified: false,
            cost_usd: 13.0,
            latency_ms: 130,
        },
    ]
}

#[test]
fn eval_stats_report_uses_exact_binary_primary_and_exploratory_point_estimates_only() {
    let (_root, mut authority, registered) = registry();
    run_roster(&mut authority, &registered, &measurements());
    let report = authority.build_report(&registered).unwrap();

    assert_eq!(
        report.report.evidence_source,
        EvidenceSource::ConformanceSourceSemantics
    );
    assert_eq!(
        report.receipt.evidence_source,
        EvidenceSource::ConformanceSourceSemantics
    );
    assert!(report.report.trials.iter().all(|trial| {
        trial.evidence_source == Some(EvidenceSource::ConformanceSourceSemantics)
    }));
    assert_eq!(report.report.analysis_status, AnalysisStatus::Complete);
    assert_eq!(report.report.metrics.len(), 5);
    assert_eq!(
        report
            .report
            .metrics
            .iter()
            .map(MetricSummary::metric)
            .collect::<Vec<_>>(),
        [
            CoreMetric::SuccessRate,
            CoreMetric::InterventionRate,
            CoreMetric::MeanCostUsd,
            CoreMetric::MeanLatencyMs,
            CoreMetric::VerificationRate,
        ]
    );
    let MetricSummary::Confirmatory {
        confidence_interval,
        noninferiority,
        ..
    } = &report.report.metrics[0]
    else {
        panic!("primary was not confirmatory")
    };
    assert_eq!(
        confidence_interval.method,
        IntervalMethod::UnconditionalBonferroniClopperPearsonV1
    );
    assert!((-1.0..=1.0).contains(&confidence_interval.lower));
    assert!((-1.0..=1.0).contains(&confidence_interval.upper));
    assert_eq!(noninferiority.metric, CoreMetric::SuccessRate);
    assert!(
        report.report.metrics[1..]
            .iter()
            .all(|metric| matches!(metric, MetricSummary::Exploratory { .. }))
    );
    let value: Value = serde_json::from_slice(&report.bytes).unwrap();
    assert!(value.get("learning_analysis").is_none());
    assert!(value["metrics"][1].get("confidence_interval").is_none());
    assert_eq!(stats::sha256(&report.bytes), report.digest);
    authority.verify_report(&registered, &report).unwrap();
    assert_eq!(
        authority.admit_next(&registered).unwrap_err(),
        StatsError::ExperimentFrozen
    );
    let repeated = authority.build_report(&registered).unwrap();
    assert_eq!(repeated.bytes, report.bytes);
    assert_eq!(repeated.receipt, report.receipt);
    if let Some(output) = std::env::var_os("KIT_M004_REPORT_DIR") {
        let output = PathBuf::from(output);
        fs::create_dir_all(&output).unwrap();
        fs::write(
            output.join("preregistration.json"),
            registered.preregistration.canonical_bytes().unwrap(),
        )
        .unwrap();
        fs::write(
            output.join("registered-preregistration.json"),
            registered.canonical_bytes().unwrap(),
        )
        .unwrap();
        fs::write(output.join("statistical-report.json"), &report.bytes).unwrap();
        fs::write(
            output.join("statistical-report-receipt.json"),
            serde_json::to_vec(&report.receipt).unwrap(),
        )
        .unwrap();
    }
    let mut caller_rehashed = report.clone();
    caller_rehashed.report.sample_counts.included_trials += 1;
    caller_rehashed.bytes = serde_json::to_vec(&caller_rehashed.report).unwrap();
    caller_rehashed.digest = stats::sha256(&caller_rehashed.bytes);
    caller_rehashed.receipt.report_digest = caller_rehashed.digest.clone();
    assert_eq!(
        authority.verify_report(&registered, &caller_rehashed),
        Err(StatsError::InvalidReportReceipt)
    );
}

#[test]
fn legacy_statistical_report_without_learning_analysis_round_trips_exact_bytes() {
    let bytes =
        include_bytes!("../../requirements/reports/m004/source-semantics/statistical-report.json");
    let report: StatisticalReport = serde_json::from_slice(bytes).unwrap();
    assert!(report.learning_analysis.is_none());
    assert_eq!(serde_json::to_vec(&report).unwrap(), bytes);
}

#[test]
fn eval_stats_report_rejects_mixed_sealed_evidence_sources() {
    let (_root, mut authority, registered) = registry();
    for (index, roster) in registered.preregistration.roster.iter().enumerate() {
        let admission = authority.admit_next(&registered).unwrap();
        let run_config = authority
            .consume_for_run_admission(&registered, &admission)
            .unwrap();
        let evidence = evidence(
            &registered.preregistration,
            roster,
            &run_config,
            measurements()[index],
        );
        let evidence = if index == 0 {
            evidence
        } else {
            production_source(evidence)
        };
        authority
            .record_harness_trial(
                &registered,
                &run_config,
                evidence.harness_report_bytes,
                evidence.events_bytes,
            )
            .unwrap();
    }
    assert_eq!(
        authority.build_report(&registered).unwrap_err(),
        StatsError::MixedEvidenceSource
    );
}

#[test]
fn eval_stats_report_authority_restart_chain_time_and_receipt_immutability() {
    let root = TestRoot::new();
    assert_eq!(
        RegistrationAuthority::open(&root.0).err(),
        Some(StatsError::AnchorUnavailable)
    );
    let anchor = ConformanceLedgerAnchor::source_semantics_fake();
    let mut authority = RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
    let first = authority.register(plan()).unwrap();
    assert!(first.registration.registered_at > first.registration.authority_epoch);
    drop(authority);

    let mut authority = RegistrationAuthority::open_with_anchor(&root.0, anchor).unwrap();
    authority.verify(&first).unwrap();
    let second = authority.register(plan()).unwrap();
    assert_eq!(
        second.registration.sequence,
        first.registration.sequence + 1
    );
    assert!(second.registration.registered_at > first.registration.registered_at);
    assert_eq!(
        second.registration.authority_position,
        first.registration.authority_position + 1
    );
    assert_ne!(
        second.registration.previous_entry_digest,
        first.registration.previous_entry_digest
    );
    let mut changed = first.clone();
    changed.preregistration.noninferiority.margin = 0.09;
    assert_eq!(
        authority.verify(&changed),
        Err(StatsError::InvalidRegistration)
    );
}

#[test]
fn eval_stats_report_alternate_genesis_and_backdated_store_rows_reject_on_restart() {
    let first = TestRoot::new();
    let second = TestRoot::new();
    let first_anchor = ConformanceLedgerAnchor::source_semantics_fake();
    let second_anchor = ConformanceLedgerAnchor::source_semantics_fake();
    let mut authority =
        RegistrationAuthority::open_with_anchor(&first.0, first_anchor.clone()).unwrap();
    authority.register(plan()).unwrap();
    drop(authority);
    let other = RegistrationAuthority::open_with_anchor(&second.0, second_anchor).unwrap();
    drop(other);
    fs::copy(
        second.0.join("registration-authority.json"),
        first.0.join("registration-authority.json"),
    )
    .unwrap();
    assert!(matches!(
        RegistrationAuthority::open_with_anchor(&first.0, first_anchor),
        Err(StatsError::LedgerRollback) | Err(StatsError::AlternateGenesis)
    ));

    let backdated = TestRoot::new();
    let backdated_anchor = ConformanceLedgerAnchor::source_semantics_fake();
    let mut authority =
        RegistrationAuthority::open_with_anchor(&backdated.0, backdated_anchor.clone()).unwrap();
    authority.register(plan()).unwrap();
    drop(authority);
    let connection = Connection::open(backdated.0.join("registration.sqlite3")).unwrap();
    assert!(connection
        .execute(
            "UPDATE registrations SET registered_at = '2020-01-01T00:00:00Z' WHERE sequence = 1",
            [],
        )
        .is_err());
    connection
        .execute("DROP TRIGGER registrations_no_update", [])
        .unwrap();
    connection
        .execute("DROP TRIGGER ledger_no_update", [])
        .unwrap();
    connection
        .execute(
            "UPDATE ledger SET recorded_at = '2020-01-01T00:00:00Z' WHERE position = 1",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        RegistrationAuthority::open_with_anchor(&backdated.0, backdated_anchor),
        Err(StatsError::LedgerTamper)
    ));
}

#[test]
fn eval_stats_report_anchor_detects_database_and_credential_snapshot_rollback_and_tail_deletion() {
    let root = TestRoot::new();
    let snapshot = TestRoot::new();
    let anchor = ConformanceLedgerAnchor::source_semantics_fake();
    let mut authority = RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
    authority.register(plan()).unwrap();
    drop(authority);
    fs::copy(
        root.0.join("registration.sqlite3"),
        snapshot.0.join("registration.sqlite3"),
    )
    .unwrap();
    fs::copy(
        root.0.join("registration-authority.json"),
        snapshot.0.join("registration-authority.json"),
    )
    .unwrap();

    let mut authority = RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
    authority.register(plan()).unwrap();
    drop(authority);
    fs::copy(
        snapshot.0.join("registration.sqlite3"),
        root.0.join("registration.sqlite3"),
    )
    .unwrap();
    fs::copy(
        snapshot.0.join("registration-authority.json"),
        root.0.join("registration-authority.json"),
    )
    .unwrap();
    assert!(matches!(
        RegistrationAuthority::open_with_anchor(&root.0, anchor),
        Err(StatsError::LedgerRollback)
    ));

    let deleted = TestRoot::new();
    let deleted_anchor = ConformanceLedgerAnchor::source_semantics_fake();
    let mut authority =
        RegistrationAuthority::open_with_anchor(&deleted.0, deleted_anchor.clone()).unwrap();
    authority.register(plan()).unwrap();
    drop(authority);
    let connection = Connection::open(deleted.0.join("registration.sqlite3")).unwrap();
    assert!(connection.execute("DELETE FROM ledger", []).is_err());
    connection
        .execute("DROP TRIGGER ledger_no_delete", [])
        .unwrap();
    connection
        .execute("DELETE FROM ledger WHERE position = 1", [])
        .unwrap();
    drop(connection);
    assert!(matches!(
        RegistrationAuthority::open_with_anchor(&deleted.0, deleted_anchor),
        Err(StatsError::LedgerRollback) | Err(StatsError::LedgerTamper)
    ));
}

#[test]
fn eval_stats_report_single_use_admission_attempt_bound_and_incomplete_roster_are_unhideable() {
    let (_root, mut authority, registered) = registry();
    let first_roster = &registered.preregistration.roster[0];
    let first = authority.admit_next(&registered).unwrap();
    assert_eq!(first.trial_id(), first_roster.trial_id);
    let run_config = authority
        .consume_for_run_admission(&registered, &first)
        .unwrap();
    assert_eq!(
        authority
            .consume_for_run_admission(&registered, &first)
            .unwrap_err(),
        StatsError::InvalidAdmission
    );
    let first_evidence = evidence(
        &registered.preregistration,
        first_roster,
        &run_config,
        measurements()[0],
    );
    authority
        .record_harness_trial(
            &registered,
            &run_config,
            first_evidence.harness_report_bytes,
            first_evidence.events_bytes,
        )
        .unwrap();
    let retry = evidence(
        &registered.preregistration,
        first_roster,
        &run_config,
        measurements()[0],
    );
    let repeated = authority
        .record_harness_trial(
            &registered,
            &run_config,
            retry.harness_report_bytes,
            retry.events_bytes,
        )
        .unwrap();
    assert_eq!(
        repeated.receipt(),
        authority
            .load_harness_trial(&registered, &run_config)
            .unwrap()
            .unwrap()
            .receipt()
    );
    let second = authority.admit_next(&registered).unwrap();
    assert_eq!(
        second.trial_id(),
        registered.preregistration.roster[1].trial_id
    );

    assert_eq!(
        authority.build_report(&registered).unwrap_err(),
        StatsError::ExperimentNotTerminal
    );
}

#[test]
fn eval_stats_report_shared_validator_rejects_cross_field_and_recomputed_digest_tamper() {
    let (root, mut authority, registered) = registry();
    let roster = &registered.preregistration.roster[0];
    let admission = authority.admit_next(&registered).unwrap();
    let run_config = authority
        .consume_for_run_admission(&registered, &admission)
        .unwrap();
    for (pointer, replacement) in [
        ("/trial_id", json!("different-trial")),
        ("/pair_id", json!("different-pair")),
        ("/task_id", json!("different-task")),
        ("/dataset_member_id", json!("different-member")),
        ("/seed", json!(999)),
        ("/arm", json!("candidate")),
        ("/config_digest", json!(digest('e'))),
    ] {
        let mismatched = tamper_event(
            evidence(
                &registered.preregistration,
                roster,
                &run_config,
                measurements()[0],
            ),
            pointer,
            replacement,
        );
        assert!(
            matches!(
                authority.record_harness_trial(
                    &registered,
                    &run_config,
                    mismatched.harness_report_bytes,
                    mismatched.events_bytes,
                ),
                Err(StatsError::InvalidTrial(_))
            ),
            "accepted tamper at {pointer}"
        );
    }
    for field in [
        "model_digest",
        "model_settings_digest",
        "provider_capability_digest",
    ] {
        let mut mismatched = evidence(
            &registered.preregistration,
            roster,
            &run_config,
            measurements()[0],
        );
        let mut report: Value = serde_json::from_slice(&mismatched.harness_report_bytes).unwrap();
        report[field] = json!(digest('e'));
        mismatched.harness_report_bytes = serde_json::to_vec(&report).unwrap();
        assert_eq!(
            authority
                .record_harness_trial(
                    &registered,
                    &run_config,
                    mismatched.harness_report_bytes,
                    mismatched.events_bytes,
                )
                .unwrap_err(),
            StatsError::InvalidTrial("model pin mismatch")
        );
    }
    let mut mismatched = evidence(
        &registered.preregistration,
        roster,
        &run_config,
        measurements()[0],
    );
    let mut report: Value = serde_json::from_slice(&mismatched.harness_report_bytes).unwrap();
    report["harness_config_digest"] = json!(digest('e'));
    mismatched.harness_report_bytes = serde_json::to_vec(&report).unwrap();
    assert!(matches!(
        authority.record_harness_trial(
            &registered,
            &run_config,
            mismatched.harness_report_bytes,
            mismatched.events_bytes,
        ),
        Err(StatsError::InvalidTrial(_))
    ));

    let valid = evidence(
        &registered.preregistration,
        roster,
        &run_config,
        measurements()[0],
    );
    authority
        .record_harness_trial(
            &registered,
            &run_config,
            valid.harness_report_bytes,
            valid.events_bytes,
        )
        .unwrap();
    let connection = Connection::open(root.0.join("registration.sqlite3")).unwrap();
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT receipt_bytes FROM executions WHERE registration_sequence = ?1 AND schedule_index = 0",
            [registered.registration.sequence],
            |row| row.get(0),
        )
        .unwrap();
    let mut receipt: Value = serde_json::from_slice(&bytes).unwrap();
    receipt["latency_ms"] = json!(1.0);
    let forged = serde_json::to_vec(&receipt).unwrap();
    assert!(connection
        .execute(
            "UPDATE executions SET receipt_bytes = ?1, digest = ?2 WHERE registration_sequence = ?3 AND schedule_index = 0",
            rusqlite::params![forged, stats::sha256(&forged), registered.registration.sequence],
        )
        .is_err());
    connection
        .execute("DROP TRIGGER executions_no_update", [])
        .unwrap();
    connection.execute(
        "UPDATE executions SET receipt_bytes = ?1, digest = ?2 WHERE registration_sequence = ?3 AND schedule_index = 0",
        rusqlite::params![forged, stats::sha256(&forged), registered.registration.sequence],
    ).unwrap();
    drop(connection);
    assert_eq!(
        authority.build_report(&registered).unwrap_err(),
        StatsError::LedgerTamper
    );
}

#[test]
fn eval_stats_report_continuous_primary_uses_paired_t_with_correct_df() {
    let root = TestRoot::new();
    let mut authority = RegistrationAuthority::open_with_anchor(
        &root.0,
        ConformanceLedgerAnchor::source_semantics_fake(),
    )
    .unwrap();
    let mut plan = plan();
    plan.primary_hypothesis.metric = CoreMetric::MeanLatencyMs;
    plan.primary_hypothesis.direction = Direction::LowerIsBetter;
    plan.primary_metric.metric = CoreMetric::MeanLatencyMs;
    plan.primary_metric.unit = MetricUnit::Milliseconds;
    plan.exploratory_metrics
        .retain(|metric| metric.metric != CoreMetric::MeanLatencyMs);
    plan.exploratory_metrics.push(stats::MetricSpec {
        metric: CoreMetric::SuccessRate,
        role: MetricRole::Exploratory,
        unit: MetricUnit::Proportion,
        estimand: "Exploratory paired success-rate difference.".to_owned(),
    });
    plan.exploratory_metrics.sort_by_key(|metric| metric.metric);
    plan.noninferiority.metric = CoreMetric::MeanLatencyMs;
    plan.noninferiority.direction = Direction::LowerIsBetter;
    plan.noninferiority.margin = 20.0;
    plan.noninferiority.scientific_max_margin = 50.0;
    refresh_design(&mut plan);
    let registered = authority.register(plan).unwrap();
    run_roster(&mut authority, &registered, &measurements());
    let report = authority.build_report(&registered).unwrap();
    let MetricSummary::Confirmatory {
        confidence_interval,
        ..
    } = &report.report.metrics[0]
    else {
        panic!("primary was not confirmatory")
    };
    assert_eq!(
        confidence_interval.method,
        IntervalMethod::PairedTFiniteSampleV1
    );
    assert_eq!(confidence_interval.degrees_of_freedom, Some(2));
}

#[test]
fn eval_stats_report_rejects_vacuous_margins_placeholder_pins_and_roster_duplicates() {
    let cases = [
        ("/noninferiority/margin", json!(0.0)),
        ("/noninferiority/direction", json!("lower_is_better")),
        ("/noninferiority/scientific_max_margin", json!(1.0)),
        ("/digests/harness", json!(digest('0'))),
        (
            "/sample_size/power_rationale",
            json!("replace this placeholder"),
        ),
        ("/roster/1/trial_id", json!("pair-a-baseline")),
        ("/roster/1/task_id", json!("task-substituted")),
        ("/roster/2/task_id", json!("task-a")),
        ("/roster/2/task_manifest_digest", json!(digest('a'))),
    ];
    let original: Value = serde_json::from_slice(include_bytes!(
        "../../eval/preregistration/templates/core-paired-v1.json"
    ))
    .unwrap();
    for (pointer, replacement) in cases {
        let mut malformed = original.clone();
        *malformed.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            Preregistration::from_json(&serde_json::to_vec(&malformed).unwrap()).is_err(),
            "accepted malformed value at {pointer}"
        );
    }
    let mut missing = original;
    missing["roster"][0]
        .as_object_mut()
        .unwrap()
        .remove("task_manifest_digest");
    assert!(Preregistration::from_json(&serde_json::to_vec(&missing).unwrap()).is_err());
}

#[test]
fn eval_stats_report_heterogeneous_task_pins_survive_admission_restart() {
    let root = TestRoot::new();
    let anchor = ConformanceLedgerAnchor::source_semantics_fake();
    let mut authority = RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
    let registered = authority.register(plan()).unwrap();
    let mut admitted = BTreeSet::new();
    for _ in 0..3 {
        let token = authority
            .admit_next(&registered)
            .unwrap()
            .scheduler_token()
            .unwrap();
        admitted.insert((token.task_id, token.task_manifest_digest));
    }
    drop(authority);

    let mut authority = RegistrationAuthority::open_with_anchor(&root.0, anchor).unwrap();
    for _ in 3..6 {
        let token = authority
            .admit_next(&registered)
            .unwrap()
            .scheduler_token()
            .unwrap();
        admitted.insert((token.task_id, token.task_manifest_digest));
    }
    assert_eq!(
        admitted,
        BTreeSet::from([
            ("task-a".to_owned(), digest('a')),
            ("task-b".to_owned(), digest('b')),
            ("task-c".to_owned(), digest('d')),
        ])
    );
}

#[test]
fn eval_stats_report_homogeneous_task_pin_roster_remains_valid() {
    let mut plan = plan();
    for entry in &mut plan.roster {
        entry.task_id = "task-a".to_owned();
        entry.task_manifest_digest = digest('a');
    }
    refresh_design(&mut plan);
    plan.canonical_bytes().unwrap();
}

fn component_validator(definition: &str) -> Validator {
    let components: Value = serde_json::from_slice(include_bytes!(
        "../../eval/preregistration/schema/v1/components.schema.json"
    ))
    .unwrap();
    jsonschema::draft202012::meta::validate(&components).unwrap();
    let wrapper = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": components["$defs"],
        "$ref": format!("#/$defs/{definition}")
    });
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&wrapper)
        .unwrap()
}

#[test]
fn eval_stats_report_schema_components_and_runtime_parity_corpus() {
    let validator = component_validator("preregistration");
    let original: Value = serde_json::from_slice(include_bytes!(
        "../../eval/preregistration/templates/core-paired-v1.json"
    ))
    .unwrap();
    let corpus = [
        ("/alpha", json!(0.1)),
        ("/primary_metric/role", json!("exploratory")),
        ("/primary_metric/unit", json!("usd")),
        ("/noninferiority/margin", json!(0.0)),
        ("/noninferiority/direction", json!("lower_is_better")),
        ("/sample_size/complete_pairs", json!(2)),
        ("/digests/dataset", json!(digest('0'))),
    ];
    assert!(validator.is_valid(&original));
    Preregistration::from_json(&serde_json::to_vec(&original).unwrap()).unwrap();
    for (pointer, replacement) in corpus {
        let mut value = original.clone();
        *value.pointer_mut(pointer).unwrap() = replacement;
        let schema_accepts = validator.is_valid(&value);
        let runtime_accepts =
            Preregistration::from_json(&serde_json::to_vec(&value).unwrap()).is_ok();
        assert_eq!(
            schema_accepts, runtime_accepts,
            "schema/runtime drift at {pointer}"
        );
        assert!(!runtime_accepts);
    }

    for (label, pointer, replacement) in [
        (
            "runtime-only pair equality",
            "/roster/1/task_id",
            json!("other-task"),
        ),
        (
            "runtime-only margin ordering",
            "/noninferiority/margin",
            json!(0.2),
        ),
        (
            "runtime-only primary metric equality",
            "/primary_hypothesis/metric",
            json!("verification_rate"),
        ),
        (
            "runtime-only derived dataset commitment",
            "/digests/dataset",
            json!(digest('e')),
        ),
    ] {
        let mut value = original.clone();
        *value.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            validator.is_valid(&value),
            "schema should leave {label} to runtime relation validation"
        );
        assert!(
            Preregistration::from_json(&serde_json::to_vec(&value).unwrap()).is_err(),
            "runtime accepted {label}"
        );
    }

    for (metric, direction, unit, margin, scientific_max_margin) in [
        (
            CoreMetric::SuccessRate,
            Direction::HigherIsBetter,
            MetricUnit::Proportion,
            0.05,
            0.25,
        ),
        (
            CoreMetric::InterventionRate,
            Direction::LowerIsBetter,
            MetricUnit::Proportion,
            0.05,
            0.25,
        ),
        (
            CoreMetric::MeanCostUsd,
            Direction::LowerIsBetter,
            MetricUnit::Usd,
            1.0,
            1_000_000.0,
        ),
        (
            CoreMetric::MeanLatencyMs,
            Direction::LowerIsBetter,
            MetricUnit::Milliseconds,
            1.0,
            86_400_000.0,
        ),
        (
            CoreMetric::VerificationRate,
            Direction::HigherIsBetter,
            MetricUnit::Proportion,
            0.05,
            0.25,
        ),
    ] {
        let mut generated = plan();
        generated.primary_hypothesis.metric = metric;
        generated.primary_hypothesis.direction = direction;
        generated.primary_metric.metric = metric;
        generated.primary_metric.unit = unit;
        generated
            .exploratory_metrics
            .retain(|spec| spec.metric != metric);
        if metric != CoreMetric::SuccessRate {
            generated.exploratory_metrics.push(stats::MetricSpec {
                metric: CoreMetric::SuccessRate,
                role: MetricRole::Exploratory,
                unit: MetricUnit::Proportion,
                estimand: "Generated schema/runtime parity estimand.".to_owned(),
            });
            generated
                .exploratory_metrics
                .sort_by_key(|spec| spec.metric);
        }
        generated.noninferiority.metric = metric;
        generated.noninferiority.direction = direction;
        generated.noninferiority.margin = margin;
        generated.noninferiority.scientific_max_margin = scientific_max_margin;
        refresh_design(&mut generated);
        let bytes = serde_json::to_vec(&generated).unwrap();
        assert!(validator.is_valid(&serde_json::from_slice(&bytes).unwrap()));
        assert!(Preregistration::from_json(&bytes).is_ok());
    }

    for wrapper in [
        "eval/preregistration/schema/v1/preregistration.schema.json",
        "eval/preregistration/schema/v1/registration.schema.json",
        "eval/reports/schema/v1/statistical-report.schema.json",
    ] {
        let value: Value = serde_json::from_slice(
            &fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(wrapper)).unwrap(),
        )
        .unwrap();
        jsonschema::draft202012::meta::validate(&value).unwrap();
        assert!(value["$ref"].as_str().is_some());
    }

    let (_root, mut authority, registered) = registry();
    run_roster(&mut authority, &registered, &measurements());
    let report = authority.build_report(&registered).unwrap();
    assert!(
        component_validator("registered_preregistration")
            .is_valid(&serde_json::to_value(&registered).unwrap())
    );
    assert!(
        component_validator("statistical_report")
            .is_valid(&serde_json::from_slice(&report.bytes).unwrap())
    );
    assert!(
        component_validator("statistical_report_receipt")
            .is_valid(&serde_json::to_value(&report.receipt).unwrap())
    );
}

#[test]
fn eval_stats_report_failure_policy_suppresses_confirmatory_claim() {
    let root = TestRoot::new();
    let mut authority = RegistrationAuthority::open_with_anchor(
        &root.0,
        ConformanceLedgerAnchor::source_semantics_fake(),
    )
    .unwrap();
    let mut plan = plan();
    plan.policies.failure = FailurePolicy::FailAnalysis;
    refresh_design(&mut plan);
    let registered = authority.register(plan).unwrap();
    run_roster(&mut authority, &registered, &measurements());
    let report = authority.build_report(&registered).unwrap();
    assert_eq!(
        report.report.analysis_status,
        AnalysisStatus::FailedByPolicy
    );
    assert!(report.report.metrics.is_empty());
}

#[test]
fn eval_stats_report_terminal_errors_are_not_free_for_cost_or_latency_noninferiority() {
    for (metric, unit, margin, ceiling) in [
        (CoreMetric::MeanCostUsd, MetricUnit::Usd, 1.0, 1000.0),
        (
            CoreMetric::MeanLatencyMs,
            MetricUnit::Milliseconds,
            1000.0,
            86_400_000.0,
        ),
    ] {
        let root = TestRoot::new();
        let mut authority = RegistrationAuthority::open_with_anchor(
            &root.0,
            ConformanceLedgerAnchor::source_semantics_fake(),
        )
        .unwrap();
        let mut plan = plan();
        plan.primary_hypothesis.metric = metric;
        plan.primary_hypothesis.direction = Direction::LowerIsBetter;
        plan.primary_metric.metric = metric;
        plan.primary_metric.unit = unit;
        plan.exploratory_metrics
            .retain(|spec| spec.metric != metric);
        plan.noninferiority.metric = metric;
        plan.noninferiority.direction = Direction::LowerIsBetter;
        plan.noninferiority.margin = margin;
        plan.noninferiority.scientific_max_margin = ceiling;
        refresh_design(&mut plan);
        let registered = authority.register(plan).unwrap();
        for _ in &registered.preregistration.roster {
            let admission = authority.admit_next(&registered).unwrap();
            let config = authority
                .consume_for_run_admission(&registered, &admission)
                .unwrap();
            let policy = registered.preregistration.policies.error_imputation;
            let partial_usage = TerminalDurableUsage {
                cost_microusd: 123,
                tokens: 3,
                turns: 1,
                tool_calls: 0,
                processes: 1,
            };
            assert!(
                authority
                    .record_terminal_error(
                        &registered,
                        &config,
                        EvidenceSource::ConformanceSourceSemantics,
                        &TerminalErrorEvidence {
                            reason: "favorable_partial_executor_error".to_owned(),
                            elapsed_millis: 7,
                            durable_usage: Some(partial_usage.clone()),
                            cost_microusd: partial_usage.cost_microusd,
                            cost_imputed: false,
                            latency_millis: 7,
                            latency_imputed: false,
                        },
                    )
                    .is_err()
            );
            let receipt = authority
                .record_terminal_error(
                    &registered,
                    &config,
                    EvidenceSource::ConformanceSourceSemantics,
                    &TerminalErrorEvidence {
                        reason: "executor_error".to_owned(),
                        elapsed_millis: 7,
                        durable_usage: Some(partial_usage.clone()),
                        cost_microusd: (policy.max_cost_usd * 1_000_000.0) as u64,
                        cost_imputed: true,
                        latency_millis: policy.max_latency_ms,
                        latency_imputed: true,
                    },
                )
                .unwrap();
            assert_eq!(receipt.receipt().outcome, TrialOutcome::Error);
            assert!(receipt.receipt().cost_usd > 0.0);
            assert!(receipt.receipt().latency_ms > 0.0);
            assert_eq!(receipt.receipt().durable_usage, Some(partial_usage));
        }
        authority.freeze_experiment(&registered).unwrap();
        let report = authority.build_report(&registered).unwrap();
        assert_eq!(
            report.report.analysis_status,
            AnalysisStatus::ConfirmatoryMetricUnavailable
        );
        assert!(report.report.metrics.is_empty());
    }
}

#[test]
fn eval_stats_report_harness_digest_mismatch_fails_before_authority_append() {
    let validator = component_validator("preregistration");
    let mut mismatched = plan();
    mismatched.digests.harness = digest('e');
    refresh_design(&mut mismatched);
    let bytes = serde_json::to_vec(&mismatched).unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let expected = StatsError::InvalidPreregistration(
        "design harness digest does not match production execution harness pin".to_owned(),
    );

    assert!(validator.is_valid(&value));
    assert_eq!(Preregistration::from_json(&bytes).unwrap_err(), expected);

    let root = TestRoot::new();
    let mut authority = RegistrationAuthority::open_with_anchor(
        &root.0,
        ConformanceLedgerAnchor::source_semantics_fake(),
    )
    .unwrap();
    assert_eq!(authority.register(mismatched).unwrap_err(), expected);

    let registered = authority.register(plan()).unwrap();
    assert_eq!(registered.registration.authority_position, 1);
    let mut mismatched_registration = registered.clone();
    mismatched_registration.preregistration.digests.harness = digest('e');
    refresh_design(&mut mismatched_registration.preregistration);
    assert_eq!(
        authority.admit_next(&mismatched_registration).unwrap_err(),
        StatsError::InvalidRegistration
    );
    assert_eq!(
        authority
            .admit_next(&registered)
            .unwrap()
            .scheduler_token()
            .unwrap()
            .authority_position,
        2
    );
}

#[derive(Default)]
struct CrashAnchor {
    receipt: Mutex<Option<stats::LedgerAnchorReceipt>>,
    fail_before_cas: AtomicBool,
    fail_after_cas: AtomicBool,
}

impl CrashAnchor {
    fn fail_before_cas(&self) {
        self.fail_before_cas.store(true, Ordering::Release);
    }

    fn fail_after_cas(&self) {
        self.fail_after_cas.store(true, Ordering::Release);
    }

    fn force_fork(&self) {
        let mut current = self.receipt.lock().unwrap();
        let previous = current.as_ref().unwrap();
        *current = Some(stats::LedgerAnchorReceipt {
            source: "conformance_fault_anchor".to_owned(),
            authority_id: previous.authority_id.clone(),
            counter: previous.counter + 1,
            ledger_position: previous.ledger_position + 1,
            ledger_head_digest: digest('f'),
            signature: "fork-signature".to_owned(),
        });
    }
}

impl stats::LedgerAnchor for CrashAnchor {
    fn current(&self) -> Result<Option<stats::LedgerAnchorReceipt>, StatsError> {
        Ok(self.receipt.lock().unwrap().clone())
    }

    fn advance(
        &self,
        previous: Option<&stats::LedgerAnchorReceipt>,
        authority_id: &str,
        ledger_position: u64,
        ledger_head_digest: &str,
    ) -> Result<stats::LedgerAnchorReceipt, StatsError> {
        if self.fail_before_cas.swap(false, Ordering::AcqRel) {
            return Err(StatsError::Anchor("injected before CAS"));
        }
        let mut current = self.receipt.lock().unwrap();
        if current.as_ref().is_some_and(|receipt| {
            receipt.authority_id == authority_id
                && receipt.ledger_position == ledger_position
                && receipt.ledger_head_digest == ledger_head_digest
        }) {
            return Ok(current.clone().unwrap());
        }
        if current.as_ref() != previous {
            return Err(StatsError::Anchor("injected CAS conflict"));
        }
        let next = stats::LedgerAnchorReceipt {
            source: "conformance_fault_anchor".to_owned(),
            authority_id: authority_id.to_owned(),
            counter: previous.map_or(1, |receipt| receipt.counter + 1),
            ledger_position,
            ledger_head_digest: ledger_head_digest.to_owned(),
            signature: "fault-anchor-signature".to_owned(),
        };
        *current = Some(next.clone());
        if self.fail_after_cas.swap(false, Ordering::AcqRel) {
            return Err(StatsError::Anchor("injected after CAS"));
        }
        Ok(next)
    }
}

#[test]
fn eval_stats_report_anchor_recovers_before_and_after_cas_and_rejects_forks() {
    for after_cas in [false, true] {
        let root = TestRoot::new();
        let anchor = Arc::new(CrashAnchor::default());
        let mut authority =
            RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
        if after_cas {
            anchor.fail_after_cas();
        } else {
            anchor.fail_before_cas();
        }
        assert!(matches!(
            authority.register(plan()),
            Err(StatsError::Anchor(_))
        ));
        drop(authority);
        let mut recovered =
            RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
        assert_eq!(recovered.register(plan()).unwrap().registration.sequence, 2);
    }

    let root = TestRoot::new();
    let anchor = Arc::new(CrashAnchor::default());
    let mut authority = RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
    anchor.fail_before_cas();
    assert!(matches!(
        authority.register(plan()),
        Err(StatsError::Anchor(_))
    ));
    drop(authority);
    anchor.force_fork();
    assert!(matches!(
        RegistrationAuthority::open_with_anchor(&root.0, anchor),
        Err(StatsError::AnchorFork)
    ));
}

#[test]
fn eval_stats_report_consumption_is_exactly_idempotent_across_anchor_crashes() {
    for after_cas in [false, true] {
        let root = TestRoot::new();
        let anchor = Arc::new(CrashAnchor::default());
        let mut authority =
            RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
        let registered = authority.register(plan()).unwrap();
        let admission = authority.admit_next(&registered).unwrap();
        let pending = kit::runtime::scheduler::PendingStatisticalTrial {
            run_id: kit::domain::ids::RunId::generate().unwrap(),
            admission_token_digest: admission.token_digest().to_owned(),
            admission_nonce: admission.scheduler_token().unwrap().nonce,
            admission_position: admission.scheduler_token().unwrap().authority_position,
            consumption_position: 1,
            consumption_digest: stats::sha256(b"scheduler-consumption"),
        };
        if after_cas {
            anchor.fail_after_cas();
        } else {
            anchor.fail_before_cas();
        }
        assert!(matches!(
            authority.consume_scheduler_admission(&registered, &admission, &pending),
            Err(StatsError::Anchor(_))
        ));
        drop(authority);

        let mut authority =
            RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
        let first = authority
            .consume_scheduler_admission(&registered, &admission, &pending)
            .unwrap();
        let repeated = authority
            .consume_scheduler_admission(&registered, &admission, &pending)
            .unwrap();
        assert_eq!(first, repeated);

        let mut mismatch = pending.clone();
        mismatch.consumption_digest = digest('e');
        assert_eq!(
            authority
                .consume_scheduler_admission(&registered, &admission, &mismatch)
                .unwrap_err(),
            StatsError::InvalidAdmission
        );
    }
}

#[test]
fn eval_stats_report_build_is_idempotent_before_and_after_report_anchor_cas() {
    for after_cas in [false, true] {
        let root = TestRoot::new();
        let anchor = Arc::new(CrashAnchor::default());
        let mut authority =
            RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
        let registered = authority.register(plan()).unwrap();
        run_roster(&mut authority, &registered, &measurements());
        authority.freeze_experiment(&registered).unwrap();
        if after_cas {
            anchor.fail_after_cas();
        } else {
            anchor.fail_before_cas();
        }
        assert!(matches!(
            authority.build_report(&registered),
            Err(StatsError::Anchor(_))
        ));
        drop(authority);

        let mut authority =
            RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
        let first = authority.build_report(&registered).unwrap();
        let repeated = authority.build_report(&registered).unwrap();
        assert_eq!(first.bytes, repeated.bytes);
        assert_eq!(first.receipt, repeated.receipt);
    }
}

#[test]
fn eval_stats_report_anchor_rejects_multiple_pending_children() {
    let root = TestRoot::new();
    let anchor = Arc::new(CrashAnchor::default());
    let mut authority = RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
    anchor.fail_before_cas();
    assert!(matches!(
        authority.register(plan()),
        Err(StatsError::Anchor(_))
    ));
    drop(authority);
    let connection = Connection::open(root.0.join("registration.sqlite3")).unwrap();
    connection
        .execute(
            "INSERT INTO anchor_commits
                 (ledger_position, previous_counter, previous_head_digest, ledger_head_digest,
                  record_digest, record_bytes, state, receipt_bytes)
             VALUES (99, 1, ?1, ?2, ?3, ?4, 'pending_anchor', NULL)",
            rusqlite::params![digest('a'), digest('b'), digest('c'), b"fork"],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        RegistrationAuthority::open_with_anchor(&root.0, anchor),
        Err(StatsError::AnchorFork)
    ));
}

#[test]
fn eval_stats_report_anchor_local_commit_failure_rolls_back_before_cas() {
    let root = TestRoot::new();
    let anchor = Arc::new(CrashAnchor::default());
    let mut authority = RegistrationAuthority::open_with_anchor(&root.0, anchor.clone()).unwrap();
    let connection = Connection::open(root.0.join("registration.sqlite3")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER inject_registration_commit_failure
             BEFORE INSERT ON registrations
             BEGIN SELECT RAISE(ABORT, 'injected local commit failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        authority.register(plan()),
        Err(StatsError::Store(_))
    ));
    connection
        .execute("DROP TRIGGER inject_registration_commit_failure", [])
        .unwrap();
    assert_eq!(
        stats::LedgerAnchor::current(anchor.as_ref())
            .unwrap()
            .unwrap()
            .ledger_position,
        0
    );
    assert_eq!(authority.register(plan()).unwrap().registration.sequence, 1);
}
