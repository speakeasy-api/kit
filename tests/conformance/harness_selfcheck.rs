use std::collections::BTreeMap;

use serde_json::json;

#[path = "../../eval/harness/core/mod.rs"]
mod harness_core;

use harness_core::{
    Check, ConformanceCoreTrialExecutor, CoreHarness, CoreTrialError, CoreTrialExecutor,
    GradeOutcome, GraderBounds, HarnessError, HarnessInputs, HiddenHandle, SourceSnapshot,
    TrialAdmissionContext, grade, sha256,
};

const REFERENCE: &[u8] = include_bytes!("../../eval/harness/core/fixtures/reference.patch");
const ADVERSARIAL: &[u8] = include_bytes!("../../eval/harness/core/fixtures/adversarial.patch");

fn bounds() -> GraderBounds {
    GraderBounds {
        max_patch_bytes: 16 * 1024,
        max_source_bytes: 64 * 1024,
        max_files: 32,
        max_checks: 16,
        max_check_bytes: 16 * 1024,
        max_log_bytes: 4096,
        max_artifact_bytes: 128 * 1024,
        max_memory_bytes: 256 * 1024 * 1024,
        max_time_millis: 10_000,
    }
}

fn fixture() -> CoreHarness {
    fixture_with_hidden_digest(sha256(b"right\n"))
}

fn source_executor() -> ConformanceCoreTrialExecutor {
    ConformanceCoreTrialExecutor::source_semantics_fake(
        harness_core::trial_runner::trusted_source_semantics_token(),
    )
}

fn fixture_with_hidden_digest(hidden_digest: String) -> CoreHarness {
    let bounds = bounds();
    let source = SourceSnapshot::new(
        BTreeMap::from([
            (
                "answer.txt".to_owned(),
                include_bytes!("../../eval/harness/core/fixtures/source/answer.txt").to_vec(),
            ),
            (
                "notes.txt".to_owned(),
                include_bytes!("../../eval/harness/core/fixtures/source/notes.txt").to_vec(),
            ),
        ]),
        &bounds,
    )
    .unwrap();
    let checks = vec![Check::Digest {
        id: "answer-is-right".to_owned(),
        path: "answer.txt".to_owned(),
        sha256: sha256(b"right\n"),
    }];
    let acceptance_rules = serde_json::to_vec(&checks).unwrap();
    let hidden_tests = serde_json::to_vec(&json!({
        "schema_version": 1,
        "canaries": ["HIDDEN-CANARY-ANSWER-7f19"],
        "checks": [{
            "kind": "file_digest",
            "id": "hidden-answer-is-right",
            "path": "answer.txt",
            "sha256": hidden_digest
        }]
    }))
    .unwrap();
    let toolchain_digest = sha256(b"rustc-1.94.0");
    let config = serde_json::to_vec(&json!({
        "schema_version": 2,
        "harness_version": "m004-core-v2",
        "toolchain_digest": toolchain_digest,
        "source_snapshot_digest": source.digest(),
        "hidden_tests_digest": sha256(&hidden_tests),
        "acceptance_rules_digest": sha256(&acceptance_rules),
        "gold_patch_digest": sha256(REFERENCE),
        "bounds": bounds,
        "checks": checks,
    }))
    .unwrap();
    let specification = b"Change answer.txt from wrong to right.\n".to_vec();
    let scaffold = b"m004-core-scaffold-v1\n".to_vec();
    let mut trial: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../eval/manifests/examples/trial.json")).unwrap();
    trial["grader"]["hidden_tests_digest"] = sha256(&hidden_tests).into();
    trial["grader"]["acceptance_digest"] = sha256(&acceptance_rules).into();
    trial["grader"]["gold_patch_digest"] = sha256(REFERENCE).into();
    trial["grader"]["harness_config_digest"] = sha256(&config).into();
    trial["task"]["specification_digest"] = sha256(&specification).into();
    trial["task"]["scaffold_digest"] = sha256(&scaffold).into();
    let task_manifest = serde_json::to_vec(&trial["task"]).unwrap();
    let grader_manifest = serde_json::to_vec(&trial["grader"]).unwrap();
    CoreHarness::load(HarnessInputs {
        trial_manifest: serde_json::to_vec(&trial).unwrap(),
        task_manifest,
        grader_manifest,
        source,
        specification,
        scaffold,
        actual_toolchain_digest: toolchain_digest,
        hidden_tests_handle: HiddenHandle::new("hidden-tests-handle").unwrap(),
        hidden_tests,
        gold_patch_handle: HiddenHandle::new("gold-patch-handle").unwrap(),
        gold_patch: REFERENCE.to_vec(),
        acceptance_handle: HiddenHandle::new("acceptance-handle").unwrap(),
        acceptance_rules,
        harness_config_handle: HiddenHandle::new("harness-config-handle").unwrap(),
        harness_config: config,
    })
    .unwrap()
}

