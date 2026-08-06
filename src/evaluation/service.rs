use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::Arc,
};

use crate::domain::ids::{ProjectId, RunId};
use crate::runtime::scheduler::DurableScheduler;

use super::{
    CoordinatorError, ProductionStatisticalTrialRequest, SqliteCoordinatorOperationStore,
    SqliteEventEvidenceStore, SqliteProviderEvidenceStore, SqliteToolEvidenceStore,
    SqliteUsageEvidenceStore, StatisticalTrialCoordinator,
    harness::{
        CalibrationSummary, CoreHarness, HarnessCalibrationToken, HarnessError,
        ProductionCoreTrialExecutor,
    },
    reports::{
        EvidenceSource, HarnessExecutionReceipt, LedgerAnchor, Preregistration,
        RegisteredPreregistration, RegistrationAuthority, StatisticalReportEnvelope, StatsError,
        TrialAdmission, sha256,
    },
};

#[cfg(any(test, debug_assertions))]
use super::StatisticalTrialRequest;
pub use super::reports::ProductionEvaluationPins;

pub struct ProductionEvaluationService {
    authority: RegistrationAuthority,
    scheduler: DurableScheduler,
    provider: SqliteProviderEvidenceStore,
    tools: SqliteToolEvidenceStore,
    usage: SqliteUsageEvidenceStore,
    events: SqliteEventEvidenceStore,
    operations: SqliteCoordinatorOperationStore,
    pins: ProductionEvaluationPins,
    calibrations: BTreeMap<(String, String), ProductionCalibrationToken>,
    database: std::path::PathBuf,
    learning_authority: Option<(ProjectId, [u8; 32])>,
}

pub struct ProductionCalibrationToken {
    calibration: HarnessCalibrationToken,
    task_id: String,
    task_manifest_digest: String,
    harness_config_digest: String,
    harness_binding_digest: String,
    pins_digest: String,
    reports: Vec<CalibrationSummary>,
}

impl ProductionCalibrationToken {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn task_manifest_digest(&self) -> &str {
        &self.task_manifest_digest
    }

    pub fn harness_config_digest(&self) -> &str {
        &self.harness_config_digest
    }

    pub fn pins_digest(&self) -> &str {
        &self.pins_digest
    }

    pub fn reports(&self) -> &[CalibrationSummary] {
        &self.reports
    }
}

#[cfg(any(test, debug_assertions))]
pub struct ConformanceEvaluationService(ProductionEvaluationService);

impl ProductionEvaluationService {
    pub fn open(
        state_root: impl AsRef<Path>,
        anchor: Option<Arc<dyn LedgerAnchor>>,
    ) -> Result<Self, ProductionEvaluationError> {
        let anchor = anchor.ok_or(ProductionEvaluationError::Unavailable(
            "production evaluation ledger anchor is not configured",
        ))?;
        let pins = ProductionEvaluationPins {
            harness: std::env::var("KIT_TRIAL_HARNESS_SHA256").unwrap_or_default(),
            grader_manifest: std::env::var("KIT_TRIAL_GRADER_MANIFEST_SHA256").unwrap_or_default(),
            helper: std::env::var("KIT_CONTAINER_HELPER_SHA256").unwrap_or_default(),
            runtime: std::env::var("KIT_CONTAINER_RUNTIME_IDENTITY").unwrap_or_default(),
            agent_image: std::env::var("KIT_TRIAL_AGENT_IMAGE").unwrap_or_default(),
            grader_image: std::env::var("KIT_TRIAL_GRADER_IMAGE").unwrap_or_default(),
        };
        Self::open_with_pins(state_root, anchor, pins)
    }

    pub fn open_with_pins(
        state_root: impl AsRef<Path>,
        anchor: Arc<dyn LedgerAnchor>,
        pins: ProductionEvaluationPins,
    ) -> Result<Self, ProductionEvaluationError> {
        if [
            &pins.harness,
            &pins.grader_manifest,
            &pins.helper,
            &pins.agent_image,
            &pins.grader_image,
        ]
        .into_iter()
        .any(|pin| !valid_pin(pin))
        {
            return Err(ProductionEvaluationError::Unavailable(
                "production evaluation pins are not fully configured",
            ));
        }
        if !valid_identity_pin(&pins.runtime) {
            return Err(ProductionEvaluationError::Unavailable(
                "production evaluation runtime identity is not configured",
            ));
        }
        let state_root = state_root.as_ref();
        let database = state_root.join("state.sqlite3");
        let authority_root = state_root.join("evaluation");
        fs::create_dir_all(&authority_root).map_err(ProductionEvaluationError::Io)?;
        Ok(Self {
            authority: RegistrationAuthority::open_with_anchor(authority_root, anchor)?,
            scheduler: DurableScheduler::open(&database)?,
            provider: SqliteProviderEvidenceStore::open(&database)?,
            tools: SqliteToolEvidenceStore::open(&database)?,
            usage: SqliteUsageEvidenceStore::open(&database)?,
            events: SqliteEventEvidenceStore::open(&database)?,
            operations: SqliteCoordinatorOperationStore::open(&database)?,
            pins,
            calibrations: BTreeMap::new(),
            database,
            learning_authority: None,
        })
    }

    pub(crate) fn with_learning_authority(mut self, project_id: ProjectId, key: [u8; 32]) -> Self {
        self.learning_authority = Some((project_id, key));
        self
    }

    pub fn register(
        &mut self,
        plan: Preregistration,
    ) -> Result<RegisteredPreregistration, ProductionEvaluationError> {
        verify_plan_pins(&plan, &self.pins)?;
        self.authority.register(plan).map_err(Into::into)
    }

