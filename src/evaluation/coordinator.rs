use crate::{
    domain::ids::RunId,
    executor::trial::TrialUsage,
    runtime::scheduler::{DurableScheduler, TrialRunBinding},
};

use super::{
    harness::{
        CoreHarness, CoreTrialExecutor, HarnessCalibrationToken, ReportEnvelope,
        TrialAdmissionContext,
    },
    reports::{
        Arm, HarnessExecutionReceipt, RegisteredPreregistration, RegistrationAuthority, StatsError,
        TerminalDurableUsage, TerminalErrorEvidence, TrialAdmission, TrialRunConfig,
    },
    sqlite::{CoordinatorPhase, SqliteCoordinatorOperationStore},
};

pub trait ProviderEvidenceStore: Send + Sync {
    fn verify(
        &self,
        run_id: RunId,
        config: &TrialRunConfig,
        provider_request_ids: &[String],
        event_high_watermark: u64,
    ) -> Result<(), CoordinatorError>;
}

pub trait ToolEvidenceStore: Send + Sync {
    fn verify(
        &self,
        run_id: RunId,
        config: &TrialRunConfig,
        event_high_watermark: u64,
    ) -> Result<(), CoordinatorError>;
}

pub trait UsageEvidenceStore: Send + Sync {
    fn verify(
        &self,
        run_id: RunId,
        config: &TrialRunConfig,
        usage: TrialUsage,
        event_high_watermark: u64,
    ) -> Result<(), CoordinatorError>;
}

pub struct PreparedEventEvidence {
    pub source: String,
    pub event_start_watermark: u64,
}

pub trait EventEvidenceStore: Send + Sync {
    fn source(&self) -> &'static str;

    fn capture_start(
        &self,
        run_id: RunId,
        pending: &crate::runtime::scheduler::PendingStatisticalTrial,
    ) -> Result<PreparedEventEvidence, CoordinatorError>;

    fn finalize_terminal(
        &self,
        scheduler: &DurableScheduler,
        run_id: RunId,
        config: &TrialRunConfig,
    ) -> Result<u64, CoordinatorError>;

    fn trusted_events(
        &self,
        run_id: RunId,
        config: &TrialRunConfig,
        event_high_watermark: u64,
    ) -> Result<Vec<u8>, CoordinatorError>;
}

#[cfg(any(test, debug_assertions))]
pub struct StatisticalTrialRequest<'a> {
    pub run_id: RunId,
    pub binding: &'a TrialRunBinding,
    pub registered: &'a RegisteredPreregistration,
    pub admission: &'a TrialAdmission,
    pub calibration: &'a super::harness::ConformanceCalibrationToken,
    pub patch: &'a [u8],
}

pub struct ProductionStatisticalTrialRequest<'a> {
    pub run_id: RunId,
    pub binding: &'a TrialRunBinding,
    pub registered: &'a RegisteredPreregistration,
    pub admission: &'a TrialAdmission,
    pub patch: &'a [u8],
}

struct TrialRequest<'a> {
    run_id: RunId,
    binding: &'a TrialRunBinding,
    registered: &'a RegisteredPreregistration,
    admission: &'a TrialAdmission,
    calibration: &'a HarnessCalibrationToken,
    patch: &'a [u8],
}

pub struct StatisticalTrialCoordinator<'a> {
    authority: &'a mut RegistrationAuthority,
    scheduler: &'a DurableScheduler,
    harness: &'a CoreHarness,
    provider_store: &'a dyn ProviderEvidenceStore,
    tool_store: &'a dyn ToolEvidenceStore,
    event_store: &'a dyn EventEvidenceStore,
    usage_store: &'a dyn UsageEvidenceStore,
    operations: Option<&'a SqliteCoordinatorOperationStore>,
}

impl<'a> StatisticalTrialCoordinator<'a> {
    pub fn new(
        authority: &'a mut RegistrationAuthority,
        scheduler: &'a DurableScheduler,
        harness: &'a CoreHarness,
        provider_store: &'a dyn ProviderEvidenceStore,
        tool_store: &'a dyn ToolEvidenceStore,
        event_store: &'a dyn EventEvidenceStore,
        usage_store: &'a dyn UsageEvidenceStore,
    ) -> Self {
        Self {
            authority,
            scheduler,
            harness,
            provider_store,
            tool_store,
            event_store,
            usage_store,
            operations: None,
        }
    }

    pub fn with_operations(mut self, operations: &'a SqliteCoordinatorOperationStore) -> Self {
        self.operations = Some(operations);
        self
    }

    #[cfg(any(test, debug_assertions))]
    pub fn run(
        &mut self,
        executor: &mut impl CoreTrialExecutor,
        request: StatisticalTrialRequest<'_>,
    ) -> Result<HarnessExecutionReceipt, CoordinatorError> {
        self.run_inner(
            executor,
            TrialRequest {
                run_id: request.run_id,
                binding: request.binding,
                registered: request.registered,
                admission: request.admission,
                calibration: &request.calibration.inner,
                patch: request.patch,
            },
            crate::evaluation::reports::EvidenceSource::ConformanceSourceSemantics,
            |_| Ok(()),
        )
    }

    pub(crate) fn run_production_verified(
        &mut self,
        executor: &mut impl CoreTrialExecutor,
        request: ProductionStatisticalTrialRequest<'_>,
        calibration: &HarnessCalibrationToken,
        verify_report: impl Fn(&ReportEnvelope) -> Result<(), CoordinatorError>,
    ) -> Result<HarnessExecutionReceipt, CoordinatorError> {
        self.run_inner(
            executor,
            TrialRequest {
                run_id: request.run_id,
                binding: request.binding,
                registered: request.registered,
                admission: request.admission,
                calibration,
                patch: request.patch,
            },
            crate::evaluation::reports::EvidenceSource::ProductionTrusted,
            verify_report,
        )
    }

