#![allow(dead_code)]

use std::collections::BTreeSet;

use kit::executor::trial::{
    BoundaryCompletion, BoundaryOutcome, ExecutionRoute, ImmutableTrialManifest, TrialPhase,
    TrialUsage,
};
use kit::{domain::secret::SecretLease, telemetry::redact::CaptureRedactor};
use serde::{Deserialize, Serialize};

#[path = "../../runner/trial/mod.rs"]
pub mod trial_runner;

#[allow(unused_imports)]
pub use trial_runner::core_grader::{
    Check, GradeOutcome, GradeReport, GraderBounds, HiddenCheckAggregate, HiddenTestManifest,
    SourceSnapshot, grade, sha256, valid_sha256, validate_checks,
};
#[allow(unused_imports)]
pub use trial_runner::{
    ConformanceCoreTrialExecutor, CoreTrialError, CoreTrialExecutor, HiddenHandle,
    ProductionCoreTrialExecutor,
};

const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    pub schema_version: u16,
    pub harness_version: String,
    pub toolchain_digest: String,
    pub source_snapshot_digest: String,
    pub hidden_tests_digest: String,
    pub acceptance_rules_digest: String,
    pub gold_patch_digest: String,
    pub bounds: GraderBounds,
    pub checks: Vec<Check>,
}

pub struct HarnessInputs {
    pub trial_manifest: Vec<u8>,
    pub task_manifest: Vec<u8>,
    pub grader_manifest: Vec<u8>,
    pub source: SourceSnapshot,
    pub specification: Vec<u8>,
    pub scaffold: Vec<u8>,
    pub actual_toolchain_digest: String,
    pub hidden_tests_handle: HiddenHandle,
    pub hidden_tests: Vec<u8>,
    pub gold_patch_handle: HiddenHandle,
    pub gold_patch: Vec<u8>,
    pub acceptance_handle: HiddenHandle,
    pub acceptance_rules: Vec<u8>,
    pub harness_config_handle: HiddenHandle,
    pub harness_config: Vec<u8>,
}

pub struct CoreHarness {
    manifest: ImmutableTrialManifest,
    task_manifest_digest: String,
    grader_manifest_digest: String,
    source: SourceSnapshot,
    specification: Vec<u8>,
    scaffold: Vec<u8>,
    actual_toolchain_digest: String,
    hidden_tests_handle: HiddenHandle,
    hidden_tests: Vec<u8>,
    hidden_canaries: Vec<Vec<u8>>,
    gold_patch_handle: HiddenHandle,
    gold_patch: Vec<u8>,
    acceptance_handle: HiddenHandle,
    acceptance_rules: Vec<u8>,
    harness_config_handle: HiddenHandle,
    harness_config_bytes: Vec<u8>,
    config: HarnessConfig,
    binding_digest: String,
}