    pub fn admit_next(
        &mut self,
        registered: &RegisteredPreregistration,
    ) -> Result<TrialAdmission, ProductionEvaluationError> {
        verify_registration_pins(registered, &self.pins)?;
        self.authority.admit_next(registered).map_err(Into::into)
    }

    pub fn run(
        &mut self,
        harness: &CoreHarness,
        executor: &mut ProductionCoreTrialExecutor<'_>,
        request: ProductionStatisticalTrialRequest<'_>,
    ) -> Result<HarnessExecutionReceipt, ProductionEvaluationError> {
        verify_registration_pins(request.registered, &self.pins)?;
        let admission_token = request.admission.scheduler_token()?;
        if !crate::runtime::scheduler::TrialAdmissionVerifier::verify(
            &self.authority,
            &admission_token,
        ) {
            return Err(ProductionEvaluationError::Unavailable(
                "production admission is not authority-authenticated",
            ));
        }
        let roster = selected_roster(request.registered, &admission_token)?;
        verify_harness_pins(harness, &roster.task_manifest_digest, &self.pins)?;
        verify_harness_arm(harness, roster)?;
        let pins_digest = self.pins.digest()?;
        let calibration_key = (
            roster.task_manifest_digest.clone(),
            harness.harness_config_digest().to_owned(),
        );
        let needs_calibration = self.calibrations.get(&calibration_key).is_none_or(|token| {
            token.pins_digest != pins_digest
                || token.task_id != roster.task_id
                || token.task_manifest_digest != roster.task_manifest_digest
                || token.harness_config_digest != harness.harness_config_digest()
                || token.harness_binding_digest != harness.binding_digest()
        });
        if needs_calibration {
            let pins = self.pins.clone();
            let task_manifest_digest = roster.task_manifest_digest.clone();
            let validation = harness
                .calibrate_verified(executor, |report| {
                    verify_report_pins(report, &task_manifest_digest, &pins)
                        .and_then(|_| verify_report_arm(report, roster))
                        .map_err(|_| HarnessError::AttestationMismatch)
                })
                .map_err(|error| {
                    ProductionEvaluationError::Coordinator(CoordinatorError::Harness(error))
                })?;
            self.calibrations.insert(
                calibration_key.clone(),
                ProductionCalibrationToken {
                    calibration: validation.token,
                    task_id: roster.task_id.clone(),
                    task_manifest_digest: roster.task_manifest_digest.clone(),
                    harness_config_digest: harness.harness_config_digest().to_owned(),
                    harness_binding_digest: harness.binding_digest().to_owned(),
                    pins_digest,
                    reports: validation.cases,
                },
            );
        }
        let pins = self.pins.clone();
        let task_manifest_digest = roster.task_manifest_digest.clone();
        let calibration = &self
            .calibrations
            .get(&calibration_key)
            .expect("production calibration was populated")
            .calibration;
        StatisticalTrialCoordinator::new(
            &mut self.authority,
            &self.scheduler,
            harness,
            &self.provider,
            &self.tools,
            &self.events,
            &self.usage,
        )
        .with_operations(&self.operations)
        .run_production_verified(executor, request, calibration, |report| {
            verify_report_pins(report, &task_manifest_digest, &pins)
                .and_then(|_| verify_report_arm(report, roster))
                .and_then(|_| {
                    if report.report.production_pins_digest.as_deref()
                        == Some(pins.digest()?.as_str())
                    {
                        Ok(())
                    } else {
                        Err(ProductionEvaluationError::Unavailable(
                            "production report pins commitment mismatch",
                        ))
                    }
                })
                .map_err(|_| {
                    CoordinatorError::Evidence("production execution pin attestation mismatch")
                })
        })
        .map_err(Into::into)
    }

    pub fn build_final_report(
        &mut self,
        registered: &RegisteredPreregistration,
    ) -> Result<StatisticalReportEnvelope, ProductionEvaluationError> {
        let ledger_cutoff = self.authority.freeze_experiment(registered)?;
        let report = if supports_learning_report(registered) {
            let learning = self.load_learning_analysis(registered, ledger_cutoff)?;
            let report = self
                .authority
                .build_report_with_learning(registered, Some(learning.clone()))?;
            self.verify_final_report_with_learning(registered, &report, &learning)?;
            report
        } else {
            let report = self.authority.build_report(registered)?;
            self.verify_final_report_evidence(registered, &report)?;
            report
        };
        Ok(report)
    }

    pub fn verify_final_report(
        &self,
        registered: &RegisteredPreregistration,
        report: &StatisticalReportEnvelope,
    ) -> Result<(), ProductionEvaluationError> {
        self.authority.verify_report(registered, report)?;
        if supports_learning_report(registered) {
            let learning = self.load_learning_analysis(registered, report.receipt.ledger_cutoff)?;
            self.verify_final_report_with_learning(registered, report, &learning)
        } else if report.report.learning_analysis.is_none() {
            self.verify_final_report_evidence(registered, report)
        } else {
            Err(ProductionEvaluationError::Unavailable(
                "legacy report version cannot carry learning analysis",
            ))
        }
    }

    fn verify_final_report_with_learning(
        &self,
        registered: &RegisteredPreregistration,
        report: &StatisticalReportEnvelope,
        learning: &crate::telemetry::tool_learning::FrozenLearningAnalysis,
    ) -> Result<(), ProductionEvaluationError> {
        self.verify_final_report_evidence(registered, report)?;
        if report.report.learning_analysis.as_ref() != Some(learning) {
            return Err(ProductionEvaluationError::Unavailable(
                "frozen learning analysis does not match its authority inputs",
            ));
        }
        Ok(())
    }