#[test]
fn public_pass_hidden_fail_is_a_failure_without_hidden_inputs_in_report() {
    let harness = fixture_with_hidden_digest(sha256(b"not-right\n"));
    let error = match harness.self_validate(&mut source_executor()) {
        Ok(_) => panic!("hidden check unexpectedly passed"),
        Err(error) => error,
    };
    let HarnessError::CalibrationFailed(cases) = error else {
        panic!("expected hidden check to fail calibration");
    };
    let reference = cases
        .iter()
        .find(|case| case.kind == harness_core::CalibrationKind::Reference)
        .unwrap();
    assert_eq!(reference.actual, GradeOutcome::Failure);
}

#[test]
fn harness_selfcheck_classifies_five_of_five_through_m003() {
    let harness = fixture();
    let mut executor = source_executor();
    let validation = harness.self_validate(&mut executor).unwrap();
    assert_eq!(validation.cases.len(), 5);
    assert!(
        validation
            .cases
            .iter()
            .all(|case| case.actual == case.expected)
    );
    assert_eq!(executor.route_calls(), 5);
    assert_eq!(executor.in_process_agent_calls(), 0);
    assert_eq!(executor.in_process_grader_calls(), 0);
    assert_eq!(executor.isolated_grader_calls(), 5);
    assert_eq!(executor.hidden_agent_accesses(), 0);
}

#[test]
fn identical_trials_have_identical_canonical_bytes_and_digest() {
    let harness = fixture();
    let mut executor = source_executor();
    let validation = harness.self_validate(&mut executor).unwrap();
    let first = harness
        .measure(&mut executor, &validation.token, REFERENCE)
        .unwrap();
    let second = harness
        .measure(&mut executor, &validation.token, REFERENCE)
        .unwrap();
    assert_eq!(first.bytes, second.bytes);
    assert_eq!(first.digest, second.digest);
    assert_ne!(
        first.volatile.agent_instance_id,
        second.volatile.agent_instance_id
    );
    assert_eq!(first.report.outcome, GradeOutcome::Success);
    assert_eq!(executor.route_calls(), 7);
}

#[test]
fn calibration_cases_are_derived_from_trusted_inputs() {
    let harness = fixture();
    let validation = harness.self_validate(&mut source_executor()).unwrap();
    assert_eq!(validation.cases.len(), 5);
}

#[test]
fn hidden_checks_require_manifest_owned_canary_coverage() {
    let hidden = harness_core::HiddenTestManifest {
        schema_version: 1,
        checks: vec![Check::Absent {
            id: "hidden".to_owned(),
            path: "secret".to_owned(),
        }],
        canaries: Vec::new(),
    };
    assert!(hidden.validated_canaries(&bounds(), &[]).is_err());
}