    fn run_inner(
        &mut self,
        executor: &mut impl CoreTrialExecutor,
        request: TrialRequest<'_>,
        evidence_source: crate::evaluation::reports::EvidenceSource,
        verify_report: impl Fn(&ReportEnvelope) -> Result<(), CoordinatorError>,
    ) -> Result<HarnessExecutionReceipt, CoordinatorError> {
        let token = request
            .binding
            .admission
            .as_ref()
            .ok_or(CoordinatorError::Binding)?;
        if &request.admission.scheduler_token()? != token {
            return Err(CoordinatorError::Binding);
        }
        let roster = request
            .registered
            .preregistration
            .roster
            .get(token.schedule_index)
            .ok_or(CoordinatorError::Binding)?;
        if request.binding.trial_id != roster.trial_id
            || normalized(&request.binding.trial_digest)
                != normalized(&request.registered.preregistration_digest)
            || normalized(&request.binding.task_digest) != normalized(&roster.task_manifest_digest)
            || normalized(&request.binding.model_digest) != normalized(&roster.model_digest)
            || normalized(&request.binding.config_digest) != normalized(&roster.config_digest)
            || self.harness.model_digest() != roster.model_digest
            || self.harness.model_settings_digest() != roster.model_settings_digest
            || self.harness.config_digest() != roster.config_digest
            || self.harness.provider_capability_digest() != roster.provider_capability_digest
        {
            return Err(CoordinatorError::Binding);
        }
        let pending = self.scheduler.admit_statistical_trial_run(
            request.run_id,
            request.binding,
            self.authority,
        )?;
        let operation = self
            .operations
            .map(|operations| operations.load(request.run_id))
            .transpose()?
            .flatten();
        let authoritative = if operation.is_none() {
            self.authority.load_scheduler_admission_consumption(
                request.registered,
                request.admission,
                &pending,
            )?
        } else {
            None
        };
        let prepared = match &operation {
            Some(operation) => PreparedEventEvidence {
                source: operation.event_source.clone(),
                event_start_watermark: operation.config.event_start_watermark,
            },
            None => match &authoritative {
                Some((config, _)) => PreparedEventEvidence {
                    source: evidence_source.as_wire().to_owned(),
                    event_start_watermark: config.event_start_watermark,
                },
                None => {
                    let mut prepared = self.event_store.capture_start(request.run_id, &pending)?;
                    prepared.source = evidence_source.as_wire().to_owned();
                    prepared
                }
            },
        };
        if prepared.source != evidence_source.as_wire() {
            return Err(CoordinatorError::Evidence("evidence source mismatch"));
        }
        let (config, consumption_receipt) = match &operation {
            Some(operation) => (
                operation.config.clone(),
                operation.consumption_receipt.clone(),
            ),
            None => match authoritative {
                Some(consumption) => consumption,
                None => self
                    .authority
                    .consume_scheduler_admission_with_event_start(
                        request.registered,
                        request.admission,
                        &pending,
                        prepared.event_start_watermark,
                    )?,
            },
        };
        if config.scheduler_run_id != request.run_id.to_string()
            || config.admission_token_digest != pending.admission_token_digest
            || config.admission_nonce != pending.admission_nonce
            || config.scheduler_consumption_position != pending.consumption_position
            || config.scheduler_consumption_digest != pending.consumption_digest
        {
            return Err(CoordinatorError::Binding);
        }
        self.scheduler.finalize_statistical_trial_anchor(
            &pending,
            &consumption_receipt,
            self.authority,
        )?;
        if let Some(operations) = self.operations {
            operations.admitted(
                request.run_id,
                &config,
                &consumption_receipt,
                &prepared.source,
            )?;
        }
        if let Some(operation) = operation {
            match operation.phase {
                CoordinatorPhase::Recorded | CoordinatorPhase::TerminalError => {
                    return self
                        .authority
                        .load_harness_trial(request.registered, &config)?
                        .ok_or(CoordinatorError::Evidence(
                            "recorded execution receipt is missing",
                        ));
                }
                CoordinatorPhase::EvidenceReady => {
                    let receipt = self.authority.record_harness_trial(
                        request.registered,
                        &config,
                        operation
                            .harness_bytes
                            .ok_or(CoordinatorError::Evidence("harness artifact is missing"))?,
                        operation
                            .events_bytes
                            .ok_or(CoordinatorError::Evidence("events artifact is missing"))?,
                    )?;
                    self.operations
                        .expect("durable operation was loaded")
                        .recorded(request.run_id, &receipt)?;
                    self.finish(request.run_id, request.registered)?;
                    return Ok(receipt);
                }
                CoordinatorPhase::Terminalizing => {
                    return self.complete_terminal_error(
                        request.run_id,
                        request.registered,
                        &config,
                        evidence_source,
                        operation
                            .terminal_evidence
                            .ok_or(CoordinatorError::Evidence(
                                "terminalizing evidence is missing",
                            ))?,
                    );
                }
                CoordinatorPhase::Executing => {
                    return self.terminal_error(
                        request.run_id,
                        request.registered,
                        &config,
                        evidence_source,
                        "execution outcome unknown after coordinator interruption",
                        None,
                    );
                }
                CoordinatorPhase::Admitted => {}
            }
        }
        if prepared.event_start_watermark != config.event_start_watermark {
            return Err(CoordinatorError::Evidence("event start watermark mismatch"));
        }
        let context = TrialAdmissionContext {
            source: prepared.source,
            authority_id: config.authority_id.clone(),
            authority_position: config.admission_position,
            registration_sequence: config.registration_sequence,
            preregistration_digest: config.preregistration_digest.clone(),
            task_set_digest: request.registered.preregistration.digests.task_set.clone(),
            dataset_digest: request.registered.preregistration.digests.dataset.clone(),
            experiment_design_digest: request
                .registered
                .preregistration
                .digests
                .experiment
                .clone(),
            production_pins_digest: request
                .registered
                .preregistration
                .execution_environment
                .digest
                .clone(),
            roster_index: config.schedule_index,
            trial_id: config.trial_id.clone(),
            pair_id: config.pair_id.clone(),
            task_id: config.task_id.clone(),
            dataset_member_id: config.dataset_member_id.clone(),
            task_manifest_digest: config.task_manifest_digest.clone(),
            model_digest: config.model_digest.clone(),
            model_settings_digest: config.model_settings_digest.clone(),
            config_digest: config.config_digest.clone(),
            provider_capability_digest: config.provider_capability_digest.clone(),
            seed: config.seed,
            arm: match config.arm {
                Arm::Baseline => "baseline",
                Arm::Candidate => "candidate",
            }
            .to_owned(),
            nonce: config.admission_nonce.clone(),
            token_digest: config.admission_token_digest.clone(),
            scheduler_run_id: config.scheduler_run_id.clone(),
            scheduler_consumption_position: config.scheduler_consumption_position,
            scheduler_consumption_digest: config.scheduler_consumption_digest.clone(),
            run_config_digest: config.immutable_digest.clone(),
            event_high_watermark: prepared.event_start_watermark,
        };
        if let Some(operations) = self.operations {
            operations.executing(request.run_id)?;
        }
        let execution_started = std::time::Instant::now();
        let outcome: Result<HarnessExecutionReceipt, CoordinatorError> = (|| {
            let report = self.harness.measure_admitted(
                executor,
                request.calibration,
                &context,
                request.patch,
            )?;
            verify_report(&report)?;
            let event_high_watermark =
                self.event_store
                    .finalize_terminal(self.scheduler, request.run_id, &config)?;
            if event_high_watermark <= config.event_start_watermark {
                return Err(CoordinatorError::Evidence(
                    "invalid terminal event watermark",
                ));
            }
            self.provider_store.verify(
                request.run_id,
                &config,
                &report.volatile.provider_request_ids,
                event_high_watermark,
            )?;
            self.tool_store
                .verify(request.run_id, &config, event_high_watermark)?;
            self.usage_store.verify(
                request.run_id,
                &config,
                report.report.usage,
                event_high_watermark,
            )?;
            let mut events =
                self.event_store
                    .trusted_events(request.run_id, &config, event_high_watermark)?;
            let mut event_value: serde_json::Value = serde_json::from_slice(&events)
                .map_err(|_| CoordinatorError::Evidence("invalid trusted event evidence"))?;
            if event_value
                .get("source")
                .and_then(serde_json::Value::as_str)
                != Some(evidence_source.as_wire())
            {
                if evidence_source == crate::evaluation::reports::EvidenceSource::ProductionTrusted
                {
                    return Err(CoordinatorError::Evidence("evidence source mismatch"));
                }
                event_value["source"] = evidence_source.as_wire().into();
                events = serde_json::to_vec(&event_value)
                    .map_err(|_| CoordinatorError::Evidence("invalid trusted event evidence"))?;
            }
            let report = self
                .harness
                .bind_trusted_events(report, &events, event_high_watermark)?;
            if let Some(operations) = self.operations {
                operations.evidence_ready(request.run_id, &report.bytes, &events)?;
            }
            let receipt = self.authority.record_harness_trial(
                request.registered,
                &config,
                report.bytes,
                events,
            )?;
            if let Some(operations) = self.operations {
                operations.recorded(request.run_id, &receipt)?;
            }
            self.finish(request.run_id, request.registered)?;
            Ok(receipt)
        })();
        match (outcome, self.operations) {
            (Ok(receipt), _) => Ok(receipt),
            (Err(error), Some(_)) => self.terminal_error(
                request.run_id,
                request.registered,
                &config,
                evidence_source,
                &error.to_string(),
                Some(execution_started.elapsed()),
            ),
            (Err(error), None) => Err(error),
        }
    }

    fn terminal_error(
        &mut self,
        run_id: RunId,
        registered: &RegisteredPreregistration,
        config: &TrialRunConfig,
        evidence_source: crate::evaluation::reports::EvidenceSource,
        reason: &str,
        elapsed: Option<std::time::Duration>,
    ) -> Result<HarnessExecutionReceipt, CoordinatorError> {
        let end = (0..=reason.len())
            .rev()
            .find(|end| *end <= 4096 && reason.is_char_boundary(*end))
            .unwrap_or(0);
        let reason = reason[..end].to_owned();
        let policy = registered.preregistration.policies.error_imputation;
        let spend = self.scheduler.totals(run_id)?.committed;
        let durable_usage = TerminalDurableUsage {
            cost_microusd: spend.cost_microusd(),
            tokens: spend.tokens(),
            turns: spend.turns(),
            tool_calls: spend.tools(),
            processes: spend.processes(),
        };
        let elapsed_millis = elapsed
            .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(1)
            .max(1);
        let evidence = TerminalErrorEvidence {
            reason,
            elapsed_millis,
            durable_usage: Some(durable_usage.clone()),
            cost_microusd: (policy.max_cost_usd * 1_000_000.0) as u64,
            cost_imputed: true,
            latency_millis: policy.max_latency_ms,
            latency_imputed: true,
        };
        self.operations
            .expect("terminal errors require durable coordinator operations")
            .terminalizing(run_id, &evidence)?;
        self.complete_terminal_error(run_id, registered, config, evidence_source, evidence)
    }

    fn complete_terminal_error(
        &mut self,
        run_id: RunId,
        registered: &RegisteredPreregistration,
        config: &TrialRunConfig,
        evidence_source: crate::evaluation::reports::EvidenceSource,
        evidence: TerminalErrorEvidence,
    ) -> Result<HarnessExecutionReceipt, CoordinatorError> {
        let receipt =
            self.authority
                .record_terminal_error(registered, config, evidence_source, &evidence)?;
        self.scheduler.finish_run(run_id, false)?;
        self.operations
            .expect("terminal errors require durable coordinator operations")
            .terminal_error(run_id, &evidence.reason, &receipt)?;
        self.finish(run_id, registered)?;
        Ok(receipt)
    }