    fn verify_final_report_evidence(
        &self,
        registered: &RegisteredPreregistration,
        report: &StatisticalReportEnvelope,
    ) -> Result<(), ProductionEvaluationError> {
        self.authority.verify_report(registered, report)?;
        if report.report.evidence_source != EvidenceSource::ProductionTrusted
            || report.receipt.evidence_source != EvidenceSource::ProductionTrusted
            || report
                .report
                .trials
                .iter()
                .any(|trial| trial.evidence_source != Some(EvidenceSource::ProductionTrusted))
        {
            return Err(ProductionEvaluationError::Unavailable(
                "release attestation requires production-trusted evidence",
            ));
        }
        Ok(())
    }

    pub fn scheduler(&self) -> &DurableScheduler {
        &self.scheduler
    }

    pub fn pins(&self) -> &ProductionEvaluationPins {
        &self.pins
    }

    pub fn calibration(
        &self,
        task_manifest_digest: &str,
        harness_config_digest: &str,
    ) -> Option<&ProductionCalibrationToken> {
        self.calibrations.get(&(
            task_manifest_digest.to_owned(),
            harness_config_digest.to_owned(),
        ))
    }

    fn load_learning_analysis(
        &self,
        registered: &RegisteredPreregistration,
        ledger_cutoff: u64,
    ) -> Result<crate::telemetry::tool_learning::FrozenLearningAnalysis, ProductionEvaluationError>
    {
        use crate::telemetry::tool_learning::{
            CausalResult, CausalUnavailable, DownstreamGrade, DownstreamGradeRecord, ExperimentArm,
            FrozenFactors, FrozenLearningAnalysis, LearningPointer, LearningSurface, PointerDomain,
            PreregisteredExperiment, ProjectPointerHasher, SequenceObservation,
            ToolLearningAnalyzer, ToolLearningEvent,
        };

        let unavailable = |reason| FrozenLearningAnalysis {
            result: CausalResult::Unavailable(reason),
            input_digests: BTreeSet::from([registered.preregistration_digest.clone()]),
        };
        let Some((project_id, key)) = self.learning_authority else {
            return Ok(unavailable(CausalUnavailable::MissingAuthority));
        };
        if registered
            .preregistration
            .tool_learning_experiments
            .is_empty()
        {
            return Ok(unavailable(CausalUnavailable::MissingPreregistration));
        }
        let hasher = ProjectPointerHasher::new(project_id, &key);
        let connection = rusqlite::Connection::open(&self.database).map_err(|_| {
            ProductionEvaluationError::Unavailable("learning coordinator records unavailable")
        })?;
        let (operation_count, operation_bytes): (u64, u64) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(
                   length(run_config_bytes)+COALESCE(length(harness_bytes),0)+
                   COALESCE(length(events_bytes),0)+length(execution_receipt_bytes)+
                   COALESCE(length(terminal_evidence_bytes),0)),0)
                 FROM statistical_coordinator_operations
                 WHERE phase IN ('recorded','terminal_error')
                   AND execution_receipt_bytes IS NOT NULL
                   AND json_extract(CAST(run_config_bytes AS TEXT),'$.preregistration_digest')=?1",
                [&registered.preregistration_digest],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| {
                ProductionEvaluationError::Unavailable("learning coordinator records unavailable")
            })?;
        const MAX_ANALYSIS_BYTES: u64 = 64 * 1024 * 1024;
        if operation_count > crate::telemetry::tool_learning::MAX_LEARNING_EVENTS as u64
            || operation_bytes > MAX_ANALYSIS_BYTES
        {
            return Ok(unavailable(CausalUnavailable::BoundExceeded));
        }
        let raw_learning_count: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events AS event
                 JOIN statistical_coordinator_operations AS operation
                   ON operation.run_id=event.correlation_id
                 WHERE event.event_type='tool_learning.recorded'
                   AND operation.phase IN ('recorded','terminal_error')
                   AND json_extract(CAST(operation.run_config_bytes AS TEXT),
                                    '$.preregistration_digest')=?1",
                [&registered.preregistration_digest],
                |row| row.get(0),
            )
            .map_err(|_| ProductionEvaluationError::Unavailable("learning records unavailable"))?;
        let maximum_input_digests = operation_count
            .checked_mul(4)
            .and_then(|count| count.checked_add(raw_learning_count))
            .and_then(|count| count.checked_add(1));
        if maximum_input_digests.is_none_or(|count| {
            count > crate::telemetry::tool_learning::MAX_LEARNING_ANALYSIS_INPUT_DIGESTS
        }) {
            return Ok(unavailable(CausalUnavailable::BoundExceeded));
        }
        let mut statement = connection
            .prepare(
                "SELECT run_id,phase,run_config_digest,run_config_bytes,harness_digest,harness_bytes,
                        events_digest,events_bytes,execution_receipt_digest,execution_receipt_bytes,
                        terminal_evidence_digest,terminal_evidence_bytes,event_source
                  FROM statistical_coordinator_operations
                  WHERE phase IN ('recorded','terminal_error')
                    AND execution_receipt_bytes IS NOT NULL
                    AND json_extract(CAST(run_config_bytes AS TEXT),'$.preregistration_digest')=?1
                   ORDER BY run_id LIMIT 10001",
            )
            .map_err(|_| {
                ProductionEvaluationError::Unavailable("learning coordinator records unavailable")
            })?;
        let rows = statement
            .query_map([&registered.preregistration_digest], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<Vec<u8>>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Vec<u8>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<Vec<u8>>>(11)?,
                    row.get::<_, String>(12)?,
                ))
            })
            .map_err(|_| {
                ProductionEvaluationError::Unavailable("learning coordinator records unavailable")
            })?;
        let mut events = Vec::new();
        let mut experiments = Vec::new();
        let mut grades = Vec::new();
        let mut candidate_count = 0_usize;
        let mut sequence_count = 0_usize;
        let mut schedules = BTreeSet::new();
        let mut input_digests = BTreeSet::from([registered.preregistration_digest.clone()]);
        for row in rows {
            let (
                run_text,
                phase,
                config_digest,
                config_bytes,
                harness_digest,
                harness_bytes,
                events_digest,
                events_bytes,
                execution_digest,
                execution_bytes,
                terminal_evidence_digest,
                terminal_evidence_bytes,
                event_source,
            ) = row.map_err(|_| {
                ProductionEvaluationError::Unavailable("learning coordinator record is corrupt")
            })?;
            let terminal = phase == "terminal_error";
            if !matches!(phase.as_str(), "recorded" | "terminal_error")
                || config_digest != sha256(&config_bytes)
                || execution_digest != sha256(&execution_bytes)
                || event_source != "production_authenticated"
                || (!terminal
                    && (harness_digest.as_deref()
                        != harness_bytes.as_deref().map(sha256).as_deref()
                        || events_digest.as_deref()
                            != events_bytes.as_deref().map(sha256).as_deref()
                        || harness_bytes.is_none()
                        || events_bytes.is_none()
                        || terminal_evidence_bytes.is_some()))
                || (terminal
                    && (harness_bytes.is_some()
                        || events_bytes.is_some()
                        || terminal_evidence_digest.as_deref()
                            != terminal_evidence_bytes.as_deref().map(sha256).as_deref()
                        || terminal_evidence_bytes.is_none()))
            {
                return Err(ProductionEvaluationError::Unavailable(
                    "learning coordinator artifact digest mismatch",
                ));
            }
            let config: super::reports::TrialRunConfig = serde_json::from_slice(&config_bytes)
                .map_err(|_| {
                    ProductionEvaluationError::Unavailable("learning run binding is corrupt")
                })?;
            let harness_bytes = harness_bytes.unwrap_or_default();
            let events_bytes = events_bytes.unwrap_or_default();
            if config.preregistration_digest != registered.preregistration_digest {
                continue;
            }
            let roster = registered
                .preregistration
                .roster
                .get(config.schedule_index)
                .ok_or(ProductionEvaluationError::Unavailable(
                    "learning trial is outside the preregistered roster",
                ))?;
            if roster.trial_id != config.trial_id
                || roster.pair_id != config.pair_id
                || roster.arm != config.arm
                || roster.config_digest != config.config_digest
                || roster.model_digest != config.model_digest
            {
                return Err(ProductionEvaluationError::Unavailable(
                    "learning trial authority binding mismatch",
                ));
            }
            let receipt: super::reports::MeasuredTrialReceipt =
                serde_json::from_slice(&execution_bytes).map_err(|_| {
                    ProductionEvaluationError::Unavailable("learning execution receipt is corrupt")
                })?;
            if receipt.preregistration_digest != registered.preregistration_digest
                || receipt.schedule_index != config.schedule_index
                || receipt.trial_id != config.trial_id
                || receipt.pair_id != config.pair_id
                || receipt.arm != config.arm
                || receipt.scheduler_run_id != run_text
                || receipt.harness_report_digest != sha256(&harness_bytes)
                || receipt.events_digest != sha256(&events_bytes)
                || receipt.recorded_at <= registered.registration.registered_at
                || receipt.authority_position > ledger_cutoff
                || receipt.evidence_source != EvidenceSource::ProductionTrusted
            {
                return Err(ProductionEvaluationError::Unavailable(
                    "learning execution receipt authority mismatch",
                ));
            }
            if !schedules.insert(config.schedule_index) {
                return Err(ProductionEvaluationError::Unavailable(
                    "learning frozen receipt set contains a duplicate trial",
                ));
            }
            let verified = self
                .authority
                .load_harness_trial(registered, &config)?
                .ok_or(ProductionEvaluationError::Unavailable(
                    "learning execution receipt is not stored by the evaluation authority",
                ))?;
            if verified.receipt() != &receipt
                || verified.digest() != execution_digest
                || verified.harness_report_bytes() != harness_bytes
                || verified.events_bytes() != events_bytes
            {
                return Err(ProductionEvaluationError::Unavailable(
                    "learning execution evidence does not match the evaluation authority",
                ));
            }
            let run = RunId::parse(&run_text).map_err(|_| {
                ProductionEvaluationError::Unavailable("learning run identity is corrupt")
            })?;
            let high = receipt.event_high_watermark;
            let mut expected_learning = BTreeMap::new();
            let mut authenticated_latency_ms = None;
            if !terminal {
                let event_manifest: serde_json::Value = serde_json::from_slice(&events_bytes)
                    .map_err(|_| {
                        ProductionEvaluationError::Unavailable("learning event manifest is corrupt")
                    })?;
                let started = event_manifest
                    .get("started_monotonic_millis")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or(ProductionEvaluationError::Unavailable(
                        "learning event manifest has no start time",
                    ))?;
                let latency_ms = event_manifest
                    .get("finished_monotonic_millis")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|finished| finished.checked_sub(started))
                    .ok_or(ProductionEvaluationError::Unavailable(
                        "learning event manifest has invalid latency",
                    ))?;
                if latency_ms == 0
                    || latency_ms > crate::telemetry::tool_learning::MAX_SEQUENCE_LATENCY_MS
                    || receipt.elapsed_millis != latency_ms
                    || receipt.latency_ms != latency_ms as f64
                {
                    return Err(ProductionEvaluationError::Unavailable(
                        "learning receipt latency does not match authenticated events",
                    ));
                }
                authenticated_latency_ms = Some(latency_ms);
                for class in ["scheduler_events", "provider_events", "tool_events"] {
                    let bindings = event_manifest
                        .get(class)
                        .and_then(serde_json::Value::as_array)
                        .ok_or(ProductionEvaluationError::Unavailable(
                            "learning event manifest is corrupt",
                        ))?;
                    if bindings.len() > crate::telemetry::tool_learning::MAX_LEARNING_EVENTS {
                        return Ok(unavailable(CausalUnavailable::BoundExceeded));
                    }
                    for binding in bindings {
                        if binding.get("kind").and_then(serde_json::Value::as_str)
                            == Some("tool_learning.recorded")
                        {
                            let position = binding
                                .get("event_position")
                                .and_then(serde_json::Value::as_u64)
                                .ok_or(ProductionEvaluationError::Unavailable(
                                    "learning event manifest has no position",
                                ))?;
                            let digest = binding
                                .get("event_digest")
                                .and_then(serde_json::Value::as_str)
                                .filter(|digest| {
                                    digest.starts_with("sha256:") && digest.len() == 71
                                })
                                .ok_or(ProductionEvaluationError::Unavailable(
                                    "learning event manifest has no authenticated digest",
                                ))?;
                            if expected_learning
                                .insert(position, digest.to_owned())
                                .is_some()
                            {
                                return Err(ProductionEvaluationError::Unavailable(
                                    "learning event manifest contains a duplicate position",
                                ));
                            }
                        }
                    }
                }
            }
            let mut run_events = Vec::new();
            let run_pointer = hasher.pointer(PointerDomain::Run, run.to_string().as_bytes());
            if !terminal {
                let (raw_count, raw_bytes, raw_candidates): (u64, u64, u64) = connection
                    .query_row(
                        "SELECT COUNT(*),COALESCE(SUM(length(payload)),0),COALESCE(SUM(
                       CASE WHEN json_extract(CAST(payload AS TEXT),'$.event_class')='opportunity'
                       THEN json_array_length(CAST(payload AS TEXT),'$.candidates') ELSE 0 END),0)
                     FROM events
                     WHERE correlation_id=?1 AND event_type='tool_learning.recorded'",
                        [run.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(|_| {
                        ProductionEvaluationError::Unavailable("learning records unavailable")
                    })?;
                if raw_count > crate::telemetry::tool_learning::MAX_LEARNING_EVENTS as u64
                    || raw_bytes > MAX_ANALYSIS_BYTES
                    || raw_candidates > crate::telemetry::tool_learning::MAX_LEARNING_EVENTS as u64
                    || events.len().saturating_add(raw_count as usize)
                        > crate::telemetry::tool_learning::MAX_LEARNING_EVENTS
                {
                    return Ok(unavailable(CausalUnavailable::BoundExceeded));
                }
                if raw_count as usize != expected_learning.len() {
                    return Err(ProductionEvaluationError::Unavailable(
                        "learning records do not exactly match authenticated event evidence",
                    ));
                }
                let mut event_statement = connection
                    .prepare(
                        "SELECT commit_position,event_id,stream,correlation_id,payload FROM events
                     WHERE correlation_id=?1 AND event_type='tool_learning.recorded'
                     ORDER BY commit_position",
                    )
                    .map_err(|_| {
                        ProductionEvaluationError::Unavailable("learning records unavailable")
                    })?;
                let records = event_statement
                    .query_map([run.to_string()], |row| {
                        Ok((
                            row.get::<_, u64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Vec<u8>>(4)?,
                        ))
                    })
                    .map_err(|_| {
                        ProductionEvaluationError::Unavailable("learning records unavailable")
                    })?;
                for stored in records {
                    let (position, event_id, stream, correlation, payload) =
                        stored.map_err(|_| {
                            ProductionEvaluationError::Unavailable("learning record is corrupt")
                        })?;
                    if position <= config.event_start_watermark
                        || position > high
                        || expected_learning.remove(&position).as_deref()
                            != Some(sha256(&payload).as_str())
                    {
                        return Err(ProductionEvaluationError::Unavailable(
                            "learning record is missing, extra, or relocated",
                        ));
                    }
                    let event: ToolLearningEvent =
                        serde_json::from_slice(&payload).map_err(|_| {
                            ProductionEvaluationError::Unavailable("learning record is corrupt")
                        })?;
                    event.validate_with(&hasher).map_err(|_| {
                        ProductionEvaluationError::Unavailable(
                            "learning record authentication failed",
                        )
                    })?;
                    if let ToolLearningEvent::Opportunity { candidates, .. } = &event {
                        candidate_count = candidate_count.saturating_add(candidates.len());
                        if candidate_count > crate::telemetry::tool_learning::MAX_LEARNING_EVENTS {
                            return Ok(unavailable(CausalUnavailable::BoundExceeded));
                        }
                    }
                    if stream != run_text
                        || correlation != run_text
                        || event.common().run != run_pointer
                        || event_id
                            != crate::domain::ids::EventId::from_stable_bytes(
                                event.common().event_id.as_str().as_bytes(),
                            )
                            .to_string()
                    {
                        return Err(ProductionEvaluationError::Unavailable(
                            "learning record row binding mismatch",
                        ));
                    }
                    input_digests.insert(sha256(&payload));
                    run_events.push(event);
                }
                if !expected_learning.is_empty() {
                    return Err(ProductionEvaluationError::Unavailable(
                        "authenticated learning event evidence is missing a raw record",
                    ));
                }
            }
            let grade = if receipt.harm_receipt.is_some() {
                DownstreamGrade::Harmful
            } else if receipt.outcome == super::reports::TrialOutcome::Success
                && receipt.verification_passed
            {
                DownstreamGrade::Passed
            } else {
                DownstreamGrade::Failed
            };
            let cost_microusd = (receipt.cost_usd * 1_000_000.0).round() as u64;
            for plan in registered
                .preregistration
                .tool_learning_experiments
                .iter()
                .filter(|plan| plan.pair_id == config.pair_id)
            {
                if experiments.len() >= crate::telemetry::tool_learning::MAX_LEARNING_EVENTS
                    || grades.len() >= crate::telemetry::tool_learning::MAX_LEARNING_EVENTS
                {
                    return Ok(unavailable(CausalUnavailable::BoundExceeded));
                }
                sequence_count = sequence_count
                    .saturating_add(plan.baseline_sequence.len() + plan.candidate_sequence.len());
                if sequence_count > crate::telemetry::tool_learning::MAX_LEARNING_EVENTS {
                    return Ok(unavailable(CausalUnavailable::BoundExceeded));
                }
                let parse = |value: &str, domain| {
                    let pointer = LearningPointer::parse(value.to_owned()).map_err(|_| {
                        ProductionEvaluationError::Unavailable(
                            "learning preregistration pointer is invalid",
                        )
                    })?;
                    hasher.validate(&pointer, domain).map_err(|_| {
                        ProductionEvaluationError::Unavailable(
                            "learning preregistration authority mismatch",
                        )
                    })?;
                    Ok::<_, ProductionEvaluationError>(pointer)
                };
                let experiment = parse(&plan.experiment, PointerDomain::Experiment)?;
                let capability = parse(&plan.capability, PointerDomain::Capability)?;
                let schema = parse(&plan.schema, PointerDomain::Schema)?;
                let declaration_artifact = parse(&plan.frozen_factors, PointerDomain::Artifact)?;
                let step_surface = |surface: &str| match surface {
                    "eager" => Ok(LearningSurface::Eager),
                    "deferred" => Ok(LearningSurface::Deferred),
                    "generic" => Ok(LearningSurface::Generic),
                    "discovery" => Ok(LearningSurface::Discovery),
                    _ => Err(ProductionEvaluationError::Unavailable(
                        "learning preregistration surface is invalid",
                    )),
                };
                let expected = match config.arm {
                    super::reports::Arm::Baseline => &plan.baseline_sequence,
                    super::reports::Arm::Candidate => &plan.candidate_sequence,
                }
                .iter()
                .map(|step| {
                    Ok::<_, ProductionEvaluationError>(
                        crate::telemetry::tool_learning::PreregisteredSequenceStep {
                            capability: parse(&step.capability, PointerDomain::Capability)?,
                            schema: parse(&step.schema, PointerDomain::Schema)?,
                            surface: step_surface(&step.surface)?,
                            ordinal: step.ordinal,
                        },
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
                let sequence = (!expected.is_empty())
                    .then(|| {
                        crate::telemetry::tool_learning::bind_sequence_calls(
                            &run_events,
                            &run_pointer,
                            &expected,
                        )
                        .and_then(|(_, calls)| {
                            calls.iter().try_fold(0_u64, |total, expected| {
                                run_events.iter().find_map(|event| match event {
                                    ToolLearningEvent::Outcome {
                                        call,
                                        status: crate::telemetry::tool_learning::LearningStatus::Succeeded,
                                        known: true,
                                        cost_microusd: Some(cost),
                                        ..
                                    } if call == expected => total.checked_add(*cost),
                                    _ => None,
                                })
                            })
                        })
                        .filter(|cost| {
                            *cost
                                <= crate::telemetry::tool_learning::MAX_SEQUENCE_COST_MICROUSD
                        })
                        .zip(authenticated_latency_ms)
                        .map(|(cost_microusd, latency_ms)| SequenceObservation {
                            cost_microusd,
                            latency_ms,
                        })
                    })
                    .flatten();
                let surface = step_surface(&plan.surface)?;
                let mut authorized = false;
                let mut offered = false;
                let mut offered_factors = BTreeSet::new();
                for event in &run_events {
                    if let ToolLearningEvent::Opportunity {
                        offered: offered_count,
                        eager,
                        deferred,
                        generic_available,
                        candidates,
                        ..
                    } = event
                    {
                        offered_factors.insert((
                            offered_count,
                            eager,
                            deferred,
                            generic_available,
                            candidates,
                        ));
                        for candidate in candidates.iter().filter(|candidate| {
                            candidate.capability == capability
                                && candidate.schema == schema
                                && candidate.surface == surface
                        }) {
                            authorized |= candidate.authorized;
                            offered |= candidate.offered;
                        }
                    }
                }
                let actual_configuration = serde_json::to_vec(&serde_json::json!({
                    "task_id": config.task_id,
                    "dataset_member_id": config.dataset_member_id,
                    "task_manifest_digest": config.task_manifest_digest,
                    "task_set_digest": registered.preregistration.digests.task_set,
                    "dataset_digest": registered.preregistration.digests.dataset,
                    "model_digest": config.model_digest,
                    "model_settings_digest": config.model_settings_digest,
                    "config_digest": config.config_digest,
                    "seed": config.seed,
                    "harness_digest": registered.preregistration.digests.harness,
                    "execution_environment_digest": registered.preregistration.execution_environment.digest,
                    "alpha": registered.preregistration.alpha,
                    "ci_method": registered.preregistration.ci_method,
                    "noninferiority": registered.preregistration.noninferiority,
                    "policies": registered.preregistration.policies,
                    "capability": capability,
                    "schema": schema,
                    "surface": plan.surface,
                    "expected_sequence": expected,
                    "offered_tools": offered_factors,
                    "provider_capability_digest": (!plan.description_only)
                        .then_some(&config.provider_capability_digest),
                }))
                .map_err(|_| {
                    ProductionEvaluationError::Unavailable(
                        "learning actual arm configuration is not canonical",
                    )
                })?;
                let frozen_factors = FrozenFactors {
                    canonical_actual_config_digest: sha256(&actual_configuration),
                    arm_config: hasher.pointer(PointerDomain::Artifact, &config_bytes),
                    receipt: hasher.pointer(PointerDomain::Artifact, execution_digest.as_bytes()),
                    declaration_artifact,
                };
                experiments.push(PreregisteredExperiment {
                    experiment: experiment.clone(),
                    run: run_pointer.clone(),
                    arm: match config.arm {
                        super::reports::Arm::Baseline => ExperimentArm::Direct,
                        super::reports::Arm::Candidate => ExperimentArm::Competing,
                    },
                    capability,
                    schema,
                    surface,
                    authorized,
                    offered,
                    description_only: plan.description_only,
                    frozen_factors,
                    expected_sequence: expected,
                });
                if !terminal {
                    grades.push(DownstreamGradeRecord {
                        experiment,
                        run: run_pointer.clone(),
                        grade,
                        cost_microusd,
                        latency_ms: receipt.latency_ms.round() as u64,
                        receipt: hasher
                            .pointer(PointerDomain::Artifact, execution_digest.as_bytes()),
                        harm_receipt: receipt.harm_receipt,
                        sequence,
                    });
                }
            }
            input_digests.extend([config_digest, execution_digest]);
            input_digests.extend(harness_digest);
            input_digests.extend(events_digest);
            input_digests.extend(terminal_evidence_digest);
            events.extend(run_events);
        }
        if schedules.len() != registered.preregistration.roster.len() {
            return Err(ProductionEvaluationError::Unavailable(
                "learning frozen receipt set is incomplete",
            ));
        }
        if events.is_empty() {
            return Ok(FrozenLearningAnalysis {
                result: CausalResult::Unavailable(CausalUnavailable::MissingLearningRecords),
                input_digests,
            });
        }
        Ok(FrozenLearningAnalysis {
            result: ToolLearningAnalyzer::new(crate::telemetry::tool_learning::MAX_LEARNING_EVENTS)
                .analyze(&events, &experiments, &grades),
            input_digests,
        })
    }
}

