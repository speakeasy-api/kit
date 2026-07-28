#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::Arc,
    time::Duration,
};

#[cfg(any(test, debug_assertions))]
use std::sync::Mutex;

use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_PREREGISTRATION_BYTES: usize = 256 * 1024;
const MAX_HARNESS_REPORT_BYTES: usize = 64 * 1024;
const MAX_EVENTS_BYTES: usize = 64 * 1024;
const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;
const MAX_TRIALS: usize = 10_000;
const MIN_COMPLETE_PAIRS: usize = 3;
const AUTHORITY_EPOCH_FLOOR: &str = "2020-01-01T00:00:00Z";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const COMPONENT_SCHEMA: &[u8] =
    include_bytes!("../../preregistration/schema/v1/components.schema.json");

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Preregistration {
    pub schema_version: String,
    pub kind: String,
    pub experiment_id: String,
    pub digests: DesignDigests,
    pub execution_environment: ProductionExecutionEnvironment,
    pub roster: Vec<RosterEntry>,
    pub primary_hypothesis: Hypothesis,
    pub primary_metric: MetricSpec,
    pub exploratory_metrics: Vec<MetricSpec>,
    pub sample_size: SampleSize,
    pub alpha: f64,
    pub ci_method: CiMethod,
    pub noninferiority: NoninferiorityPlan,
    pub policies: Policies,
}