impl CoreHarness {
    pub fn load(inputs: HarnessInputs) -> Result<Self, HarnessError> {
        for (name, bytes) in [
            ("trial", inputs.trial_manifest.as_slice()),
            ("task", inputs.task_manifest.as_slice()),
            ("grader", inputs.grader_manifest.as_slice()),
        ] {
            if bytes.is_empty() || bytes.len() > MAX_MANIFEST_BYTES {
                return Err(HarnessError::BoundExceeded(name));
            }
        }
        if inputs.harness_config.is_empty() || inputs.harness_config.len() > MAX_CONFIG_BYTES {
            return Err(HarnessError::BoundExceeded("harness config"));
        }
        let manifest = ImmutableTrialManifest::from_phase0_bytes(&inputs.trial_manifest)
            .map_err(|error| HarnessError::Manifest(error.to_string()))?;
        let trial: serde_json::Value = serde_json::from_slice(&inputs.trial_manifest)
            .map_err(|error| HarnessError::Manifest(error.to_string()))?;
        let task: serde_json::Value = serde_json::from_slice(&inputs.task_manifest)
            .map_err(|error| HarnessError::Manifest(error.to_string()))?;
        let grader: serde_json::Value = serde_json::from_slice(&inputs.grader_manifest)
            .map_err(|error| HarnessError::Manifest(error.to_string()))?;
        if trial.get("task") != Some(&task) || trial.get("grader") != Some(&grader) {
            return Err(HarnessError::ManifestSubstitution);
        }
        let config: HarnessConfig = serde_json::from_slice(&inputs.harness_config)
            .map_err(|error| HarnessError::Config(error.to_string()))?;
        if config.schema_version != 2
            || config.harness_version != "m004-core-v2"
            || !valid_sha256(&config.toolchain_digest)
            || !valid_sha256(&config.source_snapshot_digest)
            || !valid_sha256(&config.hidden_tests_digest)
            || !valid_sha256(&config.acceptance_rules_digest)
            || !valid_sha256(&config.gold_patch_digest)
        {
            return Err(HarnessError::UnsupportedConfig);
        }
        config
            .bounds
            .validate()
            .map_err(|error| HarnessError::Config(error.to_string()))?;
        validate_checks(&config.checks, &config.bounds)
            .map_err(|error| HarnessError::Config(error.to_string()))?;
        if config.checks.is_empty() || config.checks.len() > config.bounds.max_checks {
            return Err(HarnessError::BoundExceeded("checks"));
        }
        let handles = [
            inputs.hidden_tests_handle.name(),
            inputs.gold_patch_handle.name(),
            inputs.acceptance_handle.name(),
            inputs.harness_config_handle.name(),
        ];
        if handles.into_iter().collect::<BTreeSet<_>>().len() != handles.len() {
            return Err(HarnessError::HiddenAuthority);
        }
        for (actual, expected, name) in [
            (
                sha256(&inputs.specification),
                manifest.specification_digest(),
                "task specification",
            ),
            (
                sha256(&inputs.scaffold),
                manifest.scaffold_digest(),
                "task scaffold",
            ),
            (
                sha256(&inputs.harness_config),
                manifest.harness_config_digest(),
                "harness config",
            ),
            (
                sha256(&inputs.hidden_tests),
                manifest.hidden_tests_digest(),
                "hidden tests",
            ),
            (
                sha256(&inputs.acceptance_rules),
                manifest.acceptance_digest(),
                "acceptance rules",
            ),
            (
                sha256(&inputs.gold_patch),
                manifest.gold_patch_digest(),
                "gold patch",
            ),
            (
                inputs.source.digest().to_owned(),
                config.source_snapshot_digest.as_str(),
                "source snapshot",
            ),
            (
                sha256(&inputs.hidden_tests),
                config.hidden_tests_digest.as_str(),
                "config hidden tests",
            ),
            (
                sha256(&inputs.acceptance_rules),
                config.acceptance_rules_digest.as_str(),
                "config acceptance rules",
            ),
            (
                sha256(&inputs.gold_patch),
                config.gold_patch_digest.as_str(),
                "config gold patch",
            ),
            (
                inputs.actual_toolchain_digest.clone(),
                config.toolchain_digest.as_str(),
                "toolchain",
            ),
        ] {
            if actual != expected {
                return Err(HarnessError::DigestMismatch(name));
            }
        }
        let hidden: HiddenTestManifest = serde_json::from_slice(&inputs.hidden_tests)
            .map_err(|error| HarnessError::Config(error.to_string()))?;
        let hidden_canaries = hidden
            .validated_canaries(&config.bounds, &config.checks)
            .map_err(|_| HarnessError::HiddenAuthority)?;
        let hidden_total = inputs
            .hidden_tests
            .len()
            .checked_add(inputs.gold_patch.len())
            .and_then(|value| value.checked_add(inputs.acceptance_rules.len()))
            .and_then(|value| value.checked_add(inputs.harness_config.len()))
            .ok_or(HarnessError::BoundExceeded("hidden material"))?;
        let input_working_set = inputs
            .trial_manifest
            .len()
            .checked_add(inputs.task_manifest.len())
            .and_then(|value| value.checked_add(inputs.grader_manifest.len()))
            .and_then(|value| value.checked_add(inputs.source.bytes()))
            .and_then(|value| value.checked_add(inputs.specification.len()))
            .and_then(|value| value.checked_add(inputs.scaffold.len()))
            .and_then(|value| value.checked_add(hidden_total))
            .ok_or(HarnessError::BoundExceeded("input memory"))?;
        if hidden_total > config.bounds.max_artifact_bytes
            || inputs.source.bytes() > config.bounds.max_source_bytes
            || inputs.gold_patch.len() > config.bounds.max_patch_bytes
            || input_working_set > config.bounds.max_memory_bytes
        {
            return Err(HarnessError::BoundExceeded("trusted inputs"));
        }
        let task_manifest_digest = canonical_value_digest(&task)?;
        let grader_manifest_digest = canonical_value_digest(&grader)?;
        let binding_digest = canonical_digest(&HarnessBinding {
            schema_version: 2,
            task_manifest_digest: &task_manifest_digest,
            grader_manifest_digest: &grader_manifest_digest,
            source_snapshot_digest: inputs.source.digest(),
            harness_config_digest: manifest.harness_config_digest(),
            toolchain_digest: &inputs.actual_toolchain_digest,
        })?;
        Ok(Self {
            manifest,
            task_manifest_digest,
            grader_manifest_digest,
            source: inputs.source,
            specification: inputs.specification,
            scaffold: inputs.scaffold,
            actual_toolchain_digest: inputs.actual_toolchain_digest,
            hidden_tests_handle: inputs.hidden_tests_handle,
            hidden_tests: inputs.hidden_tests,
            hidden_canaries,
            gold_patch_handle: inputs.gold_patch_handle,
            gold_patch: inputs.gold_patch,
            acceptance_handle: inputs.acceptance_handle,
            acceptance_rules: inputs.acceptance_rules,
            harness_config_handle: inputs.harness_config_handle,
            harness_config_bytes: inputs.harness_config,
            config,
            binding_digest,
        })
    }

    #[cfg(any(test, debug_assertions))]
    pub fn self_validate(
        &self,
        executor: &mut impl CoreTrialExecutor,
    ) -> Result<SelfValidation, HarnessError> {
        let validation = self.calibrate_verified(executor, |_| Ok(()))?;
        Ok(SelfValidation {
            token: ConformanceCalibrationToken {
                inner: validation.token,
            },
            cases: validation.cases,
        })
    }