fn supports_learning_report(registered: &RegisteredPreregistration) -> bool {
    registered.schema_version == "1.1" && registered.preregistration.schema_version == "1.2"
}

#[cfg(any(test, debug_assertions))]
impl ConformanceEvaluationService {
    pub fn open_with_anchor(
        state_root: impl AsRef<Path>,
        anchor: Arc<dyn LedgerAnchor>,
    ) -> Result<Self, ProductionEvaluationError> {
        let pin = format!("sha256:{}", "1".repeat(64));
        ProductionEvaluationService::open_with_pins(
            state_root,
            anchor,
            ProductionEvaluationPins {
                harness: pin.clone(),
                grader_manifest: pin.clone(),
                helper: pin.clone(),
                runtime: pin.clone(),
                agent_image: pin.clone(),
                grader_image: pin,
            },
        )
        .map(Self)
    }

    pub fn run(
        &mut self,
        harness: &CoreHarness,
        executor: &mut impl super::harness::CoreTrialExecutor,
        request: StatisticalTrialRequest<'_>,
    ) -> Result<HarnessExecutionReceipt, ProductionEvaluationError> {
        StatisticalTrialCoordinator::new(
            &mut self.0.authority,
            &self.0.scheduler,
            harness,
            &self.0.provider,
            &self.0.tools,
            &self.0.events,
            &self.0.usage,
        )
        .with_operations(&self.0.operations)
        .run(executor, request)
        .map_err(Into::into)
    }