impl Preregistration {
    pub fn from_json(bytes: &[u8]) -> Result<Self, StatsError> {
        if bytes.is_empty() || bytes.len() > MAX_PREREGISTRATION_BYTES {
            return Err(StatsError::BoundExceeded("preregistration bytes"));
        }
        validate_schema(bytes, "preregistration")?;
        let plan: Self = serde_json::from_slice(bytes)
            .map_err(|error| StatsError::InvalidPreregistration(error.to_string()))?;
        plan.validate()?;
        Ok(plan)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, StatsError> {
        self.validate()?;
        canonical_bytes(self)
    }

    pub fn digest(&self) -> Result<String, StatsError> {
        Ok(sha256(&self.canonical_bytes()?))
    }

    pub fn derived_design_digests(&self) -> Result<(String, String, String), StatsError> {
        let task_pins = self
            .roster
            .iter()
            .map(TaskPinCommitment::from)
            .collect::<Vec<_>>();
        let task_set = sha256(&canonical_bytes(&TaskSetCommitment {
            domain: "kit-task-set-v1",
            entries: &task_pins,
        })?);
        let dataset = sha256(&canonical_bytes(&DatasetCommitment {
            domain: "kit-dataset-roster-v2",
            roster: &self.roster,
        })?);
        let experiment = sha256(&canonical_bytes(&ExperimentDesignCommitment {
            domain: "kit-experiment-design-v2",
            experiment_id: &self.experiment_id,
            task_set: &task_set,
            dataset: &dataset,
            harness: &self.digests.harness,
            execution_environment: &self.execution_environment.digest,
            task_pins: &task_pins,
            roster: &self.roster,
            primary_hypothesis: &self.primary_hypothesis,
            primary_metric: &self.primary_metric,
            exploratory_metrics: &self.exploratory_metrics,
            sample_size: &self.sample_size,
            alpha: self.alpha,
            ci_method: self.ci_method,
            noninferiority: &self.noninferiority,
            policies: &self.policies,
        })?);
        Ok((experiment, task_set, dataset))
    }

    fn validate(&self) -> Result<(), StatsError> {
        if self.schema_version != "1.1"
            || self.kind != "preregistration"
            || !valid_id(&self.experiment_id)
            || placeholder(&self.experiment_id)
        {
            return invalid_plan("unsupported version, kind, or experiment id");
        }
        self.digests.validate()?;
        self.execution_environment.validate()?;
        if self.digests.harness != self.execution_environment.pins.harness {
            return invalid_plan(
                "design harness digest does not match production execution harness pin",
            );
        }
        if self.roster.len() != self.sample_size.complete_pairs.saturating_mul(2)
            || self.roster.len() > MAX_TRIALS
            || self.sample_size.complete_pairs < MIN_COMPLETE_PAIRS
            || self.sample_size.complete_pairs > MAX_TRIALS / 2
            || !valid_text(&self.sample_size.power_rationale, 4096)
            || placeholder(&self.sample_size.power_rationale)
        {
            return invalid_plan("invalid fixed sample size or rationale");
        }
        let mut trial_ids = BTreeSet::new();
        let mut pair_arms: BTreeMap<&str, BTreeSet<Arm>> = BTreeMap::new();
        let mut task_manifests = BTreeMap::new();
        let mut manifest_tasks = BTreeMap::new();
        for (index, entry) in self.roster.iter().enumerate() {
            if entry.schedule_index != index
                || !entry.valid()
                || !trial_ids.insert(entry.trial_id.as_str())
                || task_manifests
                    .insert(&entry.task_id, &entry.task_manifest_digest)
                    .is_some_and(|manifest| manifest != &entry.task_manifest_digest)
                || manifest_tasks
                    .insert(&entry.task_manifest_digest, &entry.task_id)
                    .is_some_and(|task| task != &entry.task_id)
                || !pair_arms
                    .entry(&entry.pair_id)
                    .or_default()
                    .insert(entry.arm)
            {
                return invalid_plan("roster is not a unique fixed schedule");
            }
        }
        if pair_arms.len() != self.sample_size.complete_pairs
            || pair_arms
                .values()
                .any(|arms| arms != &BTreeSet::from([Arm::Baseline, Arm::Candidate]))
        {
            return invalid_plan("every pair must schedule each arm exactly once");
        }
        for pair_id in pair_arms.keys() {
            let entries = self
                .roster
                .iter()
                .filter(|entry| &entry.pair_id == pair_id)
                .collect::<Vec<_>>();
            if entries.len() != 2
                || entries[0].task_id != entries[1].task_id
                || entries[0].dataset_member_id != entries[1].dataset_member_id
                || entries[0].task_manifest_digest != entries[1].task_manifest_digest
                || entries[0].seed != entries[1].seed
            {
                return invalid_plan(
                    "paired arms must have identical task, dataset, manifest, and seed",
                );
            }
        }
        if self.primary_metric.role != MetricRole::Confirmatory
            || !self.primary_metric.valid()
            || self.primary_hypothesis.role != MetricRole::Confirmatory
            || self.primary_hypothesis.metric != self.primary_metric.metric
            || self.primary_hypothesis.direction != self.primary_metric.metric.direction()
            || !self.primary_hypothesis.valid()
        {
            return invalid_plan(
                "exactly one directionally consistent primary hypothesis is required",
            );
        }
        if self.exploratory_metrics.len() > CoreMetric::ALL.len() - 1
            || !strictly_ordered_by(&self.exploratory_metrics, |metric| metric.metric)
            || self.exploratory_metrics.iter().any(|metric| {
                metric.role != MetricRole::Exploratory
                    || !metric.valid()
                    || metric.metric == self.primary_metric.metric
            })
        {
            return invalid_plan(
                "exploratory metrics must be unique, ordered, and explicitly exploratory",
            );
        }
        if self.alpha != 0.05
            || self.ci_method != CiMethod::FiniteSamplePairedV1
            || self.noninferiority.metric != self.primary_metric.metric
            || self.noninferiority.direction != self.primary_metric.metric.direction()
            || !self.noninferiority.valid()
        {
            return invalid_plan("invalid confirmatory method, direction, or margin");
        }
        self.policies.validate()?;
        let (experiment, task_set, dataset) = self.derived_design_digests()?;
        if self.digests.experiment != experiment
            || self.digests.task_set != task_set
            || self.digests.dataset != dataset
        {
            return invalid_plan(
                "supplied roster and experiment commitments do not match the canonical plan",
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesignDigests {
    pub experiment: String,
    pub task_set: String,
    pub harness: String,
    pub dataset: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionEvaluationPins {
    pub harness: String,
    pub grader_manifest: String,
    pub helper: String,
    pub runtime: String,
    pub agent_image: String,
    pub grader_image: String,
}

impl ProductionEvaluationPins {
    pub fn digest(&self) -> Result<String, StatsError> {
        self.validate()?;
        Ok(sha256(&canonical_bytes(self)?))
    }

    fn validate(&self) -> Result<(), StatsError> {
        if [
            &self.harness,
            &self.grader_manifest,
            &self.helper,
            &self.agent_image,
            &self.grader_image,
        ]
        .into_iter()
        .all(|pin| valid_digest(pin))
            && valid_identity_pin(&self.runtime)
        {
            Ok(())
        } else {
            invalid_plan("production execution pins are invalid")
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionExecutionEnvironment {
    pub digest: String,
    pub pins: ProductionEvaluationPins,
}

impl ProductionExecutionEnvironment {
    pub fn new(pins: ProductionEvaluationPins) -> Result<Self, StatsError> {
        let digest = pins.digest()?;
        Ok(Self { digest, pins })
    }

    fn validate(&self) -> Result<(), StatsError> {
        if self.digest != self.pins.digest()? {
            invalid_plan("production execution environment digest mismatch")
        } else {
            Ok(())
        }
    }
}

impl DesignDigests {
    fn validate(&self) -> Result<(), StatsError> {
        if [
            &self.experiment,
            &self.task_set,
            &self.harness,
            &self.dataset,
        ]
        .into_iter()
        .all(|digest| valid_digest(digest))
        {
            Ok(())
        } else {
            invalid_plan("design pins must be non-zero SHA-256 digests")
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RosterEntry {
    pub schedule_index: usize,
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
    pub arm: Arm,
}

impl RosterEntry {
    fn valid(&self) -> bool {
        [
            &self.trial_id,
            &self.pair_id,
            &self.task_id,
            &self.dataset_member_id,
        ]
        .into_iter()
        .all(|value| valid_id(value) && !placeholder(value))
            && valid_digest(&self.task_manifest_digest)
            && valid_digest(&self.model_digest)
            && valid_digest(&self.model_settings_digest)
            && valid_digest(&self.config_digest)
            && valid_digest(&self.provider_capability_digest)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hypothesis {
    pub id: String,
    pub role: MetricRole,
    pub metric: CoreMetric,
    pub direction: Direction,
    pub statement: String,
}

impl Hypothesis {
    fn valid(&self) -> bool {
        valid_id(&self.id)
            && !placeholder(&self.id)
            && valid_text(&self.statement, 4096)
            && !placeholder(&self.statement)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSpec {
    pub metric: CoreMetric,
    pub role: MetricRole,
    pub unit: MetricUnit,
    pub estimand: String,
}

impl MetricSpec {
    fn valid(&self) -> bool {
        self.unit == self.metric.unit()
            && valid_text(&self.estimand, 4096)
            && !placeholder(&self.estimand)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricRole {
    Confirmatory,
    Exploratory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreMetric {
    SuccessRate,
    InterventionRate,
    MeanCostUsd,
    MeanLatencyMs,
    VerificationRate,
}

impl CoreMetric {
    const ALL: [Self; 5] = [
        Self::SuccessRate,
        Self::InterventionRate,
        Self::MeanCostUsd,
        Self::MeanLatencyMs,
        Self::VerificationRate,
    ];

    const fn aggregation(self) -> Aggregation {
        match self {
            Self::SuccessRate | Self::InterventionRate | Self::VerificationRate => {
                Aggregation::Rate
            }
            Self::MeanCostUsd | Self::MeanLatencyMs => Aggregation::Mean,
        }
    }

    const fn direction(self) -> Direction {
        match self {
            Self::SuccessRate | Self::VerificationRate => Direction::HigherIsBetter,
            Self::InterventionRate | Self::MeanCostUsd | Self::MeanLatencyMs => {
                Direction::LowerIsBetter
            }
        }
    }

    const fn unit(self) -> MetricUnit {
        match self {
            Self::SuccessRate | Self::InterventionRate | Self::VerificationRate => {
                MetricUnit::Proportion
            }
            Self::MeanCostUsd => MetricUnit::Usd,
            Self::MeanLatencyMs => MetricUnit::Milliseconds,
        }
    }

    const fn maximum_margin(self) -> f64 {
        match self {
            Self::SuccessRate | Self::InterventionRate | Self::VerificationRate => 0.25,
            Self::MeanCostUsd => 1_000_000.0,
            Self::MeanLatencyMs => 86_400_000.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    Proportion,
    Usd,
    Milliseconds,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    HigherIsBetter,
    LowerIsBetter,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    Baseline,
    Candidate,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SampleSize {
    pub complete_pairs: usize,
    pub power_rationale: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CiMethod {
    FiniteSamplePairedV1,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoninferiorityPlan {
    pub metric: CoreMetric,
    pub margin: f64,
    pub scientific_max_margin: f64,
    pub direction: Direction,
}

impl NoninferiorityPlan {
    fn valid(&self) -> bool {
        self.margin.is_finite()
            && self.margin > 0.0
            && self.scientific_max_margin.is_finite()
            && self.scientific_max_margin > 0.0
            && self.margin <= self.scientific_max_margin
            && self.scientific_max_margin <= self.metric.maximum_margin()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policies {
    pub stopping: StoppingPolicy,
    pub exclusion: ExclusionPolicy,
    pub failure: FailurePolicy,
    pub error_imputation: ErrorImputationPolicy,
}

impl Policies {
    fn validate(&self) -> Result<(), StatsError> {
        if self.stopping != StoppingPolicy::FixedRoster
            || self.exclusion.allowed_reasons.len() > 64
            || !strictly_ordered_by(&self.exclusion.allowed_reasons, Clone::clone)
            || self
                .exclusion
                .allowed_reasons
                .iter()
                .any(|reason| !valid_id(reason) || placeholder(reason))
            || !self.error_imputation.valid()
        {
            invalid_plan("invalid stopping or exclusion policy")
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoppingPolicy {
    FixedRoster,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusionPolicy {
    pub unlisted: UnlistedExclusionPolicy,
    pub allowed_reasons: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnlistedExclusionPolicy {
    Reject,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    IncludeAsObserved,
    FailAnalysis,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorImputationPolicy {
    pub max_cost_usd: f64,
    pub max_latency_ms: u64,
}

impl ErrorImputationPolicy {
    fn valid(self) -> bool {
        self.max_cost_usd.is_finite()
            && self.max_cost_usd >= 0.000_001
            && self.max_cost_usd <= 1_000_000.0
            && self.max_latency_ms > 0
            && self.max_latency_ms <= 86_400_000
    }

    fn max_cost_microusd(self) -> u64 {
        (self.max_cost_usd * 1_000_000.0) as u64
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalDurableUsage {
    pub cost_microusd: u64,
    pub tokens: u64,
    pub turns: u64,
    pub tool_calls: u64,
    pub processes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalErrorEvidence {
    pub reason: String,
    pub elapsed_millis: u64,
    pub durable_usage: Option<TerminalDurableUsage>,
    pub cost_microusd: u64,
    pub cost_imputed: bool,
    pub latency_millis: u64,
    pub latency_imputed: bool,
}

impl TerminalErrorEvidence {
    pub(crate) fn validate(&self, policy: ErrorImputationPolicy) -> Result<(), StatsError> {
        if !valid_text(&self.reason, 4096)
            || self.reason.contains('\0')
            || self.elapsed_millis == 0
            || self.cost_microusd == 0
            || self.latency_millis == 0
            || !self.cost_imputed
            || self.cost_microusd != policy.max_cost_microusd()
            || !self.latency_imputed
            || self.latency_millis != policy.max_latency_ms
        {
            return Err(StatsError::InvalidTrial("invalid terminal error evidence"));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct TaskSetCommitment<'a> {
    domain: &'static str,
    entries: &'a [TaskPinCommitment<'a>],
}

#[derive(Serialize)]
struct TaskPinCommitment<'a> {
    schedule_index: usize,
    trial_id: &'a str,
    task_id: &'a str,
    task_manifest_digest: &'a str,
}

impl<'a> From<&'a RosterEntry> for TaskPinCommitment<'a> {
    fn from(entry: &'a RosterEntry) -> Self {
        Self {
            schedule_index: entry.schedule_index,
            trial_id: &entry.trial_id,
            task_id: &entry.task_id,
            task_manifest_digest: &entry.task_manifest_digest,
        }
    }
}

#[derive(Serialize)]
struct DatasetCommitment<'a> {
    domain: &'static str,
    roster: &'a [RosterEntry],
}

#[derive(Serialize)]
struct ExperimentDesignCommitment<'a> {
    domain: &'static str,
    experiment_id: &'a str,
    task_set: &'a str,
    dataset: &'a str,
    harness: &'a str,
    execution_environment: &'a str,
    task_pins: &'a [TaskPinCommitment<'a>],
    roster: &'a [RosterEntry],
    primary_hypothesis: &'a Hypothesis,
    primary_metric: &'a MetricSpec,
    exploratory_metrics: &'a [MetricSpec],
    sample_size: &'a SampleSize,
    alpha: f64,
    ci_method: CiMethod,
    noninferiority: &'a NoninferiorityPlan,
    policies: &'a Policies,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredPreregistration {
    pub schema_version: String,
    pub kind: String,
    pub preregistration: Preregistration,
    pub preregistration_digest: String,
    pub registration: RegistrationReceipt,
}

impl RegisteredPreregistration {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, StatsError> {
        canonical_bytes(self)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationReceipt {
    pub authority_id: String,
    pub authority_epoch: String,
    pub genesis_digest: String,
    pub sequence: u64,
    pub authority_position: u64,
    pub attempt_ordinal: u64,
    pub registered_at: String,
    pub previous_entry_digest: String,
    pub authentication: Authentication,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Authentication {
    pub algorithm: String,
    pub key_id: String,
    pub tag: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerAnchorReceipt {
    pub source: String,
    pub authority_id: String,
    pub counter: u64,
    pub ledger_position: u64,
    pub ledger_head_digest: String,
    pub signature: String,
}

pub trait LedgerAnchor: Send + Sync {
    fn current(&self) -> Result<Option<LedgerAnchorReceipt>, StatsError>;

    fn advance(
        &self,
        previous: Option<&LedgerAnchorReceipt>,
        authority_id: &str,
        ledger_position: u64,
        ledger_head_digest: &str,
    ) -> Result<LedgerAnchorReceipt, StatsError>;

    fn compare_and_swap(
        &self,
        expected: Option<&LedgerAnchorReceipt>,
        authority_id: &str,
        ledger_position: u64,
        ledger_head_digest: &str,
    ) -> Result<LedgerAnchorReceipt, StatsError> {
        self.advance(expected, authority_id, ledger_position, ledger_head_digest)
    }
}

#[cfg(any(test, debug_assertions))]
#[derive(Default)]
pub struct ConformanceLedgerAnchor {
    receipt: Mutex<Option<LedgerAnchorReceipt>>,
}

#[cfg(any(test, debug_assertions))]
impl ConformanceLedgerAnchor {
    pub fn source_semantics_fake() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[cfg(any(test, debug_assertions))]
impl LedgerAnchor for ConformanceLedgerAnchor {
    fn current(&self) -> Result<Option<LedgerAnchorReceipt>, StatsError> {
        Ok(self
            .receipt
            .lock()
            .map_err(|_| StatsError::Anchor("conformance anchor lock poisoned"))?
            .clone())
    }

    fn advance(
        &self,
        previous: Option<&LedgerAnchorReceipt>,
        authority_id: &str,
        ledger_position: u64,
        ledger_head_digest: &str,
    ) -> Result<LedgerAnchorReceipt, StatsError> {
        let mut current = self
            .receipt
            .lock()
            .map_err(|_| StatsError::Anchor("conformance anchor lock poisoned"))?;
        if current.as_ref().is_some_and(|receipt| {
            receipt.authority_id == authority_id
                && receipt.ledger_position == ledger_position
                && receipt.ledger_head_digest == ledger_head_digest
                && previous.is_some_and(|previous| receipt.counter == previous.counter + 1)
        }) {
            return Ok(current.clone().expect("checked above"));
        }
        if current.as_ref() != previous
            || current.as_ref().is_some_and(|receipt| {
                receipt.authority_id != authority_id || receipt.ledger_position >= ledger_position
            })
            || !valid_digest(ledger_head_digest)
        {
            return Err(StatsError::Anchor(
                "non-monotonic or conflicting anchor update",
            ));
        }
        let counter = current
            .as_ref()
            .map_or(1, |receipt| receipt.counter.saturating_add(1));
        let signature = sha256(&canonical_bytes(&serde_json::json!({
            "domain": "kit-conformance-ledger-anchor-v1",
            "source": "conformance_source_semantics_fake",
            "authority_id": authority_id,
            "counter": counter,
            "ledger_position": ledger_position,
            "ledger_head_digest": ledger_head_digest,
        }))?);
        let next = LedgerAnchorReceipt {
            source: "conformance_source_semantics_fake".to_owned(),
            authority_id: authority_id.to_owned(),
            counter,
            ledger_position,
            ledger_head_digest: ledger_head_digest.to_owned(),
            signature,
        };
        *current = Some(next.clone());
        Ok(next)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthorityCredential {
    authority_id: String,
    key_id: String,
    genesis_digest: String,
    authority_epoch: String,
    key: [u8; 32],
    harness_key: [u8; 32],
}

pub struct RegistrationAuthority {
    connection: Connection,
    credential: AuthorityCredential,
    anchor: Arc<dyn LedgerAnchor>,
    anchored: LedgerAnchorReceipt,
}

impl Drop for RegistrationAuthority {
    fn drop(&mut self) {
        self.credential.key.fill(0);
        self.credential.harness_key.fill(0);
    }
}

impl RegistrationAuthority {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StatsError> {
        let _ = root;
        Err(StatsError::AnchorUnavailable)
    }

    pub fn open_with_anchor(
        root: impl AsRef<Path>,
        anchor: Arc<dyn LedgerAnchor>,
    ) -> Result<Self, StatsError> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(StatsError::InvalidAuthority(
                "authority root must already exist",
            ));
        }
        let database = root.join("registration.sqlite3");
        let credential_path = root.join("registration-authority.json");
        let database_exists = database.exists();
        let credential_exists = credential_path.exists();
        if database_exists != credential_exists {
            return Err(StatsError::AlternateGenesis);
        }
        let connection = Connection::open(&database).map_err(store)?;
        configure(&connection)?;
        create_tables(&connection)?;
        let credential = if credential_exists {
            let bytes = fs::read(&credential_path).map_err(io_error)?;
            serde_json::from_slice(&bytes).map_err(|_| StatsError::AlternateGenesis)?
        } else {
            initialize_credential(&connection, &credential_path)?
        };
        let mut authority = Self {
            connection,
            credential,
            anchor,
            anchored: LedgerAnchorReceipt {
                source: String::new(),
                authority_id: String::new(),
                counter: 0,
                ledger_position: 0,
                ledger_head_digest: String::new(),
                signature: String::new(),
            },
        };
        let (position, digest) = authority.ledger_head()?;
        let commits: usize = authority
            .connection
            .query_row("SELECT COUNT(*) FROM anchor_commits", [], |row| row.get(0))
            .map_err(store)?;
        if commits == 0 {
            if position != 0 || digest != authority.credential.genesis_digest {
                return Err(StatsError::LedgerRollback);
            }
            authority.insert_pending_anchor(0, &digest)?;
        } else if let Some(bytes) = authority
            .connection
            .query_row(
                "SELECT receipt_bytes FROM anchor_commits WHERE state = 'anchored' ORDER BY ledger_position DESC LIMIT 1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(store)?
        {
            authority.anchored = serde_json::from_slice(&bytes)
                .map_err(|_| StatsError::LedgerTamper)?;
        }
        authority.recover_anchor()?;
        if authority.anchored.ledger_position != position
            || authority.anchored.ledger_head_digest != digest
        {
            return Err(StatsError::LedgerRollback);
        }
        authority.verify_chain()?;
        Ok(authority)
    }

    pub fn register(
        &mut self,
        preregistration: Preregistration,
    ) -> Result<RegisteredPreregistration, StatsError> {
        self.recover_anchor()?;
        self.verify_chain()?;
        preregistration.validate()?;
        let preregistration_digest = preregistration.digest()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let last: Option<(u64, String)> = transaction
            .query_row(
                "SELECT sequence, registered_at FROM registrations ORDER BY sequence DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(store)?;
        let sequence = last.as_ref().map_or(Ok(1), |(sequence, _)| {
            sequence.checked_add(1).ok_or(StatsError::SequenceExhausted)
        })?;
        let (ledger_position, previous_entry_digest) =
            ledger_head_tx(&transaction, &self.credential.genesis_digest)?;
        let authority_position = ledger_position
            .checked_add(1)
            .ok_or(StatsError::SequenceExhausted)?;
        let previous_time: Option<String> = transaction
            .query_row(
                "SELECT recorded_at FROM ledger ORDER BY position DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(store)?;
        let registered_at = authority_time(
            &transaction,
            Some(
                previous_time
                    .as_deref()
                    .unwrap_or(&self.credential.authority_epoch),
            ),
        )?;
        let message = RegistrationMessage {
            domain: "kit-registration-v1",
            authority_id: &self.credential.authority_id,
            genesis_digest: &self.credential.genesis_digest,
            sequence,
            authority_position,
            attempt_ordinal: 0,
            registered_at: &registered_at,
            previous_entry_digest: &previous_entry_digest,
            preregistration_digest: &preregistration_digest,
        };
        let registration_authentication =
            authentication(&self.credential, &canonical_bytes(&message)?)?;
        let registered = RegisteredPreregistration {
            schema_version: "1.0".to_owned(),
            kind: "registered_preregistration".to_owned(),
            preregistration,
            preregistration_digest,
            registration: RegistrationReceipt {
                authority_id: self.credential.authority_id.clone(),
                authority_epoch: self.credential.authority_epoch.clone(),
                genesis_digest: self.credential.genesis_digest.clone(),
                sequence,
                authority_position,
                attempt_ordinal: 0,
                registered_at: registered_at.clone(),
                previous_entry_digest,
                authentication: registration_authentication,
            },
        };
        let bytes = registered.canonical_bytes()?;
        if bytes.len() > MAX_PREREGISTRATION_BYTES {
            return Err(StatsError::BoundExceeded("registered preregistration"));
        }
        let entry_digest = sha256(&bytes);
        let (appended_position, appended_previous, appended_digest) = append_ledger(
            &transaction,
            &self.credential.genesis_digest,
            LedgerAppend {
                event_type: "registration",
                recorded_at: &registered_at,
                registration_sequence: Some(sequence),
                schedule_index: None,
                attempt_ordinal: 0,
                payload_bytes: &bytes,
            },
        )?;
        if appended_position != authority_position
            || appended_previous != registered.registration.previous_entry_digest
        {
            return Err(StatsError::LedgerTamper);
        }
        transaction
            .execute(
                "INSERT INTO registrations (sequence, authority_position, registered_at, entry_digest, bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![sequence, authority_position, registered_at, entry_digest, bytes],
            )
            .map_err(store)?;
        stage_anchor(
            &transaction,
            &self.anchored,
            appended_position,
            &appended_digest,
        )?;
        transaction.commit().map_err(store)?;
        self.advance_anchor()?;
        Ok(registered)
    }

    pub fn verify(&self, registered: &RegisteredPreregistration) -> Result<(), StatsError> {
        self.verify_registered(registered)
    }

    pub fn admit_next(
        &mut self,
        registered: &RegisteredPreregistration,
    ) -> Result<TrialAdmission, StatsError> {
        self.recover_anchor()?;
        self.verify_chain()?;
        self.verify_registered(registered)?;
        let frozen: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM experiment_freezes WHERE registration_sequence = ?1)",
                [registered.registration.sequence],
                |row| row.get(0),
            )
            .map_err(store)?;
        if frozen {
            return Err(StatsError::ExperimentFrozen);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let schedule_index: usize = transaction
            .query_row(
                "SELECT COUNT(*) FROM admissions WHERE registration_sequence = ?1",
                [registered.registration.sequence],
                |row| row.get(0),
            )
            .map_err(store)?;
        let roster = registered
            .preregistration
            .roster
            .get(schedule_index)
            .ok_or(StatsError::RosterComplete)?;
        let previous_time: Option<String> = transaction
            .query_row(
                "SELECT recorded_at FROM ledger ORDER BY position DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(store)?;
        let admitted_at = authority_time(&transaction, previous_time.as_deref())?;
        let (position, previous_entry_digest) =
            ledger_head_tx(&transaction, &self.credential.genesis_digest)?;
        let authority_position = position
            .checked_add(1)
            .ok_or(StatsError::SequenceExhausted)?;
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| StatsError::EntropyUnavailable)?;
        let mut admission = TrialAdmission {
            schema_version: "1.0".to_owned(),
            kind: "trial_admission".to_owned(),
            authority_id: self.credential.authority_id.clone(),
            authority_position,
            previous_entry_digest,
            attempt_ordinal: 1,
            registration_sequence: registered.registration.sequence,
            preregistration_digest: registered.preregistration_digest.clone(),
            schedule_index,
            trial_id: roster.trial_id.clone(),
            pair_id: roster.pair_id.clone(),
            task_id: roster.task_id.clone(),
            dataset_member_id: roster.dataset_member_id.clone(),
            task_manifest_digest: roster.task_manifest_digest.clone(),
            seed: roster.seed,
            arm: roster.arm,
            nonce: hex(&nonce),
            token_digest: String::new(),
            admitted_at,
            authentication: Authentication {
                algorithm: String::new(),
                key_id: String::new(),
                tag: String::new(),
            },
        };
        admission.token_digest = sha256(&canonical_bytes(&AdmissionMessage::from(&admission))?);
        admission.authentication = authentication(
            &self.credential,
            &canonical_bytes(&AdmissionMessage::from(&admission))?,
        )?;
        let bytes = canonical_bytes(&admission)?;
        let digest = sha256(&bytes);
        let (appended_position, appended_previous, appended_digest) = append_ledger(
            &transaction,
            &self.credential.genesis_digest,
            LedgerAppend {
                event_type: "admission",
                recorded_at: &admission.admitted_at,
                registration_sequence: Some(registered.registration.sequence),
                schedule_index: Some(schedule_index),
                attempt_ordinal: 1,
                payload_bytes: &bytes,
            },
        )?;
        if appended_position != authority_position
            || appended_previous != admission.previous_entry_digest
        {
            return Err(StatsError::LedgerTamper);
        }
        transaction
            .execute(
                "INSERT INTO admissions (authority_position, registration_sequence, schedule_index, admitted_at, digest, bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![authority_position, registered.registration.sequence, schedule_index, admission.admitted_at, digest, bytes],
            )
            .map_err(store)?;
        stage_anchor(
            &transaction,
            &self.anchored,
            appended_position,
            &appended_digest,
        )?;
        transaction.commit().map_err(store)?;
        self.advance_anchor()?;
        Ok(admission)
    }

    #[cfg(test)]
    pub(crate) fn consume_for_run_admission(
        &mut self,
        registered: &RegisteredPreregistration,
        admission: &TrialAdmission,
    ) -> Result<TrialRunConfig, StatsError> {
        let run_id =
            kit::domain::ids::RunId::generate().map_err(|_| StatsError::EntropyUnavailable)?;
        self.consume_scheduler_admission(
            registered,
            admission,
            &kit::runtime::scheduler::PendingStatisticalTrial {
                run_id,
                admission_token_digest: admission.token_digest.clone(),
                admission_nonce: admission.nonce.clone(),
                admission_position: admission.authority_position,
                consumption_position: admission.authority_position,
                consumption_digest: sha256(admission.token_digest.as_bytes()),
            },
        )
        .map(|(config, _)| config)
    }

    pub(crate) fn consume_scheduler_admission(
        &mut self,
        registered: &RegisteredPreregistration,
        admission: &TrialAdmission,
        pending: &kit::runtime::scheduler::PendingStatisticalTrial,
    ) -> Result<
        (
            TrialRunConfig,
            kit::runtime::scheduler::AnchoredConsumptionReceipt,
        ),
        StatsError,
    > {
        self.consume_scheduler_admission_with_event_start(
            registered,
            admission,
            pending,
            pending.consumption_position,
        )
    }

    pub(crate) fn consume_scheduler_admission_with_event_start(
        &mut self,
        registered: &RegisteredPreregistration,
        admission: &TrialAdmission,
        pending: &kit::runtime::scheduler::PendingStatisticalTrial,
        event_start_watermark: u64,
    ) -> Result<
        (
            TrialRunConfig,
            kit::runtime::scheduler::AnchoredConsumptionReceipt,
        ),
        StatsError,
    > {
        self.recover_anchor()?;
        self.verify_chain()?;
        self.verify_registered(registered)?;
        self.verify_admission(registered, admission)?;
        if pending.admission_token_digest != admission.token_digest
            || pending.admission_nonce != admission.nonce
            || pending.admission_position != admission.authority_position
            || pending.consumption_position == 0
            || !valid_digest(&pending.consumption_digest)
        {
            return Err(StatsError::InvalidAdmission);
        }
        let existing: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT run_config_bytes FROM admission_consumptions
                 WHERE admission_position = ?1 OR token_digest = ?2 OR scheduler_run_id = ?3
                    OR scheduler_consumption_position = ?4 OR scheduler_consumption_digest = ?5",
                params![
                    admission.authority_position,
                    admission.token_digest,
                    pending.run_id.to_string(),
                    pending.consumption_position,
                    pending.consumption_digest,
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(store)?;
        if let Some(bytes) = existing {
            let config: TrialRunConfig =
                serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
            let roster = registered
                .preregistration
                .roster
                .get(admission.schedule_index)
                .ok_or(StatsError::InvalidAdmission)?;
            self.verify_run_config(registered, roster, &config)?;
            if config.admission_position != admission.authority_position
                || config.admission_token_digest != pending.admission_token_digest
                || config.admission_nonce != pending.admission_nonce
                || config.scheduler_run_id != pending.run_id.to_string()
                || config.scheduler_consumption_position != pending.consumption_position
                || config.scheduler_consumption_digest != pending.consumption_digest
            {
                return Err(StatsError::InvalidAdmission);
            }
            let receipt = self.anchored_consumption_receipt(&config)?;
            return Ok((config, receipt));
        }
        let frozen: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM experiment_freezes WHERE registration_sequence = ?1)",
                [registered.registration.sequence],
                |row| row.get(0),
            )
            .map_err(store)?;
        if frozen {
            return Err(StatsError::ExperimentFrozen);
        }
        let roster = registered
            .preregistration
            .roster
            .get(admission.schedule_index)
            .ok_or(StatsError::InvalidAdmission)?;
        let mut config = TrialRunConfig {
            schema_version: "1.1".to_owned(),
            kind: "immutable_trial_run_config".to_owned(),
            authority_id: self.credential.authority_id.clone(),
            registration_sequence: registered.registration.sequence,
            preregistration_digest: registered.preregistration_digest.clone(),
            admission_position: admission.authority_position,
            admission_nonce: admission.nonce.clone(),
            admission_token_digest: admission.token_digest.clone(),
            scheduler_run_id: pending.run_id.to_string(),
            scheduler_consumption_position: pending.consumption_position,
            scheduler_consumption_digest: pending.consumption_digest.clone(),
            schedule_index: roster.schedule_index,
            trial_id: roster.trial_id.clone(),
            pair_id: roster.pair_id.clone(),
            task_id: roster.task_id.clone(),
            dataset_member_id: roster.dataset_member_id.clone(),
            task_manifest_digest: roster.task_manifest_digest.clone(),
            seed: roster.seed,
            arm: roster.arm,
            config_digest: roster.config_digest.clone(),
            model_digest: roster.model_digest.clone(),
            model_settings_digest: roster.model_settings_digest.clone(),
            provider_capability_digest: roster.provider_capability_digest.clone(),
            event_start_watermark,
            immutable_digest: String::new(),
        };
        config.immutable_digest = trial_run_config_digest(&config)?;
        let config_bytes = canonical_bytes(&config)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let previous_time: String = transaction
            .query_row(
                "SELECT recorded_at FROM ledger ORDER BY position DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .map_err(store)?;
        let recorded_at = authority_time(&transaction, Some(&previous_time))?;
        let (position, _, head) = append_ledger(
            &transaction,
            &self.credential.genesis_digest,
            LedgerAppend {
                event_type: "admission_consumed",
                recorded_at: &recorded_at,
                registration_sequence: Some(registered.registration.sequence),
                schedule_index: Some(roster.schedule_index),
                attempt_ordinal: 1,
                payload_bytes: &config_bytes,
            },
        )?;
        transaction
            .execute(
                "INSERT INTO admission_consumptions
                     (admission_position, token_digest, run_config_digest, scheduler_run_id,
                      scheduler_consumption_position, scheduler_consumption_digest,
                      authority_position, run_config_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    admission.authority_position,
                    admission.token_digest,
                    config.immutable_digest,
                    config.scheduler_run_id,
                    config.scheduler_consumption_position,
                    config.scheduler_consumption_digest,
                    position,
                    config_bytes,
                ],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(_, _) => StatsError::AdmissionConsumed,
                other => store(other),
            })?;
        stage_anchor(&transaction, &self.anchored, position, &head)?;
        transaction.commit().map_err(store)?;
        self.advance_anchor()?;
        let receipt = self.anchored_consumption_receipt(&config)?;
        Ok((config, receipt))
    }

    pub(crate) fn load_scheduler_admission_consumption(
        &mut self,
        registered: &RegisteredPreregistration,
        admission: &TrialAdmission,
        pending: &kit::runtime::scheduler::PendingStatisticalTrial,
    ) -> Result<
        Option<(
            TrialRunConfig,
            kit::runtime::scheduler::AnchoredConsumptionReceipt,
        )>,
        StatsError,
    > {
        self.recover_anchor()?;
        self.verify_chain()?;
        self.verify_registered(registered)?;
        self.verify_admission(registered, admission)?;
        let existing = self
            .connection
            .query_row(
                "SELECT run_config_bytes FROM admission_consumptions
                 WHERE admission_position = ?1 OR token_digest = ?2 OR scheduler_run_id = ?3
                    OR scheduler_consumption_position = ?4 OR scheduler_consumption_digest = ?5",
                params![
                    admission.authority_position,
                    admission.token_digest,
                    pending.run_id.to_string(),
                    pending.consumption_position,
                    pending.consumption_digest,
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(store)?;
        let Some(bytes) = existing else {
            return Ok(None);
        };
        let config: TrialRunConfig =
            serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
        let roster = registered
            .preregistration
            .roster
            .get(admission.schedule_index)
            .ok_or(StatsError::InvalidAdmission)?;
        self.verify_run_config(registered, roster, &config)?;
        if config.admission_position != admission.authority_position
            || config.admission_token_digest != pending.admission_token_digest
            || config.admission_nonce != pending.admission_nonce
            || config.scheduler_run_id != pending.run_id.to_string()
            || config.scheduler_consumption_position != pending.consumption_position
            || config.scheduler_consumption_digest != pending.consumption_digest
        {
            return Err(StatsError::InvalidAdmission);
        }
        let receipt = self.anchored_consumption_receipt(&config)?;
        Ok(Some((config, receipt)))
    }

    fn anchored_consumption_receipt(
        &self,
        config: &TrialRunConfig,
    ) -> Result<kit::runtime::scheduler::AnchoredConsumptionReceipt, StatsError> {
        let authority_position: u64 = self
            .connection
            .query_row(
                "SELECT authority_position FROM admission_consumptions WHERE admission_position = ?1",
                [config.admission_position],
                |row| row.get(0),
            )
            .map_err(store)?;
        let (ledger_head_digest, anchor_bytes): (String, Vec<u8>) = self
            .connection
            .query_row(
                "SELECT l.entry_digest, a.receipt_bytes
                 FROM ledger l JOIN anchor_commits a ON a.ledger_position = l.position
                 WHERE l.position = ?1 AND l.event_type = 'admission_consumed'
                   AND a.state = 'anchored'",
                [authority_position],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(store)?;
        let anchor: LedgerAnchorReceipt =
            serde_json::from_slice(&anchor_bytes).map_err(|_| StatsError::LedgerTamper)?;
        if anchor.authority_id != self.credential.authority_id
            || anchor.ledger_position != authority_position
            || anchor.ledger_head_digest != ledger_head_digest
            || !valid_anchor_receipt(&anchor)
        {
            return Err(StatsError::LedgerTamper);
        }
        let mut receipt = kit::runtime::scheduler::AnchoredConsumptionReceipt {
            authority_id: self.credential.authority_id.clone(),
            scheduler_run_id: config.scheduler_run_id.clone(),
            admission_token_digest: config.admission_token_digest.clone(),
            admission_nonce: config.admission_nonce.clone(),
            scheduler_consumption_position: config.scheduler_consumption_position,
            scheduler_consumption_digest: config.scheduler_consumption_digest.clone(),
            ledger_position: authority_position,
            ledger_head_digest,
            anchor_source: anchor.source,
            anchor_identity: anchor.authority_id,
            anchor_counter: anchor.counter,
            anchor_signature: anchor.signature,
            authentication_algorithm: String::new(),
            authentication_key_id: String::new(),
            authentication_tag: String::new(),
        };
        let authentication = authentication(
            &self.credential,
            &canonical_bytes(&AnchoredConsumptionMessage::from(&receipt))?,
        )?;
        receipt.authentication_algorithm = authentication.algorithm;
        receipt.authentication_key_id = authentication.key_id;
        receipt.authentication_tag = authentication.tag;
        Ok(receipt)
    }

    pub(crate) fn record_harness_trial(
        &mut self,
        registered: &RegisteredPreregistration,
        run_config: &TrialRunConfig,
        harness_report_bytes: Vec<u8>,
        events_bytes: Vec<u8>,
    ) -> Result<BoundTrialEnvelope, StatsError> {
        self.recover_anchor()?;
        self.verify_chain()?;
        self.verify_registered(registered)?;
        let frozen: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM experiment_freezes WHERE registration_sequence = ?1)",
                [registered.registration.sequence],
                |row| row.get(0),
            )
            .map_err(store)?;
        if frozen {
            return Err(StatsError::ExperimentFrozen);
        }
        let roster = registered
            .preregistration
            .roster
            .get(run_config.schedule_index)
            .ok_or(StatsError::InvalidAdmission)?;
        self.verify_run_config(registered, roster, run_config)?;
        let evidence = TrialEvidence {
            harness_report_bytes,
            events_bytes,
        };
        let derived = validate_trial_evidence(registered, roster, run_config, &evidence)?;
        if let Some((receipt_bytes, stored_harness, stored_events)) = self
            .connection
            .query_row(
                "SELECT receipt_bytes, harness_bytes, events_bytes FROM executions
                 WHERE registration_sequence = ?1 AND schedule_index = ?2",
                params![registered.registration.sequence, run_config.schedule_index],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(store)?
        {
            if stored_harness != evidence.harness_report_bytes
                || stored_events != evidence.events_bytes
            {
                return Err(StatsError::AttemptLimit);
            }
            let receipt: MeasuredTrialReceipt =
                serde_json::from_slice(&receipt_bytes).map_err(|_| StatsError::TrialTamper)?;
            self.validate_stored_receipt(registered, &receipt, &evidence, &receipt_bytes)?;
            return Ok(BoundTrialEnvelope {
                digest: sha256(&receipt_bytes),
                receipt,
                harness_report_bytes: stored_harness,
                events_bytes: stored_events,
            });
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let existing: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM executions WHERE registration_sequence = ?1 AND schedule_index = ?2)",
                params![registered.registration.sequence, run_config.schedule_index],
                |row| row.get(0),
            )
            .map_err(store)?;
        if existing {
            return Err(StatsError::AttemptLimit);
        }
        let attempt_ordinal = 1;
        let previous_time: Option<String> = transaction
            .query_row(
                "SELECT recorded_at FROM ledger ORDER BY position DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(store)?;
        let lower = previous_time
            .as_deref()
            .into_iter()
            .chain([registered.registration.registered_at.as_str()])
            .max();
        let recorded_at = authority_time(&transaction, lower)?;
        let (position, previous_entry_digest) =
            ledger_head_tx(&transaction, &self.credential.genesis_digest)?;
        let authority_position = position
            .checked_add(1)
            .ok_or(StatsError::SequenceExhausted)?;
        let mut receipt = MeasuredTrialReceipt {
            schema_version: "1.0".to_owned(),
            kind: "measured_trial_receipt".to_owned(),
            authority_position,
            previous_entry_digest,
            attempt_ordinal,
            registration_sequence: registered.registration.sequence,
            preregistration_digest: registered.preregistration_digest.clone(),
            schedule_index: roster.schedule_index,
            trial_id: roster.trial_id.clone(),
            pair_id: roster.pair_id.clone(),
            task_id: roster.task_id.clone(),
            dataset_member_id: roster.dataset_member_id.clone(),
            seed: roster.seed,
            arm: roster.arm,
            roster_membership: RosterMembershipProof {
                schedule_index: roster.schedule_index,
                roster_entry_digest: sha256(&canonical_bytes(roster)?),
                task_set_digest: registered.preregistration.digests.task_set.clone(),
                dataset_digest: registered.preregistration.digests.dataset.clone(),
                experiment_design_digest: registered.preregistration.digests.experiment.clone(),
            },
            admission_position: run_config.admission_position,
            admission_nonce: run_config.admission_nonce.clone(),
            admission_token_digest: run_config.admission_token_digest.clone(),
            scheduler_run_id: run_config.scheduler_run_id.clone(),
            scheduler_consumption_position: run_config.scheduler_consumption_position,
            scheduler_consumption_digest: run_config.scheduler_consumption_digest.clone(),
            event_high_watermark: derived.event_high_watermark,
            config_digest: derived.config_digest,
            model_digest: derived.model_digest,
            harness_report_digest: sha256(&evidence.harness_report_bytes),
            events_digest: sha256(&evidence.events_bytes),
            evidence_source: derived.evidence_source,
            recorded_at,
            outcome: derived.outcome,
            intervention: derived.intervention,
            cost_usd: derived.cost_usd,
            latency_ms: derived.latency_ms,
            elapsed_millis: derived.latency_ms as u64,
            cost_imputed: false,
            latency_imputed: false,
            durable_usage: None,
            verification_passed: derived.verification_passed,
            failure_reason: derived.failure_reason,
            exclusion_reason: derived.exclusion_reason,
            authentication: Authentication {
                algorithm: String::new(),
                key_id: String::new(),
                tag: String::new(),
            },
        };
        receipt.authentication = harness_authentication(
            &self.credential,
            &canonical_bytes(&MeasuredMessage::from(&receipt))?,
        )?;
        let bytes = canonical_bytes(&receipt)?;
        let digest = sha256(&bytes);
        let (appended_position, appended_previous, appended_digest) = append_ledger(
            &transaction,
            &self.credential.genesis_digest,
            LedgerAppend {
                event_type: "execution",
                recorded_at: &receipt.recorded_at,
                registration_sequence: Some(registered.registration.sequence),
                schedule_index: Some(roster.schedule_index),
                attempt_ordinal,
                payload_bytes: &bytes,
            },
        )?;
        if appended_position != receipt.authority_position
            || appended_previous != receipt.previous_entry_digest
        {
            return Err(StatsError::LedgerTamper);
        }
        transaction
            .execute(
                "INSERT INTO executions (authority_position, registration_sequence, schedule_index, attempt_ordinal, recorded_at, digest, receipt_bytes, harness_bytes, events_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![authority_position, registered.registration.sequence, roster.schedule_index, attempt_ordinal, receipt.recorded_at, digest, bytes, evidence.harness_report_bytes, evidence.events_bytes],
            )
            .map_err(store)?;
        stage_anchor(
            &transaction,
            &self.anchored,
            appended_position,
            &appended_digest,
        )?;
        transaction.commit().map_err(store)?;
        self.advance_anchor()?;
        Ok(BoundTrialEnvelope {
            receipt,
            digest,
            harness_report_bytes: evidence.harness_report_bytes,
            events_bytes: evidence.events_bytes,
        })
    }

    pub(crate) fn record_terminal_error(
        &mut self,
        registered: &RegisteredPreregistration,
        run_config: &TrialRunConfig,
        evidence_source: EvidenceSource,
        evidence: &TerminalErrorEvidence,
    ) -> Result<BoundTrialEnvelope, StatsError> {
        self.recover_anchor()?;
        self.verify_chain()?;
        self.verify_registered(registered)?;
        let roster = registered
            .preregistration
            .roster
            .get(run_config.schedule_index)
            .ok_or(StatsError::InvalidAdmission)?;
        self.verify_run_config(registered, roster, run_config)?;
        evidence.validate(registered.preregistration.policies.error_imputation)?;
        let reason = &evidence.reason;
        if let Some(envelope) = self.load_harness_trial(registered, run_config)? {
            return if envelope.harness_report_bytes.is_empty()
                && envelope.events_bytes.is_empty()
                && envelope.receipt.failure_reason == *reason
                && envelope.receipt.elapsed_millis == evidence.elapsed_millis
                && envelope.receipt.cost_usd == evidence.cost_microusd as f64 / 1_000_000.0
                && envelope.receipt.latency_ms == evidence.latency_millis as f64
                && envelope.receipt.cost_imputed == evidence.cost_imputed
                && envelope.receipt.latency_imputed == evidence.latency_imputed
                && envelope.receipt.durable_usage == evidence.durable_usage
                && envelope.receipt.evidence_source == evidence_source
            {
                Ok(envelope)
            } else {
                Err(StatsError::AttemptLimit)
            };
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let previous_time: Option<String> = transaction
            .query_row(
                "SELECT recorded_at FROM ledger ORDER BY position DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(store)?;
        let recorded_at = authority_time(&transaction, previous_time.as_deref())?;
        let (position, previous_entry_digest) =
            ledger_head_tx(&transaction, &self.credential.genesis_digest)?;
        let authority_position = position
            .checked_add(1)
            .ok_or(StatsError::SequenceExhausted)?;
        let mut receipt = MeasuredTrialReceipt {
            schema_version: "1.0".to_owned(),
            kind: "measured_trial_receipt".to_owned(),
            authority_position,
            previous_entry_digest,
            attempt_ordinal: 1,
            registration_sequence: registered.registration.sequence,
            preregistration_digest: registered.preregistration_digest.clone(),
            schedule_index: roster.schedule_index,
            trial_id: roster.trial_id.clone(),
            pair_id: roster.pair_id.clone(),
            task_id: roster.task_id.clone(),
            dataset_member_id: roster.dataset_member_id.clone(),
            seed: roster.seed,
            arm: roster.arm,
            roster_membership: RosterMembershipProof {
                schedule_index: roster.schedule_index,
                roster_entry_digest: sha256(&canonical_bytes(roster)?),
                task_set_digest: registered.preregistration.digests.task_set.clone(),
                dataset_digest: registered.preregistration.digests.dataset.clone(),
                experiment_design_digest: registered.preregistration.digests.experiment.clone(),
            },
            admission_position: run_config.admission_position,
            admission_nonce: run_config.admission_nonce.clone(),
            admission_token_digest: run_config.admission_token_digest.clone(),
            scheduler_run_id: run_config.scheduler_run_id.clone(),
            scheduler_consumption_position: run_config.scheduler_consumption_position,
            scheduler_consumption_digest: run_config.scheduler_consumption_digest.clone(),
            event_high_watermark: run_config.event_start_watermark,
            config_digest: run_config.config_digest.clone(),
            model_digest: run_config.model_digest.clone(),
            harness_report_digest: sha256(&[]),
            events_digest: sha256(&[]),
            evidence_source,
            recorded_at,
            outcome: TrialOutcome::Error,
            intervention: false,
            cost_usd: evidence.cost_microusd as f64 / 1_000_000.0,
            latency_ms: evidence.latency_millis as f64,
            elapsed_millis: evidence.elapsed_millis,
            cost_imputed: evidence.cost_imputed,
            latency_imputed: evidence.latency_imputed,
            durable_usage: evidence.durable_usage.clone(),
            verification_passed: false,
            failure_reason: reason.to_owned(),
            exclusion_reason: String::new(),
            authentication: Authentication {
                algorithm: String::new(),
                key_id: String::new(),
                tag: String::new(),
            },
        };
        receipt.authentication = harness_authentication(
            &self.credential,
            &canonical_bytes(&MeasuredMessage::from(&receipt))?,
        )?;
        let receipt_bytes = canonical_bytes(&receipt)?;
        let digest = sha256(&receipt_bytes);
        let (appended_position, appended_previous, appended_digest) = append_ledger(
            &transaction,
            &self.credential.genesis_digest,
            LedgerAppend {
                event_type: "execution",
                recorded_at: &receipt.recorded_at,
                registration_sequence: Some(registered.registration.sequence),
                schedule_index: Some(roster.schedule_index),
                attempt_ordinal: 1,
                payload_bytes: &receipt_bytes,
            },
        )?;
        if appended_position != receipt.authority_position
            || appended_previous != receipt.previous_entry_digest
        {
            return Err(StatsError::LedgerTamper);
        }
        transaction
            .execute(
                "INSERT INTO executions (authority_position, registration_sequence, schedule_index,
                 attempt_ordinal, recorded_at, digest, receipt_bytes, harness_bytes, events_bytes)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, X'', X'')",
                params![
                    authority_position,
                    registered.registration.sequence,
                    roster.schedule_index,
                    receipt.recorded_at,
                    digest,
                    receipt_bytes
                ],
            )
            .map_err(store)?;
        stage_anchor(
            &transaction,
            &self.anchored,
            appended_position,
            &appended_digest,
        )?;
        transaction.commit().map_err(store)?;
        self.advance_anchor()?;
        Ok(BoundTrialEnvelope {
            receipt,
            digest,
            harness_report_bytes: Vec::new(),
            events_bytes: Vec::new(),
        })
    }

    pub(crate) fn load_harness_trial(
        &self,
        registered: &RegisteredPreregistration,
        run_config: &TrialRunConfig,
    ) -> Result<Option<BoundTrialEnvelope>, StatsError> {
        let stored = self
            .connection
            .query_row(
                "SELECT digest, receipt_bytes, harness_bytes, events_bytes FROM executions
                 WHERE registration_sequence = ?1 AND schedule_index = ?2",
                params![registered.registration.sequence, run_config.schedule_index],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(store)?;
        let Some((digest, receipt_bytes, harness_report_bytes, events_bytes)) = stored else {
            return Ok(None);
        };
        let receipt: MeasuredTrialReceipt =
            serde_json::from_slice(&receipt_bytes).map_err(|_| StatsError::TrialTamper)?;
        self.validate_stored_receipt(
            registered,
            &receipt,
            &TrialEvidence {
                harness_report_bytes: harness_report_bytes.clone(),
                events_bytes: events_bytes.clone(),
            },
            &receipt_bytes,
        )?;
        if digest != sha256(&receipt_bytes) {
            return Err(StatsError::TrialTamper);
        }
        Ok(Some(BoundTrialEnvelope {
            receipt,
            digest,
            harness_report_bytes,
            events_bytes,
        }))
    }

    pub fn freeze_experiment(
        &mut self,
        registered: &RegisteredPreregistration,
    ) -> Result<u64, StatsError> {
        self.recover_anchor()?;
        self.verify_chain()?;
        self.verify_registered(registered)?;
        if let Some(cutoff) = self
            .connection
            .query_row(
                "SELECT ledger_cutoff FROM experiment_freezes WHERE registration_sequence = ?1",
                [registered.registration.sequence],
                |row| row.get(0),
            )
            .optional()
            .map_err(store)?
        {
            return Ok(cutoff);
        }
        let (executions, consumptions): (usize, usize) = self
            .connection
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM executions WHERE registration_sequence = ?1),
                    (SELECT COUNT(*) FROM admission_consumptions c JOIN admissions a
                     ON a.authority_position = c.admission_position
                     WHERE a.registration_sequence = ?1)",
                [registered.registration.sequence],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(store)?;
        if executions != registered.preregistration.roster.len() || consumptions != executions {
            return Err(StatsError::ExperimentNotTerminal);
        }
        let (ledger_cutoff, _) = self.ledger_head()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let previous_time: String = transaction
            .query_row(
                "SELECT recorded_at FROM ledger ORDER BY position DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .map_err(store)?;
        let recorded_at = authority_time(&transaction, Some(&previous_time))?;
        let frozen = ExperimentFrozen {
            schema_version: "1.0".to_owned(),
            kind: "experiment_frozen".to_owned(),
            registration_sequence: registered.registration.sequence,
            preregistration_digest: registered.preregistration_digest.clone(),
            ledger_cutoff,
            recorded_at: recorded_at.clone(),
        };
        let bytes = canonical_bytes(&frozen)?;
        let digest = sha256(&bytes);
        let (position, _, head) = append_ledger(
            &transaction,
            &self.credential.genesis_digest,
            LedgerAppend {
                event_type: "experiment_frozen",
                recorded_at: &recorded_at,
                registration_sequence: Some(registered.registration.sequence),
                schedule_index: None,
                attempt_ordinal: 0,
                payload_bytes: &bytes,
            },
        )?;
        transaction
            .execute(
                "INSERT INTO experiment_freezes
                     (authority_position, registration_sequence, ledger_cutoff, recorded_at, digest, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    position,
                    registered.registration.sequence,
                    ledger_cutoff,
                    recorded_at,
                    digest,
                    bytes,
                ],
            )
            .map_err(store)?;
        stage_anchor(&transaction, &self.anchored, position, &head)?;
        transaction.commit().map_err(store)?;
        self.advance_anchor()?;
        Ok(ledger_cutoff)
    }

    pub fn build_report(
        &mut self,
        registered: &RegisteredPreregistration,
    ) -> Result<StatisticalReportEnvelope, StatsError> {
        self.recover_anchor()?;
        self.verify_chain()?;
        self.verify_registered(registered)?;
        let ledger_cutoff = self.freeze_experiment(registered)?;
        let existing: Option<(String, Vec<u8>, Vec<u8>)> = self
            .connection
            .query_row(
                "SELECT digest, report_bytes, receipt_bytes FROM reports WHERE registration_sequence = ?1",
                [registered.registration.sequence],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(store)?;
        if let Some((digest, bytes, receipt_bytes)) = existing {
            let report = serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
            let receipt =
                serde_json::from_slice(&receipt_bytes).map_err(|_| StatsError::LedgerTamper)?;
            let envelope = StatisticalReportEnvelope {
                report,
                bytes,
                digest,
                receipt,
            };
            self.verify_report(registered, &envelope)?;
            if envelope.receipt.ledger_cutoff != ledger_cutoff {
                return Err(StatsError::InvalidReportReceipt);
            }
            return Ok(envelope);
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT receipt_bytes, harness_bytes, events_bytes FROM executions WHERE registration_sequence = ?1 AND authority_position <= ?2 ORDER BY authority_position",
            )
            .map_err(store)?;
        let rows = statement
            .query_map(
                params![registered.registration.sequence, ledger_cutoff],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .map_err(store)?;
        let mut attempts: BTreeMap<usize, MeasuredTrialReceipt> = BTreeMap::new();
        for row in rows {
            let (receipt_bytes, harness_report_bytes, events_bytes) = row.map_err(store)?;
            let receipt: MeasuredTrialReceipt =
                serde_json::from_slice(&receipt_bytes).map_err(|_| StatsError::TrialTamper)?;
            let evidence = TrialEvidence {
                harness_report_bytes,
                events_bytes,
            };
            self.validate_stored_receipt(registered, &receipt, &evidence, &receipt_bytes)?;
            let schedule_index = receipt.schedule_index;
            if receipt.attempt_ordinal != 1 || attempts.insert(schedule_index, receipt).is_some() {
                return Err(StatsError::TrialTamper);
            }
        }
        drop(statement);
        let admitted = self.admitted_indexes(registered.registration.sequence)?;
        let evidence_source = uniform_evidence_source(attempts.values())?;
        let mut trials = Vec::new();
        let mut selected = BTreeMap::new();
        for roster in &registered.preregistration.roster {
            match attempts.get(&roster.schedule_index) {
                Some(receipt) => {
                    let status = if !receipt.exclusion_reason.is_empty() {
                        TrialStatus::ExcludedPreregistered
                    } else if receipt.outcome != TrialOutcome::Success {
                        TrialStatus::Failed
                    } else {
                        TrialStatus::Included
                    };
                    selected.insert(roster.schedule_index, receipt.clone());
                    trials.push(TrialRecord::measured(receipt, status));
                }
                None if admitted.contains(&roster.schedule_index) => {
                    trials.push(TrialRecord::placeholder(roster, TrialStatus::Incomplete));
                }
                None => trials.push(TrialRecord::placeholder(roster, TrialStatus::Missing)),
            }
        }
        trials.sort_by_key(|trial| {
            (
                trial.authority_position.unwrap_or(u64::MAX),
                trial.schedule_index,
            )
        });
        let (analysis_status, metrics) = analyze(registered, &selected)?;
        let first_measured_run = selected
            .values()
            .min_by_key(|receipt| receipt.authority_position)
            .map(|receipt| FirstMeasuredRun {
                timestamp: receipt.recorded_at.clone(),
                trial_digest: receipt.harness_report_digest.clone(),
            });
        let report = StatisticalReport {
            schema_version: "1.0".to_owned(),
            kind: "core_statistical_report".to_owned(),
            preregistration: registered.preregistration.clone(),
            preregistration_digest: registered.preregistration_digest.clone(),
            registration: registered.registration.clone(),
            evidence_source,
            first_measured_run,
            sample_counts: sample_counts(&trials),
            trials,
            analysis_status,
            metrics,
        };
        let bytes = canonical_bytes(&report)?;
        if bytes.len() > MAX_REPORT_BYTES {
            return Err(StatsError::BoundExceeded("statistical report"));
        }
        validate_schema(&bytes, "statistical_report")?;
        let digest = sha256(&bytes);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let previous_time: Option<String> = transaction
            .query_row(
                "SELECT recorded_at FROM ledger ORDER BY position DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(store)?;
        let recorded_at = authority_time(&transaction, previous_time.as_deref())?;
        let (position, previous_entry_digest, ledger_head_digest) = append_ledger(
            &transaction,
            &self.credential.genesis_digest,
            LedgerAppend {
                event_type: "report",
                recorded_at: &recorded_at,
                registration_sequence: Some(registered.registration.sequence),
                schedule_index: None,
                attempt_ordinal: 0,
                payload_bytes: &bytes,
            },
        )?;
        let mut receipt = StatisticalReportReceipt {
            schema_version: "1.0".to_owned(),
            kind: "statistical_report_receipt".to_owned(),
            authority_id: self.credential.authority_id.clone(),
            registration_sequence: registered.registration.sequence,
            preregistration_digest: registered.preregistration_digest.clone(),
            report_digest: digest.clone(),
            evidence_source,
            ledger_cutoff,
            freeze_position: ledger_cutoff + 1,
            ledger_position: position,
            previous_entry_digest,
            ledger_head_digest: ledger_head_digest.clone(),
            recorded_at: recorded_at.clone(),
            authentication: Authentication {
                algorithm: String::new(),
                key_id: String::new(),
                tag: String::new(),
            },
        };
        receipt.authentication = authentication(
            &self.credential,
            &canonical_bytes(&StatisticalReportReceiptMessage::from(&receipt))?,
        )?;
        let receipt_bytes = canonical_bytes(&receipt)?;
        transaction
            .execute(
                "INSERT INTO reports (authority_position, registration_sequence, recorded_at, digest, report_bytes, receipt_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![position, registered.registration.sequence, recorded_at, digest, bytes, receipt_bytes],
            )
            .map_err(store)?;
        stage_anchor(&transaction, &self.anchored, position, &ledger_head_digest)?;
        transaction.commit().map_err(store)?;
        self.advance_anchor()?;
        Ok(StatisticalReportEnvelope {
            report,
            bytes,
            digest,
            receipt,
        })
    }

    pub fn verify_report(
        &self,
        registered: &RegisteredPreregistration,
        envelope: &StatisticalReportEnvelope,
    ) -> Result<(), StatsError> {
        self.verify_chain()?;
        self.verify_registered(registered)?;
        if envelope.bytes != canonical_bytes(&envelope.report)?
            || envelope.digest != sha256(&envelope.bytes)
            || envelope.receipt.report_digest != envelope.digest
            || envelope.report.evidence_source != envelope.receipt.evidence_source
            || envelope.receipt.registration_sequence != registered.registration.sequence
            || envelope.receipt.preregistration_digest != registered.preregistration_digest
            || envelope.receipt.authority_id != self.credential.authority_id
        {
            return Err(StatsError::InvalidReportReceipt);
        }
        let stored: Option<(Vec<u8>, Vec<u8>)> = self
            .connection
            .query_row(
                "SELECT report_bytes, receipt_bytes FROM reports WHERE authority_position = ?1",
                [envelope.receipt.ledger_position],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(store)?;
        if stored != Some((envelope.bytes.clone(), canonical_bytes(&envelope.receipt)?)) {
            return Err(StatsError::InvalidReportReceipt);
        }
        let (ledger_cutoff, frozen_position): (u64, u64) = self
            .connection
            .query_row(
                "SELECT ledger_cutoff, authority_position FROM experiment_freezes WHERE registration_sequence = ?1",
                [registered.registration.sequence],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(store)?
            .ok_or(StatsError::InvalidReportReceipt)?;
        if envelope.receipt.ledger_position <= frozen_position
            || envelope.receipt.ledger_cutoff != ledger_cutoff
            || envelope.receipt.freeze_position != frozen_position
            || canonical_bytes(&self.reconstruct_report(registered, ledger_cutoff)?)?
                != envelope.bytes
        {
            return Err(StatsError::InvalidReportReceipt);
        }
        let ledger: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT previous_digest, entry_digest FROM ledger WHERE position = ?1 AND event_type = 'report'",
                [envelope.receipt.ledger_position],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(store)?;
        if ledger
            != Some((
                envelope.receipt.previous_entry_digest.clone(),
                envelope.receipt.ledger_head_digest.clone(),
            ))
        {
            return Err(StatsError::InvalidReportReceipt);
        }
        verify_authentication(
            &self.credential,
            &canonical_bytes(&StatisticalReportReceiptMessage::from(&envelope.receipt))?,
            &envelope.receipt.authentication,
        )
        .map_err(|_| StatsError::InvalidReportReceipt)?;
        self.verify_execution_evidence(registered)
    }

    fn reconstruct_report(
        &self,
        registered: &RegisteredPreregistration,
        ledger_cutoff: u64,
    ) -> Result<StatisticalReport, StatsError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT receipt_bytes, harness_bytes, events_bytes FROM executions
                 WHERE registration_sequence = ?1 AND authority_position <= ?2
                 ORDER BY authority_position",
            )
            .map_err(store)?;
        let rows = statement
            .query_map(
                params![registered.registration.sequence, ledger_cutoff],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .map_err(store)?;
        let mut attempts = BTreeMap::new();
        for row in rows {
            let (receipt_bytes, harness_report_bytes, events_bytes) = row.map_err(store)?;
            let receipt: MeasuredTrialReceipt =
                serde_json::from_slice(&receipt_bytes).map_err(|_| StatsError::TrialTamper)?;
            self.validate_stored_receipt(
                registered,
                &receipt,
                &TrialEvidence {
                    harness_report_bytes,
                    events_bytes,
                },
                &receipt_bytes,
            )?;
            let schedule_index = receipt.schedule_index;
            if receipt.attempt_ordinal != 1 || attempts.insert(schedule_index, receipt).is_some() {
                return Err(StatsError::TrialTamper);
            }
        }
        if attempts.len() != registered.preregistration.roster.len() {
            return Err(StatsError::ExperimentNotTerminal);
        }
        let evidence_source = uniform_evidence_source(attempts.values())?;
        let mut trials = Vec::with_capacity(attempts.len());
        for roster in &registered.preregistration.roster {
            let receipt = attempts
                .get(&roster.schedule_index)
                .ok_or(StatsError::ExperimentNotTerminal)?;
            let status = if !receipt.exclusion_reason.is_empty() {
                TrialStatus::ExcludedPreregistered
            } else if receipt.outcome != TrialOutcome::Success {
                TrialStatus::Failed
            } else {
                TrialStatus::Included
            };
            trials.push(TrialRecord::measured(receipt, status));
        }
        trials.sort_by_key(|trial| {
            (
                trial.authority_position.unwrap_or(u64::MAX),
                trial.schedule_index,
            )
        });
        let (analysis_status, metrics) = analyze(registered, &attempts)?;
        let first_measured_run = attempts
            .values()
            .min_by_key(|receipt| receipt.authority_position)
            .map(|receipt| FirstMeasuredRun {
                timestamp: receipt.recorded_at.clone(),
                trial_digest: receipt.harness_report_digest.clone(),
            });
        Ok(StatisticalReport {
            schema_version: "1.0".to_owned(),
            kind: "core_statistical_report".to_owned(),
            preregistration: registered.preregistration.clone(),
            preregistration_digest: registered.preregistration_digest.clone(),
            registration: registered.registration.clone(),
            evidence_source,
            first_measured_run,
            sample_counts: sample_counts(&trials),
            trials,
            analysis_status,
            metrics,
        })
    }

    fn verify_execution_evidence(
        &self,
        registered: &RegisteredPreregistration,
    ) -> Result<(), StatsError> {
        let mut statement = self
            .connection
            .prepare("SELECT receipt_bytes, harness_bytes, events_bytes FROM executions WHERE registration_sequence = ?1 ORDER BY authority_position")
            .map_err(store)?;
        let rows = statement
            .query_map([registered.registration.sequence], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(store)?;
        for row in rows {
            let (receipt_bytes, harness_report_bytes, events_bytes) = row.map_err(store)?;
            let receipt =
                serde_json::from_slice(&receipt_bytes).map_err(|_| StatsError::TrialTamper)?;
            self.validate_stored_receipt(
                registered,
                &receipt,
                &TrialEvidence {
                    harness_report_bytes,
                    events_bytes,
                },
                &receipt_bytes,
            )?;
        }
        Ok(())
    }

    fn verify_registered(&self, registered: &RegisteredPreregistration) -> Result<(), StatsError> {
        registered
            .preregistration
            .validate()
            .map_err(|_| StatsError::InvalidRegistration)?;
        let stored: Vec<u8> = self
            .connection
            .query_row(
                "SELECT bytes FROM registrations WHERE sequence = ?1",
                [registered.registration.sequence],
                |row| row.get(0),
            )
            .optional()
            .map_err(store)?
            .ok_or(StatsError::InvalidRegistration)?;
        if stored != registered.canonical_bytes()? {
            return Err(StatsError::InvalidRegistration);
        }
        verify_registration(&self.credential, registered)
    }

    fn verify_admission(
        &self,
        registered: &RegisteredPreregistration,
        admission: &TrialAdmission,
    ) -> Result<(), StatsError> {
        let stored: Vec<u8> = self
            .connection
            .query_row(
                "SELECT bytes FROM admissions WHERE registration_sequence = ?1 AND schedule_index = ?2",
                params![registered.registration.sequence, admission.schedule_index],
                |row| row.get(0),
            )
            .optional()
            .map_err(store)?
            .ok_or(StatsError::InvalidAdmission)?;
        let roster = registered
            .preregistration
            .roster
            .get(admission.schedule_index)
            .ok_or(StatsError::InvalidAdmission)?;
        if stored != canonical_bytes(admission)?
            || admission.schema_version != "1.0"
            || admission.kind != "trial_admission"
            || admission.authority_id != self.credential.authority_id
            || admission.registration_sequence != registered.registration.sequence
            || admission.preregistration_digest != registered.preregistration_digest
            || admission.attempt_ordinal != 1
            || admission.trial_id != roster.trial_id
            || admission.pair_id != roster.pair_id
            || admission.task_id != roster.task_id
            || admission.dataset_member_id != roster.dataset_member_id
            || admission.task_manifest_digest != roster.task_manifest_digest
            || admission.seed != roster.seed
            || admission.arm != roster.arm
            || admission.admitted_at <= registered.registration.registered_at
            || admission.token_digest
                != sha256(&canonical_bytes(&AdmissionMessage::from(admission))?)
        {
            return Err(StatsError::InvalidAdmission);
        }
        verify_authentication(
            &self.credential,
            &canonical_bytes(&AdmissionMessage::from(admission))?,
            &admission.authentication,
        )
        .map_err(|_| StatsError::InvalidAdmission)
    }

    fn verify_run_config(
        &self,
        registered: &RegisteredPreregistration,
        roster: &RosterEntry,
        config: &TrialRunConfig,
    ) -> Result<(), StatsError> {
        let stored: Option<(String, String, String, u64, String, Vec<u8>)> = self
            .connection
            .query_row(
                "SELECT token_digest, run_config_digest, scheduler_run_id,
                        scheduler_consumption_position, scheduler_consumption_digest, run_config_bytes
                 FROM admission_consumptions WHERE admission_position = ?1",
                [config.admission_position],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()
            .map_err(store)?;
        if stored
            != Some((
                config.admission_token_digest.clone(),
                config.immutable_digest.clone(),
                config.scheduler_run_id.clone(),
                config.scheduler_consumption_position,
                config.scheduler_consumption_digest.clone(),
                canonical_bytes(config)?,
            ))
            || config.schema_version != "1.1"
            || config.kind != "immutable_trial_run_config"
            || config.authority_id != self.credential.authority_id
            || config.registration_sequence != registered.registration.sequence
            || config.preregistration_digest != registered.preregistration_digest
            || config.schedule_index != roster.schedule_index
            || config.trial_id != roster.trial_id
            || config.pair_id != roster.pair_id
            || config.task_id != roster.task_id
            || config.dataset_member_id != roster.dataset_member_id
            || config.task_manifest_digest != roster.task_manifest_digest
            || config.seed != roster.seed
            || config.arm != roster.arm
            || config.config_digest != roster.config_digest
            || config.model_digest != roster.model_digest
            || config.model_settings_digest != roster.model_settings_digest
            || config.provider_capability_digest != roster.provider_capability_digest
            || !valid_id(&config.scheduler_run_id)
            || config.scheduler_consumption_position == 0
            || !valid_digest(&config.scheduler_consumption_digest)
            || config.immutable_digest != trial_run_config_digest(config)?
        {
            return Err(StatsError::InvalidAdmission);
        }
        Ok(())
    }

    fn validate_stored_receipt(
        &self,
        registered: &RegisteredPreregistration,
        receipt: &MeasuredTrialReceipt,
        evidence: &TrialEvidence,
        receipt_bytes: &[u8],
    ) -> Result<(), StatsError> {
        let roster = registered
            .preregistration
            .roster
            .get(receipt.schedule_index)
            .ok_or(StatsError::TrialTamper)?;
        let config = self.load_run_config(receipt)?;
        self.verify_run_config(registered, roster, &config)
            .map_err(|_| StatsError::TrialTamper)?;
        if evidence.harness_report_bytes.is_empty() && evidence.events_bytes.is_empty() {
            if canonical_bytes(receipt)? != receipt_bytes
                || receipt.schema_version != "1.0"
                || receipt.kind != "measured_trial_receipt"
                || receipt.registration_sequence != registered.registration.sequence
                || receipt.preregistration_digest != registered.preregistration_digest
                || receipt.schedule_index != roster.schedule_index
                || receipt.trial_id != roster.trial_id
                || receipt.pair_id != roster.pair_id
                || receipt.task_id != roster.task_id
                || receipt.dataset_member_id != roster.dataset_member_id
                || receipt.seed != roster.seed
                || receipt.arm != roster.arm
                || receipt.roster_membership.schedule_index != roster.schedule_index
                || receipt.roster_membership.roster_entry_digest
                    != sha256(&canonical_bytes(roster)?)
                || receipt.roster_membership.task_set_digest
                    != registered.preregistration.digests.task_set
                || receipt.roster_membership.dataset_digest
                    != registered.preregistration.digests.dataset
                || receipt.roster_membership.experiment_design_digest
                    != registered.preregistration.digests.experiment
                || receipt.admission_position != config.admission_position
                || receipt.admission_nonce != config.admission_nonce
                || receipt.admission_token_digest != config.admission_token_digest
                || receipt.scheduler_run_id != config.scheduler_run_id
                || receipt.scheduler_consumption_position != config.scheduler_consumption_position
                || receipt.scheduler_consumption_digest != config.scheduler_consumption_digest
                || receipt.event_high_watermark != config.event_start_watermark
                || receipt.config_digest != config.config_digest
                || receipt.model_digest != config.model_digest
                || receipt.harness_report_digest != sha256(&[])
                || receipt.events_digest != sha256(&[])
                || receipt.outcome != TrialOutcome::Error
                || receipt.intervention
                || !receipt.cost_usd.is_finite()
                || receipt.cost_usd <= 0.0
                || !receipt.latency_ms.is_finite()
                || receipt.latency_ms <= 0.0
                || receipt.elapsed_millis == 0
                || (!receipt.cost_imputed && receipt.durable_usage.is_none())
                || receipt.verification_passed
                || !valid_text(&receipt.failure_reason, 4096)
                || !receipt.exclusion_reason.is_empty()
                || receipt.recorded_at <= registered.registration.registered_at
            {
                return Err(StatsError::TrialTamper);
            }
            return verify_harness_authentication(
                &self.credential,
                &canonical_bytes(&MeasuredMessage::from(receipt))?,
                &receipt.authentication,
            )
            .map_err(|_| StatsError::TrialTamper);
        }
        let derived = validate_trial_evidence(registered, roster, &config, evidence)
            .map_err(|_| StatsError::TrialTamper)?;
        if canonical_bytes(receipt)? != receipt_bytes
            || receipt.schema_version != "1.0"
            || receipt.kind != "measured_trial_receipt"
            || receipt.registration_sequence != registered.registration.sequence
            || receipt.preregistration_digest != registered.preregistration_digest
            || receipt.trial_id != roster.trial_id
            || receipt.pair_id != roster.pair_id
            || receipt.task_id != roster.task_id
            || receipt.dataset_member_id != roster.dataset_member_id
            || receipt.seed != roster.seed
            || receipt.arm != roster.arm
            || receipt.roster_membership.schedule_index != roster.schedule_index
            || receipt.roster_membership.roster_entry_digest != sha256(&canonical_bytes(roster)?)
            || receipt.roster_membership.task_set_digest
                != registered.preregistration.digests.task_set
            || receipt.roster_membership.dataset_digest
                != registered.preregistration.digests.dataset
            || receipt.roster_membership.experiment_design_digest
                != registered.preregistration.digests.experiment
            || receipt.admission_position != config.admission_position
            || receipt.admission_nonce != config.admission_nonce
            || receipt.admission_token_digest != config.admission_token_digest
            || receipt.scheduler_run_id != config.scheduler_run_id
            || receipt.scheduler_consumption_position != config.scheduler_consumption_position
            || receipt.scheduler_consumption_digest != config.scheduler_consumption_digest
            || receipt.event_high_watermark != derived.event_high_watermark
            || receipt.config_digest != derived.config_digest
            || receipt.model_digest != derived.model_digest
            || receipt.harness_report_digest != sha256(&evidence.harness_report_bytes)
            || receipt.events_digest != sha256(&evidence.events_bytes)
            || receipt.evidence_source != derived.evidence_source
            || receipt.outcome != derived.outcome
            || receipt.intervention != derived.intervention
            || receipt.cost_usd != derived.cost_usd
            || receipt.latency_ms != derived.latency_ms
            || receipt.elapsed_millis != derived.latency_ms as u64
            || receipt.cost_imputed
            || receipt.latency_imputed
            || receipt.durable_usage.is_some()
            || receipt.verification_passed != derived.verification_passed
            || receipt.failure_reason != derived.failure_reason
            || receipt.exclusion_reason != derived.exclusion_reason
            || receipt.recorded_at <= registered.registration.registered_at
        {
            return Err(StatsError::TrialTamper);
        }
        verify_harness_authentication(
            &self.credential,
            &canonical_bytes(&MeasuredMessage::from(receipt))?,
            &receipt.authentication,
        )
        .map_err(|_| StatsError::TrialTamper)
    }

    fn load_run_config(
        &self,
        receipt: &MeasuredTrialReceipt,
    ) -> Result<TrialRunConfig, StatsError> {
        let bytes: Vec<u8> = self
            .connection
            .query_row(
                "SELECT run_config_bytes FROM admission_consumptions WHERE admission_position = ?1",
                [receipt.admission_position],
                |row| row.get(0),
            )
            .map_err(store)?;
        let config: TrialRunConfig =
            serde_json::from_slice(&bytes).map_err(|_| StatsError::TrialTamper)?;
        if canonical_bytes(&config)? != bytes {
            return Err(StatsError::TrialTamper);
        }
        if trial_run_config_digest(&config)? != config.immutable_digest {
            return Err(StatsError::TrialTamper);
        }
        Ok(config)
    }

    fn admitted_indexes(&self, sequence: u64) -> Result<BTreeSet<usize>, StatsError> {
        let mut statement = self
            .connection
            .prepare("SELECT schedule_index FROM admissions WHERE registration_sequence = ?1")
            .map_err(store)?;
        let values = statement
            .query_map([sequence], |row| row.get(0))
            .map_err(store)?;
        values.map(|value| value.map_err(store)).collect()
    }

    fn verify_chain(&self) -> Result<(), StatsError> {
        let metadata: Option<(String, String, String, String)> = self
            .connection
            .query_row(
                "SELECT authority_id, key_id, genesis_digest, authority_epoch FROM registration_authority WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(store)?;
        if metadata
            != Some((
                self.credential.authority_id.clone(),
                self.credential.key_id.clone(),
                self.credential.genesis_digest.clone(),
                self.credential.authority_epoch.clone(),
            ))
            || self.credential.authority_epoch.as_str() < AUTHORITY_EPOCH_FLOOR
        {
            return Err(StatsError::AlternateGenesis);
        }
        let mut statement = self.connection.prepare(
            "SELECT position, previous_digest, entry_digest, event_type, recorded_at, registration_sequence, schedule_index, attempt_ordinal, payload_digest, payload_bytes FROM ledger ORDER BY position",
        ).map_err(store)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<u64>>(5)?,
                    row.get::<_, Option<usize>>(6)?,
                    row.get::<_, u64>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Vec<u8>>(9)?,
                ))
            })
            .map_err(store)?;
        let mut expected_position = 1u64;
        let mut previous_digest = self.credential.genesis_digest.clone();
        let mut previous_time = self.credential.authority_epoch.clone();
        let mut counts = BTreeMap::<String, usize>::new();
        for row in rows {
            let (
                position,
                stored_previous,
                entry_digest,
                event_type,
                recorded_at,
                registration_sequence,
                schedule_index,
                attempt_ordinal,
                payload_digest,
                bytes,
            ) = row.map_err(store)?;
            let expected_digest = sha256(&canonical_bytes(&LedgerEntryMessage {
                domain: "kit-contiguous-evaluation-ledger-v1",
                position,
                previous_digest: &stored_previous,
                event_type: &event_type,
                recorded_at: &recorded_at,
                registration_sequence,
                schedule_index,
                attempt_ordinal,
                payload_digest: &payload_digest,
            })?);
            if position != expected_position
                || stored_previous != previous_digest
                || recorded_at <= previous_time
                || payload_digest != sha256(&bytes)
                || entry_digest != expected_digest
            {
                return Err(StatsError::LedgerTamper);
            }
            match event_type.as_str() {
                "registration" => {
                    let registered: RegisteredPreregistration =
                        serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
                    if registered.registration.authority_position != position
                        || registered.registration.previous_entry_digest != stored_previous
                        || registered.registration.attempt_ordinal != attempt_ordinal
                        || registered.registration.registered_at != recorded_at
                        || registration_sequence != Some(registered.registration.sequence)
                    {
                        return Err(StatsError::LedgerTamper);
                    }
                    verify_registration(&self.credential, &registered)?;
                }
                "admission" => {
                    let admission: TrialAdmission =
                        serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
                    if admission.authority_position != position
                        || admission.previous_entry_digest != stored_previous
                        || admission.attempt_ordinal != attempt_ordinal
                        || admission.admitted_at != recorded_at
                        || registration_sequence != Some(admission.registration_sequence)
                        || schedule_index != Some(admission.schedule_index)
                    {
                        return Err(StatsError::LedgerTamper);
                    }
                    verify_authentication(
                        &self.credential,
                        &canonical_bytes(&AdmissionMessage::from(&admission))?,
                        &admission.authentication,
                    )
                    .map_err(|_| StatsError::LedgerTamper)?;
                }
                "admission_consumed" => {
                    let config: TrialRunConfig =
                        serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
                    if config.registration_sequence != registration_sequence.unwrap_or_default()
                        || Some(config.schedule_index) != schedule_index
                        || config.immutable_digest != trial_run_config_digest(&config)?
                        || attempt_ordinal != 1
                    {
                        return Err(StatsError::LedgerTamper);
                    }
                }
                "execution" => {
                    let receipt: MeasuredTrialReceipt =
                        serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
                    if receipt.authority_position != position
                        || receipt.previous_entry_digest != stored_previous
                        || receipt.attempt_ordinal != attempt_ordinal
                        || receipt.recorded_at != recorded_at
                        || registration_sequence != Some(receipt.registration_sequence)
                        || schedule_index != Some(receipt.schedule_index)
                    {
                        return Err(StatsError::LedgerTamper);
                    }
                    verify_harness_authentication(
                        &self.credential,
                        &canonical_bytes(&MeasuredMessage::from(&receipt))?,
                        &receipt.authentication,
                    )
                    .map_err(|_| StatsError::LedgerTamper)?;
                }
                "experiment_frozen" => {
                    let frozen: ExperimentFrozen =
                        serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
                    if frozen.registration_sequence != registration_sequence.unwrap_or_default()
                        || frozen.recorded_at != recorded_at
                        || frozen.ledger_cutoff + 1 != position
                        || schedule_index.is_some()
                        || attempt_ordinal != 0
                    {
                        return Err(StatsError::LedgerTamper);
                    }
                }
                "report" => {}
                _ => return Err(StatsError::LedgerTamper),
            }
            *counts.entry(event_type).or_default() += 1;
            expected_position = expected_position
                .checked_add(1)
                .ok_or(StatsError::SequenceExhausted)?;
            previous_digest = entry_digest;
            previous_time = recorded_at;
        }
        drop(statement);
        for (table, event_type) in [
            ("registrations", "registration"),
            ("admissions", "admission"),
            ("admission_consumptions", "admission_consumed"),
            ("executions", "execution"),
            ("experiment_freezes", "experiment_frozen"),
            ("reports", "report"),
        ] {
            let count: usize = self
                .connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .map_err(store)?;
            if count != counts.get(event_type).copied().unwrap_or(0) {
                return Err(StatsError::LedgerTamper);
            }
        }
        let registrations = {
            let mut statement = self
                .connection
                .prepare("SELECT bytes FROM registrations ORDER BY sequence")
                .map_err(store)?;
            statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))
                .map_err(store)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store)?
        };
        for bytes in registrations {
            let registered: RegisteredPreregistration =
                serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
            self.verify_execution_evidence(&registered)
                .map_err(|_| StatsError::LedgerTamper)?;
        }
        let reports = {
            let mut statement = self
                .connection
                .prepare("SELECT authority_position, digest, report_bytes, receipt_bytes FROM reports ORDER BY authority_position")
                .map_err(store)?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })
                .map_err(store)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store)?
        };
        for (position, digest, report_bytes, receipt_bytes) in reports {
            let report: StatisticalReport =
                serde_json::from_slice(&report_bytes).map_err(|_| StatsError::LedgerTamper)?;
            let receipt: StatisticalReportReceipt =
                serde_json::from_slice(&receipt_bytes).map_err(|_| StatsError::LedgerTamper)?;
            let ledger: (String, String) = self
                .connection
                .query_row(
                    "SELECT previous_digest, entry_digest FROM ledger WHERE position = ?1 AND event_type = 'report'",
                    [position],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(store)?;
            let freeze: (u64, u64) = self
                .connection
                .query_row(
                    "SELECT ledger_cutoff, authority_position FROM experiment_freezes WHERE registration_sequence = ?1",
                    [receipt.registration_sequence],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(store)?;
            if canonical_bytes(&report)? != report_bytes
                || digest != sha256(&report_bytes)
                || receipt.report_digest != digest
                || receipt.ledger_position != position
                || receipt.previous_entry_digest != ledger.0
                || receipt.ledger_head_digest != ledger.1
                || receipt.ledger_cutoff != freeze.0
                || receipt.freeze_position != freeze.1
                || position <= freeze.1
            {
                return Err(StatsError::LedgerTamper);
            }
            verify_authentication(
                &self.credential,
                &canonical_bytes(&StatisticalReportReceiptMessage::from(&receipt))?,
                &receipt.authentication,
            )
            .map_err(|_| StatsError::LedgerTamper)?;
        }
        self.verify_anchor_commits(expected_position - 1)?;
        if self.anchored.counter != 0 {
            let current = self.anchor.current()?.ok_or(StatsError::LedgerRollback)?;
            if current != self.anchored
                || current.authority_id != self.credential.authority_id
                || current.ledger_position != expected_position - 1
                || current.ledger_head_digest != previous_digest
            {
                return Err(StatsError::LedgerRollback);
            }
        }
        Ok(())
    }

    fn verify_anchor_commits(&self, ledger_position: u64) -> Result<(), StatsError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT ledger_position, previous_counter, previous_head_digest,
                        ledger_head_digest, record_digest, record_bytes, state, receipt_bytes
                 FROM anchor_commits ORDER BY ledger_position",
            )
            .map_err(store)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<Vec<u8>>>(7)?,
                ))
            })
            .map_err(store)?;
        let mut expected_position = 0;
        let mut previous_counter = 0;
        let mut previous_head = String::new();
        let mut last = None;
        for row in rows {
            let (position, counter, previous, head, record_digest, bytes, state, receipt_bytes) =
                row.map_err(store)?;
            let record: PendingAnchorRecord =
                serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
            let receipt: LedgerAnchorReceipt =
                serde_json::from_slice(&receipt_bytes.ok_or(StatsError::LedgerTamper)?)
                    .map_err(|_| StatsError::LedgerTamper)?;
            let ledger_head = if position == 0 {
                self.credential.genesis_digest.clone()
            } else {
                self.connection
                    .query_row(
                        "SELECT entry_digest FROM ledger WHERE position = ?1",
                        [position],
                        |row| row.get(0),
                    )
                    .map_err(store)?
            };
            if position != expected_position
                || state != "anchored"
                || counter != previous_counter
                || previous != previous_head
                || head != ledger_head
                || canonical_bytes(&record)? != bytes
                || sha256(&bytes) != record_digest
                || record.domain != "kit-pending-ledger-anchor-v1"
                || record.authority_id != self.credential.authority_id
                || record.previous_counter != counter
                || record.previous_head_digest != previous
                || record.ledger_position != position
                || record.ledger_head_digest != head
                || !valid_anchor_receipt(&receipt)
                || receipt.authority_id != self.credential.authority_id
                || receipt.counter != counter + 1
                || receipt.ledger_position != position
                || receipt.ledger_head_digest != head
            {
                return Err(StatsError::LedgerTamper);
            }
            expected_position += 1;
            previous_counter = receipt.counter;
            previous_head = receipt.ledger_head_digest.clone();
            last = Some(receipt);
        }
        if expected_position != ledger_position + 1 || last.as_ref() != Some(&self.anchored) {
            return Err(StatsError::LedgerTamper);
        }
        Ok(())
    }

    fn ledger_head(&self) -> Result<(u64, String), StatsError> {
        self.connection
            .query_row(
                "SELECT position, entry_digest FROM ledger ORDER BY position DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(store)
            .map(|head| head.unwrap_or((0, self.credential.genesis_digest.clone())))
    }

    fn insert_pending_anchor(&mut self, position: u64, digest: &str) -> Result<(), StatsError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        stage_anchor(&transaction, &self.anchored, position, digest)?;
        transaction.commit().map_err(store)
    }

    fn recover_anchor(&mut self) -> Result<(), StatsError> {
        let pending = {
            let mut statement = self
                .connection
                .prepare(
                    "SELECT ledger_position, previous_counter, previous_head_digest,
                            ledger_head_digest, record_digest, record_bytes
                     FROM anchor_commits WHERE state = 'pending_anchor' ORDER BY ledger_position",
                )
                .map_err(store)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                })
                .map_err(store)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store)?;
            if rows.len() > 1 {
                return Err(StatsError::AnchorFork);
            }
            rows.into_iter().next()
        };
        let Some((position, previous_counter, previous_digest, digest, record_digest, bytes)) =
            pending
        else {
            if self.anchored.counter != 0 && self.anchor.current()?.as_ref() != Some(&self.anchored)
            {
                return Err(StatsError::LedgerRollback);
            }
            return Ok(());
        };
        let record: PendingAnchorRecord =
            serde_json::from_slice(&bytes).map_err(|_| StatsError::LedgerTamper)?;
        if canonical_bytes(&record)? != bytes
            || sha256(&bytes) != record_digest
            || record.authority_id != self.credential.authority_id
            || record.previous_counter != previous_counter
            || record.previous_head_digest != previous_digest
            || record.ledger_position != position
            || record.ledger_head_digest != digest
            || previous_counter != self.anchored.counter
            || (previous_counter != 0
                && (previous_digest != self.anchored.ledger_head_digest
                    || position <= self.anchored.ledger_position))
        {
            return Err(StatsError::AnchorFork);
        }
        let expected = (previous_counter != 0).then_some(&self.anchored);
        let current = self.anchor.current()?;
        let next = if current.as_ref() == expected {
            self.anchor.compare_and_swap(
                expected,
                &self.credential.authority_id,
                position,
                &digest,
            )?
        } else if current.as_ref().is_some_and(|receipt| {
            receipt.authority_id == self.credential.authority_id
                && receipt.counter == previous_counter + 1
                && receipt.ledger_position == position
                && receipt.ledger_head_digest == digest
        }) {
            current.expect("checked above")
        } else {
            return Err(StatsError::AnchorFork);
        };
        if !valid_anchor_receipt(&next)
            || next.authority_id != self.credential.authority_id
            || next.counter != previous_counter + 1
            || next.ledger_position != position
            || next.ledger_head_digest != digest
        {
            return Err(StatsError::Anchor("anchor returned an invalid signed head"));
        }
        let receipt_bytes = canonical_bytes(&next)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let changed = transaction
            .execute(
                "UPDATE anchor_commits SET state = 'anchored', receipt_bytes = ?2
                 WHERE ledger_position = ?1 AND state = 'pending_anchor'",
                params![position, receipt_bytes],
            )
            .map_err(store)?;
        if changed != 1 {
            return Err(StatsError::AnchorFork);
        }
        transaction.commit().map_err(store)?;
        self.anchored = next;
        Ok(())
    }

    fn advance_anchor(&mut self) -> Result<(), StatsError> {
        self.recover_anchor()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialAdmission {
    schema_version: String,
    kind: String,
    authority_id: String,
    authority_position: u64,
    previous_entry_digest: String,
    attempt_ordinal: u64,
    registration_sequence: u64,
    preregistration_digest: String,
    schedule_index: usize,
    trial_id: String,
    pair_id: String,
    task_id: String,
    dataset_member_id: String,
    task_manifest_digest: String,
    seed: u64,
    arm: Arm,
    nonce: String,
    token_digest: String,
    admitted_at: String,
    authentication: Authentication,
}

impl TrialAdmission {
    pub fn trial_id(&self) -> &str {
        &self.trial_id
    }

    pub const fn arm(&self) -> Arm {
        self.arm
    }

    pub fn token_digest(&self) -> &str {
        &self.token_digest
    }

    pub fn scheduler_token(
        &self,
    ) -> Result<kit::runtime::scheduler::TrialAdmissionToken, StatsError> {
        Ok(kit::runtime::scheduler::TrialAdmissionToken {
            authority_id: self.authority_id.clone(),
            authority_position: self.authority_position,
            registration_sequence: self.registration_sequence,
            preregistration_digest: self.preregistration_digest.clone(),
            schedule_index: self.schedule_index,
            trial_id: self.trial_id.clone(),
            pair_id: self.pair_id.clone(),
            task_id: self.task_id.clone(),
            dataset_member_id: self.dataset_member_id.clone(),
            task_manifest_digest: self.task_manifest_digest.clone(),
            seed: self.seed,
            arm: match self.arm {
                Arm::Baseline => "baseline",
                Arm::Candidate => "candidate",
            }
            .to_owned(),
            nonce: self.nonce.clone(),
            token_digest: self.token_digest.clone(),
            authentication: serde_json::to_string(&self.authentication)
                .map_err(|error| StatsError::Serialization(error.to_string()))?,
        })
    }
}

impl kit::runtime::scheduler::TrialAdmissionVerifier for RegistrationAuthority {
    fn verify(&self, token: &kit::runtime::scheduler::TrialAdmissionToken) -> bool {
        let bytes = self.connection.query_row(
            "SELECT bytes FROM admissions WHERE authority_position = ?1",
            [token.authority_position],
            |row| row.get::<_, Vec<u8>>(0),
        );
        let Ok(bytes) = bytes else {
            return false;
        };
        let Ok(admission) = serde_json::from_slice::<TrialAdmission>(&bytes) else {
            return false;
        };
        admission.scheduler_token().as_ref() == Ok(token)
            && verify_authentication(
                &self.credential,
                &canonical_bytes(&AdmissionMessage::from(&admission)).unwrap_or_default(),
                &admission.authentication,
            )
            .is_ok()
    }
}

impl kit::runtime::scheduler::AnchoredConsumptionVerifier for RegistrationAuthority {
    fn verify(
        &self,
        pending: &kit::runtime::scheduler::PendingStatisticalTrial,
        receipt: &kit::runtime::scheduler::AnchoredConsumptionReceipt,
    ) -> bool {
        if receipt.authority_id != self.credential.authority_id
            || receipt.scheduler_run_id != pending.run_id.to_string()
            || receipt.admission_token_digest != pending.admission_token_digest
            || receipt.admission_nonce != pending.admission_nonce
            || receipt.scheduler_consumption_position != pending.consumption_position
            || receipt.scheduler_consumption_digest != pending.consumption_digest
        {
            return false;
        }
        let stored = self.connection.query_row(
            "SELECT l.entry_digest, a.receipt_bytes
             FROM ledger l JOIN anchor_commits a ON a.ledger_position = l.position
             WHERE l.position = ?1 AND l.event_type = 'admission_consumed'
               AND a.state = 'anchored'",
            [receipt.ledger_position],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        );
        let Ok((ledger_head, anchor_bytes)) = stored else {
            return false;
        };
        let Ok(anchor) = serde_json::from_slice::<LedgerAnchorReceipt>(&anchor_bytes) else {
            return false;
        };
        if ledger_head != receipt.ledger_head_digest
            || anchor.authority_id != receipt.anchor_identity
            || anchor.source != receipt.anchor_source
            || anchor.counter != receipt.anchor_counter
            || anchor.ledger_position != receipt.ledger_position
            || anchor.ledger_head_digest != receipt.ledger_head_digest
            || anchor.signature != receipt.anchor_signature
        {
            return false;
        }
        verify_authentication(
            &self.credential,
            &canonical_bytes(&AnchoredConsumptionMessage::from(receipt)).unwrap_or_default(),
            &Authentication {
                algorithm: receipt.authentication_algorithm.clone(),
                key_id: receipt.authentication_key_id.clone(),
                tag: receipt.authentication_tag.clone(),
            },
        )
        .is_ok()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialRunConfig {
    pub schema_version: String,
    pub kind: String,
    pub authority_id: String,
    pub registration_sequence: u64,
    pub preregistration_digest: String,
    pub admission_position: u64,
    pub admission_nonce: String,
    pub admission_token_digest: String,
    pub scheduler_run_id: String,
    pub scheduler_consumption_position: u64,
    pub scheduler_consumption_digest: String,
    pub schedule_index: usize,
    pub trial_id: String,
    pub pair_id: String,
    pub task_id: String,
    pub dataset_member_id: String,
    pub task_manifest_digest: String,
    pub seed: u64,
    pub arm: Arm,
    pub config_digest: String,
    pub model_digest: String,
    pub model_settings_digest: String,
    pub provider_capability_digest: String,
    pub event_start_watermark: u64,
    pub immutable_digest: String,
}

#[derive(Clone, Debug)]
pub struct BoundTrialEnvelope {
    receipt: MeasuredTrialReceipt,
    digest: String,
    harness_report_bytes: Vec<u8>,
    events_bytes: Vec<u8>,
}

pub type HarnessExecutionReceipt = BoundTrialEnvelope;

#[derive(Clone, Debug)]
struct TrialEvidence {
    harness_report_bytes: Vec<u8>,
    events_bytes: Vec<u8>,
}

impl BoundTrialEnvelope {
    pub fn receipt(&self) -> &MeasuredTrialReceipt {
        &self.receipt
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredTrialReceipt {
    pub schema_version: String,
    pub kind: String,
    pub authority_position: u64,
    pub previous_entry_digest: String,
    pub attempt_ordinal: u64,
    pub registration_sequence: u64,
    pub preregistration_digest: String,
    pub schedule_index: usize,
    pub trial_id: String,
    pub pair_id: String,
    pub task_id: String,
    pub dataset_member_id: String,
    pub seed: u64,
    pub arm: Arm,
    pub roster_membership: RosterMembershipProof,
    pub admission_position: u64,
    pub admission_nonce: String,
    pub admission_token_digest: String,
    pub scheduler_run_id: String,
    pub scheduler_consumption_position: u64,
    pub scheduler_consumption_digest: String,
    pub event_high_watermark: u64,
    pub config_digest: String,
    pub model_digest: String,
    pub harness_report_digest: String,
    pub events_digest: String,
    pub evidence_source: EvidenceSource,
    pub recorded_at: String,
    pub outcome: TrialOutcome,
    pub intervention: bool,
    pub cost_usd: f64,
    pub latency_ms: f64,
    pub elapsed_millis: u64,
    pub cost_imputed: bool,
    pub latency_imputed: bool,
    pub durable_usage: Option<TerminalDurableUsage>,
    pub verification_passed: bool,
    pub failure_reason: String,
    pub exclusion_reason: String,
    pub authentication: Authentication,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RosterMembershipProof {
    pub schedule_index: usize,
    pub roster_entry_digest: String,
    pub task_set_digest: String,
    pub dataset_digest: String,
    pub experiment_design_digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialOutcome {
    Success,
    Failure,
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    ProductionTrusted,
    ConformanceSourceSemantics,
}

impl EvidenceSource {
    pub(crate) fn from_wire(value: &str) -> Option<Self> {
        match value {
            "production_authenticated" => Some(Self::ProductionTrusted),
            "conformance_source_semantics_fake" => Some(Self::ConformanceSourceSemantics),
            _ => None,
        }
    }

    pub(crate) const fn as_wire(self) -> &'static str {
        match self {
            Self::ProductionTrusted => "production_authenticated",
            Self::ConformanceSourceSemantics => "conformance_source_semantics_fake",
        }
    }
}

#[derive(Serialize)]
struct RegistrationMessage<'a> {
    domain: &'static str,
    authority_id: &'a str,
    genesis_digest: &'a str,
    sequence: u64,
    authority_position: u64,
    attempt_ordinal: u64,
    registered_at: &'a str,
    previous_entry_digest: &'a str,
    preregistration_digest: &'a str,
}

#[derive(Serialize)]
struct AdmissionMessage<'a> {
    domain: &'static str,
    authority_id: &'a str,
    authority_position: u64,
    previous_entry_digest: &'a str,
    attempt_ordinal: u64,
    registration_sequence: u64,
    preregistration_digest: &'a str,
    schedule_index: usize,
    trial_id: &'a str,
    pair_id: &'a str,
    task_id: &'a str,
    dataset_member_id: &'a str,
    task_manifest_digest: &'a str,
    seed: u64,
    arm: Arm,
    nonce: &'a str,
    admitted_at: &'a str,
}

impl<'a> From<&'a TrialAdmission> for AdmissionMessage<'a> {
    fn from(value: &'a TrialAdmission) -> Self {
        Self {
            domain: "kit-trial-admission-v1",
            authority_id: &value.authority_id,
            authority_position: value.authority_position,
            previous_entry_digest: &value.previous_entry_digest,
            attempt_ordinal: value.attempt_ordinal,
            registration_sequence: value.registration_sequence,
            preregistration_digest: &value.preregistration_digest,
            schedule_index: value.schedule_index,
            trial_id: &value.trial_id,
            pair_id: &value.pair_id,
            task_id: &value.task_id,
            dataset_member_id: &value.dataset_member_id,
            task_manifest_digest: &value.task_manifest_digest,
            seed: value.seed,
            arm: value.arm,
            nonce: &value.nonce,
            admitted_at: &value.admitted_at,
        }
    }
}

#[derive(Serialize)]
struct MeasuredMessage<'a> {
    domain: &'static str,
    authority_position: u64,
    previous_entry_digest: &'a str,
    attempt_ordinal: u64,
    registration_sequence: u64,
    preregistration_digest: &'a str,
    schedule_index: usize,
    trial_id: &'a str,
    pair_id: &'a str,
    task_id: &'a str,
    dataset_member_id: &'a str,
    seed: u64,
    arm: Arm,
    roster_membership: &'a RosterMembershipProof,
    admission_position: u64,
    admission_nonce: &'a str,
    admission_token_digest: &'a str,
    scheduler_run_id: &'a str,
    scheduler_consumption_position: u64,
    scheduler_consumption_digest: &'a str,
    event_high_watermark: u64,
    config_digest: &'a str,
    model_digest: &'a str,
    harness_report_digest: &'a str,
    events_digest: &'a str,
    evidence_source: EvidenceSource,
    recorded_at: &'a str,
    outcome: TrialOutcome,
    intervention: bool,
    cost_usd: f64,
    latency_ms: f64,
    elapsed_millis: u64,
    cost_imputed: bool,
    latency_imputed: bool,
    durable_usage: &'a Option<TerminalDurableUsage>,
    verification_passed: bool,
    failure_reason: &'a str,
    exclusion_reason: &'a str,
}

impl<'a> From<&'a MeasuredTrialReceipt> for MeasuredMessage<'a> {
    fn from(value: &'a MeasuredTrialReceipt) -> Self {
        Self {
            domain: "kit-measured-trial-v1",
            authority_position: value.authority_position,
            previous_entry_digest: &value.previous_entry_digest,
            attempt_ordinal: value.attempt_ordinal,
            registration_sequence: value.registration_sequence,
            preregistration_digest: &value.preregistration_digest,
            schedule_index: value.schedule_index,
            trial_id: &value.trial_id,
            pair_id: &value.pair_id,
            task_id: &value.task_id,
            dataset_member_id: &value.dataset_member_id,
            seed: value.seed,
            arm: value.arm,
            roster_membership: &value.roster_membership,
            admission_position: value.admission_position,
            admission_nonce: &value.admission_nonce,
            admission_token_digest: &value.admission_token_digest,
            scheduler_run_id: &value.scheduler_run_id,
            scheduler_consumption_position: value.scheduler_consumption_position,
            scheduler_consumption_digest: &value.scheduler_consumption_digest,
            event_high_watermark: value.event_high_watermark,
            config_digest: &value.config_digest,
            model_digest: &value.model_digest,
            harness_report_digest: &value.harness_report_digest,
            events_digest: &value.events_digest,
            evidence_source: value.evidence_source,
            recorded_at: &value.recorded_at,
            outcome: value.outcome,
            intervention: value.intervention,
            cost_usd: value.cost_usd,
            latency_ms: value.latency_ms,
            elapsed_millis: value.elapsed_millis,
            cost_imputed: value.cost_imputed,
            latency_imputed: value.latency_imputed,
            durable_usage: &value.durable_usage,
            verification_passed: value.verification_passed,
            failure_reason: &value.failure_reason,
            exclusion_reason: &value.exclusion_reason,
        }
    }
}

#[derive(Serialize)]
struct StatisticalReportReceiptMessage<'a> {
    domain: &'static str,
    authority_id: &'a str,
    registration_sequence: u64,
    preregistration_digest: &'a str,
    report_digest: &'a str,
    evidence_source: EvidenceSource,
    ledger_cutoff: u64,
    freeze_position: u64,
    ledger_position: u64,
    previous_entry_digest: &'a str,
    ledger_head_digest: &'a str,
    recorded_at: &'a str,
}

#[derive(Serialize)]
struct AnchoredConsumptionMessage<'a> {
    domain: &'static str,
    authority_id: &'a str,
    scheduler_run_id: &'a str,
    admission_token_digest: &'a str,
    admission_nonce: &'a str,
    scheduler_consumption_position: u64,
    scheduler_consumption_digest: &'a str,
    ledger_position: u64,
    ledger_head_digest: &'a str,
    anchor_source: &'a str,
    anchor_identity: &'a str,
    anchor_counter: u64,
    anchor_signature: &'a str,
}

impl<'a> From<&'a kit::runtime::scheduler::AnchoredConsumptionReceipt>
    for AnchoredConsumptionMessage<'a>
{
    fn from(value: &'a kit::runtime::scheduler::AnchoredConsumptionReceipt) -> Self {
        Self {
            domain: "kit-anchored-scheduler-consumption-v1",
            authority_id: &value.authority_id,
            scheduler_run_id: &value.scheduler_run_id,
            admission_token_digest: &value.admission_token_digest,
            admission_nonce: &value.admission_nonce,
            scheduler_consumption_position: value.scheduler_consumption_position,
            scheduler_consumption_digest: &value.scheduler_consumption_digest,
            ledger_position: value.ledger_position,
            ledger_head_digest: &value.ledger_head_digest,
            anchor_source: &value.anchor_source,
            anchor_identity: &value.anchor_identity,
            anchor_counter: value.anchor_counter,
            anchor_signature: &value.anchor_signature,
        }
    }
}

impl<'a> From<&'a StatisticalReportReceipt> for StatisticalReportReceiptMessage<'a> {
    fn from(value: &'a StatisticalReportReceipt) -> Self {
        Self {
            domain: "kit-statistical-report-receipt-v1",
            authority_id: &value.authority_id,
            registration_sequence: value.registration_sequence,
            preregistration_digest: &value.preregistration_digest,
            report_digest: &value.report_digest,
            evidence_source: value.evidence_source,
            ledger_cutoff: value.ledger_cutoff,
            freeze_position: value.freeze_position,
            ledger_position: value.ledger_position,
            previous_entry_digest: &value.previous_entry_digest,
            ledger_head_digest: &value.ledger_head_digest,
            recorded_at: &value.recorded_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessReport {
    schema_version: u16,
    harness_version: String,
    trial_id: String,
    admission_source: String,
    run_config_digest: String,
    admission_position: u64,
    admission_nonce: String,
    admission_token_digest: String,
    scheduler_run_id: String,
    scheduler_consumption_position: u64,
    scheduler_consumption_digest: String,
    event_high_watermark: u64,
    task_set_digest: String,
    dataset_digest: String,
    experiment_design_digest: String,
    production_pins_digest: String,
    manifest_identity_digest: String,
    manifest_bytes_digest: String,
    task_manifest_digest: String,
    grader_manifest_digest: String,
    base_tree_digest: String,
    patch_digest: String,
    final_tree_digest: String,
    grader_image_digest: String,
    grader_harness_commit: String,
    hidden_tests_digest: String,
    acceptance_digest: String,
    gold_patch_digest: String,
    harness_config_digest: String,
    toolchain_digest: String,
    model_digest: String,
    model_settings_digest: String,
    config_digest: String,
    provider_capability_digest: String,
    agent: HarnessBoundary,
    grader: HarnessBoundary,
    agent_result_digest: String,
    events_digest: String,
    logs_digest: String,
    artifacts_digest: String,
    usage: HarnessUsage,
    outcome: TrialOutcome,
    grade: HarnessGrade,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessBoundary {
    phase: String,
    route: String,
    image_digest: String,
    runtime_identity: String,
    helper_identity: String,
    permitted_profile_digest: String,
    survivor_processes: u32,
    quiescent: bool,
    outcome: kit::executor::trial::BoundaryOutcome,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessUsage {
    turns: HarnessUsageMeasure,
    input_tokens: HarnessUsageMeasure,
    output_tokens: HarnessUsageMeasure,
    cost_microusd: HarnessUsageMeasure,
    tool_calls: HarnessUsageMeasure,
    processes: HarnessUsageMeasure,
}

#[derive(Deserialize)]
#[serde(tag = "availability", content = "value", rename_all = "snake_case")]
enum HarnessUsageMeasure {
    Measured(u64),
    Unavailable(HarnessUsageUnavailableReason),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum HarnessUsageUnavailableReason {
    ProviderDidNotReport,
    SchedulerEvidenceMissing,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessGrade {
    schema_version: u16,
    outcome: TrialOutcome,
    base_tree_digest: String,
    patch_digest: String,
    final_tree_digest: String,
    checks: Vec<HarnessCheck>,
    hidden: HarnessHidden,
    diagnostic: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessCheck {
    id: String,
    passed: bool,
    path: String,
    expected: String,
    actual: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessHidden {
    verdict: TrialOutcome,
    count: usize,
    digest: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SchedulerEvents {
    schema_version: u16,
    source: String,
    run_config_digest: String,
    admission_position: u64,
    admission_nonce: String,
    admission_token_digest: String,
    scheduler_run_id: String,
    scheduler_consumption_position: u64,
    scheduler_consumption_digest: String,
    event_high_watermark: u64,
    trial_id: String,
    pair_id: String,
    task_id: String,
    dataset_member_id: String,
    seed: u64,
    arm: Arm,
    config_digest: String,
    started_monotonic_millis: u64,
    finished_monotonic_millis: u64,
    intervention: bool,
    exclusion_reason: String,
    scheduler_events: Vec<RuntimeEventBinding>,
    provider_events: Vec<RuntimeEventBinding>,
    tool_events: Vec<RuntimeEventBinding>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeEventBinding {
    kind: String,
    event_position: u64,
    admission_token_digest: String,
}

struct DerivedMeasurement {
    config_digest: String,
    model_digest: String,
    outcome: TrialOutcome,
    intervention: bool,
    cost_usd: f64,
    latency_ms: f64,
    verification_passed: bool,
    failure_reason: String,
    exclusion_reason: String,
    event_high_watermark: u64,
    evidence_source: EvidenceSource,
}

fn validate_trial_evidence(
    registered: &RegisteredPreregistration,
    roster: &RosterEntry,
    run_config: &TrialRunConfig,
    evidence: &TrialEvidence,
) -> Result<DerivedMeasurement, StatsError> {
    let report = validate_harness_report(&evidence.harness_report_bytes)?;
    if evidence.events_bytes.is_empty()
        || evidence.events_bytes.len() > MAX_EVENTS_BYTES
        || has_json_whitespace_outside_strings(&evidence.events_bytes)
    {
        return Err(StatsError::InvalidTrial(
            "invalid canonical scheduler events",
        ));
    }
    let event_value: serde_json::Value = serde_json::from_slice(&evidence.events_bytes)
        .map_err(|_| StatsError::InvalidTrial("invalid canonical scheduler events"))?;
    if canonical_bytes(&event_value)? != evidence.events_bytes {
        return Err(StatsError::InvalidTrial(
            "invalid canonical scheduler events",
        ));
    }
    let events: SchedulerEvents = serde_json::from_value(event_value)
        .map_err(|_| StatsError::InvalidTrial("invalid canonical scheduler events"))?;
    let evidence_source = EvidenceSource::from_wire(&events.source)
        .ok_or(StatsError::InvalidTrial("invalid evidence source"))?;
    let config_digest = roster.config_digest.clone();
    let model_digest = roster.model_digest.clone();
    let runtime_events = events
        .scheduler_events
        .iter()
        .chain(&events.provider_events)
        .chain(&events.tool_events)
        .collect::<Vec<_>>();
    let positions = runtime_events
        .iter()
        .map(|event| event.event_position)
        .collect::<BTreeSet<_>>();
    let runtime_events_valid = !events.scheduler_events.is_empty()
        && !events.provider_events.is_empty()
        && !events.tool_events.is_empty()
        && positions.len() == runtime_events.len()
        && runtime_events.iter().all(|event| {
            valid_id(&event.kind)
                && event.event_position > run_config.event_start_watermark
                && event.event_position <= events.event_high_watermark
                && event.admission_token_digest == run_config.admission_token_digest
        })
        && positions.iter().next_back().copied() == Some(events.event_high_watermark);
    if events.schema_version != 1
        || !matches!(
            events.source.as_str(),
            "production_authenticated" | "conformance_source_semantics_fake"
        )
        || events.run_config_digest != run_config.immutable_digest
        || events.admission_position != run_config.admission_position
        || events.admission_nonce != run_config.admission_nonce
        || events.admission_token_digest != run_config.admission_token_digest
        || events.scheduler_run_id != run_config.scheduler_run_id
        || events.scheduler_consumption_position != run_config.scheduler_consumption_position
        || events.scheduler_consumption_digest != run_config.scheduler_consumption_digest
        || events.event_high_watermark <= run_config.event_start_watermark
        || !runtime_events_valid
        || events.trial_id != roster.trial_id
        || events.pair_id != roster.pair_id
        || events.task_id != roster.task_id
        || events.dataset_member_id != roster.dataset_member_id
        || events.seed != roster.seed
        || events.arm != roster.arm
        || events.config_digest != config_digest
        || events.finished_monotonic_millis < events.started_monotonic_millis
        || report.events_digest != sha256(&evidence.events_bytes)
        || report.trial_id != roster.trial_id
        || report.admission_source != events.source
        || report.run_config_digest != run_config.immutable_digest
        || report.admission_position != run_config.admission_position
        || report.admission_nonce != run_config.admission_nonce
        || report.admission_token_digest != run_config.admission_token_digest
        || report.scheduler_run_id != run_config.scheduler_run_id
        || report.scheduler_consumption_position != run_config.scheduler_consumption_position
        || report.scheduler_consumption_digest != run_config.scheduler_consumption_digest
        || report.event_high_watermark != events.event_high_watermark
        || report.task_set_digest != registered.preregistration.digests.task_set
        || report.dataset_digest != registered.preregistration.digests.dataset
        || report.experiment_design_digest != registered.preregistration.digests.experiment
        || report.production_pins_digest != registered.preregistration.execution_environment.digest
        || report.task_manifest_digest != roster.task_manifest_digest
        || report.harness_config_digest != registered.preregistration.digests.harness
        || report.config_digest != roster.config_digest
    {
        return Err(StatsError::InvalidTrial(
            "trial identity or event binding mismatch",
        ));
    }
    if report.model_digest != roster.model_digest
        || report.model_settings_digest != roster.model_settings_digest
        || report.provider_capability_digest != roster.provider_capability_digest
    {
        return Err(StatsError::InvalidTrial("model pin mismatch"));
    }
    if !events.exclusion_reason.is_empty()
        && !registered
            .preregistration
            .policies
            .exclusion
            .allowed_reasons
            .contains(&events.exclusion_reason)
    {
        return Err(StatsError::UnregisteredExclusion);
    }
    let cost_microusd = match report.usage.cost_microusd {
        HarnessUsageMeasure::Measured(value) => value,
        HarnessUsageMeasure::Unavailable(_) => {
            return Err(StatsError::InvalidTrial("cost is unavailable"));
        }
    };
    let latency_ms = events
        .finished_monotonic_millis
        .checked_sub(events.started_monotonic_millis)
        .ok_or(StatsError::InvalidTrial("invalid latency"))? as f64;
    if latency_ms > 31_536_000_000.0 {
        return Err(StatsError::InvalidTrial("latency bound exceeded"));
    }
    if cost_microusd > 1_000_000_000_000_000 {
        return Err(StatsError::InvalidTrial("cost bound exceeded"));
    }
    let verification_passed = report.outcome == TrialOutcome::Success
        && report.grade.hidden.verdict == TrialOutcome::Success
        && report.grade.checks.iter().all(|check| check.passed);
    let failure_reason = if report.outcome == TrialOutcome::Success {
        String::new()
    } else {
        report
            .grade
            .diagnostic
            .clone()
            .filter(|value| valid_text(value, 4096))
            .unwrap_or_else(|| match report.outcome {
                TrialOutcome::Failure => "grader_failure".to_owned(),
                TrialOutcome::Error => "harness_error".to_owned(),
                TrialOutcome::Success => unreachable!(),
            })
    };
    Ok(DerivedMeasurement {
        config_digest,
        model_digest,
        outcome: report.outcome,
        intervention: events.intervention,
        cost_usd: cost_microusd as f64 / 1_000_000.0,
        latency_ms,
        verification_passed,
        failure_reason,
        exclusion_reason: events.exclusion_reason,
        event_high_watermark: events.event_high_watermark,
        evidence_source,
    })
}

fn validate_harness_report(bytes: &[u8]) -> Result<HarnessReport, StatsError> {
    if bytes.is_empty()
        || bytes.len() > MAX_HARNESS_REPORT_BYTES
        || has_json_whitespace_outside_strings(bytes)
    {
        return Err(StatsError::InvalidHarnessReport);
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| StatsError::InvalidHarnessReport)?;
    if canonical_bytes(&value)? != bytes {
        return Err(StatsError::InvalidHarnessReport);
    }
    let report: HarnessReport =
        serde_json::from_value(value).map_err(|_| StatsError::InvalidHarnessReport)?;
    let digests = [
        &report.manifest_identity_digest,
        &report.manifest_bytes_digest,
        &report.task_manifest_digest,
        &report.grader_manifest_digest,
        &report.base_tree_digest,
        &report.patch_digest,
        &report.final_tree_digest,
        &report.grader_image_digest,
        &report.hidden_tests_digest,
        &report.acceptance_digest,
        &report.gold_patch_digest,
        &report.harness_config_digest,
        &report.toolchain_digest,
        &report.model_digest,
        &report.model_settings_digest,
        &report.config_digest,
        &report.provider_capability_digest,
        &report.agent_result_digest,
        &report.events_digest,
        &report.logs_digest,
        &report.artifacts_digest,
    ];
    if report.schema_version != 2
        || report.harness_version != "m004-core-v2"
        || !valid_id(&report.trial_id)
        || report.grader_harness_commit.len() != 40
        || !report
            .grader_harness_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || !digests.iter().all(|digest| valid_digest(digest))
        || !valid_boundary(&report.agent, "agent")
        || !valid_boundary(&report.grader, "grader")
        || !valid_usage(&report.usage)
        || !valid_grade(&report)
    {
        return Err(StatsError::InvalidHarnessReport);
    }
    Ok(report)
}

fn valid_boundary(boundary: &HarnessBoundary, phase: &str) -> bool {
    boundary.phase == phase
        && matches!(boundary.route.as_str(), "production" | "conformance_fake")
        && valid_digest(&boundary.image_digest)
        && valid_text(&boundary.runtime_identity, 256)
        && valid_text(&boundary.helper_identity, 256)
        && valid_content_digest(&boundary.permitted_profile_digest)
        && boundary.survivor_processes == 0
        && boundary.quiescent
        && boundary.outcome == kit::executor::trial::BoundaryOutcome::Success
}

fn valid_usage(usage: &HarnessUsage) -> bool {
    fn valid_measure(measure: &HarnessUsageMeasure) -> bool {
        match measure {
            HarnessUsageMeasure::Measured(value) => *value <= i64::MAX as u64,
            HarnessUsageMeasure::Unavailable(reason) => matches!(
                reason,
                HarnessUsageUnavailableReason::ProviderDidNotReport
                    | HarnessUsageUnavailableReason::SchedulerEvidenceMissing
            ),
        }
    }
    [
        &usage.turns,
        &usage.input_tokens,
        &usage.output_tokens,
        &usage.cost_microusd,
        &usage.tool_calls,
        &usage.processes,
    ]
    .into_iter()
    .all(valid_measure)
}

fn valid_grade(report: &HarnessReport) -> bool {
    let grade = &report.grade;
    grade.schema_version == 1
        && grade.outcome == report.outcome
        && grade.base_tree_digest == report.base_tree_digest
        && grade.patch_digest == report.patch_digest
        && grade.final_tree_digest == report.final_tree_digest
        && valid_digest(&grade.hidden.digest)
        && grade.hidden.count <= MAX_TRIALS
        && (report.outcome != TrialOutcome::Success
            || grade.hidden.verdict == TrialOutcome::Success)
        && grade
            .diagnostic
            .as_ref()
            .is_none_or(|value| valid_text(value, 4096))
        && grade.checks.len() <= MAX_TRIALS
        && grade.checks.iter().all(|check| {
            valid_id(&check.id)
                && valid_text(&check.path, 4096)
                && valid_text(&check.expected, 4096)
                && valid_text(&check.actual, 4096)
                && check.passed == (check.expected == check.actual)
        })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatisticalReport {
    pub schema_version: String,
    pub kind: String,
    pub preregistration: Preregistration,
    pub preregistration_digest: String,
    pub registration: RegistrationReceipt,
    pub evidence_source: EvidenceSource,
    pub first_measured_run: Option<FirstMeasuredRun>,
    pub sample_counts: SampleCounts,
    pub trials: Vec<TrialRecord>,
    pub analysis_status: AnalysisStatus,
    pub metrics: Vec<MetricSummary>,
}

#[derive(Clone, Debug)]
pub struct StatisticalReportEnvelope {
    pub report: StatisticalReport,
    pub bytes: Vec<u8>,
    pub digest: String,
    pub receipt: StatisticalReportReceipt,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatisticalReportReceipt {
    pub schema_version: String,
    pub kind: String,
    pub authority_id: String,
    pub registration_sequence: u64,
    pub preregistration_digest: String,
    pub report_digest: String,
    pub evidence_source: EvidenceSource,
    pub ledger_cutoff: u64,
    pub freeze_position: u64,
    pub ledger_position: u64,
    pub previous_entry_digest: String,
    pub ledger_head_digest: String,
    pub recorded_at: String,
    pub authentication: Authentication,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentFrozen {
    pub schema_version: String,
    pub kind: String,
    pub registration_sequence: u64,
    pub preregistration_digest: String,
    pub ledger_cutoff: u64,
    pub recorded_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FirstMeasuredRun {
    pub timestamp: String,
    pub trial_digest: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SampleCounts {
    pub roster_trials: usize,
    pub attempted_trials: usize,
    pub included_trials: usize,
    pub failed_trials: usize,
    pub excluded_trials: usize,
    pub incomplete_trials: usize,
    pub missing_trials: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialRecord {
    pub authority_position: Option<u64>,
    pub schedule_index: usize,
    pub attempt_ordinal: Option<u64>,
    pub trial_id: String,
    pub pair_id: String,
    pub task_id: String,
    pub dataset_member_id: String,
    pub seed: u64,
    pub arm: Arm,
    pub status: TrialStatus,
    pub measured_receipt_digest: Option<String>,
    pub evidence_source: Option<EvidenceSource>,
    pub outcome: Option<TrialOutcome>,
    pub failure_reason: String,
    pub exclusion_reason: String,
}

impl TrialRecord {
    fn measured(receipt: &MeasuredTrialReceipt, status: TrialStatus) -> Self {
        Self {
            authority_position: Some(receipt.authority_position),
            schedule_index: receipt.schedule_index,
            attempt_ordinal: Some(receipt.attempt_ordinal),
            trial_id: receipt.trial_id.clone(),
            pair_id: receipt.pair_id.clone(),
            task_id: receipt.task_id.clone(),
            dataset_member_id: receipt.dataset_member_id.clone(),
            seed: receipt.seed,
            arm: receipt.arm,
            status,
            measured_receipt_digest: Some(sha256(
                &canonical_bytes(receipt).expect("serializable receipt"),
            )),
            evidence_source: Some(receipt.evidence_source),
            outcome: Some(receipt.outcome),
            failure_reason: if receipt.failure_reason.is_empty() {
                "none".to_owned()
            } else {
                receipt.failure_reason.clone()
            },
            exclusion_reason: if receipt.exclusion_reason.is_empty() {
                "none".to_owned()
            } else {
                receipt.exclusion_reason.clone()
            },
        }
    }

    fn placeholder(roster: &RosterEntry, status: TrialStatus) -> Self {
        Self {
            authority_position: None,
            schedule_index: roster.schedule_index,
            attempt_ordinal: None,
            trial_id: roster.trial_id.clone(),
            pair_id: roster.pair_id.clone(),
            task_id: roster.task_id.clone(),
            dataset_member_id: roster.dataset_member_id.clone(),
            seed: roster.seed,
            arm: roster.arm,
            status,
            measured_receipt_digest: None,
            evidence_source: None,
            outcome: None,
            failure_reason: "none".to_owned(),
            exclusion_reason: match status {
                TrialStatus::Incomplete => "incomplete".to_owned(),
                TrialStatus::Missing => "missing".to_owned(),
                _ => "none".to_owned(),
            },
        }
    }
}

fn uniform_evidence_source<'a>(
    receipts: impl Iterator<Item = &'a MeasuredTrialReceipt>,
) -> Result<EvidenceSource, StatsError> {
    let mut sources = receipts.map(|receipt| receipt.evidence_source);
    let source = sources.next().ok_or(StatsError::ExperimentNotTerminal)?;
    if sources.any(|candidate| candidate != source) {
        return Err(StatsError::MixedEvidenceSource);
    }
    Ok(source)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialStatus {
    Included,
    Failed,
    ExcludedPreregistered,
    Incomplete,
    Missing,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStatus {
    Complete,
    IncompleteRoster,
    FailedByPolicy,
    ExcludedByPolicy,
    ConfirmatoryMetricUnavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregation {
    Rate,
    Mean,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum MetricSummary {
    Confirmatory {
        metric: CoreMetric,
        aggregation: Aggregation,
        sample_count: usize,
        baseline_mean: f64,
        candidate_mean: f64,
        paired_delta: f64,
        confidence_interval: ConfidenceInterval,
        noninferiority: NoninferiorityDecision,
    },
    Exploratory {
        metric: CoreMetric,
        aggregation: Aggregation,
        sample_count: usize,
        baseline_mean: f64,
        candidate_mean: f64,
        paired_delta: f64,
    },
}

impl MetricSummary {
    pub const fn metric(&self) -> CoreMetric {
        match self {
            Self::Confirmatory { metric, .. } | Self::Exploratory { metric, .. } => *metric,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalMethod {
    PairedTFiniteSampleV1,
    UnconditionalBonferroniClopperPearsonV1,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidenceInterval {
    pub method: IntervalMethod,
    pub confidence_level: f64,
    pub sample_count: usize,
    pub degrees_of_freedom: Option<usize>,
    pub estimate: f64,
    pub lower: f64,
    pub upper: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoninferiorityDecision {
    pub metric: CoreMetric,
    pub direction: Direction,
    pub margin: f64,
    pub benefit_estimate: f64,
    pub one_sided_lower_bound: f64,
    pub passed: bool,
}

fn analyze(
    registered: &RegisteredPreregistration,
    selected: &BTreeMap<usize, MeasuredTrialReceipt>,
) -> Result<(AnalysisStatus, Vec<MetricSummary>), StatsError> {
    let plan = &registered.preregistration;
    if selected.len() != plan.roster.len() {
        return Ok((AnalysisStatus::IncompleteRoster, Vec::new()));
    }
    if selected
        .values()
        .any(|receipt| !receipt.exclusion_reason.is_empty())
    {
        return Ok((AnalysisStatus::ExcludedByPolicy, Vec::new()));
    }
    if plan.policies.failure == FailurePolicy::FailAnalysis
        && selected
            .values()
            .any(|receipt| receipt.outcome != TrialOutcome::Success)
    {
        return Ok((AnalysisStatus::FailedByPolicy, Vec::new()));
    }
    if (matches!(plan.primary_metric.metric, CoreMetric::MeanCostUsd)
        && selected.values().any(|receipt| receipt.cost_imputed))
        || (matches!(plan.primary_metric.metric, CoreMetric::MeanLatencyMs)
            && selected.values().any(|receipt| receipt.latency_imputed))
    {
        return Ok((AnalysisStatus::ConfirmatoryMetricUnavailable, Vec::new()));
    }
    let mut pairs: BTreeMap<&str, [Option<&MeasuredTrialReceipt>; 2]> = BTreeMap::new();
    for (index, receipt) in selected {
        let roster = &plan.roster[*index];
        let pair = pairs.entry(&roster.pair_id).or_insert([None, None]);
        pair[match roster.arm {
            Arm::Baseline => 0,
            Arm::Candidate => 1,
        }] = Some(receipt);
    }
    if pairs.len() != plan.sample_size.complete_pairs
        || pairs
            .values()
            .any(|pair| pair[0].is_none() || pair[1].is_none())
    {
        return Ok((AnalysisStatus::IncompleteRoster, Vec::new()));
    }
    let pairs = pairs
        .values()
        .map(|pair| (pair[0].expect("checked"), pair[1].expect("checked")))
        .collect::<Vec<_>>();
    let ordered_metrics =
        std::iter::once(&plan.primary_metric).chain(plan.exploratory_metrics.iter());
    let mut summaries = Vec::new();
    for spec in ordered_metrics {
        let baseline = pairs
            .iter()
            .map(|pair| metric_value(spec.metric, pair.0))
            .collect::<Vec<_>>();
        let candidate = pairs
            .iter()
            .map(|pair| metric_value(spec.metric, pair.1))
            .collect::<Vec<_>>();
        let deltas = candidate
            .iter()
            .zip(&baseline)
            .map(|(candidate, baseline)| checked_sub(*candidate, *baseline))
            .collect::<Result<Vec<_>, _>>()?;
        let baseline_mean = checked_mean(&baseline)?;
        let candidate_mean = checked_mean(&candidate)?;
        let paired_delta = checked_mean(&deltas)?;
        if spec.role == MetricRole::Confirmatory {
            let confidence_interval = paired_interval(spec.metric, &baseline, &candidate)?;
            let benefit = match spec.metric.direction() {
                Direction::HigherIsBetter => deltas.clone(),
                Direction::LowerIsBetter => deltas.iter().map(|value| -*value).collect(),
            };
            let benefit_estimate = checked_mean(&benefit)?;
            let one_sided_lower_bound = one_sided_lower(spec.metric, &baseline, &candidate)?;
            let margin = plan.noninferiority.margin;
            summaries.push(MetricSummary::Confirmatory {
                metric: spec.metric,
                aggregation: spec.metric.aggregation(),
                sample_count: pairs.len(),
                baseline_mean,
                candidate_mean,
                paired_delta,
                confidence_interval,
                noninferiority: NoninferiorityDecision {
                    metric: spec.metric,
                    direction: spec.metric.direction(),
                    margin,
                    benefit_estimate,
                    one_sided_lower_bound,
                    passed: one_sided_lower_bound > -margin,
                },
            });
        } else {
            summaries.push(MetricSummary::Exploratory {
                metric: spec.metric,
                aggregation: spec.metric.aggregation(),
                sample_count: pairs.len(),
                baseline_mean,
                candidate_mean,
                paired_delta,
            });
        }
    }
    Ok((AnalysisStatus::Complete, summaries))
}

fn metric_value(metric: CoreMetric, receipt: &MeasuredTrialReceipt) -> f64 {
    match metric {
        CoreMetric::SuccessRate => u8::from(receipt.outcome == TrialOutcome::Success).into(),
        CoreMetric::InterventionRate => u8::from(receipt.intervention).into(),
        CoreMetric::MeanCostUsd => receipt.cost_usd,
        CoreMetric::MeanLatencyMs => receipt.latency_ms,
        CoreMetric::VerificationRate => u8::from(receipt.verification_passed).into(),
    }
}

fn paired_interval(
    metric: CoreMetric,
    baseline: &[f64],
    candidate: &[f64],
) -> Result<ConfidenceInterval, StatsError> {
    if metric.aggregation() == Aggregation::Rate {
        exact_binary_interval(baseline, candidate, 0.05)
    } else {
        let deltas = candidate
            .iter()
            .zip(baseline)
            .map(|(candidate, baseline)| checked_sub(*candidate, *baseline))
            .collect::<Result<Vec<_>, _>>()?;
        paired_t_interval(&deltas, 0.05)
    }
}

fn paired_t_interval(deltas: &[f64], alpha: f64) -> Result<ConfidenceInterval, StatsError> {
    if deltas.len() < MIN_COMPLETE_PAIRS {
        return Err(StatsError::Numeric(
            "paired t interval requires at least three pairs",
        ));
    }
    let (estimate, standard_error) = mean_and_standard_error(deltas)?;
    let degrees_of_freedom = deltas.len() - 1;
    let critical = student_t_quantile(1.0 - alpha / 2.0, degrees_of_freedom)?;
    let radius = checked_mul(critical, standard_error)?;
    Ok(ConfidenceInterval {
        method: IntervalMethod::PairedTFiniteSampleV1,
        confidence_level: 1.0 - alpha,
        sample_count: deltas.len(),
        degrees_of_freedom: Some(degrees_of_freedom),
        estimate,
        lower: checked_sub(estimate, radius)?,
        upper: checked_add(estimate, radius)?,
    })
}

fn exact_binary_interval(
    baseline: &[f64],
    candidate: &[f64],
    alpha: f64,
) -> Result<ConfidenceInterval, StatsError> {
    let (improvements, deteriorations) = discordant_counts(baseline, candidate)?;
    let sample_count = baseline.len();
    let estimate = (improvements as f64 - deteriorations as f64) / sample_count as f64;
    let component_alpha = alpha / 2.0;
    let (lower_10, upper_10) = clopper_pearson(improvements, sample_count, component_alpha)?;
    let (lower_01, upper_01) = clopper_pearson(deteriorations, sample_count, component_alpha)?;
    Ok(ConfidenceInterval {
        method: IntervalMethod::UnconditionalBonferroniClopperPearsonV1,
        confidence_level: 1.0 - alpha,
        sample_count,
        degrees_of_freedom: None,
        estimate,
        lower: (lower_10 - upper_01).clamp(-1.0, 1.0),
        upper: (upper_10 - lower_01).clamp(-1.0, 1.0),
    })
}

fn one_sided_lower(
    metric: CoreMetric,
    baseline: &[f64],
    candidate: &[f64],
) -> Result<f64, StatsError> {
    if metric.aggregation() == Aggregation::Rate {
        let (candidate_wins, baseline_wins) = discordant_counts(baseline, candidate)?;
        let (benefit_wins, benefit_losses) = match metric.direction() {
            Direction::HigherIsBetter => (candidate_wins, baseline_wins),
            Direction::LowerIsBetter => (baseline_wins, candidate_wins),
        };
        let sample_count = baseline.len();
        let tail_alpha = 0.05 / 2.0;
        let lower_wins = clopper_pearson_lower(benefit_wins, sample_count, tail_alpha)?;
        let upper_losses = clopper_pearson_upper(benefit_losses, sample_count, tail_alpha)?;
        Ok((lower_wins - upper_losses).clamp(-1.0, 1.0))
    } else {
        let benefit = candidate
            .iter()
            .zip(baseline)
            .map(|(candidate, baseline)| match metric.direction() {
                Direction::HigherIsBetter => checked_sub(*candidate, *baseline),
                Direction::LowerIsBetter => checked_sub(*baseline, *candidate),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (estimate, standard_error) = mean_and_standard_error(&benefit)?;
        let critical = student_t_quantile(0.95, benefit.len() - 1)?;
        checked_sub(estimate, checked_mul(critical, standard_error)?)
    }
}

fn clopper_pearson(successes: usize, trials: usize, alpha: f64) -> Result<(f64, f64), StatsError> {
    Ok((
        clopper_pearson_lower(successes, trials, alpha / 2.0)?,
        clopper_pearson_upper(successes, trials, alpha / 2.0)?,
    ))
}

fn clopper_pearson_lower(
    successes: usize,
    trials: usize,
    tail_alpha: f64,
) -> Result<f64, StatsError> {
    if successes > trials || trials == 0 || !(0.0..1.0).contains(&tail_alpha) {
        return Err(StatsError::Numeric("invalid binomial bounds"));
    }
    if successes == 0 {
        Ok(0.0)
    } else {
        beta_quantile(
            tail_alpha,
            successes as f64,
            (trials - successes + 1) as f64,
        )
    }
}

fn clopper_pearson_upper(
    successes: usize,
    trials: usize,
    tail_alpha: f64,
) -> Result<f64, StatsError> {
    if successes > trials || trials == 0 || !(0.0..1.0).contains(&tail_alpha) {
        return Err(StatsError::Numeric("invalid binomial bounds"));
    }
    if successes == trials {
        Ok(1.0)
    } else {
        beta_quantile(
            1.0 - tail_alpha,
            (successes + 1) as f64,
            (trials - successes) as f64,
        )
    }
}

fn discordant_counts(baseline: &[f64], candidate: &[f64]) -> Result<(usize, usize), StatsError> {
    if baseline.len() != candidate.len() || baseline.len() < MIN_COMPLETE_PAIRS {
        return Err(StatsError::Numeric("invalid paired binary sample"));
    }
    let mut improvements = 0;
    let mut deteriorations = 0;
    for (baseline, candidate) in baseline.iter().zip(candidate) {
        if !matches!(*baseline, 0.0 | 1.0) || !matches!(*candidate, 0.0 | 1.0) {
            return Err(StatsError::Numeric("non-binary rate value"));
        }
        if candidate > baseline {
            improvements += 1;
        } else if candidate < baseline {
            deteriorations += 1;
        }
    }
    Ok((improvements, deteriorations))
}

fn mean_and_standard_error(values: &[f64]) -> Result<(f64, f64), StatsError> {
    if values.len() < MIN_COMPLETE_PAIRS {
        return Err(StatsError::Numeric("fewer than three paired values"));
    }
    let mean = checked_mean(values)?;
    let squared_deviations = values.iter().try_fold(0.0, |sum, value| {
        let deviation = checked_sub(*value, mean)?;
        checked_add(sum, checked_mul(deviation, deviation)?)
    })?;
    let variance = squared_deviations / (values.len() - 1) as f64;
    let standard_error = (variance / values.len() as f64).sqrt();
    if standard_error.is_finite() {
        Ok((mean, standard_error))
    } else {
        Err(StatsError::Numeric("non-finite standard error"))
    }
}

fn checked_mean(values: &[f64]) -> Result<f64, StatsError> {
    if values.is_empty() {
        return Err(StatsError::Numeric("empty sample"));
    }
    let sum = values.iter().try_fold(0.0, |sum, value| {
        if value.is_finite() {
            checked_add(sum, *value)
        } else {
            Err(StatsError::Numeric("non-finite value"))
        }
    })?;
    let mean = sum / values.len() as f64;
    mean.is_finite()
        .then_some(mean)
        .ok_or(StatsError::Numeric("non-finite mean"))
}

fn student_t_quantile(probability: f64, degrees_of_freedom: usize) -> Result<f64, StatsError> {
    if !(0.5..1.0).contains(&probability) || degrees_of_freedom == 0 {
        return Err(StatsError::Numeric("invalid t quantile"));
    }
    let mut low = 0.0;
    let mut high = 1.0;
    while student_t_cdf(high, degrees_of_freedom)? < probability {
        high *= 2.0;
        if high > 128.0 {
            return Err(StatsError::Numeric("t quantile did not converge"));
        }
    }
    for _ in 0..100 {
        let middle = (low + high) / 2.0;
        if student_t_cdf(middle, degrees_of_freedom)? < probability {
            low = middle
        } else {
            high = middle
        }
    }
    Ok((low + high) / 2.0)
}

fn student_t_cdf(value: f64, degrees_of_freedom: usize) -> Result<f64, StatsError> {
    let degrees = degrees_of_freedom as f64;
    let x = degrees / (degrees + value * value);
    let tail = 0.5 * regularized_beta(x, degrees / 2.0, 0.5)?;
    Ok(if value >= 0.0 { 1.0 - tail } else { tail })
}

fn beta_quantile(probability: f64, a: f64, b: f64) -> Result<f64, StatsError> {
    if !(0.0..=1.0).contains(&probability) || a <= 0.0 || b <= 0.0 {
        return Err(StatsError::Numeric("invalid beta quantile"));
    }
    if probability == 0.0 || probability == 1.0 {
        return Ok(probability);
    }
    let mut low = 0.0;
    let mut high = 1.0;
    for _ in 0..120 {
        let middle = (low + high) / 2.0;
        if regularized_beta(middle, a, b)? < probability {
            low = middle
        } else {
            high = middle
        }
    }
    Ok((low + high) / 2.0)
}

fn regularized_beta(x: f64, a: f64, b: f64) -> Result<f64, StatsError> {
    if !(0.0..=1.0).contains(&x) || a <= 0.0 || b <= 0.0 {
        return Err(StatsError::Numeric("invalid incomplete beta arguments"));
    }
    if x == 0.0 || x == 1.0 {
        return Ok(x);
    }
    let front = (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (-x).ln_1p()).exp();
    let value = if x < (a + 1.0) / (a + b + 2.0) {
        front * beta_continued_fraction(x, a, b)? / a
    } else {
        1.0 - front * beta_continued_fraction(1.0 - x, b, a)? / b
    };
    value
        .is_finite()
        .then_some(value.clamp(0.0, 1.0))
        .ok_or(StatsError::Numeric("incomplete beta overflow"))
}

fn beta_continued_fraction(x: f64, a: f64, b: f64) -> Result<f64, StatsError> {
    const EPSILON: f64 = 3.0e-14;
    const FLOOR: f64 = 1.0e-300;
    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < FLOOR {
        d = FLOOR;
    }
    d = 1.0 / d;
    let mut result = d;
    for iteration in 1..=200 {
        let m = iteration as f64;
        let m2 = 2.0 * m;
        let mut aa = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < FLOOR {
            d = FLOOR;
        }
        c = 1.0 + aa / c;
        if c.abs() < FLOOR {
            c = FLOOR;
        }
        d = 1.0 / d;
        result *= d * c;
        aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < FLOOR {
            d = FLOOR;
        }
        c = 1.0 + aa / c;
        if c.abs() < FLOOR {
            c = FLOOR;
        }
        d = 1.0 / d;
        let delta = d * c;
        result *= delta;
        if (delta - 1.0).abs() < EPSILON {
            return Ok(result);
        }
    }
    Err(StatsError::Numeric("incomplete beta did not converge"))
}

fn ln_gamma(value: f64) -> f64 {
    const COEFFICIENTS: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if value < 0.5 {
        return std::f64::consts::PI.ln()
            - (std::f64::consts::PI * value).sin().ln()
            - ln_gamma(1.0 - value);
    }
    let shifted = value - 1.0;
    let sum = COEFFICIENTS[1..]
        .iter()
        .enumerate()
        .fold(COEFFICIENTS[0], |sum, (index, coefficient)| {
            sum + coefficient / (shifted + index as f64 + 1.0)
        });
    let t = shifted + 7.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (shifted + 0.5) * t.ln() - t + sum.ln()
}

fn sample_counts(trials: &[TrialRecord]) -> SampleCounts {
    let count = |status| trials.iter().filter(|trial| trial.status == status).count();
    SampleCounts {
        roster_trials: trials
            .iter()
            .map(|trial| trial.schedule_index)
            .collect::<BTreeSet<_>>()
            .len(),
        attempted_trials: trials
            .iter()
            .filter(|trial| trial.authority_position.is_some())
            .count(),
        included_trials: count(TrialStatus::Included),
        failed_trials: count(TrialStatus::Failed),
        excluded_trials: count(TrialStatus::ExcludedPreregistered),
        incomplete_trials: count(TrialStatus::Incomplete),
        missing_trials: count(TrialStatus::Missing),
    }
}

fn initialize_credential(
    connection: &Connection,
    credential_path: &Path,
) -> Result<AuthorityCredential, StatsError> {
    let existing: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM registration_authority)",
            [],
            |row| row.get(0),
        )
        .map_err(store)?;
    if existing {
        return Err(StatsError::AlternateGenesis);
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|_| StatsError::EntropyUnavailable)?;
    let mut harness_key = [0u8; 32];
    getrandom::fill(&mut harness_key).map_err(|_| StatsError::EntropyUnavailable)?;
    let mut identity = [0u8; 16];
    getrandom::fill(&mut identity).map_err(|_| StatsError::EntropyUnavailable)?;
    let authority_id = format!("authority-{}", hex(&identity));
    let key_id = format!("registration-{}", &sha256(&key)[7..23]);
    let authority_epoch: String = connection
        .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", [], |row| {
            row.get(0)
        })
        .map_err(store)?;
    let genesis_digest = sha256(&canonical_bytes(&serde_json::json!({
        "domain": "kit-registration-genesis-v1",
        "authority_id": authority_id,
        "authority_epoch": authority_epoch,
        "key_id": key_id,
    }))?);
    let credential = AuthorityCredential {
        authority_id,
        key_id,
        genesis_digest,
        authority_epoch,
        key,
        harness_key,
    };
    let bytes = canonical_bytes(&credential)?;
    write_credential(credential_path, &bytes)?;
    connection
        .execute(
            "INSERT INTO registration_authority (singleton, authority_id, key_id, genesis_digest, authority_epoch) VALUES (1, ?1, ?2, ?3, ?4)",
            params![credential.authority_id, credential.key_id, credential.genesis_digest, credential.authority_epoch],
        )
        .map_err(store)?;
    Ok(credential)
}

fn write_credential(path: &Path, bytes: &[u8]) -> Result<(), StatsError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn configure(connection: &Connection) -> Result<(), StatsError> {
    connection.busy_timeout(BUSY_TIMEOUT).map_err(store)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(store)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(store)?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(store)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StatsError::InvalidAuthority("WAL unavailable"));
    }
    Ok(())
}

fn create_tables(connection: &Connection) -> Result<(), StatsError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS registration_authority (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 authority_id TEXT NOT NULL UNIQUE,
                 key_id TEXT NOT NULL,
                 genesis_digest TEXT NOT NULL,
                 authority_epoch TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS registrations (
                 sequence INTEGER PRIMARY KEY CHECK (sequence > 0),
                 authority_position INTEGER NOT NULL UNIQUE CHECK (authority_position > 0),
                 registered_at TEXT NOT NULL UNIQUE,
                 entry_digest TEXT NOT NULL UNIQUE,
                 bytes BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS admissions (
                 authority_position INTEGER PRIMARY KEY CHECK (authority_position > 0),
                 registration_sequence INTEGER NOT NULL REFERENCES registrations(sequence),
                 schedule_index INTEGER NOT NULL CHECK (schedule_index >= 0),
                 admitted_at TEXT NOT NULL UNIQUE,
                 digest TEXT NOT NULL UNIQUE,
                 bytes BLOB NOT NULL,
                 UNIQUE (registration_sequence, schedule_index)
             );
             CREATE TABLE IF NOT EXISTS admission_consumptions (
                 admission_position INTEGER PRIMARY KEY REFERENCES admissions(authority_position),
                 token_digest TEXT NOT NULL UNIQUE,
                 run_config_digest TEXT NOT NULL UNIQUE,
                 scheduler_run_id TEXT NOT NULL UNIQUE,
                 scheduler_consumption_position INTEGER NOT NULL UNIQUE CHECK (scheduler_consumption_position > 0),
                 scheduler_consumption_digest TEXT NOT NULL UNIQUE,
                 authority_position INTEGER NOT NULL UNIQUE CHECK (authority_position > 0),
                 run_config_bytes BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS executions (
                 authority_position INTEGER PRIMARY KEY CHECK (authority_position > 0),
                 registration_sequence INTEGER NOT NULL REFERENCES registrations(sequence),
                 schedule_index INTEGER NOT NULL CHECK (schedule_index >= 0),
                 attempt_ordinal INTEGER NOT NULL CHECK (attempt_ordinal >= 0),
                 recorded_at TEXT NOT NULL UNIQUE,
                 digest TEXT NOT NULL UNIQUE,
                 receipt_bytes BLOB NOT NULL,
                 harness_bytes BLOB NOT NULL,
                 events_bytes BLOB NOT NULL,
                 UNIQUE (registration_sequence, schedule_index, attempt_ordinal),
                 FOREIGN KEY (registration_sequence, schedule_index)
                      REFERENCES admissions(registration_sequence, schedule_index)
             );
             CREATE TABLE IF NOT EXISTS reports (
                 authority_position INTEGER PRIMARY KEY CHECK (authority_position > 0),
                 registration_sequence INTEGER NOT NULL REFERENCES registrations(sequence),
                 recorded_at TEXT NOT NULL UNIQUE,
                 digest TEXT NOT NULL UNIQUE,
                 report_bytes BLOB NOT NULL,
                 receipt_bytes BLOB NOT NULL,
                 UNIQUE (registration_sequence)
             );
             CREATE TABLE IF NOT EXISTS experiment_freezes (
                 authority_position INTEGER PRIMARY KEY CHECK (authority_position > 0),
                 registration_sequence INTEGER NOT NULL UNIQUE REFERENCES registrations(sequence),
                 ledger_cutoff INTEGER NOT NULL UNIQUE CHECK (ledger_cutoff > 0),
                 recorded_at TEXT NOT NULL UNIQUE,
                 digest TEXT NOT NULL UNIQUE,
                 bytes BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ledger (
                 position INTEGER PRIMARY KEY CHECK (position > 0),
                 previous_digest TEXT NOT NULL,
                 entry_digest TEXT NOT NULL UNIQUE,
                 event_type TEXT NOT NULL CHECK (event_type IN ('registration', 'admission', 'admission_consumed', 'execution', 'experiment_frozen', 'report')),
                 recorded_at TEXT NOT NULL UNIQUE,
                 registration_sequence INTEGER,
                 schedule_index INTEGER,
                 attempt_ordinal INTEGER NOT NULL CHECK (attempt_ordinal >= 0),
                 payload_digest TEXT NOT NULL,
                 payload_bytes BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS anchor_commits (
                 ledger_position INTEGER PRIMARY KEY CHECK (ledger_position >= 0),
                 previous_counter INTEGER NOT NULL CHECK (previous_counter >= 0),
                 previous_head_digest TEXT NOT NULL,
                 ledger_head_digest TEXT NOT NULL UNIQUE,
                 record_digest TEXT NOT NULL UNIQUE,
                 record_bytes BLOB NOT NULL,
                 state TEXT NOT NULL CHECK (state IN ('pending_anchor', 'anchored')),
                 receipt_bytes BLOB,
                 CHECK ((state = 'pending_anchor' AND receipt_bytes IS NULL)
                     OR (state = 'anchored' AND receipt_bytes IS NOT NULL))
             );
             CREATE TRIGGER IF NOT EXISTS ledger_no_update BEFORE UPDATE ON ledger
             BEGIN SELECT RAISE(ABORT, 'append-only ledger'); END;
             CREATE TRIGGER IF NOT EXISTS ledger_no_delete BEFORE DELETE ON ledger
             BEGIN SELECT RAISE(ABORT, 'append-only ledger'); END;
             CREATE TRIGGER IF NOT EXISTS anchor_commits_no_delete BEFORE DELETE ON anchor_commits
             BEGIN SELECT RAISE(ABORT, 'append-only anchor commits'); END;
             CREATE TRIGGER IF NOT EXISTS anchor_commits_immutable BEFORE UPDATE ON anchor_commits
             WHEN NOT (OLD.state = 'pending_anchor' AND NEW.state = 'anchored'
                 AND OLD.ledger_position = NEW.ledger_position
                 AND OLD.previous_counter = NEW.previous_counter
                 AND OLD.previous_head_digest = NEW.previous_head_digest
                 AND OLD.ledger_head_digest = NEW.ledger_head_digest
                 AND OLD.record_digest = NEW.record_digest
                 AND OLD.record_bytes = NEW.record_bytes
                 AND OLD.receipt_bytes IS NULL AND NEW.receipt_bytes IS NOT NULL)
             BEGIN SELECT RAISE(ABORT, 'immutable anchor commit'); END;
             CREATE TRIGGER IF NOT EXISTS registrations_no_update BEFORE UPDATE ON registrations
             BEGIN SELECT RAISE(ABORT, 'append-only registrations'); END;
             CREATE TRIGGER IF NOT EXISTS registrations_no_delete BEFORE DELETE ON registrations
             BEGIN SELECT RAISE(ABORT, 'append-only registrations'); END;
             CREATE TRIGGER IF NOT EXISTS admissions_no_update BEFORE UPDATE ON admissions
             BEGIN SELECT RAISE(ABORT, 'append-only admissions'); END;
             CREATE TRIGGER IF NOT EXISTS admissions_no_delete BEFORE DELETE ON admissions
             BEGIN SELECT RAISE(ABORT, 'append-only admissions'); END;
             CREATE TRIGGER IF NOT EXISTS admission_consumptions_no_update BEFORE UPDATE ON admission_consumptions
             BEGIN SELECT RAISE(ABORT, 'single-use admissions'); END;
             CREATE TRIGGER IF NOT EXISTS admission_consumptions_no_delete BEFORE DELETE ON admission_consumptions
             BEGIN SELECT RAISE(ABORT, 'single-use admissions'); END;
             CREATE TRIGGER IF NOT EXISTS executions_no_update BEFORE UPDATE ON executions
             BEGIN SELECT RAISE(ABORT, 'append-only executions'); END;
             CREATE TRIGGER IF NOT EXISTS executions_no_delete BEFORE DELETE ON executions
             BEGIN SELECT RAISE(ABORT, 'append-only executions'); END;
             CREATE TRIGGER IF NOT EXISTS reports_no_update BEFORE UPDATE ON reports
             BEGIN SELECT RAISE(ABORT, 'append-only reports'); END;
             CREATE TRIGGER IF NOT EXISTS reports_no_delete BEFORE DELETE ON reports
             BEGIN SELECT RAISE(ABORT, 'append-only reports'); END;
             CREATE TRIGGER IF NOT EXISTS experiment_freezes_no_update BEFORE UPDATE ON experiment_freezes
             BEGIN SELECT RAISE(ABORT, 'append-only experiment freezes'); END;
             CREATE TRIGGER IF NOT EXISTS experiment_freezes_no_delete BEFORE DELETE ON experiment_freezes
             BEGIN SELECT RAISE(ABORT, 'append-only experiment freezes'); END;",
        )
        .map_err(store)
}

fn authority_time(
    transaction: &Transaction<'_>,
    lower_bound: Option<&str>,
) -> Result<String, StatsError> {
    let now: String = transaction
        .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", [], |row| {
            row.get(0)
        })
        .map_err(store)?;
    match lower_bound {
        Some(lower) if now.as_str() <= lower => transaction
            .query_row(
                "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1, '+1 second')",
                [lower],
                |row| row.get(0),
            )
            .map_err(store),
        _ => Ok(now),
    }
}

#[derive(Serialize)]
struct LedgerEntryMessage<'a> {
    domain: &'static str,
    position: u64,
    previous_digest: &'a str,
    event_type: &'a str,
    recorded_at: &'a str,
    registration_sequence: Option<u64>,
    schedule_index: Option<usize>,
    attempt_ordinal: u64,
    payload_digest: &'a str,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingAnchorRecord {
    domain: String,
    authority_id: String,
    previous_counter: u64,
    previous_head_digest: String,
    ledger_position: u64,
    ledger_head_digest: String,
}

fn stage_anchor(
    transaction: &Transaction<'_>,
    previous: &LedgerAnchorReceipt,
    ledger_position: u64,
    ledger_head_digest: &str,
) -> Result<(), StatsError> {
    let pending: usize = transaction
        .query_row(
            "SELECT COUNT(*) FROM anchor_commits WHERE state = 'pending_anchor'",
            [],
            |row| row.get(0),
        )
        .map_err(store)?;
    if pending != 0
        || previous.counter != 0 && ledger_position <= previous.ledger_position
        || !valid_digest(ledger_head_digest)
    {
        return Err(StatsError::AnchorFork);
    }
    let record = PendingAnchorRecord {
        domain: "kit-pending-ledger-anchor-v1".to_owned(),
        authority_id: if previous.counter == 0 {
            transaction
                .query_row(
                    "SELECT authority_id FROM registration_authority WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )
                .map_err(store)?
        } else {
            previous.authority_id.clone()
        },
        previous_counter: previous.counter,
        previous_head_digest: previous.ledger_head_digest.clone(),
        ledger_position,
        ledger_head_digest: ledger_head_digest.to_owned(),
    };
    let bytes = canonical_bytes(&record)?;
    let record_digest = sha256(&bytes);
    transaction
        .execute(
            "INSERT INTO anchor_commits
                 (ledger_position, previous_counter, previous_head_digest, ledger_head_digest,
                  record_digest, record_bytes, state, receipt_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending_anchor', NULL)",
            params![
                ledger_position,
                previous.counter,
                previous.ledger_head_digest,
                ledger_head_digest,
                record_digest,
                bytes,
            ],
        )
        .map_err(|_| StatsError::AnchorFork)?;
    Ok(())
}

fn ledger_head_tx(
    transaction: &Transaction<'_>,
    genesis: &str,
) -> Result<(u64, String), StatsError> {
    transaction
        .query_row(
            "SELECT position, entry_digest FROM ledger ORDER BY position DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(store)
        .map(|head| head.unwrap_or((0, genesis.to_owned())))
}

struct LedgerAppend<'a> {
    event_type: &'a str,
    recorded_at: &'a str,
    registration_sequence: Option<u64>,
    schedule_index: Option<usize>,
    attempt_ordinal: u64,
    payload_bytes: &'a [u8],
}

fn append_ledger(
    transaction: &Transaction<'_>,
    genesis: &str,
    append: LedgerAppend<'_>,
) -> Result<(u64, String, String), StatsError> {
    let (position, previous_digest) = ledger_head_tx(transaction, genesis)?;
    let position = position
        .checked_add(1)
        .ok_or(StatsError::SequenceExhausted)?;
    let payload_digest = sha256(append.payload_bytes);
    let entry_digest = sha256(&canonical_bytes(&LedgerEntryMessage {
        domain: "kit-contiguous-evaluation-ledger-v1",
        position,
        previous_digest: &previous_digest,
        event_type: append.event_type,
        recorded_at: append.recorded_at,
        registration_sequence: append.registration_sequence,
        schedule_index: append.schedule_index,
        attempt_ordinal: append.attempt_ordinal,
        payload_digest: &payload_digest,
    })?);
    transaction
        .execute(
            "INSERT INTO ledger (position, previous_digest, entry_digest, event_type, recorded_at, registration_sequence, schedule_index, attempt_ordinal, payload_digest, payload_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![position, previous_digest, entry_digest, append.event_type, append.recorded_at, append.registration_sequence, append.schedule_index, append.attempt_ordinal, payload_digest, append.payload_bytes],
        )
        .map_err(store)?;
    Ok((position, previous_digest, entry_digest))
}

fn verify_registration(
    credential: &AuthorityCredential,
    registered: &RegisteredPreregistration,
) -> Result<(), StatsError> {
    let receipt = &registered.registration;
    if registered.schema_version != "1.0"
        || registered.kind != "registered_preregistration"
        || registered.preregistration_digest != registered.preregistration.digest()?
        || receipt.authority_id != credential.authority_id
        || receipt.authority_epoch != credential.authority_epoch
        || receipt.genesis_digest != credential.genesis_digest
        || receipt.sequence == 0
        || receipt.authority_position == 0
        || receipt.attempt_ordinal != 0
        || receipt.registered_at <= receipt.authority_epoch
    {
        return Err(StatsError::InvalidRegistration);
    }
    let message = RegistrationMessage {
        domain: "kit-registration-v1",
        authority_id: &receipt.authority_id,
        genesis_digest: &receipt.genesis_digest,
        sequence: receipt.sequence,
        authority_position: receipt.authority_position,
        attempt_ordinal: receipt.attempt_ordinal,
        registered_at: &receipt.registered_at,
        previous_entry_digest: &receipt.previous_entry_digest,
        preregistration_digest: &registered.preregistration_digest,
    };
    verify_authentication(
        credential,
        &canonical_bytes(&message)?,
        &receipt.authentication,
    )
    .map_err(|_| StatsError::InvalidRegistration)
}

fn authentication(
    credential: &AuthorityCredential,
    message: &[u8],
) -> Result<Authentication, StatsError> {
    let mut mac =
        HmacSha256::new_from_slice(&credential.key).map_err(|_| StatsError::InvalidRegistration)?;
    mac.update(message);
    Ok(Authentication {
        algorithm: "hmac-sha256".to_owned(),
        key_id: credential.key_id.clone(),
        tag: format!("hmac-sha256:{:x}", mac.finalize().into_bytes()),
    })
}

fn verify_authentication(
    credential: &AuthorityCredential,
    message: &[u8],
    authentication: &Authentication,
) -> Result<(), StatsError> {
    if authentication.algorithm != "hmac-sha256" || authentication.key_id != credential.key_id {
        return Err(StatsError::InvalidRegistration);
    }
    let tag = parse_tag(&authentication.tag)?;
    let mut mac =
        HmacSha256::new_from_slice(&credential.key).map_err(|_| StatsError::InvalidRegistration)?;
    mac.update(message);
    mac.verify_slice(&tag)
        .map_err(|_| StatsError::InvalidRegistration)
}

fn harness_authentication(
    credential: &AuthorityCredential,
    message: &[u8],
) -> Result<Authentication, StatsError> {
    let mut mac = HmacSha256::new_from_slice(&credential.harness_key)
        .map_err(|_| StatsError::InvalidHarnessReport)?;
    mac.update(message);
    Ok(Authentication {
        algorithm: "hmac-sha256".to_owned(),
        key_id: format!("harness-{}", &sha256(&credential.harness_key)[7..23]),
        tag: format!("hmac-sha256:{:x}", mac.finalize().into_bytes()),
    })
}

fn verify_harness_authentication(
    credential: &AuthorityCredential,
    message: &[u8],
    authentication: &Authentication,
) -> Result<(), StatsError> {
    if authentication.algorithm != "hmac-sha256"
        || authentication.key_id != format!("harness-{}", &sha256(&credential.harness_key)[7..23])
    {
        return Err(StatsError::InvalidHarnessReport);
    }
    let tag = parse_tag(&authentication.tag)?;
    let mut mac = HmacSha256::new_from_slice(&credential.harness_key)
        .map_err(|_| StatsError::InvalidHarnessReport)?;
    mac.update(message);
    mac.verify_slice(&tag)
        .map_err(|_| StatsError::InvalidHarnessReport)
}

fn trial_run_config_digest(config: &TrialRunConfig) -> Result<String, StatsError> {
    let mut unsigned = config.clone();
    unsigned.immutable_digest.clear();
    Ok(sha256(&canonical_bytes(&unsigned)?))
}

fn validate_schema(bytes: &[u8], definition: &str) -> Result<(), StatsError> {
    let components: serde_json::Value = serde_json::from_slice(COMPONENT_SCHEMA)
        .map_err(|error| StatsError::Schema(error.to_string()))?;
    let definitions = components
        .get("$defs")
        .cloned()
        .ok_or_else(|| StatsError::Schema("components has no $defs".to_owned()))?;
    let wrapper = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": definitions,
        "$ref": format!("#/$defs/{definition}"),
    });
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&wrapper)
        .map_err(|error| StatsError::Schema(error.to_string()))?;
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| StatsError::Schema(error.to_string()))?;
    validator
        .validate(&value)
        .map_err(|error| StatsError::Schema(error.to_string()))
}

fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>, StatsError> {
    serde_json::to_vec(value).map_err(|error| StatsError::Serialization(error.to_string()))
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && value[7..].bytes().any(|byte| byte != b'0')
}

fn valid_identity_pin(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn valid_content_digest(value: &str) -> bool {
    valid_digest(value)
        || value.len() == 71
            && value.starts_with("blake3:")
            && value[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_anchor_receipt(receipt: &LedgerAnchorReceipt) -> bool {
    !receipt.source.is_empty()
        && receipt.source.len() <= 256
        && valid_id(&receipt.authority_id)
        && receipt.counter > 0
        && valid_digest(&receipt.ledger_head_digest)
        && !receipt.signature.is_empty()
        && receipt.signature.len() <= 4096
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        && value.as_bytes()[0].is_ascii_alphanumeric()
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.contains('\0')
}

fn placeholder(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("replace") || value.contains("placeholder")
}

fn strictly_ordered_by<T, K: Ord>(values: &[T], key: impl Fn(&T) -> K) -> bool {
    values.windows(2).all(|pair| key(&pair[0]) < key(&pair[1]))
}

fn parse_tag(value: &str) -> Result<[u8; 32], StatsError> {
    let hex = value
        .strip_prefix("hmac-sha256:")
        .filter(|hex| hex.len() == 64)
        .ok_or(StatsError::InvalidRegistration)?;
    let mut bytes = [0u8; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    Ok(bytes)
}

fn hex_digit(byte: u8) -> Result<u8, StatsError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(StatsError::InvalidRegistration),
    }
}

fn has_json_whitespace_outside_strings(bytes: &[u8]) -> bool {
    let mut in_string = false;
    let mut escaped = false;
    for byte in bytes {
        if in_string {
            if escaped {
                escaped = false
            } else if *byte == b'\\' {
                escaped = true
            } else if *byte == b'"' {
                in_string = false
            }
        } else if *byte == b'"' {
            in_string = true;
        } else if byte.is_ascii_whitespace() {
            return true;
        }
    }
    false
}

fn checked_add(left: f64, right: f64) -> Result<f64, StatsError> {
    let value = left + right;
    value
        .is_finite()
        .then_some(value)
        .ok_or(StatsError::Numeric("floating-point overflow"))
}

fn checked_sub(left: f64, right: f64) -> Result<f64, StatsError> {
    checked_add(left, -right)
}

fn checked_mul(left: f64, right: f64) -> Result<f64, StatsError> {
    let value = left * right;
    value
        .is_finite()
        .then_some(value)
        .ok_or(StatsError::Numeric("floating-point overflow"))
}

fn invalid_plan<T>(detail: &str) -> Result<T, StatsError> {
    Err(StatsError::InvalidPreregistration(detail.to_owned()))
}

fn store(error: rusqlite::Error) -> StatsError {
    StatsError::Store(error.to_string())
}

fn io_error(error: std::io::Error) -> StatsError {
    StatsError::Store(error.to_string())
}

#[derive(Debug, PartialEq)]
pub enum StatsError {
    InvalidPreregistration(String),
    InvalidRegistration,
    InvalidAuthority(&'static str),
    AnchorUnavailable,
    Anchor(&'static str),
    AlternateGenesis,
    LedgerRollback,
    AnchorFork,
    LedgerTamper,
    ExperimentNotTerminal,
    ExperimentFrozen,
    ReportAlreadyBuilt,
    InvalidAdmission,
    AdmissionConsumed,
    AttemptLimit,
    InvalidHarnessReport,
    InvalidTrial(&'static str),
    UnregisteredExclusion,
    TrialTamper,
    MixedEvidenceSource,
    InvalidReportReceipt,
    RosterComplete,
    SequenceExhausted,
    EntropyUnavailable,
    BoundExceeded(&'static str),
    Numeric(&'static str),
    Schema(String),
    Serialization(String),
    Store(String),
}

impl std::fmt::Display for StatsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPreregistration(detail) => {
                write!(formatter, "invalid preregistration: {detail}")
            }
            Self::InvalidRegistration => {
                formatter.write_str("invalid immutable registration receipt")
            }
            Self::InvalidAuthority(detail) => {
                write!(formatter, "invalid registration authority: {detail}")
            }
            Self::AnchorUnavailable => formatter.write_str(
                "production evaluation authority requires a configured transparency or secure ledger anchor",
            ),
            Self::Anchor(detail) => write!(formatter, "external ledger anchor failed: {detail}"),
            Self::AlternateGenesis => formatter.write_str("alternate or missing authority genesis"),
            Self::LedgerRollback => formatter.write_str("ledger or credential rollback detected by external anchor"),
            Self::AnchorFork => formatter.write_str("ledger anchor fork or multiple pending descendants detected"),
            Self::LedgerTamper => formatter.write_str("contiguous evaluation ledger is invalid"),
            Self::ExperimentNotTerminal => formatter.write_str("fixed roster is not terminal or still has pending attempts"),
            Self::ExperimentFrozen => formatter.write_str("experiment is frozen at its ledger cutoff"),
            Self::ReportAlreadyBuilt => formatter.write_str("final statistical report already exists"),
            Self::InvalidAdmission => formatter.write_str("invalid authenticated trial admission"),
            Self::AdmissionConsumed => formatter.write_str("trial admission token was already consumed"),
            Self::AttemptLimit => formatter.write_str("fixed one-attempt roster limit reached"),
            Self::InvalidHarnessReport => {
                formatter.write_str("invalid canonical M004 harness report")
            }
            Self::InvalidTrial(detail) => write!(formatter, "invalid trial: {detail}"),
            Self::UnregisteredExclusion => formatter.write_str("exclusion was not preregistered"),
            Self::TrialTamper => {
                formatter.write_str("authenticated measured trial was tampered with")
            }
            Self::MixedEvidenceSource => {
                formatter.write_str("statistical report cannot mix evidence sources")
            }
            Self::InvalidReportReceipt => formatter.write_str("invalid authority-signed statistical report receipt"),
            Self::RosterComplete => formatter.write_str("fixed trial roster is complete"),
            Self::SequenceExhausted => formatter.write_str("authority sequence exhausted"),
            Self::EntropyUnavailable => formatter.write_str("authority entropy unavailable"),
            Self::BoundExceeded(name) => write!(formatter, "{name} bound exceeded"),
            Self::Numeric(detail) => write!(formatter, "invalid numeric calculation: {detail}"),
            Self::Schema(detail) => write!(formatter, "schema validation failed: {detail}"),
            Self::Serialization(detail) => write!(formatter, "serialization failed: {detail}"),
            Self::Store(detail) => write!(formatter, "authority store failed: {detail}"),
        }
    }
}

impl std::error::Error for StatsError {}

#[cfg(test)]
mod numerical_tests {
    use super::*;

    #[test]
    fn finite_sample_t_known_critical_and_interval() {
        let critical = student_t_quantile(0.975, 2).unwrap();
        assert!((critical - 4.302_652_729_696_142).abs() < 1e-10);
        let interval = paired_t_interval(&[-1.0, 0.0, 2.0], 0.05).unwrap();
        assert_eq!(interval.degrees_of_freedom, Some(2));
        assert!((interval.estimate - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn exact_binary_known_vector_is_bounded() {
        let baseline = [0.0, 0.0, 1.0, 1.0, 1.0];
        let candidate = [1.0, 1.0, 1.0, 0.0, 1.0];
        let interval = exact_binary_interval(&baseline, &candidate, 0.05).unwrap();
        assert_eq!(
            interval.method,
            IntervalMethod::UnconditionalBonferroniClopperPearsonV1
        );
        assert!((interval.estimate - 0.2).abs() < 1e-12);
        assert!(interval.lower <= interval.estimate);
        assert!(interval.upper >= interval.estimate);
        assert!((-1.0..=1.0).contains(&interval.lower));
        assert!((-1.0..=1.0).contains(&interval.upper));
    }

    #[test]
    fn paired_t_simulation_has_declared_coverage() {
        let critical = student_t_quantile(0.975, 7).unwrap();
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut covered = 0usize;
        let simulations = 5_000usize;
        for _ in 0..simulations {
            let sample = (0..8).map(|_| normal(&mut state)).collect::<Vec<_>>();
            let (mean, standard_error) = mean_and_standard_error(&sample).unwrap();
            if mean.abs() <= critical * standard_error {
                covered += 1;
            }
        }
        let coverage = covered as f64 / simulations as f64;
        assert!((0.93..=0.97).contains(&coverage), "coverage={coverage}");
    }

    #[test]
    fn unconditional_binary_grid_has_coverage_and_type_one_control() {
        for n in 3..=10 {
            for p10_step in 0..=10 {
                for p01_step in 0..=10 - p10_step {
                    let p10 = p10_step as f64 / 10.0;
                    let p01 = p01_step as f64 / 10.0;
                    let difference = p10 - p01;
                    let mut covered = 0.0;
                    let mut rejected = 0.0;
                    for k10 in 0..=n {
                        for k01 in 0..=n - k10 {
                            let probability = multinomial_probability(n, k10, k01, p10, p01);
                            let mut baseline = vec![0.0; n];
                            let mut candidate = vec![0.0; n];
                            for value in candidate.iter_mut().take(k10) {
                                *value = 1.0;
                            }
                            for value in baseline.iter_mut().skip(k10).take(k01) {
                                *value = 1.0;
                            }
                            let interval =
                                exact_binary_interval(&baseline, &candidate, 0.05).unwrap();
                            if interval.lower - 1e-12 <= difference
                                && difference <= interval.upper + 1e-12
                            {
                                covered += probability;
                            }
                            let lower = clopper_pearson_lower(k10, n, 0.025).unwrap()
                                - clopper_pearson_upper(k01, n, 0.025).unwrap();
                            if lower > difference + 1e-12 {
                                rejected += probability;
                            }
                        }
                    }
                    assert!(
                        covered >= 0.95 - 1e-10,
                        "n={n} p10={p10} p01={p01} coverage={covered}"
                    );
                    assert!(
                        rejected <= 0.05 + 1e-10,
                        "n={n} p10={p10} p01={p01} rejection={rejected}"
                    );
                }
            }
        }
    }

    fn multinomial_probability(n: usize, k10: usize, k01: usize, p10: f64, p01: f64) -> f64 {
        let ties = n - k10 - k01;
        let coefficient = binomial_coefficient(n, k10) * binomial_coefficient(n - k10, k01);
        coefficient as f64
            * p10.powi(k10 as i32)
            * p01.powi(k01 as i32)
            * (1.0 - p10 - p01).powi(ties as i32)
    }

    fn binomial_coefficient(n: usize, k: usize) -> u64 {
        let k = k.min(n - k);
        (1..=k).fold(1u64, |value, index| {
            value * (n - k + index) as u64 / index as u64
        })
    }

    fn normal(state: &mut u64) -> f64 {
        let uniform = |state: &mut u64| {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((*state >> 11) as f64 + 1.0) / ((1u64 << 53) as f64 + 1.0)
        };
        let first = uniform(state);
        let second = uniform(state);
        (-2.0 * first.ln()).sqrt() * (2.0 * std::f64::consts::PI * second).cos()
    }
}
