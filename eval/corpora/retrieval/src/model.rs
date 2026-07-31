use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusManifest {
    pub schema_version: String,
    pub kind: String,
    pub selection_algorithm: String,
    pub candidate_repository_count: usize,
    pub classes: Vec<ClassDefinition>,
    pub units: Vec<CorpusUnit>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClassDefinition {
    pub name: RepositoryClass,
    pub minimum_rust_sloc: u64,
    pub maximum_rust_sloc: u64,
    pub units: usize,
    pub analysis_unit: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryClass {
    Small,
    Medium,
    Large,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusUnit {
    pub schedule_index: usize,
    pub unit_id: String,
    pub repository_class: RepositoryClass,
    pub package: PackagePin,
    pub rust_sloc: u64,
    pub source_file_count: usize,
    pub source_bytes: u64,
    pub source_digest: String,
    pub rust_source_digest: String,
    pub checksum_manifest_digest: String,
    pub task: TaskPin,
    pub oracle: OraclePin,
    pub arm_order: Vec<Arm>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackagePin {
    pub name: String,
    pub version: String,
    pub normalized_repository_url: String,
    pub vcs_commit: String,
    pub path_in_vcs: String,
    pub license: String,
    pub cargo_lock_checksum: String,
    pub registry_source: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPin {
    pub task_id: String,
    pub query: String,
    pub query_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OraclePin {
    pub selection_hash: String,
    pub target: SymbolPin,
    pub decoys: Vec<SymbolPin>,
    pub reference_edit: ReferenceEdit,
    pub expected_post_edit_tree_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolPin {
    pub path: String,
    pub symbol: String,
    pub symbol_kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub doc_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceEdit {
    pub operation: String,
    pub utf8_text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Arm {
    L,
    C,
    F,
    #[serde(rename = "F-S")]
    FS,
    #[serde(rename = "F-P")]
    FP,
    #[serde(rename = "F-G")]
    FG,
    #[serde(rename = "F-H")]
    FH,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalSource {
    Lexical,
    TreeSitter,
    Structural,
    Lsp,
    PersonalizedMap,
    StructureGraph,
    History,
    FilesystemMetadata,
    CargoMetadataWithoutSourceParse,
    GitPathHistory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSemantics {
    LexicalContext,
    ExactItem,
    OtherContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizationContract {
    pub version: String,
    pub raw_score_rule: String,
    pub deduplication: String,
    pub rank_order: String,
    pub tie_break: Vec<String>,
    pub top_k: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArmConfig {
    pub arm: Arm,
    pub enabled_sources: Vec<RetrievalSource>,
    pub syntax_initialization_permitted: bool,
    pub token_budget: usize,
    pub normalization: NormalizationContract,
}

impl ArmConfig {
    pub fn frozen(arm: Arm) -> Self {
        use RetrievalSource::*;
        let enabled_sources = match arm {
            Arm::L => vec![Lexical],
            Arm::C => vec![Lexical, TreeSitter, Structural, Lsp],
            Arm::F => vec![
                Lexical,
                TreeSitter,
                Structural,
                Lsp,
                PersonalizedMap,
                StructureGraph,
                History,
            ],
            Arm::FS => vec![
                Lexical,
                FilesystemMetadata,
                CargoMetadataWithoutSourceParse,
                GitPathHistory,
            ],
            Arm::FP => vec![
                Lexical,
                TreeSitter,
                Structural,
                Lsp,
                StructureGraph,
                History,
            ],
            Arm::FG => vec![
                Lexical,
                TreeSitter,
                Structural,
                Lsp,
                PersonalizedMap,
                History,
            ],
            Arm::FH => vec![
                Lexical,
                TreeSitter,
                Structural,
                Lsp,
                PersonalizedMap,
                StructureGraph,
            ],
        };
        Self {
            arm,
            syntax_initialization_permitted: !matches!(arm, Arm::L | Arm::FS),
            enabled_sources,
            token_budget: 2_048,
            normalization: NormalizationContract {
                version: "m005-w07-normalization-v1".into(),
                raw_score_rule: "clamp source-native score to [-1000000,1000000]; add exact integer token-overlap micros; retain no inferred ranges".into(),
                deduplication: "exact (path,start_byte,end_byte,start_line,end_line); retain highest score then source then response ordinal".into(),
                rank_order: "score_micros descending; retain first 10 and truncate projected snippets in rank order to the hard 2048-token estimate".into(),
                tie_break: vec![
                    "path ascending UTF-8 bytes".into(),
                    "start_byte ascending".into(),
                    "end_byte ascending".into(),
                    "source enum ascending".into(),
                    "response_ordinal ascending".into(),
                    "provenance_digest ascending".into(),
                ],
                top_k: 10,
            },
        }
    }
}

#[cfg(test)]
mod strict_model_tests {
    #[test]
    fn worker_contract_rejects_unknown_fields() {
        let input = br#"{"task_id":"t","query":"q","query_digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","oracle":"hidden"}"#;
        assert!(serde_json::from_slice::<super::WorkerQuery>(input).is_err());
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerQuery {
    pub task_id: String,
    pub query: String,
    pub query_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerArmRequest {
    pub unit_id: String,
    pub source_digest: String,
    pub admission_digest: String,
    pub executor_evidence: ExecutorEvidence,
    pub cache_id: String,
    pub worker_executable_digest: String,
    pub config: ArmConfig,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    Available,
    TerminalUnavailable,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateRange {
    pub path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawCandidate {
    pub range: CandidateRange,
    pub symbol: Option<String>,
    pub snippet: String,
    pub snippet_truncated: bool,
    pub semantics: CandidateSemantics,
    pub source: RetrievalSource,
    pub source_revision_digest: String,
    pub provenance_digest: String,
    pub raw_score_micros: i64,
    pub token_overlap_micros: i64,
    pub response_ordinal: usize,
}

impl RawCandidate {
    pub fn score_micros(&self) -> i64 {
        self.raw_score_micros
            .clamp(-1_000_000, 1_000_000)
            .saturating_add(self.token_overlap_micros)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceObservation {
    pub source: RetrievalSource,
    pub api: String,
    pub status: SourceStatus,
    pub started_at: String,
    pub ended_at: String,
    pub elapsed_ns: u64,
    pub complete_candidate_count: usize,
    pub candidates: Vec<RawCandidate>,
    pub truncated: bool,
    pub source_revision_digest: String,
    pub error_code: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutorEvidence {
    M004ProductionTrusted,
    LocalSandboxNotTrusted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialTerminal {
    Complete,
    Error,
    Timeout,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawTrial {
    pub schema_version: String,
    pub kind: String,
    pub unit_id: String,
    pub task_id: String,
    pub arm: Arm,
    pub executor_evidence: ExecutorEvidence,
    pub admission_digest: String,
    pub source_digest: String,
    pub task_query_digest: String,
    pub arm_config_digest: String,
    pub worker_executable_digest: String,
    pub process_id: u32,
    pub cache_id: String,
    pub measured_started_at: String,
    pub measured_ended_at: String,
    pub elapsed_ns: u64,
    pub index_latency_ms: u64,
    pub query_latency_ms: u64,
    pub token_count: usize,
    pub syntax_initializations: usize,
    pub terminal: TrialTerminal,
    pub observations: Vec<SourceObservation>,
    pub worker_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedCandidate {
    pub rank: usize,
    pub range: CandidateRange,
    pub symbol: Option<String>,
    pub snippet: String,
    pub snippet_truncated: bool,
    pub semantics: CandidateSemantics,
    pub source: RetrievalSource,
    pub source_revision_digest: String,
    pub provenance_digest: String,
    pub score_micros: i64,
    pub response_ordinal: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialGrade {
    pub schema_version: String,
    pub kind: String,
    pub unit_id: String,
    pub arm: Arm,
    pub raw_trial_digest: String,
    pub projected_top_k: Vec<ProjectedCandidate>,
    pub localization_success: bool,
    pub wrong_decoy_success: bool,
    pub downstream_mechanical_success: bool,
    pub freshness_success: bool,
    pub latency_success: bool,
    pub provenance_success: bool,
    pub token_budget_success: bool,
    pub terminal_success: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationRecord {
    pub schema_version: String,
    pub kind: String,
    pub experiment_id: String,
    pub preregistration_digest: String,
    pub corpus_manifest_digest: String,
    pub immutable_inputs_digest: String,
    pub git_commit_sha: String,
    pub git_commit_time: String,
    pub registered_at: String,
    pub route: ExecutorEvidence,
    pub runtime_manifest_digest: String,
    pub materialization_receipt_digest: String,
    pub materialization_receipt_count: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitMaterializationReceipt {
    pub unit_id: String,
    pub git_path: String,
    pub git_digest: String,
    pub git_version: String,
    pub fetch_depth: usize,
    pub commands: Vec<Vec<String>>,
    pub result: String,
    pub repository_url: String,
    pub vcs_commit: String,
    pub path_in_vcs: String,
    pub head: String,
    pub rust_source_digest: String,
    pub package_file_set_digest: String,
    pub package_files: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionRecord {
    pub schema_version: String,
    pub kind: String,
    pub unit_id: String,
    pub arm: Arm,
    pub sequence_index: usize,
    pub source_digest: String,
    pub task_query_digest: String,
    pub arm_config_digest: String,
    pub admitted_at: String,
    pub registration_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialBinding {
    pub schema_version: String,
    pub kind: String,
    pub unit_id: String,
    pub arm: Arm,
    pub raw_trial_digest: String,
    pub grade_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredRuntimeManifest {
    pub schema_version: String,
    pub kind: String,
    pub route: ExecutorEvidence,
    pub os: String,
    pub uname_executable_digest: String,
    pub architecture: String,
    pub os_version: String,
    pub sw_vers_executable_digest: String,
    pub rustc: String,
    pub rustc_executable_digest: String,
    pub cargo: String,
    pub cargo_executable_digest: String,
    pub git_path: String,
    pub git: String,
    pub git_executable_digest: String,
    pub sandbox_exec: String,
    pub executable_digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerEvent {
    Registration,
    Materialization,
    Admission,
    Trial,
    Report,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedLedgerEntry {
    pub sequence: u64,
    pub event: LedgerEvent,
    pub recorded_at: String,
    pub previous_entry_digest: String,
    pub payload_path: String,
    pub payload_digest: String,
    pub key_id: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerTableRow {
    pub sequence: u64,
    pub event: LedgerEvent,
    pub payload_path: String,
    pub payload_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedLedger {
    pub schema_version: String,
    pub kind: String,
    pub algorithm: String,
    pub public_key_digest: String,
    pub entries: Vec<SignedLedgerEntry>,
    pub table_rows: Vec<LedgerTableRow>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClassAnalysis {
    pub repository_class: RepositoryClass,
    pub pairs: usize,
    pub l_successes: usize,
    pub c_successes: usize,
    pub estimate: Option<f64>,
    pub confidence_level: Option<f64>,
    pub lower: Option<f64>,
    pub upper: Option<f64>,
    pub passed: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredReport {
    pub schema_version: String,
    pub kind: String,
    pub experiment_id: String,
    pub status: String,
    pub route: ExecutorEvidence,
    pub statistical_verdict: String,
    pub gate_claim: String,
    pub preregistration_digest: String,
    pub corpus_manifest_digest: String,
    pub registration_digest: String,
    pub runtime_manifest_digest: String,
    pub materialization_receipt_digest: String,
    pub materialization_receipt_count: usize,
    pub measured_started_at: String,
    pub measured_ended_at: String,
    pub reported_at: String,
    pub units: usize,
    pub measured_trials: usize,
    pub terminal_trials: usize,
    pub class_analyses: Vec<ClassAnalysis>,
    pub guardrails_passed: bool,
    pub external_blockers: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockedReport {
    pub schema_version: String,
    pub kind: String,
    pub experiment_id: String,
    pub status: String,
    pub gate_claim: String,
    pub measured_trials: usize,
    pub blocker: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Preregistration {
    pub schema_version: String,
    pub kind: String,
    pub experiment_id: String,
    pub state: String,
    pub language: String,
    pub corpus_manifest_digest: String,
    pub immutable_inputs: ImmutableInputs,
    pub runtime_environment: RuntimeEnvironment,
    pub public_receipt_key: PublicReceiptKey,
    pub oracle_selection: OracleSelection,
    pub trial_protocol: TrialProtocol,
    pub analysis: AnalysisPlan,
    pub external_blockers: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableInputs {
    pub build_inputs: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeEnvironment {
    pub manifest_digest: String,
    pub os: String,
    pub uname_executable_digest: String,
    pub architecture: String,
    pub os_version: String,
    pub sw_vers_executable_digest: String,
    pub rustc: String,
    pub rustc_executable_digest: String,
    pub cargo: String,
    pub cargo_executable_digest: String,
    pub git_path: String,
    pub git: String,
    pub git_executable_digest: String,
    pub sandbox_exec: String,
    pub route: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicReceiptKey {
    pub algorithm: String,
    pub key_id: String,
    pub subject_public_key_info_sha256: String,
    pub pem_path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OracleSelection {
    pub algorithm: String,
    pub eligibility: String,
    pub query_derivation: String,
    pub decoy_count: usize,
    pub outcome_independent: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialProtocol {
    pub trusted_executor: String,
    pub local_executor: String,
    pub worker_visible_inputs: Vec<String>,
    pub worker_forbidden_inputs: Vec<String>,
    pub fresh_process_per_non_oracle_arm: bool,
    pub fresh_cache_per_non_oracle_arm: bool,
    pub oracle_is_grader_only: bool,
    pub syntax_free_arm: String,
    pub syntax_free_sources: Vec<String>,
    pub arm_source_sets: BTreeMap<Arm, Vec<RetrievalSource>>,
    pub normalization: NormalizationContract,
    pub lexical_context_rule: String,
    pub localization_relevance: String,
    pub history_materialization_rule: String,
    pub measured_timestamp_rule: String,
    pub raw_observation_rule: String,
    pub verification_rule: String,
    pub build_rule: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisPlan {
    pub contrast: String,
    pub estimand: String,
    pub k: usize,
    pub familywise_alpha: f64,
    pub class_alpha: f64,
    pub interval: String,
    pub acceptance: String,
    pub stopping: String,
    pub exclusions: Vec<String>,
    pub replacement: bool,
    pub retry: bool,
    pub missing_and_error: String,
    pub guardrails: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReport {
    pub schema_version: String,
    pub kind: String,
    pub experiment_id: String,
    pub status: String,
    pub statistical_verdict: Option<String>,
    pub gate_claim: String,
    pub chronology: String,
    pub preregistration_digest: String,
    pub corpus_manifest_digest: String,
    pub units: usize,
    pub class_counts: BTreeMap<RepositoryClass, usize>,
    pub measured_trials: usize,
    pub external_blockers: Vec<String>,
}