    pub fn authority_mut(&mut self) -> &mut RegistrationAuthority {
        &mut self.0.authority
    }

    pub fn scheduler(&self) -> &DurableScheduler {
        &self.0.scheduler
    }

    pub fn build_final_report(
        &mut self,
        registered: &RegisteredPreregistration,
    ) -> Result<StatisticalReportEnvelope, ProductionEvaluationError> {
        self.0
            .authority
            .build_report(registered)
            .map_err(Into::into)
    }

    pub fn verify_release_attestation(
        &self,
        registered: &RegisteredPreregistration,
        report: &StatisticalReportEnvelope,
    ) -> Result<(), ProductionEvaluationError> {
        self.0.verify_final_report(registered, report)
    }
}

pub(crate) fn verify_harness_pins(
    harness: &CoreHarness,
    task_manifest_digest: &str,
    pins: &ProductionEvaluationPins,
) -> Result<(), ProductionEvaluationError> {
    if harness.harness_config_digest() != pins.harness
        || harness.task_manifest_digest() != task_manifest_digest
        || harness.grader_manifest_digest() != pins.grader_manifest
        || harness.agent_image_digest() != pins.agent_image
        || harness.grader_image_digest() != pins.grader_image
    {
        return Err(ProductionEvaluationError::Unavailable(
            "production harness pins do not match the configured pins",
        ));
    }
    Ok(())
}