    pub(crate) fn calibrate_verified(
        &self,
        executor: &mut impl CoreTrialExecutor,
        mut verify: impl FnMut(&ReportEnvelope) -> Result<(), HarnessError>,
    ) -> Result<HarnessSelfValidation, HarnessError> {
        let cases = self.trusted_calibration_cases();
        if cases.len() != CalibrationKind::ALL.len() {
            return Err(HarnessError::CalibrationShape);
        }
        let mut seen = BTreeSet::new();
        let mut summaries = Vec::with_capacity(cases.len());
        let mut report_digests = Vec::with_capacity(cases.len());
        for case in &cases {
            if !seen.insert(case.kind) || case.expected != case.kind.required_outcome() {
                return Err(HarnessError::CalibrationShape);
            }
            let report = self.run_patch(executor, &case.patch, None)?;
            verify(&report)?;
            let actual = report.report.outcome;
            summaries.push(CalibrationSummary {
                kind: case.kind,
                expected: case.expected,
                actual,
                report_digest: report.digest.clone(),
            });
            report_digests.push(report.digest);
        }
        if seen.len() != CalibrationKind::ALL.len()
            || summaries.iter().any(|case| case.expected != case.actual)
        {
            return Err(HarnessError::CalibrationFailed(summaries));
        }
        let digest = canonical_digest(&CalibrationBinding {
            harness_binding_digest: &self.binding_digest,
            report_digests: &report_digests,
        })?;
        Ok(HarnessSelfValidation {
            token: HarnessCalibrationToken {
                harness_binding_digest: self.binding_digest.clone(),
                digest,
            },
            cases: summaries,
        })
    }

    pub fn harness_config_digest(&self) -> &str {
        self.manifest.harness_config_digest()
    }

    pub fn task_manifest_digest(&self) -> &str {
        &self.task_manifest_digest
    }

    pub fn grader_manifest_digest(&self) -> &str {
        &self.grader_manifest_digest
    }

    pub fn agent_image_digest(&self) -> &str {
        self.manifest.agent_image_digest()
    }

    pub fn grader_image_digest(&self) -> &str {
        self.manifest.grader_image_digest()
    }

    pub fn model_digest(&self) -> &str {
        self.manifest.model_digest()
    }

    pub fn model_settings_digest(&self) -> &str {
        self.manifest.model_settings_digest()
    }

    pub fn provider_capability_digest(&self) -> &str {
        self.manifest.provider_capability_digest()
    }

    pub fn config_digest(&self) -> String {
        self.manifest.config_digest()
    }

    pub(crate) fn binding_digest(&self) -> &str {
        &self.binding_digest
    }

    fn trusted_calibration_cases(&self) -> [CalibrationCase; 5] {
        [
            CalibrationCase::required(CalibrationKind::Original, Vec::new()),
            CalibrationCase::required(CalibrationKind::Reference, self.gold_patch.clone()),
            CalibrationCase::required(CalibrationKind::Empty, Vec::new()),
            CalibrationCase::required(
                CalibrationKind::Malformed,
                include_bytes!("fixtures/malformed.patch").to_vec(),
            ),
            CalibrationCase::required(
                CalibrationKind::Adversarial,
                include_bytes!("fixtures/adversarial.patch").to_vec(),
            ),
        ]
    }

    #[cfg(any(test, debug_assertions))]
    pub fn measure(
        &self,
        executor: &mut impl CoreTrialExecutor,
        calibration: &ConformanceCalibrationToken,
        patch: &[u8],
    ) -> Result<ReportEnvelope, HarnessError> {
        if calibration.inner.harness_binding_digest != self.binding_digest
            || !valid_sha256(&calibration.inner.digest)
        {
            return Err(HarnessError::CalibrationTokenMismatch);
        }
        self.run_patch(executor, patch, None)
    }

    pub(crate) fn measure_admitted(
        &self,
        executor: &mut impl CoreTrialExecutor,
        calibration: &HarnessCalibrationToken,
        admission: &TrialAdmissionContext,
        patch: &[u8],
    ) -> Result<ReportEnvelope, HarnessError> {
        if calibration.harness_binding_digest != self.binding_digest
            || !valid_sha256(&calibration.digest)
            || !admission.valid()
            || admission.trial_id != self.manifest.trial_id()
            || admission.task_manifest_digest != self.task_manifest_digest
            || admission.model_digest != self.manifest.model_digest()
            || admission.model_settings_digest != self.manifest.model_settings_digest()
            || admission.config_digest != self.manifest.config_digest()
            || admission.provider_capability_digest != self.manifest.provider_capability_digest()
        {
            return Err(HarnessError::AdmissionMismatch);
        }
        self.run_patch(executor, patch, Some(admission))
    }