    fn finish(
        &mut self,
        run_id: RunId,
        registered: &RegisteredPreregistration,
    ) -> Result<(), CoordinatorError> {
        self.scheduler.finish_run(run_id, false)?;
        match self.authority.freeze_experiment(registered) {
            Ok(_) | Err(StatsError::ExperimentNotTerminal) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn normalized(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

#[derive(Debug)]
pub enum CoordinatorError {
    Binding,
    Evidence(&'static str),
    Scheduler(crate::runtime::scheduler::SchedulerError),
    Harness(super::harness::HarnessError),
    Statistics(StatsError),
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Binding => formatter.write_str("statistical trial binding mismatch"),
            Self::Evidence(detail) => write!(formatter, "trusted trial evidence failed: {detail}"),
            Self::Scheduler(error) => error.fmt(formatter),
            Self::Harness(error) => error.fmt(formatter),
            Self::Statistics(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CoordinatorError {}

impl From<crate::runtime::scheduler::SchedulerError> for CoordinatorError {
    fn from(error: crate::runtime::scheduler::SchedulerError) -> Self {
        Self::Scheduler(error)
    }
}

impl From<super::harness::HarnessError> for CoordinatorError {
    fn from(error: super::harness::HarnessError) -> Self {
        Self::Harness(error)
    }
}

impl From<StatsError> for CoordinatorError {
    fn from(error: StatsError) -> Self {
        Self::Statistics(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        path::PathBuf,
    };

    use rusqlite::{Connection, params};
    use serde_json::json;

    use crate::{
        domain::{
            ids::{AttemptId, PrincipalId, RunId},
            lifecycle::{AttemptOwnership, FencingToken},
        },
        evaluation::{
            ConformanceEvaluationService, ProductionEvaluationPins, SqliteEventEvidenceStore,
            harness::{
                Check, ConformanceCoreTrialExecutor, CoreHarness, CoreTrialError,
                CoreTrialExecutor, GraderBounds, HarnessInputs, HiddenHandle, SourceSnapshot,
                sha256,
            },
            reports::{
                ConformanceLedgerAnchor, Preregistration, ProductionExecutionEnvironment,
                RegistrationAuthority,
            },
        },
        runtime::scheduler::{
            AdmissionKind, DurableScheduler, ReservationRequest, TrialRunBinding, limits::Spend,
            reserve::ReservationId,
        },
        store::sqlite::trial_usage::SqliteTrialUsageReceiptStore,
    };

    use super::*;
    use crate::evaluation::harness::trial_runner::{CoreTrialExecution, CoreTrialRequest};

    const REFERENCE: &[u8] = include_bytes!("../../eval/harness/core/fixtures/reference.patch");

    struct Root(PathBuf);

    impl Root {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "kit-{name}-{}-{}",
                std::process::id(),
                getrandom::u64().unwrap()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn harness() -> CoreHarness {
        harness_with_task_version_and_arm("1", Arm::Baseline)
    }

    fn harness_with_task_version(task_version: &str) -> CoreHarness {
        harness_with_task_version_and_arm(task_version, Arm::Baseline)
    }

    fn harness_with_task_version_and_arm(task_version: &str, arm: Arm) -> CoreHarness {
        let trial_id = match arm {
            Arm::Baseline => "empty-trial",
            Arm::Candidate => "empty-trial-candidate",
        };
        harness_with_task_version_arm_and_id(task_version, arm, trial_id)
    }

    fn harness_with_task_version_arm_and_id(
        task_version: &str,
        arm: Arm,
        trial_id: &str,
    ) -> CoreHarness {
        let bounds = GraderBounds {
            max_patch_bytes: 16 * 1024,
            max_source_bytes: 64 * 1024,
            max_files: 32,
            max_checks: 16,
            max_check_bytes: 16 * 1024,
            max_log_bytes: 4096,
            max_artifact_bytes: 128 * 1024,
            max_memory_bytes: 256 * 1024 * 1024,
            max_time_millis: 10_000,
        };
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
                "sha256": sha256(b"right\n")
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
            serde_json::from_slice(include_bytes!("../../eval/manifests/examples/trial.json"))
                .unwrap();
        trial["trial_id"] = trial_id.into();
        if arm == Arm::Candidate {
            trial["environment"]["model"]["model_digest"] =
                format!("sha256:{}", "7".repeat(64)).into();
            trial["environment"]["model"]["settings_digest"] =
                format!("sha256:{}", "8".repeat(64)).into();
            trial["environment"]["model"]["provider_capability_digest"] =
                format!("sha256:{}", "9".repeat(64)).into();
        }
        trial["grader"]["hidden_tests_digest"] = sha256(&hidden_tests).into();
        trial["grader"]["acceptance_digest"] = sha256(&acceptance_rules).into();
        trial["grader"]["gold_patch_digest"] = sha256(REFERENCE).into();
        trial["grader"]["harness_config_digest"] = sha256(&config).into();
        trial["task"]["specification_digest"] = sha256(&specification).into();
        trial["task"]["scaffold_digest"] = sha256(&scaffold).into();
        trial["task"]["task_version"] = task_version.into();
        CoreHarness::load(HarnessInputs {
            trial_manifest: serde_json::to_vec(&trial).unwrap(),
            task_manifest: serde_json::to_vec(&trial["task"]).unwrap(),
            grader_manifest: serde_json::to_vec(&trial["grader"]).unwrap(),
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
    #[cfg(debug_assertions)]
    fn cross_task_harness_rejects_another_tasks_calibration_token() {
        let first = harness_with_task_version("task-a");
        let second = harness_with_task_version("task-b");
        let mut executor = ConformanceCoreTrialExecutor::source_semantics_fake(
            crate::evaluation::harness::trial_runner::trusted_source_semantics_token(),
        );
        let calibration = first.self_validate(&mut executor).unwrap();
        let second_calibration = second.self_validate(&mut executor).unwrap();
        let ordinary = second
            .measure(&mut executor, &second_calibration.token, REFERENCE)
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
            production_pins_digest: sha256(b"pins"),
            roster_index: 0,
            trial_id: ordinary.report.trial_id,
            pair_id: "pair-a".to_owned(),
            task_id: "task-b".to_owned(),
            dataset_member_id: "dataset-a".to_owned(),
            task_manifest_digest: second.task_manifest_digest().to_owned(),
            model_digest: second.model_digest().to_owned(),
            model_settings_digest: second.model_settings_digest().to_owned(),
            config_digest: second.config_digest(),
            provider_capability_digest: second.provider_capability_digest().to_owned(),
            seed: 1,
            arm: "baseline".to_owned(),
            nonce: "nonce".to_owned(),
            token_digest: sha256(b"token"),
            scheduler_run_id: "018f0000-0000-7000-8000-000000000001".to_owned(),
            scheduler_consumption_position: 8,
            scheduler_consumption_digest: sha256(b"consumption"),
            run_config_digest: sha256(b"config"),
            event_high_watermark: 9,
        };
        assert!(matches!(
            second.measure_admitted(
                &mut executor,
                &calibration.token.inner,
                &admission,
                REFERENCE,
            ),
            Err(crate::evaluation::harness::HarnessError::AdmissionMismatch)
        ));
    }

    struct Evidence;

    struct LearningEvidence {
        database: PathBuf,
        hasher: crate::telemetry::tool_learning::ProjectPointerHasher,
        capability: crate::telemetry::tool_learning::LearningPointer,
        schema: crate::telemetry::tool_learning::LearningPointer,
        second_capability: crate::telemetry::tool_learning::LearningPointer,
        second_schema: crate::telemetry::tool_learning::LearningPointer,
        baseline_call: crate::telemetry::tool_learning::LearningPointer,
        candidate_call: crate::telemetry::tool_learning::LearningPointer,
    }

    impl LearningEvidence {
        fn record(&self, run_id: RunId, config: &TrialRunConfig) -> Result<(), CoordinatorError> {
            use crate::telemetry::tool_learning::{
                LearningCandidate, LearningCommon, LearningOperation, LearningStatus,
                LearningSurface, PointerDomain, ToolLearningEvent,
            };

            if config.pair_id != "pair-a" {
                return Ok(());
            }
            let call = match config.arm {
                Arm::Baseline => self.baseline_call.clone(),
                Arm::Candidate => self.candidate_call.clone(),
            };
            let common = |ordinal, operation, identity: &[u8], capability, schema| {
                LearningCommon::new(
                    &self.hasher,
                    run_id,
                    ordinal,
                    operation,
                    LearningSurface::Deferred,
                    identity,
                    None,
                    capability,
                    schema,
                )
            };
            let mut events = vec![ToolLearningEvent::Opportunity {
                common: common(1, LearningOperation::Projection, b"opportunity", None, None),
                offered: 1,
                eager: 0,
                deferred: 1,
                generic_available: false,
                projection: self.hasher.pointer(PointerDomain::Schema, b"projection"),
                candidates: vec![LearningCandidate {
                    capability: self.capability.clone(),
                    schema: self.schema.clone(),
                    surface: LearningSurface::Deferred,
                    authorized: true,
                    offered: true,
                }],
                detail_artifact: None,
            }];
            for order in 1..=2 {
                let (step_capability, step_schema, step_call) = if order == 1 {
                    (self.capability.clone(), self.schema.clone(), call.clone())
                } else {
                    (
                        self.second_capability.clone(),
                        self.second_schema.clone(),
                        self.hasher.pointer(
                            PointerDomain::Call,
                            format!("{run_id}:second-call").as_bytes(),
                        ),
                    )
                };
                events.extend([
                    ToolLearningEvent::Call {
                        common: common(
                            order * 2,
                            LearningOperation::Invoke,
                            step_call.as_str().as_bytes(),
                            Some(step_capability.clone()),
                            Some(step_schema.clone()),
                        ),
                        call: step_call.clone(),
                        binding: Some(self.hasher.pointer(PointerDomain::Binding, b"binding")),
                        source: Some(self.hasher.pointer(PointerDomain::Source, b"source")),
                        kind: Some(crate::telemetry::tool_learning::LearningCapabilityKind::Tool),
                        sequence: Some(
                            self.hasher
                                .pointer(PointerDomain::Sequence, run_id.to_string().as_bytes()),
                        ),
                        sequence_order: Some(order as u16),
                        kernel_intent: Some(
                            self.hasher
                                .pointer(PointerDomain::KernelEvent, b"kernel-intent"),
                        ),
                    },
                    ToolLearningEvent::Outcome {
                        common: common(
                            order * 2 + 1,
                            LearningOperation::Invoke,
                            format!("outcome:{order}").as_bytes(),
                            Some(step_capability),
                            Some(step_schema),
                        ),
                        call: step_call,
                        status: LearningStatus::Succeeded,
                        dispatched: true,
                        known: true,
                        cost_microusd: Some(match config.arm {
                            Arm::Baseline => 10,
                            Arm::Candidate => 5,
                        }),
                        kernel_outcome: Some(
                            self.hasher
                                .pointer(PointerDomain::KernelEvent, b"kernel-outcome"),
                        ),
                    },
                ]);
            }
            for event in &mut events {
                let mut value = serde_json::to_value(&*event)
                    .map_err(|_| CoordinatorError::Evidence("learning serialization"))?;
                value
                    .get_mut("common")
                    .and_then(serde_json::Value::as_object_mut)
                    .and_then(|common| common.remove("event_id"))
                    .ok_or(CoordinatorError::Evidence("learning event authority"))?;
                let authority = self.hasher.pointer(
                    PointerDomain::Event,
                    &serde_json::to_vec(&value)
                        .map_err(|_| CoordinatorError::Evidence("learning serialization"))?,
                );
                match event {
                    ToolLearningEvent::Opportunity { common, .. }
                    | ToolLearningEvent::Search { common, .. }
                    | ToolLearningEvent::Inspection { common, .. }
                    | ToolLearningEvent::Call { common, .. }
                    | ToolLearningEvent::Error { common, .. }
                    | ToolLearningEvent::Outcome { common, .. } => common.event_id = authority,
                }
            }

            let connection = Connection::open(&self.database)
                .map_err(|_| CoordinatorError::Evidence("learning database"))?;
            let start = i64::try_from(config.event_start_watermark)
                .map_err(|_| CoordinatorError::Evidence("learning watermark"))?;
            connection
                .execute(
                    "INSERT INTO stream_heads (stream,version) VALUES (?1,5)",
                    [run_id.to_string()],
                )
                .map_err(|_| CoordinatorError::Evidence("learning stream"))?;
            for (offset, event) in events.iter().enumerate() {
                let common = event.common();
                let event_id = crate::domain::ids::EventId::from_stable_bytes(
                    common.event_id.as_str().as_bytes(),
                );
                connection
                    .execute(
                        "INSERT INTO events (event_id,stream,sequence,commit_position,event_type,
                         schema_version,occurred_at,causation_id,correlation_id,attempt_id,trace_id,
                         payload,artifacts) VALUES (?1,?2,?3,?4,'tool_learning.recorded',1,
                         '2026-01-01T00:00:00Z',?1,?2,NULL,'learning-analysis',?5,X'5b5d')",
                        params![
                            event_id.to_string(),
                            run_id.to_string(),
                            i64::try_from(offset + 1).unwrap(),
                            start + i64::try_from(offset + 1).unwrap(),
                            serde_json::to_vec(event).unwrap(),
                        ],
                    )
                    .map_err(|_| CoordinatorError::Evidence("learning event"))?;
            }
            connection
                .execute(
                    "UPDATE commit_watermark SET position=max(position,?1) WHERE singleton=1",
                    [start + 5],
                )
                .map_err(|_| CoordinatorError::Evidence("learning watermark"))?;
            Ok(())
        }
    }

    impl EventEvidenceStore for LearningEvidence {
        fn source(&self) -> &'static str {
            "production_authenticated"
        }

        fn capture_start(
            &self,
            _: RunId,
            pending: &crate::runtime::scheduler::PendingStatisticalTrial,
        ) -> Result<PreparedEventEvidence, CoordinatorError> {
            Ok(PreparedEventEvidence {
                source: self.source().to_owned(),
                event_start_watermark: pending.consumption_position.saturating_mul(10),
            })
        }

        fn finalize_terminal(
            &self,
            scheduler: &DurableScheduler,
            run_id: RunId,
            config: &TrialRunConfig,
        ) -> Result<u64, CoordinatorError> {
            scheduler.finish_run(run_id, false)?;
            Ok(config.event_start_watermark + 5)
        }

        fn trusted_events(
            &self,
            run_id: RunId,
            config: &TrialRunConfig,
            event_high_watermark: u64,
        ) -> Result<Vec<u8>, CoordinatorError> {
            self.record(run_id, config)?;
            let bytes = Evidence.trusted_events(run_id, config, event_high_watermark)?;
            let mut value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| CoordinatorError::Evidence("learning event evidence"))?;
            value["source"] = self.source().into();
            let connection = Connection::open(&self.database)
                .map_err(|_| CoordinatorError::Evidence("learning event evidence database"))?;
            let mut statement = connection
                .prepare(
                    "SELECT commit_position,event_type,payload FROM events
                     WHERE correlation_id=?1 AND event_type='tool_learning.recorded'
                     ORDER BY commit_position",
                )
                .map_err(|_| CoordinatorError::Evidence("learning event evidence query"))?;
            let bindings = statement
                .query_map([run_id.to_string()], |row| {
                    let position = row.get::<_, u64>(0)?;
                    let kind = row.get::<_, String>(1)?;
                    let payload = row.get::<_, Vec<u8>>(2)?;
                    Ok(json!({
                        "kind": kind,
                        "event_position": position,
                        "admission_token_digest": config.admission_token_digest,
                        "event_digest": crate::evaluation::reports::sha256(&payload),
                    }))
                })
                .map_err(|_| CoordinatorError::Evidence("learning event evidence rows"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| CoordinatorError::Evidence("learning event evidence row"))?;
            if bindings.is_empty() {
                for class in ["scheduler_events", "provider_events", "tool_events"] {
                    let event = value
                        .get_mut(class)
                        .and_then(serde_json::Value::as_array_mut)
                        .and_then(|events| events.first_mut())
                        .ok_or(CoordinatorError::Evidence("learning event evidence count"))?;
                    event["event_digest"] = crate::evaluation::reports::sha256(
                        event["kind"].as_str().unwrap_or_default().as_bytes(),
                    )
                    .into();
                }
            } else if bindings.len() == 5 {
                value["scheduler_events"] = json!([bindings[0].clone()]);
                value["provider_events"] = json!([bindings[1].clone()]);
                value["tool_events"] = json!(bindings[2..].to_vec());
            } else {
                return Err(CoordinatorError::Evidence("learning event evidence count"));
            }
            serde_json::to_vec(&value)
                .map_err(|_| CoordinatorError::Evidence("learning event evidence"))
        }
    }

    struct DurableEvidenceExecutor<'a> {
        inner: ConformanceCoreTrialExecutor,
        scheduler: &'a DurableScheduler,
        usage: SqliteTrialUsageReceiptStore,
        run_id: RunId,
        principal: PrincipalId,
        owner: AttemptOwnership,
        database: PathBuf,
        calls: usize,
    }

    impl CoreTrialExecutor for DurableEvidenceExecutor<'_> {
        fn execute_core(
            &mut self,
            request: CoreTrialRequest<'_>,
        ) -> Result<CoreTrialExecution, CoreTrialError> {
            self.calls += 1;
            let trial_id = request.manifest.trial_id().to_owned();
            let mut execution = self.inner.execute_core(request)?;
            let id = ReservationId::new(self.calls as u128);
            let spend = Spend::new(6, 9, 1, 0, 0);
            self.scheduler
                .reserve(&ReservationRequest {
                    id,
                    run_id: self.run_id,
                    principal_id: self.principal,
                    attempt: Some(self.owner),
                    idempotency_key: "evaluation-model".to_owned(),
                    kind: AdmissionKind::Model,
                    spend,
                })
                .and_then(|_| self.scheduler.mark_dispatched(id))
                .and_then(|_| self.scheduler.debit(id).map(drop))
                .and_then(|_| self.scheduler.reconcile(id, spend).map(drop))
                .map_err(|error| CoreTrialError::Executor(error.to_string()))?;
            append_execution_events(&self.database, self.run_id)
                .map_err(|error| CoreTrialError::Executor(error.to_string()))?;
            self.scheduler
                .finish_run_with_event_watermark(self.run_id, false)
                .map_err(|error| CoreTrialError::Executor(error.to_string()))?;
            self.usage
                .mint(self.run_id, &trial_id)
                .map_err(|error| CoreTrialError::Executor(error.to_string()))?;
            execution.provider_request_ids = vec!["provider-response-1".to_owned()];
            execution.usage = TrialUsage {
                turns: crate::executor::trial::UsageMeasure::Measured(1),
                input_tokens: crate::executor::trial::UsageMeasure::Measured(4),
                output_tokens: crate::executor::trial::UsageMeasure::Measured(5),
                cost_microusd: crate::executor::trial::UsageMeasure::Measured(6),
                tool_calls: crate::executor::trial::UsageMeasure::Measured(1),
                processes: crate::executor::trial::UsageMeasure::Measured(0),
            };
            Ok(execution)
        }
    }

    impl ProviderEvidenceStore for Evidence {
        fn verify(
            &self,
            _: RunId,
            _: &TrialRunConfig,
            _: &[String],
            _: u64,
        ) -> Result<(), CoordinatorError> {
            Ok(())
        }
    }

    impl ToolEvidenceStore for Evidence {
        fn verify(&self, _: RunId, _: &TrialRunConfig, _: u64) -> Result<(), CoordinatorError> {
            Ok(())
        }
    }

    impl UsageEvidenceStore for Evidence {
        fn verify(
            &self,
            _: RunId,
            _: &TrialRunConfig,
            _: TrialUsage,
            _: u64,
        ) -> Result<(), CoordinatorError> {
            Ok(())
        }
    }

    impl EventEvidenceStore for Evidence {
        fn source(&self) -> &'static str {
            "conformance_source_semantics_fake"
        }

        fn capture_start(
            &self,
            _: RunId,
            pending: &crate::runtime::scheduler::PendingStatisticalTrial,
        ) -> Result<PreparedEventEvidence, CoordinatorError> {
            Ok(PreparedEventEvidence {
                source: "conformance_source_semantics_fake".to_owned(),
                event_start_watermark: pending.consumption_position,
            })
        }

        fn finalize_terminal(
            &self,
            scheduler: &DurableScheduler,
            run_id: RunId,
            config: &TrialRunConfig,
        ) -> Result<u64, CoordinatorError> {
            scheduler.finish_run(run_id, false)?;
            Ok(config.event_start_watermark + 3)
        }

        fn trusted_events(
            &self,
            _: RunId,
            config: &TrialRunConfig,
            event_high_watermark: u64,
        ) -> Result<Vec<u8>, CoordinatorError> {
            let high = event_high_watermark;
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "source": "conformance_source_semantics_fake",
                "run_config_digest": config.immutable_digest,
                "admission_position": config.admission_position,
                "admission_nonce": config.admission_nonce,
                "admission_token_digest": config.admission_token_digest,
                "scheduler_run_id": config.scheduler_run_id,
                "scheduler_consumption_position": config.scheduler_consumption_position,
                "scheduler_consumption_digest": config.scheduler_consumption_digest,
                "event_high_watermark": high,
                "trial_id": config.trial_id,
                "pair_id": config.pair_id,
                "task_id": config.task_id,
                "dataset_member_id": config.dataset_member_id,
                "seed": config.seed,
                "arm": config.arm,
                "config_digest": config.config_digest,
                "started_monotonic_millis": 1000,
                "finished_monotonic_millis": 1001,
                "intervention": false,
                "exclusion_reason": "",
                "scheduler_events": [{
                    "kind": "run_admitted",
                    "event_position": config.event_start_watermark + 1,
                    "admission_token_digest": config.admission_token_digest
                }],
                "provider_events": [{
                    "kind": "provider_completed",
                    "event_position": config.event_start_watermark + 2,
                    "admission_token_digest": config.admission_token_digest
                }],
                "tool_events": [{
                    "kind": "tool_completed",
                    "event_position": high,
                    "admission_token_digest": config.admission_token_digest
                }]
            }))
            .map_err(|_| CoordinatorError::Evidence("serialization"))
        }
    }

    #[test]
    fn coordinator_is_the_bound_scheduler_harness_and_evidence_path() {
        let harness = harness();
        let candidate_harness = harness_with_task_version_and_arm("1", Arm::Candidate);
        let mut executor = ConformanceCoreTrialExecutor::source_semantics_fake(
            crate::evaluation::harness::trial_runner::trusted_source_semantics_token(),
        );
        let calibration = harness.self_validate(&mut executor).unwrap();
        let ordinary = harness
            .measure(&mut executor, &calibration.token, REFERENCE)
            .unwrap();
        let candidate_ordinary = candidate_harness
            .measure(&mut executor, &calibration.token, REFERENCE)
            .unwrap();
        assert_eq!(
            harness.harness_config_digest(),
            candidate_harness.harness_config_digest()
        );
        assert_eq!(
            harness.grader_manifest_digest(),
            candidate_harness.grader_manifest_digest()
        );
        assert_eq!(harness.binding_digest(), candidate_harness.binding_digest());
        let mut plan = Preregistration::from_json(include_bytes!(
            "../../eval/preregistration/templates/core-paired-v1.json"
        ))
        .unwrap();
        plan.roster[0].trial_id = ordinary.report.trial_id.clone();
        plan.roster[0].task_manifest_digest = ordinary.report.task_manifest_digest.clone();
        plan.roster[1].task_manifest_digest = ordinary.report.task_manifest_digest.clone();
        plan.digests.harness = ordinary.report.harness_config_digest.clone();
        plan.execution_environment.pins.harness = plan.digests.harness.clone();
        plan.execution_environment =
            ProductionExecutionEnvironment::new(plan.execution_environment.pins.clone()).unwrap();
        plan.roster[0].model_digest = ordinary.report.model_digest.clone();
        plan.roster[0].model_settings_digest = ordinary.report.model_settings_digest.clone();
        plan.roster[0].config_digest = ordinary.report.config_digest.clone();
        plan.roster[0].provider_capability_digest =
            ordinary.report.provider_capability_digest.clone();
        plan.roster[1].trial_id = candidate_ordinary.report.trial_id.clone();
        plan.roster[1].task_manifest_digest =
            candidate_ordinary.report.task_manifest_digest.clone();
        plan.roster[1].model_digest = candidate_ordinary.report.model_digest.clone();
        plan.roster[1].model_settings_digest =
            candidate_ordinary.report.model_settings_digest.clone();
        plan.roster[1].config_digest = candidate_ordinary.report.config_digest.clone();
        plan.roster[1].provider_capability_digest =
            candidate_ordinary.report.provider_capability_digest.clone();
        let (experiment, task_set, dataset) = plan.derived_design_digests().unwrap();
        plan.digests.experiment = experiment;
        plan.digests.task_set = task_set;
        plan.digests.dataset = dataset;

        let root = Root::new("coordinator-authority");
        let mut authority = RegistrationAuthority::open_with_anchor(
            &root.0,
            ConformanceLedgerAnchor::source_semantics_fake(),
        )
        .unwrap();
        let registered = authority.register(plan).unwrap();
        let admission = authority.admit_next(&registered).unwrap();
        let scheduler_root = Root::new("coordinator-scheduler");
        let scheduler_db = scheduler_root.0.join("state.sqlite3");
        drop(crate::test_support::open_service_store(&scheduler_db).unwrap());
        let scheduler = DurableScheduler::open(&scheduler_db).unwrap();
        let run_id = RunId::generate().unwrap();
        let principal = PrincipalId::generate().unwrap();
        scheduler
            .register_statistical_trial_run(
                run_id,
                principal,
                "coordinator-run",
                &registered.preregistration.roster[0].config_digest,
            )
            .unwrap();
        let binding = TrialRunBinding {
            trial_id: registered.preregistration.roster[0].trial_id.clone(),
            trial_digest: registered.preregistration_digest.clone(),
            task_digest: registered.preregistration.roster[0]
                .task_manifest_digest
                .clone(),
            model_digest: registered.preregistration.roster[0].model_digest.clone(),
            config_digest: registered.preregistration.roster[0].config_digest.clone(),
            attempt: AttemptOwnership::new(
                AttemptId::generate().unwrap(),
                principal,
                FencingToken::new(1),
            ),
            admission: Some(admission.scheduler_token().unwrap()),
        };
        let evidence = Evidence;
        let receipt = {
            let mut coordinator = StatisticalTrialCoordinator::new(
                &mut authority,
                &scheduler,
                &harness,
                &evidence,
                &evidence,
                &evidence,
                &evidence,
            );
            coordinator
                .run(
                    &mut executor,
                    StatisticalTrialRequest {
                        run_id,
                        binding: &binding,
                        registered: &registered,
                        admission: &admission,
                        calibration: &calibration.token,
                        patch: REFERENCE,
                    },
                )
                .unwrap()
        };
        assert_eq!(receipt.receipt().scheduler_run_id, run_id.to_string());
        let repeated = StatisticalTrialCoordinator::new(
            &mut authority,
            &scheduler,
            &harness,
            &evidence,
            &evidence,
            &evidence,
            &evidence,
        )
        .run(
            &mut executor,
            StatisticalTrialRequest {
                run_id,
                binding: &binding,
                registered: &registered,
                admission: &admission,
                calibration: &calibration.token,
                patch: REFERENCE,
            },
        )
        .unwrap();
        assert_eq!(repeated.digest(), receipt.digest());
        let candidate_admission = authority.admit_next(&registered).unwrap();
        let candidate_run_id = RunId::generate().unwrap();
        scheduler
            .register_statistical_trial_run(
                candidate_run_id,
                principal,
                "coordinator-candidate-run",
                &registered.preregistration.roster[1].config_digest,
            )
            .unwrap();
        let candidate_binding = TrialRunBinding {
            trial_id: registered.preregistration.roster[1].trial_id.clone(),
            trial_digest: registered.preregistration_digest.clone(),
            task_digest: registered.preregistration.roster[1]
                .task_manifest_digest
                .clone(),
            model_digest: registered.preregistration.roster[1].model_digest.clone(),
            config_digest: registered.preregistration.roster[1].config_digest.clone(),
            attempt: AttemptOwnership::new(
                AttemptId::generate().unwrap(),
                principal,
                FencingToken::new(1),
            ),
            admission: Some(candidate_admission.scheduler_token().unwrap()),
        };
        assert!(matches!(
            StatisticalTrialCoordinator::new(
                &mut authority,
                &scheduler,
                &harness,
                &evidence,
                &evidence,
                &evidence,
                &evidence,
            )
            .run(
                &mut executor,
                StatisticalTrialRequest {
                    run_id: candidate_run_id,
                    binding: &candidate_binding,
                    registered: &registered,
                    admission: &candidate_admission,
                    calibration: &calibration.token,
                    patch: REFERENCE,
                },
            ),
            Err(CoordinatorError::Binding)
        ));
        let candidate_receipt = StatisticalTrialCoordinator::new(
            &mut authority,
            &scheduler,
            &candidate_harness,
            &evidence,
            &evidence,
            &evidence,
            &evidence,
        )
        .run(
            &mut executor,
            StatisticalTrialRequest {
                run_id: candidate_run_id,
                binding: &candidate_binding,
                registered: &registered,
                admission: &candidate_admission,
                calibration: &calibration.token,
                patch: REFERENCE,
            },
        )
        .unwrap();
        assert_eq!(
            candidate_receipt.receipt().pair_id,
            receipt.receipt().pair_id
        );
        assert_ne!(
            candidate_receipt.receipt().model_digest,
            receipt.receipt().model_digest
        );
        assert_eq!(
            scheduler
                .admit_statistical_trial_run(run_id, &binding, &authority)
                .unwrap()
                .admission_token_digest,
            admission.token_digest()
        );
        assert_eq!(
            authority.build_report(&registered).unwrap_err(),
            StatsError::ExperimentNotTerminal
        );
    }

    #[test]
    fn conformance_tool_learning_report_uses_preregistered_arms_and_verified_receipts() {
        use crate::telemetry::tool_learning::{
            CausalResult, CausalUnavailable, PointerDomain, ProjectPointerHasher,
        };

        let harnesses = [
            harness_with_task_version_arm_and_id("1", Arm::Baseline, "pair-a-baseline"),
            harness_with_task_version_arm_and_id("1", Arm::Candidate, "pair-a-candidate"),
            harness_with_task_version_arm_and_id("2", Arm::Candidate, "pair-b-candidate"),
            harness_with_task_version_arm_and_id("2", Arm::Baseline, "pair-b-baseline"),
            harness_with_task_version_arm_and_id("3", Arm::Baseline, "pair-c-baseline"),
            harness_with_task_version_arm_and_id("3", Arm::Candidate, "pair-c-candidate"),
        ];
        let mut measured = Vec::new();
        for harness in &harnesses {
            let mut executor = ConformanceCoreTrialExecutor::source_semantics_fake(
                crate::evaluation::harness::trial_runner::trusted_source_semantics_token(),
            );
            let calibration = harness.self_validate(&mut executor).unwrap();
            let report = harness
                .measure(&mut executor, &calibration.token, REFERENCE)
                .unwrap();
            measured.push((calibration, report));
        }

        let project = crate::domain::ids::ProjectId::generate().unwrap();
        let key = [73; 32];
        let hasher = ProjectPointerHasher::new(project, &key);
        let capability = hasher.pointer(PointerDomain::Capability, b"fixture-capability");
        let schema = hasher.pointer(PointerDomain::Schema, b"fixture-schema");
        let second_capability =
            hasher.pointer(PointerDomain::Capability, b"fixture-second-capability");
        let second_schema = hasher.pointer(PointerDomain::Schema, b"fixture-second-schema");
        let baseline_call = hasher.pointer(PointerDomain::Call, b"baseline-call");
        let candidate_call = hasher.pointer(PointerDomain::Call, b"candidate-call");
        let experiment = hasher.pointer(PointerDomain::Experiment, b"pair-a-experiment");

        let mut plan = Preregistration::from_json(include_bytes!(
            "../../eval/preregistration/templates/core-paired-v1.json"
        ))
        .unwrap();
        for ((entry, _), (_, report)) in plan.roster.iter_mut().zip(&harnesses).zip(&measured) {
            entry.trial_id = report.report.trial_id.clone();
            entry.task_manifest_digest = report.report.task_manifest_digest.clone();
            entry.model_digest = report.report.model_digest.clone();
            entry.model_settings_digest = report.report.model_settings_digest.clone();
            entry.config_digest = report.report.config_digest.clone();
            entry.provider_capability_digest = report.report.provider_capability_digest.clone();
        }
        plan.digests.harness = harnesses[0].harness_config_digest().to_owned();
        plan.execution_environment.pins.harness = plan.digests.harness.clone();
        plan.execution_environment =
            ProductionExecutionEnvironment::new(plan.execution_environment.pins.clone()).unwrap();
        plan.schema_version = "1.2".to_owned();
        plan.tool_learning_experiments =
            vec![crate::evaluation::reports::ToolLearningExperimentPlan {
                experiment: experiment.as_str().to_owned(),
                pair_id: "pair-a".to_owned(),
                capability: capability.as_str().to_owned(),
                schema: schema.as_str().to_owned(),
                surface: "deferred".to_owned(),
                description_only: false,
                frozen_factors: hasher
                    .pointer(PointerDomain::Artifact, b"frozen-factors")
                    .as_str()
                    .to_owned(),
                baseline_sequence: vec![
                    crate::evaluation::reports::ToolLearningSequenceStepPlan {
                        capability: capability.as_str().to_owned(),
                        schema: schema.as_str().to_owned(),
                        surface: "deferred".to_owned(),
                        ordinal: 1,
                    },
                    crate::evaluation::reports::ToolLearningSequenceStepPlan {
                        capability: second_capability.as_str().to_owned(),
                        schema: second_schema.as_str().to_owned(),
                        surface: "deferred".to_owned(),
                        ordinal: 2,
                    },
                ],
                candidate_sequence: vec![
                    crate::evaluation::reports::ToolLearningSequenceStepPlan {
                        capability: capability.as_str().to_owned(),
                        schema: schema.as_str().to_owned(),
                        surface: "deferred".to_owned(),
                        ordinal: 1,
                    },
                    crate::evaluation::reports::ToolLearningSequenceStepPlan {
                        capability: second_capability.as_str().to_owned(),
                        schema: second_schema.as_str().to_owned(),
                        surface: "deferred".to_owned(),
                        ordinal: 2,
                    },
                ],
            }];
        let (experiment_digest, task_set, dataset) = plan.derived_design_digests().unwrap();
        plan.digests.experiment = experiment_digest;
        plan.digests.task_set = task_set;
        plan.digests.dataset = dataset;
        let pins = plan.execution_environment.pins.clone();

        let root = Root::new("production-learning-report");
        let database = root.0.join("state.sqlite3");
        drop(crate::test_support::open_service_store(&database).unwrap());
        fs::create_dir(root.0.join("evaluation")).unwrap();
        let anchor = ConformanceLedgerAnchor::source_semantics_fake();
        let mut authority =
            RegistrationAuthority::open_with_anchor(root.0.join("evaluation"), anchor.clone())
                .unwrap();
        let registered = authority.register(plan).unwrap();
        let scheduler = DurableScheduler::open(&database).unwrap();
        let operations = SqliteCoordinatorOperationStore::open(&database).unwrap();
        let evidence = Evidence;
        let learning = LearningEvidence {
            database: database.clone(),
            hasher,
            capability,
            schema,
            second_capability,
            second_schema,
            baseline_call,
            candidate_call,
        };
        let principal = PrincipalId::generate().unwrap();

        for (index, harness) in harnesses.iter().enumerate() {
            let admission = authority.admit_next(&registered).unwrap();
            let run_id = RunId::generate().unwrap();
            let idempotency_key = format!("production-learning-{index}");
            scheduler
                .register_statistical_trial_run(
                    run_id,
                    principal,
                    &idempotency_key,
                    &registered.preregistration.roster[index].config_digest,
                )
                .unwrap();
            let binding = TrialRunBinding {
                trial_id: registered.preregistration.roster[index].trial_id.clone(),
                trial_digest: registered.preregistration_digest.clone(),
                task_digest: registered.preregistration.roster[index]
                    .task_manifest_digest
                    .clone(),
                model_digest: registered.preregistration.roster[index]
                    .model_digest
                    .clone(),
                config_digest: registered.preregistration.roster[index]
                    .config_digest
                    .clone(),
                attempt: AttemptOwnership::new(
                    AttemptId::generate().unwrap(),
                    principal,
                    FencingToken::new(1),
                ),
                admission: Some(admission.scheduler_token().unwrap()),
            };
            let mut executor = ConformanceCoreTrialExecutor::source_semantics_fake(
                crate::evaluation::harness::trial_runner::trusted_source_semantics_token(),
            );
            let receipt = StatisticalTrialCoordinator::new(
                &mut authority,
                &scheduler,
                harness,
                &evidence,
                &evidence,
                &learning,
                &evidence,
            )
            .with_operations(&operations)
            .run_production_verified(
                &mut executor,
                ProductionStatisticalTrialRequest {
                    run_id,
                    binding: &binding,
                    registered: &registered,
                    admission: &admission,
                    patch: REFERENCE,
                },
                &measured[index].0.token.inner,
                |_| {
                    if index == 1 {
                        Err(CoordinatorError::Evidence("forced terminal trial"))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap();
            assert_eq!(
                receipt.receipt().outcome,
                if index == 1 {
                    crate::evaluation::reports::TrialOutcome::Error
                } else {
                    crate::evaluation::reports::TrialOutcome::Success
                },
                "{:?}",
                receipt.receipt().failure_reason
            );
        }
        let operation_summary: (i64, String) = Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT COUNT(*), COALESCE(group_concat(phase || ':' || event_source), '')
                 FROM statistical_coordinator_operations",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(operation_summary.0, 6, "{}", operation_summary.1);
        let recorded: i64 = Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM statistical_coordinator_operations
                 WHERE phase='recorded' AND harness_bytes IS NOT NULL AND events_bytes IS NOT NULL
                   AND execution_receipt_bytes IS NOT NULL AND event_source='production_authenticated'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 5, "{}", operation_summary.1);
        drop(authority);

        let mut service =
            crate::evaluation::ProductionEvaluationService::open_with_pins(&root.0, anchor, pins)
                .unwrap()
                .with_learning_authority(project, key);
        let report = service.build_final_report(&registered).unwrap();
        assert_eq!(report.report.sample_counts.failed_trials, 1);
        assert_eq!(report.report.trials.len(), 6);
        service.verify_final_report(&registered, &report).unwrap();
        let analysis = report.report.learning_analysis.as_ref().unwrap();
        assert_eq!(
            analysis.result,
            CausalResult::Unavailable(CausalUnavailable::MissingDownstreamGrade)
        );
        let connection = Connection::open(&database).unwrap();
        let mut expected_inputs = BTreeSet::from([registered.preregistration_digest.clone()]);
        let mut statement = connection
            .prepare(
                "SELECT run_config_digest,execution_receipt_digest,harness_digest,events_digest,
                        terminal_evidence_digest
                 FROM statistical_coordinator_operations
                 WHERE phase IN ('recorded','terminal_error') ORDER BY run_id",
            )
            .unwrap();
        for row in statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .unwrap()
        {
            let (config, receipt, harness, events, terminal) = row.unwrap();
            expected_inputs.extend([config, receipt]);
            expected_inputs.extend(harness);
            expected_inputs.extend(events);
            expected_inputs.extend(terminal);
        }
        let mut statement = connection
            .prepare(
                "SELECT payload FROM events WHERE event_type='tool_learning.recorded'
                 ORDER BY commit_position",
            )
            .unwrap();
        for payload in statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
        {
            expected_inputs.insert(sha256(&payload.unwrap()));
        }
        assert_eq!(analysis.input_digests, expected_inputs);
        assert_eq!(
            service.build_final_report(&registered).unwrap().bytes,
            report.bytes
        );
    }

    #[test]
    fn production_evaluation_service_pins_evidence_and_interruption_recovery() {
        let harness = harness();
        let candidate_harness = harness_with_task_version_and_arm("1", Arm::Candidate);
        let mut calibration_executor = ConformanceCoreTrialExecutor::source_semantics_fake(
            crate::evaluation::harness::trial_runner::trusted_source_semantics_token(),
        );
        let calibration = harness.self_validate(&mut calibration_executor).unwrap();
        let mut ordinary = harness
            .measure(&mut calibration_executor, &calibration.token, REFERENCE)
            .unwrap();
        let candidate_ordinary = candidate_harness
            .measure(&mut calibration_executor, &calibration.token, REFERENCE)
            .unwrap();
        assert!(
            crate::evaluation::service::verify_report_pins(
                &ordinary,
                harness.task_manifest_digest(),
                &ProductionEvaluationPins {
                    harness: harness.harness_config_digest().to_owned(),
                    grader_manifest: harness.grader_manifest_digest().to_owned(),
                    helper: ordinary.report.agent.helper_identity.clone(),
                    runtime: ordinary.report.agent.runtime_identity.clone(),
                    agent_image: harness.agent_image_digest().to_owned(),
                    grader_image: harness.grader_image_digest().to_owned(),
                }
            )
            .is_err()
        );
        ordinary.report.agent.route =
            crate::executor::trial::ExecutionRoute::TrustedContainerHelper;
        ordinary.report.grader.route =
            crate::executor::trial::ExecutionRoute::TrustedContainerHelper;
        let helper = format!("sha256:{}", "7".repeat(64));
        ordinary.report.agent.helper_identity = helper.clone();
        ordinary.report.grader.helper_identity = helper;
        let pins = ProductionEvaluationPins {
            harness: harness.harness_config_digest().to_owned(),
            grader_manifest: harness.grader_manifest_digest().to_owned(),
            helper: ordinary.report.agent.helper_identity.clone(),
            runtime: ordinary.report.agent.runtime_identity.clone(),
            agent_image: harness.agent_image_digest().to_owned(),
            grader_image: harness.grader_image_digest().to_owned(),
        };
        crate::evaluation::service::verify_harness_pins(
            &harness,
            harness.task_manifest_digest(),
            &pins,
        )
        .unwrap();
        crate::evaluation::service::verify_harness_pins(
            &candidate_harness,
            candidate_harness.task_manifest_digest(),
            &pins,
        )
        .unwrap();
        crate::evaluation::service::verify_report_pins(
            &ordinary,
            harness.task_manifest_digest(),
            &pins,
        )
        .unwrap();
        let wrong_task = format!("sha256:{}", "f".repeat(64));
        assert!(
            crate::evaluation::service::verify_harness_pins(&harness, &wrong_task, &pins).is_err()
        );
        assert!(
            crate::evaluation::service::verify_report_pins(&ordinary, &wrong_task, &pins).is_err()
        );
        for field in 0..6 {
            let mut substituted = pins.clone();
            let replacement = format!("sha256:{}", "f".repeat(64));
            match field {
                0 => substituted.harness = replacement,
                1 => substituted.grader_manifest = replacement,
                2 => substituted.helper = replacement,
                3 => substituted.runtime = replacement,
                4 => substituted.agent_image = replacement,
                5 => substituted.grader_image = replacement,
                _ => unreachable!(),
            }
            assert!(
                crate::evaluation::service::verify_harness_pins(
                    &harness,
                    harness.task_manifest_digest(),
                    &substituted,
                )
                .is_err()
                    || crate::evaluation::service::verify_report_pins(
                        &ordinary,
                        harness.task_manifest_digest(),
                        &substituted,
                    )
                    .is_err()
            );
        }
        let mut plan = Preregistration::from_json(include_bytes!(
            "../../eval/preregistration/templates/core-paired-v1.json"
        ))
        .unwrap();
        plan.roster[0].trial_id = ordinary.report.trial_id.clone();
        plan.roster[0].task_manifest_digest = ordinary.report.task_manifest_digest.clone();
        plan.roster[1].task_manifest_digest = ordinary.report.task_manifest_digest.clone();
        plan.digests.harness = ordinary.report.harness_config_digest.clone();
        plan.roster[0].model_digest = ordinary.report.model_digest.clone();
        plan.roster[0].model_settings_digest = ordinary.report.model_settings_digest.clone();
        plan.roster[0].config_digest = ordinary.report.config_digest.clone();
        plan.roster[0].provider_capability_digest =
            ordinary.report.provider_capability_digest.clone();
        plan.roster[1].trial_id = candidate_ordinary.report.trial_id.clone();
        plan.roster[1].task_manifest_digest =
            candidate_ordinary.report.task_manifest_digest.clone();
        plan.roster[1].model_digest = candidate_ordinary.report.model_digest.clone();
        plan.roster[1].model_settings_digest =
            candidate_ordinary.report.model_settings_digest.clone();
        plan.roster[1].config_digest = candidate_ordinary.report.config_digest.clone();
        plan.roster[1].provider_capability_digest =
            candidate_ordinary.report.provider_capability_digest.clone();
        plan.execution_environment = ProductionExecutionEnvironment::new(pins.clone()).unwrap();
        let (experiment, task_set, dataset) = plan.derived_design_digests().unwrap();
        plan.digests.experiment = experiment;
        plan.digests.task_set = task_set;
        plan.digests.dataset = dataset;

        let registration_root = Root::new("production-registration-pins");
        let registration_database = registration_root.0.join("state.sqlite3");
        drop(crate::test_support::open_service_store(&registration_database).unwrap());
        let registration_anchor = ConformanceLedgerAnchor::source_semantics_fake();
        let mut production = crate::evaluation::ProductionEvaluationService::open_with_pins(
            &registration_root.0,
            registration_anchor.clone(),
            pins.clone(),
        )
        .unwrap();
        let mut mismatched_harness_plan = plan.clone();
        mismatched_harness_plan.digests.harness = format!("sha256:{}", "d".repeat(64));
        let (experiment, task_set, dataset) =
            mismatched_harness_plan.derived_design_digests().unwrap();
        mismatched_harness_plan.digests.experiment = experiment;
        mismatched_harness_plan.digests.task_set = task_set;
        mismatched_harness_plan.digests.dataset = dataset;
        assert!(matches!(
            production.register(mismatched_harness_plan),
            Err(crate::evaluation::ProductionEvaluationError::Unavailable(
                "preregistered production execution pins do not match the configured pins"
            ))
        ));
        let mut mismatched_plan = plan.clone();
        let mut mismatched_pins = pins.clone();
        mismatched_pins.helper = format!("sha256:{}", "e".repeat(64));
        mismatched_plan.execution_environment =
            ProductionExecutionEnvironment::new(mismatched_pins).unwrap();
        let (experiment, task_set, dataset) = mismatched_plan.derived_design_digests().unwrap();
        mismatched_plan.digests.experiment = experiment;
        mismatched_plan.digests.task_set = task_set;
        mismatched_plan.digests.dataset = dataset;
        assert!(production.register(mismatched_plan).is_err());
        let production_registered = production.register(plan.clone()).unwrap();
        let mut mismatched_registration = production_registered.clone();
        mismatched_registration.preregistration.digests.harness =
            format!("sha256:{}", "d".repeat(64));
        let (experiment, task_set, dataset) = mismatched_registration
            .preregistration
            .derived_design_digests()
            .unwrap();
        mismatched_registration.preregistration.digests.experiment = experiment;
        mismatched_registration.preregistration.digests.task_set = task_set;
        mismatched_registration.preregistration.digests.dataset = dataset;
        assert!(matches!(
            production.admit_next(&mismatched_registration),
            Err(crate::evaluation::ProductionEvaluationError::Unavailable(
                "preregistered production execution pins do not match the configured pins"
            ))
        ));
        drop(production);
        for field in 0..6 {
            let mut changed_pins = pins.clone();
            let replacement = format!(
                "sha256:{}",
                char::from(b'1' + field as u8).to_string().repeat(64)
            );
            match field {
                0 => changed_pins.harness = replacement,
                1 => changed_pins.grader_manifest = replacement,
                2 => changed_pins.helper = replacement,
                3 => changed_pins.runtime = "changed-runtime-after-restart".to_owned(),
                4 => changed_pins.agent_image = replacement,
                5 => changed_pins.grader_image = replacement,
                _ => unreachable!(),
            }
            let mut restarted = crate::evaluation::ProductionEvaluationService::open_with_pins(
                &registration_root.0,
                registration_anchor.clone(),
                changed_pins,
            )
            .unwrap();
            assert!(restarted.admit_next(&production_registered).is_err());
        }
        let mut restarted = crate::evaluation::ProductionEvaluationService::open_with_pins(
            &registration_root.0,
            registration_anchor,
            pins.clone(),
        )
        .unwrap();
        restarted.admit_next(&production_registered).unwrap();

        let root = Root::new("production-evaluation-service");
        let database = root.0.join("state.sqlite3");
        drop(crate::test_support::open_service_store(&database).unwrap());
        let anchor = ConformanceLedgerAnchor::source_semantics_fake();
        let mut service = ConformanceEvaluationService::open_with_anchor(&root.0, anchor).unwrap();
        let registered = service.authority_mut().register(plan).unwrap();
        let admission = service.authority_mut().admit_next(&registered).unwrap();
        let run_id = RunId::generate().unwrap();
        let principal = PrincipalId::generate().unwrap();
        let owner = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal,
            FencingToken::new(1),
        );
        service
            .scheduler()
            .register_statistical_trial_run(
                run_id,
                principal,
                "production-evaluation",
                &registered.preregistration.roster[0].config_digest,
            )
            .unwrap();
        let binding = TrialRunBinding {
            trial_id: registered.preregistration.roster[0].trial_id.clone(),
            trial_digest: registered.preregistration_digest.clone(),
            task_digest: registered.preregistration.roster[0]
                .task_manifest_digest
                .clone(),
            model_digest: registered.preregistration.roster[0].model_digest.clone(),
            config_digest: registered.preregistration.roster[0].config_digest.clone(),
            attempt: owner,
            admission: Some(admission.scheduler_token().unwrap()),
        };
        let scheduler = service.scheduler().clone();
        let pending = scheduler
            .admit_statistical_trial_run(run_id, &binding, service.authority_mut())
            .unwrap();
        let (run_config, consumption_receipt) = service
            .authority_mut()
            .consume_scheduler_admission_with_event_start(&registered, &admission, &pending, 8)
            .unwrap();
        scheduler
            .finalize_statistical_trial_anchor(
                &pending,
                &consumption_receipt,
                service.authority_mut(),
            )
            .unwrap();
        populate_production_evidence(&database, run_id, owner, &registered);
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "INSERT INTO events SELECT 'unrelated-after-consumption', 'unrelated-stream', 1, 9,
                 event_type, schema_version, occurred_at, 'unrelated-after-consumption',
                 'unrelated-run', NULL, trace_id, payload, artifacts
                 FROM events WHERE correlation_id = ?1 AND commit_position = 8",
                [run_id.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE commit_watermark SET position = 9 WHERE singleton = 1",
                [],
            )
            .unwrap();
        let usage = SqliteTrialUsageReceiptStore::open(&database).unwrap();
        let mut executor = DurableEvidenceExecutor {
            inner: ConformanceCoreTrialExecutor::source_semantics_fake(
                crate::evaluation::harness::trial_runner::trusted_source_semantics_token(),
            ),
            scheduler: &scheduler,
            usage,
            run_id,
            principal,
            owner,
            database: database.clone(),
            calls: 0,
        };
        let receipt = service
            .run(
                &harness,
                &mut executor,
                StatisticalTrialRequest {
                    run_id,
                    binding: &binding,
                    registered: &registered,
                    admission: &admission,
                    calibration: &calibration.token,
                    patch: REFERENCE,
                },
            )
            .unwrap();
        let calls = executor.calls;
        let repeated = service
            .run(
                &harness,
                &mut executor,
                StatisticalTrialRequest {
                    run_id,
                    binding: &binding,
                    registered: &registered,
                    admission: &admission,
                    calibration: &calibration.token,
                    patch: REFERENCE,
                },
            )
            .unwrap();
        assert_eq!(receipt.digest(), repeated.digest());
        assert_eq!(executor.calls, calls);
        assert_eq!(
            receipt.receipt().outcome,
            crate::evaluation::reports::TrialOutcome::Success,
            "{}",
            receipt.receipt().failure_reason
        );
        let operation = SqliteCoordinatorOperationStore::open(&database)
            .unwrap()
            .load(run_id)
            .unwrap()
            .unwrap();
        let event_evidence: serde_json::Value =
            serde_json::from_slice(operation.events_bytes.as_deref().expect("recorded events"))
                .unwrap();
        let positions = ["scheduler_events", "provider_events", "tool_events"]
            .into_iter()
            .flat_map(|class| event_evidence[class].as_array().unwrap())
            .map(|event| event["event_position"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert!(
            positions
                .iter()
                .all(|position| *position > 8 && *position <= 16)
        );

        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "INSERT INTO events SELECT 'late-event', stream, 17, 17, event_type,
                 schema_version, occurred_at, 'late-event', correlation_id, attempt_id,
                 trace_id, payload, artifacts FROM events WHERE correlation_id = ?1
                 AND commit_position = 16",
                [run_id.to_string()],
            )
            .unwrap();
        assert!(
            SqliteEventEvidenceStore::open(&database)
                .unwrap()
                .trusted_events(run_id, &run_config, 16)
                .is_err()
        );
        let interrupted_admission = service.authority_mut().admit_next(&registered).unwrap();
        let interrupted_run = RunId::generate().unwrap();
        let interrupted_owner = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal,
            FencingToken::new(1),
        );
        scheduler
            .register_statistical_trial_run(
                interrupted_run,
                principal,
                "interrupted-production-evaluation",
                &registered.preregistration.roster[1].config_digest,
            )
            .unwrap();
        let interrupted_binding = TrialRunBinding {
            trial_id: registered.preregistration.roster[1].trial_id.clone(),
            trial_digest: registered.preregistration_digest.clone(),
            task_digest: registered.preregistration.roster[1]
                .task_manifest_digest
                .clone(),
            model_digest: registered.preregistration.roster[1].model_digest.clone(),
            config_digest: registered.preregistration.roster[1].config_digest.clone(),
            attempt: interrupted_owner,
            admission: Some(interrupted_admission.scheduler_token().unwrap()),
        };
        let interrupted_pending = scheduler
            .admit_statistical_trial_run(
                interrupted_run,
                &interrupted_binding,
                service.authority_mut(),
            )
            .unwrap();
        let (interrupted_config, interrupted_consumption) = service
            .authority_mut()
            .consume_scheduler_admission(&registered, &interrupted_admission, &interrupted_pending)
            .unwrap();
        scheduler
            .finalize_statistical_trial_anchor(
                &interrupted_pending,
                &interrupted_consumption,
                service.authority_mut(),
            )
            .unwrap();
        let partial = Spend::new(6, 9, 1, 0, 0);
        let reservation = ReservationId::new(2);
        scheduler
            .reserve(&ReservationRequest {
                id: reservation,
                run_id: interrupted_run,
                principal_id: principal,
                attempt: Some(interrupted_owner),
                idempotency_key: "interrupted-postdispatch".to_owned(),
                kind: AdmissionKind::Model,
                spend: partial,
            })
            .and_then(|_| scheduler.mark_dispatched(reservation))
            .and_then(|_| scheduler.debit(reservation).map(drop))
            .and_then(|_| scheduler.reconcile(reservation, partial).map(drop))
            .unwrap();
        let operations = SqliteCoordinatorOperationStore::open(&database).unwrap();
        operations
            .admitted(
                interrupted_run,
                &interrupted_config,
                &interrupted_consumption,
                "conformance_source_semantics_fake",
            )
            .unwrap();
        operations.executing(interrupted_run).unwrap();
        let policy = registered.preregistration.policies.error_imputation;
        let interrupted = service
            .run(
                &candidate_harness,
                &mut executor,
                StatisticalTrialRequest {
                    run_id: interrupted_run,
                    binding: &interrupted_binding,
                    registered: &registered,
                    admission: &interrupted_admission,
                    calibration: &calibration.token,
                    patch: REFERENCE,
                },
            )
            .unwrap();
        assert_eq!(
            interrupted.receipt().outcome,
            crate::evaluation::reports::TrialOutcome::Error
        );
        assert!(interrupted.receipt().elapsed_millis > 0);
        assert_eq!(interrupted.receipt().cost_usd, policy.max_cost_usd);
        assert_eq!(
            interrupted.receipt().latency_ms,
            policy.max_latency_ms as f64
        );
        assert!(interrupted.receipt().cost_imputed);
        assert!(interrupted.receipt().latency_imputed);
        assert_eq!(
            interrupted
                .receipt()
                .durable_usage
                .as_ref()
                .unwrap()
                .cost_microusd,
            6
        );
        assert_eq!(executor.calls, calls);

        for index in 2..registered.preregistration.roster.len() {
            let admission = service.authority_mut().admit_next(&registered).unwrap();
            let token = admission.scheduler_token().unwrap();
            let pending = crate::runtime::scheduler::PendingStatisticalTrial {
                run_id: RunId::generate().unwrap(),
                admission_token_digest: token.token_digest,
                admission_nonce: token.nonce,
                admission_position: token.authority_position,
                consumption_position: (index + 1) as u64,
                consumption_digest: sha256(format!("terminal-{index}").as_bytes()),
            };
            let (config, _) = service
                .authority_mut()
                .consume_scheduler_admission(&registered, &admission, &pending)
                .unwrap();
            let policy = registered.preregistration.policies.error_imputation;
            service
                .authority_mut()
                .record_terminal_error(
                    &registered,
                    &config,
                    crate::evaluation::reports::EvidenceSource::ConformanceSourceSemantics,
                    &TerminalErrorEvidence {
                        reason: "preregistered execution failure".to_owned(),
                        elapsed_millis: 1,
                        durable_usage: Some(TerminalDurableUsage {
                            cost_microusd: 0,
                            tokens: 0,
                            turns: 0,
                            tool_calls: 0,
                            processes: 0,
                        }),
                        cost_microusd: (policy.max_cost_usd * 1_000_000.0) as u64,
                        cost_imputed: true,
                        latency_millis: policy.max_latency_ms,
                        latency_imputed: true,
                    },
                )
                .unwrap();
        }
        let report = service.build_final_report(&registered).unwrap();
        let repeated = service.build_final_report(&registered).unwrap();
        assert_eq!(report.bytes, repeated.bytes);
        assert_eq!(report.receipt, repeated.receipt);
        assert!(matches!(
            service.verify_release_attestation(&registered, &report),
            Err(crate::evaluation::ProductionEvaluationError::Unavailable(
                "release attestation requires production-trusted evidence"
            ))
        ));
    }

    fn populate_production_evidence(
        database: &PathBuf,
        run_id: RunId,
        owner: AttemptOwnership,
        registered: &RegisteredPreregistration,
    ) {
        let connection = Connection::open(database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE provider_stream_watermark (
                     singleton INTEGER PRIMARY KEY, position INTEGER NOT NULL);
                 INSERT INTO provider_stream_watermark VALUES (1, 1);
                 CREATE TABLE provider_streams (
                     attempt_id TEXT NOT NULL, model_call_id TEXT NOT NULL, fence INTEGER NOT NULL,
                     idempotency_key TEXT NOT NULL, committed_sequence INTEGER NOT NULL,
                     outcome_position INTEGER, outcome BLOB, outcome_artifacts BLOB,
                     PRIMARY KEY (attempt_id, model_call_id, fence, idempotency_key));",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO provider_streams VALUES (?1, ?2, 1, 'provider-effect', 0, 1, X'7b7d', X'5b5d')",
                params![owner.attempt_id.to_string(), "model_call_00000000000000000000000001"],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO stream_heads (stream, version) VALUES (?1, 7)",
                [run_id.to_string()],
            )
            .unwrap();
        let common = json!({
            "schema_version": 1,
            "attempt_id": owner.attempt_id,
            "attempt_fence": 1,
            "model_call_id": "model_call_00000000000000000000000001",
            "reservation_id": "00000000000000000000000000000001",
        });
        let mut intent = common.clone();
        intent["provider"] = "conformance-provider".into();
        intent["model"] = "conformance-model".into();
        intent["model_snapshot_digest"] = registered.preregistration.roster[0]
            .model_digest
            .clone()
            .into();
        intent["config_snapshot_digest"] = registered.preregistration.roster[0]
            .config_digest
            .clone()
            .into();
        let mut outcome = common.clone();
        outcome["status"] = "succeeded".into();
        outcome["charged"] = true.into();
        outcome["provider_request_id"] = "provider-response-1".into();
        outcome["usage"] = json!({
            "tokens": {
                "input_tokens": 4,
                "output_tokens": 2,
                "reasoning_tokens": 3,
                "cached_input_tokens": 0,
                "cache_write_input_tokens": 0
            },
            "cost": {"currency": "USD", "provider_amount": "0.000006"}
        });
        let tool = |kind: &str| {
            json!({
                "schema_version": 1,
                "attempt_id": owner.attempt_id,
                "attempt_fence": 1,
                "invocation_id": "invocation_00000000000000000000000001",
                "kind": kind,
            })
        };
        let events = [
            (2, "run.start", json!({"schema_version": 1})),
            (3, "model_call.intent", intent),
            (4, "model_call.dispatched", common),
            (5, "model_call.outcome", outcome),
            (6, "capability.invocation_intent", tool("intent")),
            (7, "capability.invocation_dispatched", tool("dispatched")),
            (8, "capability.invocation_outcome", tool("outcome")),
        ];
        for (sequence, kind, payload) in events {
            connection
                .execute(
                    "INSERT INTO events (event_id, stream, sequence, commit_position, event_type,
                     schema_version, occurred_at, causation_id, correlation_id, attempt_id,
                     trace_id, payload, artifacts)
                     VALUES (?1, ?2, ?3, ?3, ?4, 1, '2026-01-01T00:00:00Z', ?1, ?7, ?5,
                     'evaluation-trace', ?6, X'5b5d')",
                    params![
                        format!("evaluation-event-{sequence}"),
                        run_id.to_string(),
                        sequence,
                        kind,
                        (sequence != 2).then(|| owner.attempt_id.to_string()),
                        serde_json::to_vec(&payload).unwrap(),
                        if sequence == 2 {
                            run_id.to_string()
                        } else {
                            "seeded-before-dispatch".to_owned()
                        }
                    ],
                )
                .unwrap();
        }
        connection
            .execute(
                "UPDATE commit_watermark SET position = 8 WHERE singleton = 1",
                [],
            )
            .unwrap();
    }

    fn append_execution_events(database: &PathBuf, run_id: RunId) -> rusqlite::Result<()> {
        let connection = Connection::open(database)?;
        connection.execute(
            "INSERT INTO events (event_id, stream, sequence, commit_position, event_type,
             schema_version, occurred_at, causation_id, correlation_id, attempt_id, trace_id,
             payload, artifacts)
             SELECT 'execution-event-' || (commit_position + 8), stream, sequence + 8,
                    commit_position + 8, event_type, schema_version, occurred_at,
                    'execution-event-' || (commit_position + 8), stream, attempt_id,
                    trace_id, payload, artifacts
             FROM events WHERE stream = ?1 AND commit_position BETWEEN 2 AND 8",
            [run_id.to_string()],
        )?;
        connection.execute(
            "UPDATE commit_watermark SET position = 16 WHERE singleton = 1",
            [],
        )?;
        Ok(())
    }
}