pub(crate) fn verify_report_pins(
    report_envelope: &super::harness::ReportEnvelope,
    task_manifest_digest: &str,
    pins: &ProductionEvaluationPins,
) -> Result<(), ProductionEvaluationError> {
    let report = &report_envelope.report;
    if report.harness_config_digest != pins.harness
        || report.task_manifest_digest != task_manifest_digest
        || report.grader_manifest_digest != pins.grader_manifest
        || report.grader_image_digest != pins.grader_image
        || report.agent.image_digest != pins.agent_image
        || report.grader.image_digest != pins.grader_image
        || report.agent.helper_identity != pins.helper
        || report.grader.helper_identity != pins.helper
        || report.agent.runtime_identity != pins.runtime
        || report.grader.runtime_identity != pins.runtime
        || report.agent.route == crate::executor::trial::ExecutionRoute::ConformanceFake
        || report.grader.route == crate::executor::trial::ExecutionRoute::ConformanceFake
        || report.patch_digest != report_envelope.artifacts.applied_patch.digest
        || report.final_tree_digest != report_envelope.artifacts.final_tree.digest
        || report.agent_result_digest != report_envelope.artifacts.agent_output.digest
        || report.events_digest != report_envelope.artifacts.events.digest
        || report.logs_digest != report_envelope.artifacts.logs.digest
        || report.artifacts_digest != report_envelope.artifacts.artifacts_digest
        || report_envelope.digest != sha256(&report_envelope.bytes)
    {
        return Err(ProductionEvaluationError::Unavailable(
            "production execution pin attestation mismatch",
        ));
    }
    Ok(())
}