#[test]
fn hidden_material_and_digest_substitution_are_rejected() {
    let harness = fixture();
    let mut executor = source_executor();
    let validation = harness.self_validate(&mut executor).unwrap();
    let report = harness
        .measure(&mut executor, &validation.token, ADVERSARIAL)
        .unwrap();
    assert_eq!(report.report.outcome, GradeOutcome::Failure);
    assert!(!String::from_utf8_lossy(&report.bytes).contains("HIDDEN-CANARY-ANSWER-7f19"));
    assert_eq!(executor.hidden_agent_accesses(), 0);

    let bounds = bounds();
    let source =
        SourceSnapshot::new([("answer.txt".to_owned(), b"wrong\n".to_vec())], &bounds).unwrap();
    let mut trial: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../eval/manifests/examples/trial.json")).unwrap();
    let config = serde_json::to_vec(&json!({
        "schema_version": 2,
        "harness_version": "m004-core-v2",
        "toolchain_digest": sha256(b"toolchain"),
        "source_snapshot_digest": source.digest(),
        "hidden_tests_digest": sha256(b"expected"),
        "acceptance_rules_digest": sha256(b"acceptance"),
        "gold_patch_digest": sha256(REFERENCE),
        "bounds": bounds,
        "checks": [{"kind":"file_absent", "id":"x", "path":"x"}],
    }))
    .unwrap();
    trial["grader"]["harness_config_digest"] = sha256(&config).into();
    trial["grader"]["hidden_tests_digest"] = sha256(b"expected").into();
    trial["grader"]["acceptance_digest"] = sha256(b"acceptance").into();
    trial["grader"]["gold_patch_digest"] = sha256(REFERENCE).into();
    let specification = b"specification".to_vec();
    let scaffold = b"scaffold".to_vec();
    trial["task"]["specification_digest"] = sha256(&specification).into();
    trial["task"]["scaffold_digest"] = sha256(&scaffold).into();
    let result = CoreHarness::load(HarnessInputs {
        task_manifest: serde_json::to_vec(&trial["task"]).unwrap(),
        grader_manifest: serde_json::to_vec(&trial["grader"]).unwrap(),
        trial_manifest: serde_json::to_vec(&trial).unwrap(),
        source,
        specification,
        scaffold,
        actual_toolchain_digest: sha256(b"toolchain"),
        hidden_tests_handle: HiddenHandle::new("hidden").unwrap(),
        hidden_tests: b"substituted".to_vec(),
        gold_patch_handle: HiddenHandle::new("gold").unwrap(),
        gold_patch: REFERENCE.to_vec(),
        acceptance_handle: HiddenHandle::new("acceptance").unwrap(),
        acceptance_rules: b"acceptance".to_vec(),
        harness_config_handle: HiddenHandle::new("config").unwrap(),
        harness_config: config,
    });
    assert!(matches!(
        result,
        Err(HarnessError::DigestMismatch("hidden tests"))
    ));
}

#[test]
fn bounds_crash_unknown_and_external_block_are_typed_non_passes() {
    let harness = fixture();
    let mut executor = source_executor();
    let validation = harness.self_validate(&mut executor).unwrap();

    executor.crash_once();
    assert!(matches!(
        harness.measure(&mut executor, &validation.token, REFERENCE),
        Err(HarnessError::Trial(CoreTrialError::Crashed))
    ));
    executor.outcome_unknown_once();
    assert!(matches!(
        harness.measure(&mut executor, &validation.token, REFERENCE),
        Err(HarnessError::Trial(CoreTrialError::OutcomeUnknown))
    ));
    assert!(matches!(
        harness.measure(&mut executor, &validation.token, &vec![b'x'; 16 * 1024 + 1]),
        Err(HarnessError::BoundExceeded("patch"))
    ));

    struct ExternalBlocked;
    impl CoreTrialExecutor for ExternalBlocked {
        fn execute_core(
            &mut self,
            _: harness_core::trial_runner::CoreTrialRequest<'_>,
        ) -> Result<harness_core::trial_runner::CoreTrialExecution, CoreTrialError> {
            Err(CoreTrialError::ExternalBlocked(
                "trusted helper/image unavailable".to_owned(),
            ))
        }
    }
    assert!(matches!(
        harness.measure(&mut ExternalBlocked, &validation.token, REFERENCE),
        Err(HarnessError::Trial(CoreTrialError::ExternalBlocked(_)))
    ));
}