    fn run_patch(
        &self,
        executor: &mut impl CoreTrialExecutor,
        patch: &[u8],
        admission: Option<&TrialAdmissionContext>,
    ) -> Result<ReportEnvelope, HarnessError> {
        if patch.len() > self.config.bounds.max_patch_bytes {
            return Err(HarnessError::BoundExceeded("patch"));
        }
        let execution = executor
            .execute_core(trial_runner::CoreTrialRequest {
                manifest: &self.manifest,
                source: &self.source,
                patch,
                specification: &self.specification,
                scaffold: &self.scaffold,
                checks: &self.config.checks,
                bounds: &self.config.bounds,
                hidden_tests: trial_runner::HiddenMaterial {
                    handle: &self.hidden_tests_handle,
                    bytes: &self.hidden_tests,
                },
                gold_patch: trial_runner::HiddenMaterial {
                    handle: &self.gold_patch_handle,
                    bytes: &self.gold_patch,
                },
                acceptance_rules: trial_runner::HiddenMaterial {
                    handle: &self.acceptance_handle,
                    bytes: &self.acceptance_rules,
                },
                harness_config: trial_runner::HiddenMaterial {
                    handle: &self.harness_config_handle,
                    bytes: &self.harness_config_bytes,
                },
                admission: admission.map(|admission| trial_runner::CoreTrialAdmissionBinding {
                    scheduler_run_id: &admission.scheduler_run_id,
                    authority_position: admission.authority_position,
                    nonce: &admission.nonce,
                    token_digest: &admission.token_digest,
                    consumption_position: admission.scheduler_consumption_position,
                    consumption_digest: &admission.scheduler_consumption_digest,
                    run_config_digest: &admission.run_config_digest,
                }),
            })
            .map_err(HarnessError::Trial)?;
        let grader_report = serde_json::to_vec(&execution.grade)
            .map_err(|error| HarnessError::Serialization(error.to_string()))?;
        let grader_checks = serde_json::to_vec(&execution.grade.checks)
            .map_err(|error| HarnessError::Serialization(error.to_string()))?;
        scan_outward_bytes(
            &self.hidden_canaries,
            [
                execution.artifacts.applied_patch.bytes.as_slice(),
                execution.artifacts.final_tree.bytes.as_slice(),
                execution.artifacts.agent_output.bytes.as_slice(),
                execution.artifacts.events.bytes.as_slice(),
                execution.artifacts.logs.bytes.as_slice(),
                grader_report.as_slice(),
                grader_checks.as_slice(),
            ],
        )?;
        let artifacts = &execution.artifacts;
        let primary_attestation_valid = !(artifacts.applied_patch.bytes != patch
            || artifacts.applied_patch.digest != sha256(&artifacts.applied_patch.bytes)
            || artifacts.final_tree.digest != sha256(&artifacts.final_tree.bytes)
            || artifacts.agent_output.digest != sha256(&artifacts.agent_output.bytes)
            || artifacts.events.digest != sha256(&artifacts.events.bytes)
            || artifacts.logs.digest != sha256(&artifacts.logs.bytes)
            || execution.grade.patch_digest != artifacts.applied_patch.digest
            || execution.grade.base_tree_digest != self.source.digest()
            || execution.grade.final_tree_digest != artifacts.final_tree.digest
            || execution.grade.final_tree_artifact != artifacts.final_tree.bytes
            || execution.grade.schema_version != 1
            || execution.grade.timing.wall_millis > self.config.bounds.max_time_millis
            || !valid_grade_checks(&execution.grade, &self.config.checks)
            || !valid_boundaries(&execution.boundaries, &self.manifest, &self.config.bounds));
        if !primary_attestation_valid || self.manifest.validate_usage(execution.usage).is_err() {
            return Err(HarnessError::AttestationMismatch);
        }
        let artifacts_digest = canonical_digest(&ArtifactBinding {
            patch_digest: &artifacts.applied_patch.digest,
            agent_result_digest: &artifacts.agent_output.digest,
            events_digest: &artifacts.events.digest,
            logs_digest: &artifacts.logs.digest,
            final_tree_digest: &artifacts.final_tree.digest,
        })?;
        let artifact_bytes = artifacts
            .applied_patch
            .bytes
            .len()
            .checked_add(artifacts.final_tree.bytes.len())
            .and_then(|value| value.checked_add(artifacts.agent_output.bytes.len()))
            .and_then(|value| value.checked_add(artifacts.events.bytes.len()))
            .and_then(|value| value.checked_add(artifacts.logs.bytes.len()))
            .ok_or(HarnessError::BoundExceeded("artifacts"))?;
        if artifacts.artifacts_digest != artifacts_digest
            || artifact_bytes > self.config.bounds.max_artifact_bytes
            || artifacts.logs.bytes.len() > self.config.bounds.max_log_bytes
        {
            return Err(HarnessError::AttestationMismatch);
        }
        let report = CoreReport {
            schema_version: 2,
            harness_version: self.config.harness_version.clone(),
            trial_id: self.manifest.trial_id().to_owned(),
            admission_source: admission.map(|value| value.source.clone()),
            run_config_digest: admission.map(|value| value.run_config_digest.clone()),
            admission_position: admission.map(|value| value.authority_position),
            admission_nonce: admission.map(|value| value.nonce.clone()),
            admission_token_digest: admission.map(|value| value.token_digest.clone()),
            scheduler_run_id: admission.map(|value| value.scheduler_run_id.clone()),
            scheduler_consumption_position: admission
                .map(|value| value.scheduler_consumption_position),
            scheduler_consumption_digest: admission
                .map(|value| value.scheduler_consumption_digest.clone()),
            event_high_watermark: admission.map(|value| value.event_high_watermark),
            task_set_digest: admission.map(|value| value.task_set_digest.clone()),
            dataset_digest: admission.map(|value| value.dataset_digest.clone()),
            experiment_design_digest: admission.map(|value| value.experiment_design_digest.clone()),
            production_pins_digest: admission.map(|value| value.production_pins_digest.clone()),
            manifest_identity_digest: self.manifest.identity_digest().to_owned(),
            manifest_bytes_digest: self.manifest.manifest_bytes_digest().to_owned(),
            task_manifest_digest: self.task_manifest_digest.clone(),
            grader_manifest_digest: self.grader_manifest_digest.clone(),
            base_tree_digest: self.source.digest().to_owned(),
            patch_digest: artifacts.applied_patch.digest.clone(),
            final_tree_digest: execution.grade.final_tree_digest.clone(),
            grader_image_digest: self.manifest.grader_image_digest().to_owned(),
            grader_harness_commit: self.manifest.grader_harness_commit().to_owned(),
            hidden_tests_digest: self.manifest.hidden_tests_digest().to_owned(),
            acceptance_digest: self.manifest.acceptance_digest().to_owned(),
            gold_patch_digest: self.manifest.gold_patch_digest().to_owned(),
            harness_config_digest: self.manifest.harness_config_digest().to_owned(),
            toolchain_digest: self.actual_toolchain_digest.clone(),
            model_digest: self.manifest.model_digest().to_owned(),
            model_settings_digest: self.manifest.model_settings_digest().to_owned(),
            config_digest: self.manifest.config_digest(),
            provider_capability_digest: self.manifest.provider_capability_digest().to_owned(),
            agent: NormalizedBoundary::from(&execution.boundaries.agent),
            grader: NormalizedBoundary::from(&execution.boundaries.grader),
            agent_result_digest: artifacts.agent_output.digest.clone(),
            events_digest: artifacts.events.digest.clone(),
            logs_digest: artifacts.logs.digest.clone(),
            artifacts_digest: artifacts.artifacts_digest.clone(),
            usage: execution.usage,
            outcome: execution.grade.outcome,
            grade: CanonicalGradeReport::from(&execution.grade),
        };
        let bytes = serde_json::to_vec(
            &serde_json::to_value(&report)
                .map_err(|error| HarnessError::Serialization(error.to_string()))?,
        )
        .map_err(|error| HarnessError::Serialization(error.to_string()))?;
        if bytes.len() > 64 * 1024 {
            return Err(HarnessError::SecretOrReportBound);
        }
        scan_outward_bytes(&self.hidden_canaries, [bytes.as_slice()])?;
        Ok(ReportEnvelope {
            digest: sha256(&bytes),
            bytes,
            report,
            artifacts: execution.artifacts,
            volatile: VolatileExecution {
                agent_boundary_id: execution.boundaries.agent.boundary_id,
                agent_instance_id: execution.boundaries.agent.instance_id,
                agent_plan_digest: execution.boundaries.agent.plan_digest,
                agent_invocation_digest: execution.boundaries.agent.invocation_digest,
                grader_boundary_id: execution.boundaries.grader.boundary_id,
                grader_instance_id: execution.boundaries.grader.instance_id,
                grader_plan_digest: execution.boundaries.grader.plan_digest,
                grader_invocation_digest: execution.boundaries.grader.invocation_digest,
                grader_timing: execution.grade.timing,
                provider_request_ids: execution.provider_request_ids,
            },
        })
    }