fn verify_harness_arm(
    harness: &CoreHarness,
    roster: &super::reports::RosterEntry,
) -> Result<(), ProductionEvaluationError> {
    if harness.model_digest() != roster.model_digest
        || harness.model_settings_digest() != roster.model_settings_digest
        || harness.config_digest() != roster.config_digest
        || harness.provider_capability_digest() != roster.provider_capability_digest
    {
        return Err(ProductionEvaluationError::Unavailable(
            "production harness arm pins do not match the selected roster entry",
        ));
    }
    Ok(())
}

fn verify_report_arm(
    report: &super::harness::ReportEnvelope,
    roster: &super::reports::RosterEntry,
) -> Result<(), ProductionEvaluationError> {
    if report.report.model_digest != roster.model_digest
        || report.report.model_settings_digest != roster.model_settings_digest
        || report.report.config_digest != roster.config_digest
        || report.report.provider_capability_digest != roster.provider_capability_digest
    {
        return Err(ProductionEvaluationError::Unavailable(
            "production report arm pins do not match the selected roster entry",
        ));
    }
    Ok(())
}

fn verify_plan_pins(
    plan: &Preregistration,
    pins: &ProductionEvaluationPins,
) -> Result<(), ProductionEvaluationError> {
    if plan.digests.harness != pins.harness
        || plan.execution_environment.pins != *pins
        || plan.execution_environment.digest != pins.digest()?
    {
        return Err(ProductionEvaluationError::Unavailable(
            "preregistered production execution pins do not match the configured pins",
        ));
    }
    Ok(())
}