#[test]
#[cfg(not(target_os = "linux"))]
fn unsupported_local_memory_backend_is_typed_conformance_unavailable() {
    let harness = fixture();
    let error = match harness.self_validate(&mut ConformanceCoreTrialExecutor::default()) {
        Ok(_) => panic!("unsupported host claimed hard memory conformance"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        HarnessError::Trial(CoreTrialError::ConformanceUnavailable(_))
    ));
}

#[test]
fn full_protocol_artifacts_attestations_and_usage_are_validated() {
    let harness = fixture();
    let mut executor = source_executor();
    let validation = harness.self_validate(&mut executor).unwrap();
    let report = harness
        .measure(&mut executor, &validation.token, REFERENCE)
        .unwrap();
    assert_eq!(report.artifacts.applied_patch.bytes, REFERENCE);
    assert_eq!(
        report.artifacts.final_tree.digest,
        report.report.final_tree_digest
    );
    assert_eq!(report.report.grade.checks[0].id, "answer-is-right");
    assert_eq!(report.report.grade.checks.len(), 1);
    assert_eq!(report.report.grade.hidden.count, 1);
    assert_eq!(report.report.grade.hidden.verdict, GradeOutcome::Success);
    assert_eq!(
        report.report.agent.route,
        kit::executor::trial::ExecutionRoute::ConformanceFake
    );
    assert!(!String::from_utf8_lossy(&report.bytes).contains("hidden-answer-is-right"));
    assert_eq!(report.report.usage, kit::executor::trial::TrialUsage::ZERO);
    assert!(report.volatile.provider_request_ids.is_empty());
    assert_eq!(executor.in_process_grader_calls(), 0);
}

#[test]
fn admitted_measurement_embeds_the_immutable_single_use_token_binding() {
    let harness = fixture();
    let mut executor = source_executor();
    let validation = harness.self_validate(&mut executor).unwrap();
    let ordinary = harness
        .measure(&mut executor, &validation.token, REFERENCE)
        .unwrap();
    let admission = TrialAdmissionContext {
        source: "conformance_source_semantics_fake".to_owned(),
        authority_id: "authority-conformance".to_owned(),
        authority_position: 7,
        registration_sequence: 1,
        preregistration_digest: sha256(b"plan"),
        task_set_digest: sha256(b"task-set"),
        dataset_digest: sha256(b"dataset"),
        experiment_design_digest: sha256(b"experiment"),
        production_pins_digest: sha256(b"production-pins"),
        roster_index: 0,
        trial_id: ordinary.report.trial_id.clone(),
        pair_id: "pair-a".to_owned(),
        task_id: "task-a".to_owned(),
        dataset_member_id: "dataset-a".to_owned(),
        task_manifest_digest: ordinary.report.task_manifest_digest.clone(),
        model_digest: harness.model_digest().to_owned(),
        model_settings_digest: harness.model_settings_digest().to_owned(),
        config_digest: harness.config_digest(),
        provider_capability_digest: harness.provider_capability_digest().to_owned(),
        seed: 11,
        arm: "baseline".to_owned(),
        nonce: "conformance-nonce".to_owned(),
        token_digest: sha256(b"token"),
        scheduler_run_id: "018f0000-0000-7000-8000-000000000001".to_owned(),
        scheduler_consumption_position: 8,
        scheduler_consumption_digest: sha256(b"consumption"),
        run_config_digest: sha256(b"run-config"),
        event_high_watermark: 10,
    };
    let admitted = harness
        .measure_admitted(
            &mut executor,
            &validation.token.inner,
            &admission,
            REFERENCE,
        )
        .unwrap();
    let mut substituted = admission.clone();
    substituted.model_settings_digest = sha256(b"substituted-model-settings");
    assert!(matches!(
        harness.measure_admitted(
            &mut executor,
            &validation.token.inner,
            &substituted,
            REFERENCE,
        ),
        Err(HarnessError::AdmissionMismatch)
    ));
    assert_eq!(admitted.report.admission_position, Some(7));
    assert_eq!(
        admitted.report.admission_nonce.as_deref(),
        Some("conformance-nonce")
    );
    assert_eq!(
        admitted.report.admission_token_digest.as_deref(),
        Some(admission.token_digest.as_str())
    );
    assert_eq!(
        admitted.report.run_config_digest.as_deref(),
        Some(admission.run_config_digest.as_str())
    );
    assert_eq!(admitted.report.event_high_watermark, Some(10));
}

