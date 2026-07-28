use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

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
        })
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
        let report = self.authority.build_report(registered)?;
        self.verify_final_report(registered, &report)?;
        Ok(report)
    }

    pub fn verify_final_report(
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