fn verify_registration_pins(
    registered: &RegisteredPreregistration,
    pins: &ProductionEvaluationPins,
) -> Result<(), ProductionEvaluationError> {
    verify_plan_pins(&registered.preregistration, pins)
}

fn selected_roster<'a>(
    registered: &'a RegisteredPreregistration,
    token: &crate::runtime::scheduler::TrialAdmissionToken,
) -> Result<&'a super::reports::RosterEntry, ProductionEvaluationError> {
    registered
        .preregistration
        .roster
        .get(token.schedule_index)
        .filter(|roster| {
            token.preregistration_digest == registered.preregistration_digest
                && token.trial_id == roster.trial_id
                && token.pair_id == roster.pair_id
                && token.task_id == roster.task_id
                && token.dataset_member_id == roster.dataset_member_id
                && token.task_manifest_digest == roster.task_manifest_digest
                && token.seed == roster.seed
                && token.arm
                    == match roster.arm {
                        super::reports::Arm::Baseline => "baseline",
                        super::reports::Arm::Candidate => "candidate",
                    }
        })
        .ok_or(ProductionEvaluationError::Unavailable(
            "production admission task pin does not match the selected roster entry",
        ))
}

fn valid_pin(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && value[7..].bytes().any(|byte| byte != b'0')
}

fn valid_identity_pin(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'\0')
}

#[derive(Debug)]
pub enum ProductionEvaluationError {
    Unavailable(&'static str),
    Io(std::io::Error),
    Coordinator(CoordinatorError),
    Statistics(StatsError),
    Scheduler(crate::runtime::scheduler::SchedulerError),
}

impl std::fmt::Display for ProductionEvaluationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => {
                write!(formatter, "production evaluation unavailable: {detail}")
            }
            Self::Io(error) => error.fmt(formatter),
            Self::Coordinator(error) => error.fmt(formatter),
            Self::Statistics(error) => error.fmt(formatter),
            Self::Scheduler(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ProductionEvaluationError {}

impl From<CoordinatorError> for ProductionEvaluationError {
    fn from(error: CoordinatorError) -> Self {
        Self::Coordinator(error)
    }
}

impl From<StatsError> for ProductionEvaluationError {
    fn from(error: StatsError) -> Self {
        Self::Statistics(error)
    }
}

impl From<crate::runtime::scheduler::SchedulerError> for ProductionEvaluationError {
    fn from(error: crate::runtime::scheduler::SchedulerError) -> Self {
        Self::Scheduler(error)
    }
}