#[test]
fn component_route_grade_and_budget_substitution_cannot_pass() {
    let harness = fixture();
    let mut executor = source_executor();
    let validation = harness.self_validate(&mut executor).unwrap();
    for inject in [
        ConformanceCoreTrialExecutor::substitute_artifact_digest_once,
        ConformanceCoreTrialExecutor::substitute_grade_check_once,
        ConformanceCoreTrialExecutor::mismatch_route_once,
        ConformanceCoreTrialExecutor::exceed_usage_once,
        ConformanceCoreTrialExecutor::unavailable_usage_once,
    ] {
        inject(&mut executor);
        assert!(matches!(
            harness.measure(&mut executor, &validation.token, REFERENCE),
            Err(HarnessError::AttestationMismatch)
        ));
    }
}

#[test]
fn grader_side_canaries_in_every_outward_channel_are_rejected() {
    let harness = fixture();
    let mut executor = source_executor();
    let validation = harness.self_validate(&mut executor).unwrap();
    for channel in [
        kit::executor::trial::GraderTestChannel::GraderLog,
        kit::executor::trial::GraderTestChannel::CanonicalReport,
        kit::executor::trial::GraderTestChannel::Checks,
        kit::executor::trial::GraderTestChannel::FinalTree,
        kit::executor::trial::GraderTestChannel::ExtraArtifact,
    ] {
        for encoding in [
            kit::executor::trial::GraderTestEncoding::Raw,
            kit::executor::trial::GraderTestEncoding::Percent,
            kit::executor::trial::GraderTestEncoding::Base64,
            kit::executor::trial::GraderTestEncoding::Split,
            kit::executor::trial::GraderTestEncoding::Binary,
        ] {
            executor.leak_sensitive_probe_once(channel, encoding);
            assert!(
                matches!(
                    harness.measure(&mut executor, &validation.token, REFERENCE),
                    Err(HarnessError::SecretOrReportBound)
                ),
                "source protocol accepted {channel:?}/{encoding:?}"
            );
        }
    }
}

#[test]
fn offline_git_apply_differential_corpus_covers_edge_semantics() {
    let bounds = bounds();
    for (path, before, patch, after) in [
        (
            "gone",
            b"bye\n".as_slice(),
            b"--- a/gone\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n".as_slice(),
            None,
        ),
        (
            "no-newline",
            b"bye".as_slice(),
            b"--- a/no-newline\n+++ b/no-newline\n@@ -1 +1 @@\n-bye\n\\ No newline at end of file\n+hi\n\\ No newline at end of file\n".as_slice(),
            Some(b"hi".as_slice()),
        ),
        (
            "--flag",
            b"off\n".as_slice(),
            b"--- a/--flag\n+++ b/--flag\n@@ -1 +1 @@\n-off\n+on\n".as_slice(),
            Some(b"on\n".as_slice()),
        ),
    ] {
        let source = SourceSnapshot::new([(path.to_owned(), before.to_vec())], &bounds).unwrap();
        let report = grade(&source, patch, &[], &bounds).unwrap();
        let expected = SourceSnapshot::new(
            after
                .into_iter()
                .map(|bytes| (path.to_owned(), bytes.to_vec())),
            &bounds,
        )
        .unwrap();
        assert_eq!(report.outcome, GradeOutcome::Success);
        assert_eq!(report.final_tree_digest, expected.digest());
    }

    let source = SourceSnapshot::new([("file".to_owned(), b"x\n".to_vec())], &bounds).unwrap();
    let metadata =
        b"old mode 100644\nnew mode 100755\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-x\n+y\n";
    assert_eq!(
        grade(&source, metadata, &[], &bounds).unwrap().outcome,
        GradeOutcome::Error
    );
}