    pub(crate) fn bind_trusted_events(
        &self,
        mut envelope: ReportEnvelope,
        events: &[u8],
        event_high_watermark: u64,
    ) -> Result<ReportEnvelope, HarnessError> {
        scan_outward_bytes(&self.hidden_canaries, [events])?;
        envelope.report.event_high_watermark = Some(event_high_watermark);
        envelope.report.events_digest = sha256(events);
        envelope.bytes = serde_json::to_vec(
            &serde_json::to_value(&envelope.report)
                .map_err(|error| HarnessError::Serialization(error.to_string()))?,
        )
        .map_err(|error| HarnessError::Serialization(error.to_string()))?;
        if envelope.bytes.len() > 64 * 1024 {
            return Err(HarnessError::SecretOrReportBound);
        }
        scan_outward_bytes(&self.hidden_canaries, [envelope.bytes.as_slice()])?;
        envelope.digest = sha256(&envelope.bytes);
        Ok(envelope)
    }
}

fn scan_outward_bytes<'a>(
    canaries: &[Vec<u8>],
    artifacts: impl IntoIterator<Item = &'a [u8]>,
) -> Result<(), HarnessError> {
    let leases = canaries
        .iter()
        .map(|canary| SecretLease::new(canary.clone()))
        .collect::<Vec<_>>();
    let mut scanner = CaptureRedactor::new(&leases).scanner();
    for bytes in artifacts {
        for chunk in bytes.chunks(4096) {
            scanner.push(chunk);
            if scanner.found() {
                return Err(HarnessError::SecretOrReportBound);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationKind {
    Original,
    Reference,
    Empty,
    Malformed,
    Adversarial,
}

impl CalibrationKind {
    pub const ALL: [Self; 5] = [
        Self::Original,
        Self::Reference,
        Self::Empty,
        Self::Malformed,
        Self::Adversarial,
    ];

    const fn required_outcome(self) -> GradeOutcome {
        match self {
            Self::Reference => GradeOutcome::Success,
            Self::Malformed => GradeOutcome::Error,
            Self::Original | Self::Empty | Self::Adversarial => GradeOutcome::Failure,
        }
    }
}

struct CalibrationCase {
    kind: CalibrationKind,
    patch: Vec<u8>,
    expected: GradeOutcome,
}

impl CalibrationCase {
    fn required(kind: CalibrationKind, patch: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            patch: patch.into(),
            expected: kind.required_outcome(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CalibrationSummary {
    pub kind: CalibrationKind,
    pub expected: GradeOutcome,
    pub actual: GradeOutcome,
    pub report_digest: String,
}

#[cfg(any(test, debug_assertions))]
pub struct SelfValidation {
    pub token: ConformanceCalibrationToken,
    pub cases: Vec<CalibrationSummary>,
}

#[cfg(any(test, debug_assertions))]
pub struct ConformanceCalibrationToken {
    pub(crate) inner: HarnessCalibrationToken,
}

pub(crate) struct HarnessSelfValidation {
    pub token: HarnessCalibrationToken,
    pub cases: Vec<CalibrationSummary>,
}

pub(crate) struct HarnessCalibrationToken {
    harness_binding_digest: String,
    digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NormalizedBoundary {
    pub phase: TrialPhase,
    pub route: ExecutionRoute,
    pub image_digest: String,
    pub runtime_identity: String,
    pub helper_identity: String,
    pub permitted_profile_digest: String,
    pub survivor_processes: u32,
    pub quiescent: bool,
    pub outcome: BoundaryOutcome,
}

impl From<&BoundaryCompletion> for NormalizedBoundary {
    fn from(boundary: &BoundaryCompletion) -> Self {
        Self {
            phase: boundary.phase,
            route: boundary.route,
            image_digest: boundary.image_digest.clone(),
            runtime_identity: boundary.runtime_identity.clone(),
            helper_identity: boundary.helper_identity.clone(),
            permitted_profile_digest: boundary.permitted_profile_digest.clone(),
            survivor_processes: boundary.survivor_processes,
            quiescent: boundary.quiescent,
            outcome: boundary.outcome,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CoreReport {
    pub schema_version: u16,
    pub harness_version: String,
    pub trial_id: String,
    pub admission_source: Option<String>,
    pub run_config_digest: Option<String>,
    pub admission_position: Option<u64>,
    pub admission_nonce: Option<String>,
    pub admission_token_digest: Option<String>,
    pub scheduler_run_id: Option<String>,
    pub scheduler_consumption_position: Option<u64>,
    pub scheduler_consumption_digest: Option<String>,
    pub event_high_watermark: Option<u64>,
    pub task_set_digest: Option<String>,
    pub dataset_digest: Option<String>,
    pub experiment_design_digest: Option<String>,
    pub production_pins_digest: Option<String>,
    pub manifest_identity_digest: String,
    pub manifest_bytes_digest: String,
    pub task_manifest_digest: String,
    pub grader_manifest_digest: String,
    pub base_tree_digest: String,
    pub patch_digest: String,
    pub final_tree_digest: String,
    pub grader_image_digest: String,
    pub grader_harness_commit: String,
    pub hidden_tests_digest: String,
    pub acceptance_digest: String,
    pub gold_patch_digest: String,
    pub harness_config_digest: String,
    pub toolchain_digest: String,
    pub model_digest: String,
    pub model_settings_digest: String,
    pub config_digest: String,
    pub provider_capability_digest: String,
    pub agent: NormalizedBoundary,
    pub grader: NormalizedBoundary,
    pub agent_result_digest: String,
    pub events_digest: String,
    pub logs_digest: String,
    pub artifacts_digest: String,
    pub usage: TrialUsage,
    pub outcome: GradeOutcome,
    pub grade: CanonicalGradeReport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TrialAdmissionContext {
    pub source: String,
    pub authority_id: String,
    pub authority_position: u64,
    pub registration_sequence: u64,
    pub preregistration_digest: String,
    pub task_set_digest: String,
    pub dataset_digest: String,
    pub experiment_design_digest: String,
    pub production_pins_digest: String,
    pub roster_index: usize,
    pub trial_id: String,
    pub pair_id: String,
    pub task_id: String,
    pub dataset_member_id: String,
    pub task_manifest_digest: String,
    pub model_digest: String,
    pub model_settings_digest: String,
    pub config_digest: String,
    pub provider_capability_digest: String,
    pub seed: u64,
    pub arm: String,
    pub nonce: String,
    pub token_digest: String,
    pub scheduler_run_id: String,
    pub scheduler_consumption_position: u64,
    pub scheduler_consumption_digest: String,
    pub run_config_digest: String,
    pub event_high_watermark: u64,
}

impl TrialAdmissionContext {
    fn valid(&self) -> bool {
        self.authority_position > 0
            && self.registration_sequence > 0
            && !self.authority_id.is_empty()
            && matches!(
                self.source.as_str(),
                "production_authenticated" | "conformance_source_semantics_fake"
            )
            && !self.trial_id.is_empty()
            && !self.pair_id.is_empty()
            && !self.task_id.is_empty()
            && !self.dataset_member_id.is_empty()
            && !self.nonce.is_empty()
            && !self.scheduler_run_id.is_empty()
            && self.scheduler_consumption_position > 0
            && matches!(self.arm.as_str(), "baseline" | "candidate")
            && [
                &self.preregistration_digest,
                &self.task_set_digest,
                &self.dataset_digest,
                &self.experiment_design_digest,
                &self.production_pins_digest,
                &self.task_manifest_digest,
                &self.model_digest,
                &self.model_settings_digest,
                &self.config_digest,
                &self.provider_capability_digest,
                &self.token_digest,
                &self.scheduler_consumption_digest,
                &self.run_config_digest,
            ]
            .into_iter()
            .all(|digest| valid_sha256(digest))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CanonicalCheckEvidence {
    pub id: String,
    pub passed: bool,
    pub path: String,
    pub expected: String,
    pub actual: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CanonicalGradeReport {
    pub schema_version: u16,
    pub outcome: GradeOutcome,
    pub base_tree_digest: String,
    pub patch_digest: String,
    pub final_tree_digest: String,
    pub checks: Vec<CanonicalCheckEvidence>,
    pub hidden: HiddenCheckAggregate,
    pub diagnostic: Option<String>,
}

impl From<&GradeReport> for CanonicalGradeReport {
    fn from(report: &GradeReport) -> Self {
        Self {
            schema_version: report.schema_version,
            outcome: report.outcome,
            base_tree_digest: report.base_tree_digest.clone(),
            patch_digest: report.patch_digest.clone(),
            final_tree_digest: report.final_tree_digest.clone(),
            checks: report
                .checks
                .iter()
                .map(|check| CanonicalCheckEvidence {
                    id: check.id.clone(),
                    passed: check.passed,
                    path: check.path.clone(),
                    expected: check.expected.clone(),
                    actual: check.actual.clone(),
                })
                .collect(),
            hidden: report.hidden.clone(),
            diagnostic: report.diagnostic.clone(),
        }
    }
}

pub struct ReportEnvelope {
    pub bytes: Vec<u8>,
    pub digest: String,
    pub report: CoreReport,
    pub artifacts: trial_runner::CoreTrialArtifacts,
    pub volatile: VolatileExecution,
}

pub struct VolatileExecution {
    pub agent_boundary_id: String,
    pub agent_instance_id: String,
    pub agent_plan_digest: String,
    pub agent_invocation_digest: String,
    pub grader_boundary_id: String,
    pub grader_instance_id: String,
    pub grader_plan_digest: String,
    pub grader_invocation_digest: String,
    pub grader_timing: trial_runner::core_grader::GradeTiming,
    pub provider_request_ids: Vec<String>,
}

#[derive(Serialize)]
struct HarnessBinding<'a> {
    schema_version: u16,
    task_manifest_digest: &'a str,
    grader_manifest_digest: &'a str,
    source_snapshot_digest: &'a str,
    harness_config_digest: &'a str,
    toolchain_digest: &'a str,
}

#[derive(Serialize)]
struct CalibrationBinding<'a> {
    harness_binding_digest: &'a str,
    report_digests: &'a [String],
}

#[derive(Serialize)]
struct ArtifactBinding<'a> {
    patch_digest: &'a str,
    agent_result_digest: &'a str,
    events_digest: &'a str,
    logs_digest: &'a str,
    final_tree_digest: &'a str,
}

fn valid_grade_checks(report: &GradeReport, checks: &[Check]) -> bool {
    if report.diagnostic.is_some() {
        return report.outcome != GradeOutcome::Success && report.checks.is_empty();
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct TreeEntry {
        path: String,
        bytes: Vec<u8>,
    }
    let Ok(entries) = serde_json::from_slice::<Vec<TreeEntry>>(&report.final_tree_artifact) else {
        return false;
    };
    if serde_json::to_vec(&entries).ok().as_deref() != Some(report.final_tree_artifact.as_slice())
        || entries.windows(2).any(|pair| pair[0].path >= pair[1].path)
    {
        return false;
    }
    let mut files = std::collections::BTreeMap::new();
    if entries
        .into_iter()
        .any(|entry| files.insert(entry.path, entry.bytes).is_some())
    {
        return false;
    }
    report.checks.len() == checks.len()
        && report.checks.iter().zip(checks).all(|(evidence, check)| {
            let (id, path, expected, actual) = match check {
                Check::Digest { id, path, sha256 } => (
                    id,
                    path,
                    sha256.clone(),
                    files
                        .get(path)
                        .map_or_else(|| "missing".to_owned(), |bytes| self::sha256(bytes)),
                ),
                Check::Contains { id, path, text } => (
                    id,
                    path,
                    "present".to_owned(),
                    files
                        .get(path)
                        .map_or("missing", |bytes| {
                            if bytes
                                .windows(text.len())
                                .any(|window| window == text.as_bytes())
                            {
                                "present"
                            } else {
                                "absent"
                            }
                        })
                        .to_owned(),
                ),
                Check::Absent { id, path } => (
                    id,
                    path,
                    "absent".to_owned(),
                    if files.contains_key(path) {
                        "present"
                    } else {
                        "absent"
                    }
                    .to_owned(),
                ),
            };
            evidence.id == *id
                && evidence.path == *path
                && evidence.expected == expected
                && evidence.actual == actual
                && evidence.passed == (evidence.actual == evidence.expected)
        })
        && ((report.outcome == GradeOutcome::Success)
            == (report.checks.iter().all(|check| check.passed)
                && report.hidden.verdict == GradeOutcome::Success))
}

fn valid_boundaries(
    boundaries: &kit::executor::trial::BoundaryPair,
    manifest: &ImmutableTrialManifest,
    bounds: &GraderBounds,
) -> bool {
    let Ok(profile) = manifest.profile() else {
        return false;
    };
    let grader_profile_digest = if boundaries.grader.route == ExecutionRoute::ConformanceFake {
        profile.digest().to_string()
    } else {
        let Ok(profile) = kit::executor::trial::constrained_grader_profile(
            manifest,
            kit::executor::trial::GraderResourceBounds {
                memory_bytes: bounds.max_memory_bytes as u64,
                output_bytes: bounds.max_artifact_bytes as u64,
                wall_time_millis: bounds.max_time_millis,
            },
        ) else {
            return false;
        };
        profile.digest().to_string()
    };
    boundaries.agent.phase == TrialPhase::Agent
        && boundaries.grader.phase == TrialPhase::Grader
        && boundaries.agent.route == boundaries.grader.route
        && boundaries.agent.image_digest == manifest.agent_image_digest()
        && boundaries.grader.image_digest == manifest.grader_image_digest()
        && boundaries.agent.runtime_identity == boundaries.grader.runtime_identity
        && boundaries.agent.helper_identity == boundaries.grader.helper_identity
        && boundaries.agent.permitted_profile_digest == profile.digest().to_string()
        && boundaries.grader.permitted_profile_digest == grader_profile_digest
        && boundaries.agent.survivor_processes == 0
        && boundaries.grader.survivor_processes == 0
        && boundaries.agent.quiescent
        && boundaries.grader.quiescent
        && boundaries.agent.outcome == BoundaryOutcome::Success
        && boundaries.grader.outcome == BoundaryOutcome::Success
}

fn canonical_value_digest(value: &serde_json::Value) -> Result<String, HarnessError> {
    canonical_digest(value)
}

fn canonical_digest(value: &impl Serialize) -> Result<String, HarnessError> {
    serde_json::to_vec(value)
        .map(|bytes| sha256(&bytes))
        .map_err(|error| HarnessError::Serialization(error.to_string()))
}

#[derive(Debug)]
pub enum HarnessError {
    Manifest(String),
    Config(String),
    UnsupportedConfig,
    ManifestSubstitution,
    DigestMismatch(&'static str),
    BoundExceeded(&'static str),
    HiddenAuthority,
    CalibrationShape,
    CalibrationFailed(Vec<CalibrationSummary>),
    CalibrationTokenMismatch,
    AdmissionMismatch,
    Trial(CoreTrialError),
    AttestationMismatch,
    SecretOrReportBound,
    Serialization(String),
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(detail) => write!(formatter, "invalid Phase-0 manifest: {detail}"),
            Self::Config(detail) => write!(formatter, "invalid harness config: {detail}"),
            Self::UnsupportedConfig => formatter.write_str("unsupported harness configuration"),
            Self::ManifestSubstitution => {
                formatter.write_str("task or grader manifest substituted")
            }
            Self::DigestMismatch(name) => write!(formatter, "{name} digest mismatch"),
            Self::BoundExceeded(name) => write!(formatter, "{name} bound exceeded"),
            Self::HiddenAuthority => formatter.write_str("hidden material authority is invalid"),
            Self::CalibrationShape => formatter.write_str("invalid five-case calibration suite"),
            Self::CalibrationFailed(_) => formatter.write_str("harness self-validation failed"),
            Self::CalibrationTokenMismatch => formatter.write_str("calibration token mismatch"),
            Self::AdmissionMismatch => formatter.write_str("trial admission binding mismatch"),
            Self::Trial(error) => write!(formatter, "trial failed: {error}"),
            Self::AttestationMismatch => formatter.write_str("trial attestation mismatch"),
            Self::SecretOrReportBound => {
                formatter.write_str("report exceeded bounds or contained hidden material")
            }
            Self::Serialization(detail) => write!(formatter, "serialization failed: {detail}"),
        }
    }
}

impl std::error::Error for HarnessError {}
