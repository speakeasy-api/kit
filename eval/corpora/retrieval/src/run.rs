use crate::{
    AdmissionRecord, Arm, ArmConfig, ClassAnalysis, CorpusManifest, ExecutorEvidence,
    GitMaterializationReceipt, LedgerEvent, LedgerTableRow, MeasuredReport,
    MeasuredRuntimeManifest, NormalizedSymlink, PackagePin, ProtocolError, RawTrial,
    RegistrationRecord, RepositoryClass, Result, SandboxOutcome, SignedLedger, SignedLedgerEntry,
    TaskPin, TrialBinding, TrialGrade, TrialTerminal, WorkerArmRequest, WorkerQuery, canonical,
    grade, run_local_sandbox, sha256,
};
use ed25519_dalek::{
    Signer, SigningKey, VerifyingKey,
    pkcs8::{DecodePrivateKey, DecodePublicKey},
};
use serde::{Deserialize, Serialize, de::IgnoredAny};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const MANIFEST_PATH: &str = "eval/reports/m005/source-semantics/corpus-manifest.json";
const PREREG_PATH: &str = "eval/preregistration/m005-w07.yaml";
const REPORT_PATH: &str = "eval/reports/m005/source-semantics/retrieval-report.json";
const RUN_PATH: &str = "eval/reports/m005/source-semantics/retrieval-run";
const PUBLIC_KEY_PATH: &str = "eval/corpora/retrieval/public-key.pem";
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
const ZERO_DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Deserialize)]
struct ScheduleManifest {
    schema_version: String,
    kind: String,
    units: Vec<ScheduleUnit>,
}

#[derive(Deserialize)]
struct ScheduleUnit {
    schedule_index: usize,
    unit_id: String,
    repository_class: RepositoryClass,
    package: PackagePin,
    rust_sloc: u64,
    source_file_count: usize,
    source_bytes: u64,
    source_digest: String,
    rust_source_digest: String,
    checksum_manifest_digest: String,
    task: TaskPin,
    oracle: IgnoredAny,
    arm_order: Vec<Arm>,
}

#[derive(Clone)]
pub(crate) struct PinnedGit {
    path: PathBuf,
    digest: String,
    version: String,
}

struct MaterializedCheckout {
    path: PathBuf,
    package_files: BTreeMap<String, String>,
}

#[derive(Serialize)]
pub(crate) struct SignatureMessage<'a> {
    pub(crate) domain: &'static str,
    pub(crate) sequence: u64,
    pub(crate) event: LedgerEvent,
    pub(crate) recorded_at: &'a str,
    pub(crate) previous_entry_digest: &'a str,
    pub(crate) payload_path: &'a str,
    pub(crate) payload_digest: &'a str,
    pub(crate) key_id: &'a str,
}

pub fn run_local(vendor: &Path) -> Result<()> {
    let vendor = crate::canonicalize_vendor_root(vendor)?;
    let root = workspace_root();
    let preregistration_bytes =
        read_bounded(&root.join(PREREG_PATH), crate::MAX_JSON_BYTES as u64)?;
    let preregistration: crate::Preregistration = serde_json::from_slice(&preregistration_bytes)?;
    let manifest_bytes = read_bounded(&root.join(MANIFEST_PATH), crate::MAX_JSON_BYTES as u64)?;
    let schedule: ScheduleManifest = serde_json::from_slice(&manifest_bytes)?;
    validate_schedule(&schedule)?;
    crate::protocol::verify_input_pins(&root, &preregistration)?;
    verify_postcommit_inputs(&root, &preregistration)?;
    require_pristine_not_run(&root)?;

    let key_path = signing_key()?;
    validate_private_key(&key_path)?;
    let signing_key = load_signing_key(&key_path)?;
    verify_key_pair(&signing_key, &root.join(PUBLIC_KEY_PATH))?;
    let git = pinned_git(&preregistration)?;
    let commit_sha = git_output(
        &git,
        &root,
        &["log", "-1", "--format=%H", "--", PREREG_PATH],
    )?;
    let commit_time = git_output(
        &git,
        &root,
        &["log", "-1", "--format=%cI", "--", PREREG_PATH],
    )?;
    let commit_time = normalize_timestamp(&commit_time)?;

    let run_root = root.join(RUN_PATH);
    if run_root.exists() {
        return Err(ProtocolError("retained retrieval-run directory already exists".into()).into());
    }
    fs::create_dir(&run_root)?;
    let temporary = unique_temp_root()?;
    fs::create_dir(&temporary)?;
    let result = run_local_inner(
        &root,
        &vendor,
        &run_root,
        &temporary,
        &schedule,
        &preregistration,
        &manifest_bytes,
        &key_path,
        &signing_key,
        &git,
        commit_sha,
        commit_time,
    );
    let _ = fs::remove_dir_all(&temporary);
    result
}

#[allow(clippy::too_many_arguments)]
fn run_local_inner(
    root: &Path,
    vendor: &Path,
    run_root: &Path,
    temporary: &Path,
    schedule: &ScheduleManifest,
    preregistration: &crate::Preregistration,
    manifest_bytes: &[u8],
    key_path: &Path,
    signing_key: &SigningKey,
    git: &PinnedGit,
    commit_sha: String,
    commit_time: String,
) -> Result<()> {
    let executable_source = env::current_exe()?.canonicalize()?;
    let executable = temporary.join("w07-worker");
    let executable_digest = copy_bounded(&executable_source, &executable, 256 << 20, None)?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o500))?;
    let runtime = measured_runtime(&executable, git)?;
    write_json(&run_root.join("runtime.json"), &runtime)?;
    let runtime_digest = sha256(&canonical(&runtime)?);
    let (checkouts, materialization_receipt_digest, materialization_receipt_count) =
        prepare_checkouts(git, vendor, temporary, run_root, schedule)?;
    let registered_at = timestamp()?;
    if parse_timestamp(&commit_time)? >= parse_timestamp(&registered_at)? {
        return Err(
            ProtocolError("preregistration commit does not precede registration".into()).into(),
        );
    }
    let registration = RegistrationRecord {
        schema_version: "2.0".into(),
        kind: "m005_w07_registration".into(),
        experiment_id: preregistration.experiment_id.clone(),
        preregistration_digest: sha256(&canonical(preregistration)?),
        corpus_manifest_digest: preregistration.corpus_manifest_digest.clone(),
        immutable_inputs_digest: sha256(&canonical(&preregistration.immutable_inputs)?),
        git_commit_sha: commit_sha,
        git_commit_time: commit_time,
        registered_at: registered_at.clone(),
        route: ExecutorEvidence::LocalSandboxNotTrusted,
        runtime_manifest_digest: runtime_digest.clone(),
        materialization_receipt_digest: materialization_receipt_digest.clone(),
        materialization_receipt_count,
    };
    write_json(&run_root.join("registration.json"), &registration)?;
    let registration_digest = sha256(&canonical(&registration)?);
    let mut ledger = LedgerWriter::new(
        preregistration
            .public_receipt_key
            .subject_public_key_info_sha256
            .clone(),
        preregistration.public_receipt_key.key_id.clone(),
        signing_key,
    );
    ledger.push(
        LedgerEvent::Registration,
        &registered_at,
        "registration.json",
        &registration_digest,
    )?;
    let materialized_at = timestamp_after(ledger.last_time())?;
    ledger.push(
        LedgerEvent::Materialization,
        &materialized_at,
        "git-materialization.jsonl",
        &materialization_receipt_digest,
    )?;

    let admissions_path = run_root.join("admissions.jsonl");
    let raw_path = run_root.join("raw-trials.jsonl");
    let mut measured_started_at = None;
    let mut measured_ended_at = None;
    let mut raw_records = Vec::with_capacity(crate::UNIT_COUNT * 7);
    for unit in &schedule.units {
        for arm in &unit.arm_order {
            let config = ArmConfig::frozen(*arm);
            let admitted_at = timestamp_after(ledger.last_time())?;
            let admission = AdmissionRecord {
                schema_version: "2.0".into(),
                kind: "m005_w07_trial_admission".into(),
                unit_id: unit.unit_id.clone(),
                arm: *arm,
                sequence_index: raw_records.len(),
                source_digest: unit.rust_source_digest.clone(),
                task_query_digest: unit.task.query_digest.clone(),
                arm_config_digest: sha256(&canonical(&config)?),
                admitted_at: admitted_at.clone(),
                registration_digest: registration_digest.clone(),
            };
            let admission_bytes = canonical(&admission)?;
            let admission_digest = sha256(&admission_bytes);
            let line = append_jsonl(&admissions_path, &admission)?;
            ledger.push(
                LedgerEvent::Admission,
                &admitted_at,
                &format!("admissions.jsonl#{line}"),
                &admission_digest,
            )?;

            let trial_root = temporary.join(format!("trial-{:04}", raw_records.len()));
            fs::create_dir(&trial_root)?;
            let repository = trial_root.join("repository");
            clone_checkout(
                git,
                vendor,
                &checkouts[unit.schedule_index],
                &repository,
                unit,
            )?;
            let source = if unit.package.path_in_vcs.is_empty() {
                repository.clone()
            } else {
                repository.join(&unit.package.path_in_vcs)
            };
            let inputs = trial_root.join("inputs");
            let output = trial_root.join("output");
            let cache = trial_root.join("cache");
            let temp = trial_root.join("tmp");
            for path in [&inputs, &output, &cache, &temp] {
                fs::create_dir(path)?;
            }
            let query_path = inputs.join("query.json");
            let request_path = inputs.join("arm.json");
            let worker_output = output.join("raw.json");
            let cache_id =
                sha256(format!("{}\0{:?}\0{}", unit.unit_id, arm, raw_records.len()).as_bytes());
            write_json(
                &query_path,
                &WorkerQuery {
                    task_id: unit.task.task_id.clone(),
                    query: unit.task.query.clone(),
                    query_digest: unit.task.query_digest.clone(),
                },
            )?;
            write_json(
                &request_path,
                &WorkerArmRequest {
                    unit_id: unit.unit_id.clone(),
                    source_digest: unit.rust_source_digest.clone(),
                    admission_digest: admission_digest.clone(),
                    executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
                    cache_id: cache_id.clone(),
                    worker_executable_digest: executable_digest.clone(),
                    config,
                },
            )?;
            if file_digest(&executable)? != executable_digest {
                return Err(ProtocolError("copied worker changed before launch".into()).into());
            }
            let launch_time = timestamp_after(&admitted_at)?;
            let readonly_roots =
                history_read_roots(&repository, &source, &checkouts[unit.schedule_index])?;
            let outcome = run_local_sandbox(crate::LocalSandboxRequest {
                executable: executable.clone(),
                allowed_executables: vec![git.path.clone()],
                arguments: vec![
                    OsString::from("worker"),
                    source.clone().into_os_string(),
                    readonly_roots[0].clone().into_os_string(),
                    query_path.clone().into_os_string(),
                    request_path.clone().into_os_string(),
                    worker_output.clone().into_os_string(),
                    cache.clone().into_os_string(),
                ],
                source_snapshot: source.clone(),
                readonly_roots,
                request_files: vec![query_path, request_path],
                writable_roots: vec![output, cache, temp],
                forbidden_paths: vec![root.to_path_buf(), key_path.to_path_buf()],
                expected_executable_digest: executable_digest.clone(),
                max_duration: Duration::from_secs(120),
            })?;
            let raw = match outcome {
                SandboxOutcome::Exited(status) if status.success() => {
                    fs::set_permissions(&worker_output, fs::Permissions::from_mode(0o400))?;
                    let sealed = read_bounded(&worker_output, crate::MAX_JSON_BYTES as u64)?;
                    let _sealed_digest = sha256(&sealed);
                    serde_json::from_slice::<RawTrial>(&sealed)?
                }
                SandboxOutcome::Exited(status) => failed_raw(
                    unit,
                    *arm,
                    admission_digest,
                    cache_id,
                    executable_digest.clone(),
                    launch_time,
                    TrialTerminal::Error,
                    format!("worker exited with {status}"),
                )?,
                SandboxOutcome::TimedOut => failed_raw(
                    unit,
                    *arm,
                    admission_digest,
                    cache_id,
                    executable_digest.clone(),
                    launch_time,
                    TrialTerminal::Timeout,
                    "worker timed out".into(),
                )?,
            };
            validate_trial_binding(unit, &admission, &raw)?;
            measured_started_at.get_or_insert_with(|| raw.measured_started_at.clone());
            measured_ended_at = Some(raw.measured_ended_at.clone());
            append_jsonl(&raw_path, &raw)?;
            raw_records.push(raw);
            fs::remove_dir_all(&trial_root)?;
            git_status(
                git,
                &checkouts[unit.schedule_index].path,
                &["worktree", "prune"],
            )?;
        }
    }

    // Oracle-bearing data is deserialized only after every worker raw record is sealed and hashed.
    let manifest: CorpusManifest = serde_json::from_slice(manifest_bytes)?;
    let grades_path = run_root.join("grades.jsonl");
    let bindings_path = run_root.join("trial-bindings.jsonl");
    let mut grades = Vec::with_capacity(raw_records.len());
    for (index, raw) in raw_records.iter().enumerate() {
        let unit = manifest
            .units
            .iter()
            .find(|unit| unit.unit_id == raw.unit_id)
            .ok_or_else(|| ProtocolError("raw trial unit is absent from oracle manifest".into()))?;
        let grade_source = temporary.join(format!("grade-{index:04}"));
        fs::create_dir(&grade_source)?;
        materialize_full(vendor, &grade_source, unit)?;
        let computed = grade(unit, raw, &grade_source)?;
        fs::remove_dir_all(&grade_source)?;
        let grade_line = append_jsonl(&grades_path, &computed)?;
        let binding = TrialBinding {
            schema_version: "2.0".into(),
            kind: "m005_w07_trial_binding".into(),
            unit_id: raw.unit_id.clone(),
            arm: raw.arm,
            raw_trial_digest: sha256(&canonical(raw)?),
            grade_digest: sha256(&canonical(&computed)?),
        };
        let binding_line = append_jsonl(&bindings_path, &binding)?;
        let at = timestamp_after(ledger.last_time())?;
        ledger.push(
            LedgerEvent::Trial,
            &at,
            &format!("trial-bindings.jsonl#{binding_line};grades.jsonl#{grade_line}"),
            &sha256(&canonical(&binding)?),
        )?;
        grades.push(computed);
    }

    let reported_at = timestamp_after(
        measured_ended_at
            .as_deref()
            .ok_or_else(|| ProtocolError("measured run produced no terminal trials".into()))?,
    )?;
    let report = measured_report(
        &manifest,
        &grades,
        preregistration,
        registration_digest,
        runtime_digest,
        materialization_receipt_digest,
        materialization_receipt_count,
        measured_started_at.expect("nonempty fixed schedule"),
        measured_ended_at.expect("nonempty fixed schedule"),
        reported_at,
    )?;
    replace_json(&root.join(REPORT_PATH), &report)?;
    let report_entry_time = timestamp_after(&report.reported_at)?;
    ledger.push(
        LedgerEvent::Report,
        &report_entry_time,
        "../retrieval-report.json",
        &sha256(&canonical(&report)?),
    )?;
    write_json(&run_root.join("signed-ledger.json"), &ledger.finish())?;
    println!("{}", report.status);
    Ok(())
}

pub fn run_trusted() -> Result<()> {
    let root = workspace_root();
    crate::protocol::verify()?;
    require_pristine_not_run(&root)?;
    let report = crate::BlockedReport {
        schema_version: "2.0".into(),
        kind: "m005_w07_blocked_report".into(),
        experiment_id: "m005-w07-rust-registry-v2".into(),
        status: "BLOCKED_G03_G04".into(),
        gate_claim: "NONE_BLOCKED_EXTERNAL".into(),
        measured_trials: 0,
        blocker: "BLOCKED_G03_G04: M005-W07 trusted execution requires the unavailable pinned M004 production isolated adapter and satisfied G04; no trial was measured".into(),
    };
    crate::protocol::validate_schema(
        include_bytes!("../schema/v2/blocked-report.schema.json"),
        &canonical(&report)?,
    )?;
    replace_json(&root.join(REPORT_PATH), &report)?;
    println!("BLOCKED_G03_G04");
    Ok(())
}

pub fn cleanup_failed_run() -> Result<()> {
    let root = workspace_root();
    cleanup_failed_run_at(&root)
}

fn cleanup_failed_run_at(root: &Path) -> Result<()> {
    let report: serde_json::Value = serde_json::from_slice(&read_bounded(
        &root.join(REPORT_PATH),
        crate::MAX_JSON_BYTES as u64,
    )?)?;
    if report.get("kind").and_then(serde_json::Value::as_str) == Some("m005_w07_measured_report")
        || report
            .get("measured_trials")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|count| count > 0)
    {
        return Err(ProtocolError("refusing to remove a completed measured run".into()).into());
    }
    let run = root.join(RUN_PATH);
    let ledger_path = run.join("signed-ledger.json");
    if ledger_path.exists() {
        let ledger: SignedLedger =
            serde_json::from_slice(&read_bounded(&ledger_path, crate::MAX_JSON_BYTES as u64)?)?;
        if ledger.entries.last().is_some_and(|entry| {
            entry.event == LedgerEvent::Report
                && entry.signature.len() == 128
                && entry.signature.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(ProtocolError("refusing to remove a terminal signed run".into()).into());
        }
    }
    match fs::symlink_metadata(&run) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(run)?;
        }
        Ok(_) => {
            return Err(ProtocolError("retrieval-run is not a regular directory".into()).into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn require_pristine_not_run(root: &Path) -> Result<()> {
    if root.join(RUN_PATH).exists() {
        return Err(ProtocolError("retrieval run state already exists".into()).into());
    }
    let report: serde_json::Value = serde_json::from_slice(&read_bounded(
        &root.join(REPORT_PATH),
        crate::MAX_JSON_BYTES as u64,
    )?)?;
    if report.get("kind").and_then(serde_json::Value::as_str) != Some("m005_w07_status_report")
        || report.get("status").and_then(serde_json::Value::as_str) != Some("NOT_RUN_PRECOMMIT")
        || report
            .get("measured_trials")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
    {
        return Err(ProtocolError("trusted run requires pristine NOT_RUN state".into()).into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn measured_report(
    manifest: &CorpusManifest,
    grades: &[TrialGrade],
    preregistration: &crate::Preregistration,
    registration_digest: String,
    runtime_manifest_digest: String,
    materialization_receipt_digest: String,
    materialization_receipt_count: usize,
    measured_started_at: String,
    measured_ended_at: String,
    reported_at: String,
) -> Result<MeasuredReport> {
    let mut analyses = Vec::new();
    for class in [
        RepositoryClass::Small,
        RepositoryClass::Medium,
        RepositoryClass::Large,
    ] {
        let units = manifest
            .units
            .iter()
            .filter(|unit| unit.repository_class == class)
            .collect::<Vec<_>>();
        let baseline = units
            .iter()
            .map(|unit| grade_for(grades, &unit.unit_id, Arm::L).localization_success)
            .collect::<Vec<_>>();
        let candidate = units
            .iter()
            .map(|unit| grade_for(grades, &unit.unit_id, Arm::C).localization_success)
            .collect::<Vec<_>>();
        match kit::evaluation::reports::exact_paired_binary_interval(
            &baseline,
            &candidate,
            0.05 / 3.0,
        ) {
            Ok(interval) => analyses.push(ClassAnalysis {
                repository_class: class,
                pairs: units.len(),
                l_successes: baseline.iter().filter(|value| **value).count(),
                c_successes: candidate.iter().filter(|value| **value).count(),
                estimate: Some(interval.estimate),
                confidence_level: Some(interval.confidence_level),
                lower: Some(interval.lower),
                upper: Some(interval.upper),
                passed: interval.lower > 0.0,
                error: None,
            }),
            Err(error) => analyses.push(ClassAnalysis {
                repository_class: class,
                pairs: units.len(),
                l_successes: baseline.iter().filter(|value| **value).count(),
                c_successes: candidate.iter().filter(|value| **value).count(),
                estimate: None,
                confidence_level: None,
                lower: None,
                upper: None,
                passed: false,
                error: Some(error.to_string()),
            }),
        }
    }
    let guardrails_passed = treatment_guardrails_pass(grades);
    let passed = report_passes(&analyses, guardrails_passed);
    Ok(MeasuredReport {
        schema_version: "2.0".into(),
        kind: "m005_w07_measured_report".into(),
        experiment_id: preregistration.experiment_id.clone(),
        status: if passed {
            "PASS_LOCAL_CALIBRATION"
        } else {
            "FAIL_LOCAL_CALIBRATION"
        }
        .into(),
        route: ExecutorEvidence::LocalSandboxNotTrusted,
        statistical_verdict: if passed { "PASS" } else { "FAIL" }.into(),
        gate_claim: "NONE_BLOCKED_EXTERNAL".into(),
        preregistration_digest: sha256(&canonical(preregistration)?),
        corpus_manifest_digest: sha256(&canonical(manifest)?),
        registration_digest,
        runtime_manifest_digest,
        materialization_receipt_digest,
        materialization_receipt_count,
        measured_started_at,
        measured_ended_at,
        reported_at,
        units: manifest.units.len(),
        measured_trials: grades.len(),
        terminal_trials: grades.len(),
        class_analyses: analyses,
        guardrails_passed,
        external_blockers: vec!["G03".into(), "G04".into(), "BLK-14".into(), "EXT-15".into()],
    })
}

fn treatment_guardrails_pass(grades: &[TrialGrade]) -> bool {
    grades
        .iter()
        .filter(|grade| grade.arm != Arm::L)
        .all(|grade| {
            grade.downstream_mechanical_success
                && grade.freshness_success
                && grade.latency_success
                && grade.provenance_success
                && grade.token_budget_success
                && grade.wrong_decoy_success
                && grade.terminal_success
        })
}

fn report_passes(analyses: &[ClassAnalysis], guardrails_passed: bool) -> bool {
    guardrails_passed
        && analyses.len() == 3
        && analyses
            .iter()
            .all(|analysis| analysis.passed && analysis.lower.is_some_and(|lower| lower > 0.0))
}

fn grade_for<'a>(grades: &'a [TrialGrade], unit: &str, arm: Arm) -> &'a TrialGrade {
    grades
        .iter()
        .find(|grade| grade.unit_id == unit && grade.arm == arm)
        .expect("fixed complete trial matrix")
}

fn validate_schedule(schedule: &ScheduleManifest) -> Result<()> {
    if schedule.schema_version != "2.0"
        || schedule.kind != "m005_w07_registry_corpus"
        || schedule.units.len() != crate::UNIT_COUNT
    {
        return Err(ProtocolError("invalid public schedule".into()).into());
    }
    let mut ids = BTreeSet::new();
    for (index, unit) in schedule.units.iter().enumerate() {
        let _ = &unit.oracle;
        let expected_class = [
            RepositoryClass::Small,
            RepositoryClass::Medium,
            RepositoryClass::Large,
        ][index / crate::UNITS_PER_CLASS];
        if unit.schedule_index != index
            || unit.repository_class != expected_class
            || !ids.insert(&unit.unit_id)
            || unit.arm_order.len() != 7
            || unit
                .arm_order
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != 7
            || unit.source_file_count == 0
            || unit.source_bytes == 0
            || unit.rust_sloc == 0
            || !crate::valid_digest(&unit.rust_source_digest)
            || unit.package.vcs_commit.len() != 40
            || !unit
                .package
                .vcs_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || (!unit.package.path_in_vcs.is_empty()
                && validate_relative(&unit.package.path_in_vcs).is_err())
        {
            return Err(ProtocolError("invalid scheduled unit".into()).into());
        }
    }
    Ok(())
}

fn prepare_checkouts(
    git: &PinnedGit,
    vendor: &Path,
    temporary: &Path,
    run_root: &Path,
    schedule: &ScheduleManifest,
) -> Result<(Vec<MaterializedCheckout>, String, usize)> {
    let checkouts_root = temporary.join("checkouts");
    fs::create_dir(&checkouts_root)?;
    let receipts = run_root.join("git-materialization.jsonl");
    let mut checkouts = Vec::with_capacity(schedule.units.len());
    for unit in &schedule.units {
        let checkout = checkouts_root.join(format!("unit-{:02}", unit.schedule_index));
        let commands = vec![
            vec!["init".into(), checkout.to_string_lossy().into_owned()],
            vec![
                "remote".into(),
                "add".into(),
                "origin".into(),
                unit.package.normalized_repository_url.clone(),
            ],
            vec![
                "fetch".into(),
                "--quiet".into(),
                "--depth=100".into(),
                "origin".into(),
                unit.package.vcs_commit.clone(),
            ],
            vec![
                "checkout".into(),
                "--quiet".into(),
                "--detach".into(),
                "FETCH_HEAD".into(),
            ],
        ];
        git_status_owned(git, temporary, &commands[0])?;
        for command in &commands[1..] {
            git_status_owned(git, &checkout, command)?;
        }
        let files = validate_checkout(git, vendor, &checkout, unit).map_err(|error| {
            ProtocolError(format!(
                "failed to validate package/VCS materialization for {}: {error}",
                unit.unit_id
            ))
        })?;
        let normalized_symlinks = filter_checkout(&checkout, &unit.package.path_in_vcs, &files)
            .map_err(|error| {
                ProtocolError(format!(
                    "failed to filter package/VCS materialization for {}: {error}",
                    unit.unit_id
                ))
            })?;
        let source = if unit.package.path_in_vcs.is_empty() {
            checkout.clone()
        } else {
            checkout.join(&unit.package.path_in_vcs)
        };
        materialize_package_files(
            &vendor.join(format!("{}-{}", unit.package.name, unit.package.version)),
            &source,
            &files,
            &unit.unit_id,
        )?;
        let package_files = files.keys().cloned().collect::<Vec<_>>();
        append_jsonl(
            &receipts,
            &GitMaterializationReceipt {
                unit_id: unit.unit_id.clone(),
                git_path: git.path.to_string_lossy().into_owned(),
                git_digest: git.digest.clone(),
                git_version: git.version.clone(),
                fetch_depth: 100,
                commands,
                result: "FETCH_HEAD_DETACHED_EXACT".into(),
                repository_url: unit.package.normalized_repository_url.clone(),
                vcs_commit: unit.package.vcs_commit.clone(),
                path_in_vcs: unit.package.path_in_vcs.clone(),
                head: unit.package.vcs_commit.clone(),
                rust_source_digest: unit.rust_source_digest.clone(),
                package_file_set_digest: sha256(&canonical(&files)?),
                package_files: package_files.clone(),
                normalized_symlinks,
            },
        )?;
        checkouts.push(MaterializedCheckout {
            path: checkout,
            package_files: files,
        });
    }
    Ok((checkouts, file_digest(&receipts)?, schedule.units.len()))
}

fn validate_checkout(
    git: &PinnedGit,
    vendor: &Path,
    checkout: &Path,
    unit: &ScheduleUnit,
) -> Result<BTreeMap<String, String>> {
    if git_output(git, checkout, &["rev-parse", "HEAD"])? != unit.package.vcs_commit
        || git_output(git, checkout, &["remote", "get-url", "origin"])?
            != unit.package.normalized_repository_url
    {
        return Err(ProtocolError("pinned upstream checkout identity mismatch".into()).into());
    }
    let package = vendor.join(format!("{}-{}", unit.package.name, unit.package.version));
    let files = crate::protocol::validated_pin_files(
        &package,
        &unit.package.cargo_lock_checksum,
        &unit.checksum_manifest_digest,
        &unit.source_digest,
        unit.source_bytes,
        unit.source_file_count,
        unit.rust_sloc,
    )?;
    let source = if unit.package.path_in_vcs.is_empty() {
        checkout.to_path_buf()
    } else {
        checkout.join(&unit.package.path_in_vcs)
    };
    let mut rust = std::collections::BTreeMap::new();
    for (relative, expected) in files.iter().filter(|(path, _)| path.ends_with(".rs")) {
        let checkout_path = source.join(relative);
        match fs::symlink_metadata(&checkout_path) {
            Ok(metadata) if !metadata.file_type().is_symlink() => {
                let checkout_bytes = read_bounded(&checkout_path, crate::MAX_SOURCE_FILE_BYTES)
                    .map_err(|error| {
                        ProtocolError(format!(
                            "failed to read VCS Rust path {}:{relative}: {error}",
                            unit.unit_id
                        ))
                    })?;
                let digest = format!("{:x}", Sha256::digest(&checkout_bytes));
                if digest != *expected {
                    return Err(ProtocolError(format!(
                        "upstream/package Rust bytes differ for {}:{relative}",
                        unit.unit_id
                    ))
                    .into());
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ProtocolError(format!(
                    "failed to inspect VCS Rust path {}:{relative}: {error}",
                    unit.unit_id
                ))
                .into());
            }
        }
        rust.insert(relative.clone(), expected.clone());
    }
    if sha256(&canonical(&rust)?) != unit.rust_source_digest {
        return Err(ProtocolError("upstream Rust source-tree digest mismatch".into()).into());
    }
    Ok(files)
}

fn clone_checkout(
    git: &PinnedGit,
    vendor: &Path,
    source: &MaterializedCheckout,
    destination: &Path,
    unit: &ScheduleUnit,
) -> Result<()> {
    git_status_owned(
        git,
        &source.path,
        &[
            "worktree".into(),
            "add".into(),
            "--quiet".into(),
            "--detach".into(),
            destination.to_string_lossy().into_owned(),
            unit.package.vcs_commit.clone(),
        ],
    )?;
    filter_checkout(
        destination,
        &unit.package.path_in_vcs,
        &source.package_files,
    )?;
    let package_source = if unit.package.path_in_vcs.is_empty() {
        destination.to_path_buf()
    } else {
        destination.join(&unit.package.path_in_vcs)
    };
    materialize_package_files(
        &vendor.join(format!("{}-{}", unit.package.name, unit.package.version)),
        &package_source,
        &source.package_files,
        &unit.unit_id,
    )?;
    if git_output(git, destination, &["rev-parse", "HEAD"])? != unit.package.vcs_commit {
        return Err(ProtocolError("fresh checkout HEAD mismatch".into()).into());
    }
    Ok(())
}

fn filter_checkout(
    checkout: &Path,
    package_prefix: &str,
    package_files: &BTreeMap<String, String>,
) -> Result<Vec<NormalizedSymlink>> {
    let wanted = package_files
        .iter()
        .map(|(path, digest)| {
            if package_prefix.is_empty() {
                (path.clone(), digest.clone())
            } else {
                (format!("{package_prefix}/{path}"), digest.clone())
            }
        })
        .collect::<BTreeMap<_, _>>();
    let canonical_root = checkout.canonicalize()?;
    let mut normalized = normalize_wanted_symlinks(&canonical_root, &wanted)?;
    filter_directory(&canonical_root, &canonical_root, &wanted)?;
    if !package_prefix.is_empty() {
        let prefix = format!("{package_prefix}/");
        for normalization in &mut normalized {
            normalization.path = normalization
                .path
                .strip_prefix(&prefix)
                .expect("wanted path has package prefix")
                .to_owned();
        }
    }
    Ok(normalized)
}

fn normalize_wanted_symlinks(
    root: &Path,
    wanted: &BTreeMap<String, String>,
) -> Result<Vec<NormalizedSymlink>> {
    let mut normalized = Vec::new();
    for (relative, expected) in wanted {
        validate_relative(relative)?;
        let components = Path::new(relative).components().collect::<Vec<_>>();
        let mut current = root.to_path_buf();
        for (index, component) in components.iter().enumerate() {
            current.push(component.as_os_str());
            let metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            };
            if !metadata.file_type().is_symlink() {
                continue;
            }
            if index + 1 != components.len() {
                return Err(ProtocolError(format!(
                    "symlink in fetched worktree at {} is forbidden: path is an ancestor of a frozen package file",
                    path_string(current.strip_prefix(root)?)
                ))
                .into());
            }
            normalized.push(normalize_wanted_symlink(
                root, &current, relative, expected,
            )?);
        }
    }
    Ok(normalized)
}

fn normalize_wanted_symlink(
    root: &Path,
    path: &Path,
    relative: &str,
    expected: &str,
) -> Result<NormalizedSymlink> {
    let target = path.canonicalize().map_err(|error| {
        ProtocolError(format!(
            "failed to resolve frozen package symlink {relative}: {error}"
        ))
    })?;
    if !target.starts_with(root) {
        return Err(ProtocolError(format!(
            "frozen package symlink {relative} escapes the checkout root"
        ))
        .into());
    }
    let (bytes, mode) = read_bounded_file(&target, crate::MAX_SOURCE_FILE_BYTES, true)?;
    let digest = sha256(&bytes);
    if digest.strip_prefix("sha256:") != Some(expected) {
        return Err(ProtocolError(format!(
            "frozen package symlink target digest mismatch: {relative}"
        ))
        .into());
    }

    fs::remove_file(path)?;
    let write_result = (|| -> Result<()> {
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        output.set_permissions(fs::Permissions::from_mode(mode))?;
        output.write_all(&bytes)?;
        output.sync_all()?;
        let metadata = output.metadata()?;
        if !metadata.file_type().is_file()
            || metadata.len() != bytes.len() as u64
            || metadata.permissions().mode() & 0o7777 != mode
        {
            return Err(ProtocolError("normalized symlink file mismatch".into()).into());
        }
        Ok(())
    })();
    if write_result.is_err()
        && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
    {
        let _ = fs::remove_file(path);
    }
    write_result?;
    Ok(NormalizedSymlink {
        path: relative.to_owned(),
        target_digest: digest,
    })
}

fn filter_directory(
    root: &Path,
    directory: &Path,
    wanted: &BTreeMap<String, String>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path_string(path.strip_prefix(root)?);
        if relative == ".git" {
            continue;
        }
        let exact = wanted.contains_key(&relative);
        let ancestor = wanted
            .keys()
            .any(|wanted| wanted.starts_with(&format!("{relative}/")));
        let retained = exact || ancestor;
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            ProtocolError(format!(
                "failed to inspect filtered checkout path {}: {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            if retained {
                let relation = if exact {
                    "a frozen package file"
                } else {
                    "an ancestor of a frozen package file"
                };
                return Err(ProtocolError(format!(
                    "symlink in fetched worktree at {relative} is forbidden: path is {relation}"
                ))
                .into());
            }
            fs::remove_file(&path)?;
            continue;
        }
        if metadata.is_dir() && retained {
            filter_directory(root, &path, wanted)?;
            if fs::read_dir(&path)?.next().is_none() {
                fs::remove_dir(&path)?;
            }
        } else if !retained {
            if metadata.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

fn materialize_package_files(
    package: &Path,
    destination: &Path,
    files: &BTreeMap<String, String>,
    unit_id: &str,
) -> Result<()> {
    for (relative, expected) in files {
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            crate::reject_symlink_components(parent, true)?;
            fs::create_dir_all(parent)?;
        }
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_file() => fs::remove_file(&target)?,
            Ok(_) => {
                return Err(ProtocolError(format!(
                    "refusing to replace non-file package path {unit_id}:{relative}"
                ))
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        copy_bounded(
            &package.join(relative),
            &target,
            crate::MAX_SOURCE_FILE_BYTES,
            Some(expected),
        )
        .map_err(|error| {
            ProtocolError(format!(
                "failed to materialize package path {unit_id}:{relative}: {error}"
            ))
        })?;
    }
    Ok(())
}

fn history_read_roots(
    repository: &Path,
    source: &Path,
    checkout: &MaterializedCheckout,
) -> Result<Vec<PathBuf>> {
    let dot_git = fs::symlink_metadata(repository.join(".git"))?;
    if !dot_git.file_type().is_file() || dot_git.file_type().is_symlink() {
        return Err(
            ProtocolError("trial worktree lacks regular Git metadata pointer".into()).into(),
        );
    }
    let mut roots = vec![checkout.path.join(".git").canonicalize()?];
    if source != repository {
        roots.push(repository.to_path_buf());
    }
    Ok(roots)
}

pub(crate) fn materialize_full(
    vendor: &Path,
    destination: &Path,
    unit: &crate::CorpusUnit,
) -> Result<()> {
    let package = vendor.join(format!("{}-{}", unit.package.name, unit.package.version));
    let files = crate::protocol::validated_unit_files(&package, unit)?;
    copy_snapshot(&package, destination, files.iter())?;
    crate::protocol::validated_unit_files(destination, unit)?;
    Ok(())
}

fn copy_snapshot<'a>(
    source: &Path,
    destination: &Path,
    files: impl Iterator<Item = (&'a String, &'a String)>,
) -> Result<()> {
    for (relative, expected) in files {
        validate_relative(relative)?;
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            crate::reject_symlink_components(parent, true)?;
            fs::create_dir_all(parent)?;
        }
        copy_bounded(
            &source.join(relative),
            &target,
            crate::MAX_SOURCE_FILE_BYTES,
            Some(expected),
        )?;
    }
    copy_bounded(
        &source.join(".cargo-checksum.json"),
        &destination.join(".cargo-checksum.json"),
        16 << 20,
        None,
    )?;
    Ok(())
}

fn copy_bounded(
    source: &Path,
    destination: &Path,
    maximum: u64,
    expected_hex_digest: Option<&str>,
) -> Result<String> {
    let bytes = read_bounded(source, maximum)?;
    let digest = sha256(&bytes);
    if expected_hex_digest.is_some_and(|expected| digest.strip_prefix("sha256:") != Some(expected))
    {
        return Err(
            ProtocolError(format!("copy source digest mismatch: {}", source.display())).into(),
        );
    }
    crate::reject_symlink_components(destination, true)?;
    let mut output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    let metadata = output.metadata()?;
    if metadata.len() != bytes.len() as u64 {
        return Err(ProtocolError("copied file size mismatch".into()).into());
    }
    Ok(digest)
}

#[allow(clippy::too_many_arguments)]
fn failed_raw(
    unit: &ScheduleUnit,
    arm: Arm,
    admission_digest: String,
    cache_id: String,
    worker_executable_digest: String,
    started: String,
    terminal: TrialTerminal,
    error: String,
) -> Result<RawTrial> {
    Ok(RawTrial {
        schema_version: "2.0".into(),
        kind: "m005_w07_raw_arm_trial".into(),
        unit_id: unit.unit_id.clone(),
        task_id: unit.task.task_id.clone(),
        arm,
        executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
        admission_digest,
        source_digest: unit.rust_source_digest.clone(),
        task_query_digest: unit.task.query_digest.clone(),
        arm_config_digest: sha256(&canonical(&ArmConfig::frozen(arm))?),
        worker_executable_digest,
        process_id: 0,
        cache_id,
        measured_started_at: started,
        measured_ended_at: timestamp()?,
        elapsed_ns: 0,
        index_latency_ms: 0,
        query_latency_ms: 0,
        token_count: 0,
        syntax_initializations: 0,
        terminal,
        observations: Vec::new(),
        worker_error: Some(error.chars().take(4096).collect()),
    })
}

fn validate_trial_binding(
    unit: &ScheduleUnit,
    admission: &AdmissionRecord,
    raw: &RawTrial,
) -> Result<()> {
    if raw.unit_id != unit.unit_id
        || raw.task_id != unit.task.task_id
        || raw.arm != admission.arm
        || raw.source_digest != unit.rust_source_digest
        || raw.task_query_digest != unit.task.query_digest
        || raw.admission_digest != sha256(&canonical(admission)?)
        || parse_timestamp(&admission.admitted_at)? >= parse_timestamp(&raw.measured_started_at)?
        || parse_timestamp(&raw.measured_started_at)? >= parse_timestamp(&raw.measured_ended_at)?
    {
        return Err(ProtocolError(
            "worker raw trial violates signed admission or chronology".into(),
        )
        .into());
    }
    Ok(())
}

struct LedgerWriter<'a> {
    ledger: SignedLedger,
    key_id: String,
    key: &'a SigningKey,
}

impl<'a> LedgerWriter<'a> {
    fn new(public_key_digest: String, key_id: String, key: &'a SigningKey) -> Self {
        Self {
            ledger: SignedLedger {
                schema_version: "2.0".into(),
                kind: "m005_w07_public_signed_ledger".into(),
                algorithm: "Ed25519".into(),
                public_key_digest,
                entries: Vec::new(),
                table_rows: Vec::new(),
            },
            key_id,
            key,
        }
    }

    fn last_time(&self) -> &str {
        self.ledger
            .entries
            .last()
            .map(|entry| entry.recorded_at.as_str())
            .unwrap_or("1970-01-01T00:00:00Z")
    }

    fn push(&mut self, event: LedgerEvent, at: &str, path: &str, digest: &str) -> Result<()> {
        if !self.ledger.entries.is_empty()
            && parse_timestamp(self.last_time())? >= parse_timestamp(at)?
        {
            return Err(ProtocolError("ledger timestamp is not strictly increasing".into()).into());
        }
        let sequence = self.ledger.entries.len() as u64 + 1;
        let previous = self
            .ledger
            .entries
            .last()
            .map(|entry| sha256(&canonical(entry).expect("bounded ledger entry")))
            .unwrap_or_else(|| ZERO_DIGEST.into());
        let message = SignatureMessage {
            domain: "m005-w07-ledger-entry-v1",
            sequence,
            event,
            recorded_at: at,
            previous_entry_digest: &previous,
            payload_path: path,
            payload_digest: digest,
            key_id: &self.key_id,
        };
        let signature = sign(self.key, &canonical(&message)?);
        self.ledger.entries.push(SignedLedgerEntry {
            sequence,
            event,
            recorded_at: at.into(),
            previous_entry_digest: previous,
            payload_path: path.into(),
            payload_digest: digest.into(),
            key_id: self.key_id.clone(),
            signature,
        });
        self.ledger.table_rows.push(LedgerTableRow {
            sequence,
            event,
            payload_path: path.into(),
            payload_digest: digest.into(),
        });
        Ok(())
    }

    fn finish(self) -> SignedLedger {
        self.ledger
    }
}

fn sign(key: &SigningKey, message: &[u8]) -> String {
    key.sign(message)
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn measured_runtime(executable: &Path, git: &PinnedGit) -> Result<MeasuredRuntimeManifest> {
    Ok(MeasuredRuntimeManifest {
        schema_version: "2.0".into(),
        kind: "m005_w07_runtime_manifest".into(),
        route: ExecutorEvidence::LocalSandboxNotTrusted,
        os: command_output("uname", &["-s"])?,
        architecture: command_output("uname", &["-m"])?,
        os_version: command_output("sw_vers", &["-productVersion"])?,
        rustc: command_output("rustc", &["--version", "--verbose"])?,
        cargo: command_output("cargo", &["--version", "--verbose"])?,
        rustc_executable_digest: file_digest(&resolve_executable("rustc")?)?,
        cargo_executable_digest: file_digest(&resolve_executable("cargo")?)?,
        uname_executable_digest: file_digest(&resolve_executable("uname")?)?,
        sw_vers_executable_digest: file_digest(&resolve_executable("sw_vers")?)?,
        git_path: git.path.to_string_lossy().into_owned(),
        git_executable_digest: git.digest.clone(),
        git: format!(
            "{}\n{}",
            command(git, &workspace_root(), &["--version"])?,
            git.digest
        ),
        sandbox_exec: file_digest(Path::new("/usr/bin/sandbox-exec"))?,
        executable_digest: file_digest(executable)?,
    })
}

pub(crate) fn verify_postcommit_inputs(
    root: &Path,
    preregistration: &crate::Preregistration,
) -> Result<()> {
    crate::protocol::verify_input_pins(root, preregistration)?;
    verify_postcommit_git(root, preregistration)
}

pub(crate) fn verify_postcommit_git(
    root: &Path,
    preregistration: &crate::Preregistration,
) -> Result<()> {
    let git = pinned_git(preregistration)?;
    for path in [PREREG_PATH, MANIFEST_PATH] {
        let committed = git_output_bytes(&git, root, &["show", &format!("HEAD:{path}")])?;
        if committed != read_bounded(&root.join(path), crate::MAX_JSON_BYTES as u64)? {
            return Err(ProtocolError(format!("frozen input differs from HEAD: {path}")).into());
        }
    }
    let output = git_output_bytes(
        &git,
        root,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
            ".",
            ":(exclude)eval/preregistration/m005-w07.yaml",
            ":(exclude)eval/reports/m005/source-semantics/corpus-manifest.json",
            ":(exclude)eval/reports/m005/source-semantics/retrieval-report.json",
            ":(exclude)eval/reports/m005/source-semantics/retrieval-run/**",
        ],
    )?;
    if !output.is_empty() {
        return Err(ProtocolError(
            "build-relevant tracked or untracked inputs are not committed and clean".into(),
        )
        .into());
    }
    Ok(())
}

fn signing_key() -> Result<PathBuf> {
    env::var_os("KIT_M005_W07_SIGNING_KEY")
        .map(PathBuf::from)
        .ok_or_else(|| ProtocolError("KIT_M005_W07_SIGNING_KEY is required".into()).into())
}

fn validate_private_key(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(ProtocolError("signing key must be a regular 0600 file".into()).into());
    }
    Ok(())
}

fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let pem = String::from_utf8(read_bounded(path, 4096)?)?;
    Ok(SigningKey::from_pkcs8_pem(&pem)?)
}

fn verify_key_pair(private: &SigningKey, public: &Path) -> Result<()> {
    let pem = String::from_utf8(read_bounded(public, 4096)?)?;
    let pinned = VerifyingKey::from_public_key_pem(&pem)?;
    if private.verifying_key() != pinned {
        return Err(ProtocolError("signing key does not match pinned public key".into()).into());
    }
    Ok(())
}

pub(crate) fn pinned_git(preregistration: &crate::Preregistration) -> Result<PinnedGit> {
    let path = PathBuf::from(&preregistration.runtime_environment.git_path);
    let digest = file_digest(&path)?;
    if !path.is_absolute() || digest != preregistration.runtime_environment.git_executable_digest {
        return Err(ProtocolError("pinned Git descriptor mismatch".into()).into());
    }
    let git = PinnedGit {
        path,
        digest,
        version: preregistration.runtime_environment.git.clone(),
    };
    if git_output(&git, &workspace_root(), &["--version"])? != git.version {
        return Err(ProtocolError("pinned Git descriptor mismatch".into()).into());
    }
    Ok(git)
}

pub(crate) fn preregistration_git() -> Result<PinnedGit> {
    let path = PathBuf::from("/usr/bin/git").canonicalize()?;
    let digest = file_digest(&path)?;
    let mut git = PinnedGit {
        path,
        digest,
        version: String::new(),
    };
    git.version = git_output(&git, &workspace_root(), &["--version"])?;
    Ok(git)
}

pub(crate) fn git_path(git: &PinnedGit) -> &Path {
    &git.path
}

pub(crate) fn git_digest(git: &PinnedGit) -> &str {
    &git.digest
}

pub(crate) fn trusted_git_output(
    git: &PinnedGit,
    root: &Path,
    arguments: &[&str],
) -> Result<String> {
    git_output(git, root, arguments)
}

#[cfg(test)]
pub(crate) fn trusted_git_status(git: &PinnedGit, root: &Path, arguments: &[&str]) -> Result<()> {
    git_status(git, root, arguments)
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<usize> {
    let line = canonical(value)?;
    if line.is_empty() || line.len() > MAX_LINE_BYTES || line.contains(&b'\n') {
        return Err(ProtocolError("JSONL record exceeds line bound".into()).into());
    }
    crate::reject_symlink_components(path, true)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let number = count_lines(&mut file)? + 1;
    file.write_all(&line)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(number)
}

fn count_lines(file: &mut fs::File) -> Result<usize> {
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut count = 0_usize;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(buffer[..read].iter().filter(|byte| **byte == b'\n').count())
            .ok_or_else(|| ProtocolError("JSONL line count overflow".into()))?;
    }
    file.seek(SeekFrom::End(0))?;
    Ok(count)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if bytes.len() > crate::MAX_JSON_BYTES {
        return Err(ProtocolError("JSON output exceeds bound".into()).into());
    }
    crate::reject_symlink_components(path, true)?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn replace_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    crate::reject_symlink_components(path, true)?;
    let parent = path
        .parent()
        .ok_or_else(|| ProtocolError("report path has no parent".into()))?;
    let temporary = parent.join(format!(
        ".retrieval-report-{}-{}.tmp",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    write_json(&temporary, value)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn timestamp() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

fn timestamp_after(previous: &str) -> Result<String> {
    let previous = parse_timestamp(previous)?;
    loop {
        let now = OffsetDateTime::now_utc();
        if now > previous {
            return Ok(now.format(&Rfc3339)?);
        }
        std::thread::yield_now();
    }
}

pub(crate) fn normalize_timestamp(value: &str) -> Result<String> {
    let normalized = OffsetDateTime::parse(value, &Rfc3339)?
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)?;
    parse_timestamp(&normalized)?;
    Ok(normalized)
}

pub(crate) fn parse_timestamp(value: &str) -> Result<OffsetDateTime> {
    if !value.ends_with('Z') || value.len() > 40 {
        return Err(ProtocolError("timestamp is not canonical UTC RFC3339".into()).into());
    }
    Ok(OffsetDateTime::parse(value, &Rfc3339)?)
}

pub(crate) fn resolve_executable(name: &str) -> Result<PathBuf> {
    let path = env::var_os("PATH").ok_or_else(|| ProtocolError("PATH is unavailable".into()))?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Ok(candidate.canonicalize()?);
        }
    }
    Err(ProtocolError(format!("required executable is unavailable: {name}")).into())
}

fn command(program: &PinnedGit, root: &Path, arguments: &[&str]) -> Result<String> {
    git_output(program, root, arguments)
}

pub(crate) fn trusted_git_command(git: &PinnedGit, root: &Path) -> Result<Command> {
    if !git.path.is_absolute() || file_digest(&git.path)? != git.digest {
        return Err(ProtocolError("pinned Git executable changed".into()).into());
    }
    let mut command = Command::new(&git.path);
    command
        .env_clear()
        .env("HOME", env::temp_dir().canonicalize()?)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .env("GIT_ALLOW_PROTOCOL", "https")
        .env("LC_ALL", "C")
        .current_dir(root)
        .stdin(Stdio::null());
    Ok(command)
}

fn git_output(git: &PinnedGit, root: &Path, arguments: &[&str]) -> Result<String> {
    let output = git_output_bytes(git, root, arguments)?;
    if output.is_empty() {
        return Err(ProtocolError("pinned Git returned empty output".into()).into());
    }
    Ok(String::from_utf8(output)?.trim().into())
}

fn git_output_bytes(git: &PinnedGit, root: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = trusted_git_command(git, root)?
        .args(arguments)
        .output()
        .map_err(|error| {
            ProtocolError(format!(
                "failed to launch pinned Git in {} with {arguments:?}: {error}",
                root.display()
            ))
        })?;
    if !output.status.success()
        || output.stdout.len() > 64 * 1024 * 1024
        || output.stderr.len() > 64 * 1024
    {
        return Err(ProtocolError(format!(
            "pinned Git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(output.stdout)
}

fn git_status(git: &PinnedGit, root: &Path, arguments: &[&str]) -> Result<()> {
    let _ = git_output_bytes(git, root, arguments)?;
    Ok(())
}

fn git_status_owned(git: &PinnedGit, root: &Path, arguments: &[String]) -> Result<()> {
    let output = trusted_git_command(git, root)?
        .args(arguments)
        .output()
        .map_err(|error| {
            ProtocolError(format!(
                "failed to launch pinned Git materialization in {} with {arguments:?}: {error}",
                root.display()
            ))
        })?;
    if !output.status.success()
        || output.stdout.len() > 64 * 1024
        || output.stderr.len() > 64 * 1024
    {
        return Err(ProtocolError(format!(
            "pinned Git materialization failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(())
}

fn command_output(program: &str, arguments: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .current_dir(workspace_root())
        .args(arguments)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() || output.stdout.is_empty() || output.stdout.len() > 64 * 1024 {
        return Err(ProtocolError(format!("command failed: {program}")).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

fn file_digest(path: &Path) -> Result<String> {
    Ok(sha256(&read_bounded(path, 256 << 20)?))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    Ok(read_bounded_file(path, maximum, false)?.0)
}

fn read_bounded_file(path: &Path, maximum: u64, allow_empty: bool) -> Result<(Vec<u8>, u32)> {
    crate::reject_symlink_components(path, false)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            ProtocolError(format!(
                "failed to open bounded file {}: {error}",
                path.display()
            ))
        })?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || (!allow_empty && metadata.len() == 0)
        || metadata.len() > maximum
    {
        return Err(ProtocolError(format!("invalid bounded file: {}", path.display())).into());
    }
    let mode = metadata.permissions().mode() & 0o7777;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != metadata.len()
        || after.len() != metadata.len()
        || after.modified()? != metadata.modified()?
        || after.permissions().mode() & 0o7777 != mode
    {
        return Err(ProtocolError("bounded file changed during read".into()).into());
    }
    Ok((bytes, mode))
}

fn validate_relative(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 4096
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ProtocolError("unsafe vendored relative path".into()).into());
    }
    Ok(())
}

fn unique_temp_root() -> Result<PathBuf> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let suffix = OffsetDateTime::now_utc().unix_timestamp_nanos();
    Ok(env::temp_dir().canonicalize()?.join(format!(
        "kit-m005-w07-{}-{suffix}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    )))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frozen_files(files: &[(&str, &[u8])]) -> BTreeMap<String, String> {
        files
            .iter()
            .map(|(path, bytes)| (path.to_string(), format!("{:x}", Sha256::digest(bytes))))
            .collect()
    }

    fn guardrail_grade(arm: Arm, success: bool) -> TrialGrade {
        TrialGrade {
            schema_version: "2.0".into(),
            kind: "m005_w07_trial_grade".into(),
            unit_id: "unit".into(),
            arm,
            raw_trial_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            projected_top_k: Vec::new(),
            localization_success: success,
            wrong_decoy_success: success,
            downstream_mechanical_success: success,
            freshness_success: success,
            latency_success: success,
            provenance_success: success,
            token_budget_success: success,
            terminal_success: success,
        }
    }

    #[test]
    fn baseline_guardrail_failures_do_not_prevent_pass_but_treatment_breaches_do() {
        let mut grades = vec![guardrail_grade(Arm::L, false)];
        grades.extend(
            [Arm::C, Arm::F, Arm::FS, Arm::FP, Arm::FG, Arm::FH]
                .map(|arm| guardrail_grade(arm, true)),
        );
        assert!(treatment_guardrails_pass(&grades));
        let analyses = [
            RepositoryClass::Small,
            RepositoryClass::Medium,
            RepositoryClass::Large,
        ]
        .map(|repository_class| ClassAnalysis {
            repository_class,
            pairs: 24,
            l_successes: 0,
            c_successes: 24,
            estimate: Some(1.0),
            confidence_level: Some(0.95),
            lower: Some(0.5),
            upper: Some(1.0),
            passed: true,
            error: None,
        });
        assert!(report_passes(&analyses, treatment_guardrails_pass(&grades)));
        grades[1].wrong_decoy_success = false;
        assert!(!report_passes(
            &analyses,
            treatment_guardrails_pass(&grades)
        ));
    }

    #[test]
    fn ed25519_signature_and_chronology_tampering_are_rejected() {
        let root = unique_temp_root().unwrap();
        fs::create_dir(&root).unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        let signature = sign(&key, b"registered message");
        let signature_bytes = (0..signature.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&signature[index..index + 2], 16).unwrap())
            .collect::<Vec<_>>();
        crate::verifier::verify_signature_with_key(
            &key.verifying_key(),
            b"registered message",
            &signature_bytes,
        )
        .unwrap();
        assert!(
            crate::verifier::verify_signature_with_key(
                &key.verifying_key(),
                b"tampered message",
                &signature_bytes,
            )
            .is_err()
        );
        assert!(parse_timestamp("2026-01-01T00:00:00+00:00").is_err());
        assert!(
            parse_timestamp("2026-01-01T00:00:00Z").unwrap()
                < parse_timestamp("2026-01-01T00:00:00.000000001Z").unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_commit_timestamps_are_normalized_before_artifact_validation() {
        assert_eq!(
            normalize_timestamp("2026-07-31T09:30:00+05:30").unwrap(),
            "2026-07-31T04:00:00Z"
        );
        assert_eq!(
            normalize_timestamp("2026-07-30T20:00:00-07:00").unwrap(),
            "2026-07-31T03:00:00Z"
        );

        let git = preregistration_git().unwrap();
        let commit_time = git_output(
            &git,
            &workspace_root(),
            &["log", "-1", "--format=%cI", "--", PREREG_PATH],
        )
        .unwrap();
        parse_timestamp(&normalize_timestamp(&commit_time).unwrap()).unwrap();
    }

    #[test]
    fn checkout_filter_allows_registry_only_files() {
        let root = unique_temp_root().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn retained() {}\n").unwrap();
        fs::write(root.join("unpublished.rs"), "pub fn removed() {}\n").unwrap();

        filter_checkout(
            &root,
            "",
            &frozen_files(&[
                (".cargo_vcs_info.json", b"missing"),
                ("src/lib.rs", b"pub fn retained() {}\n"),
            ]),
        )
        .unwrap();

        assert!(root.join("src/lib.rs").is_file());
        assert!(!root.join(".cargo_vcs_info.json").exists());
        assert!(!root.join("unpublished.rs").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_materialization_adds_registry_only_files_and_replaces_vcs_bytes() {
        let root = unique_temp_root().unwrap();
        let package = root.join("package");
        let checkout = root.join("checkout");
        fs::create_dir_all(package.join("generated")).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        fs::write(package.join("existing.rs"), b"published\n").unwrap();
        fs::write(package.join("generated/test.rs"), b"generated\n").unwrap();
        fs::write(checkout.join("existing.rs"), b"vcs\n").unwrap();
        let files = frozen_files(&[
            ("existing.rs", b"published\n"),
            ("generated/test.rs", b"generated\n"),
        ]);

        materialize_package_files(&package, &checkout, &files, "unit").unwrap();

        assert_eq!(
            fs::read(checkout.join("existing.rs")).unwrap(),
            b"published\n"
        );
        assert_eq!(
            fs::read(checkout.join("generated/test.rs")).unwrap(),
            b"generated\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_filter_removes_unwanted_symlink_without_following_it() {
        let root = unique_temp_root().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn retained() {}\n").unwrap();
        std::os::unix::fs::symlink("src", root.join("unwanted")).unwrap();

        filter_checkout(
            &root,
            "",
            &frozen_files(&[("src/lib.rs", b"pub fn retained() {}\n")]),
        )
        .unwrap();

        assert!(root.join("src/lib.rs").is_file());
        assert!(fs::symlink_metadata(root.join("unwanted")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_filter_regularizes_in_root_wanted_symlink_with_exact_digest_and_mode() {
        let root = unique_temp_root().unwrap();
        let bytes = b"licensed bytes\n";
        fs::create_dir_all(root.join("crates/package")).unwrap();
        fs::write(root.join("LICENSE-APACHE"), bytes).unwrap();
        fs::set_permissions(
            root.join("LICENSE-APACHE"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../../LICENSE-APACHE",
            root.join("crates/package/LICENSE-APACHE"),
        )
        .unwrap();

        let normalized = filter_checkout(
            &root,
            "crates/package",
            &frozen_files(&[("LICENSE-APACHE", bytes)]),
        )
        .unwrap();

        let wanted = root.join("crates/package/LICENSE-APACHE");
        let metadata = fs::symlink_metadata(&wanted).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o640);
        assert_eq!(fs::read(&wanted).unwrap(), bytes);
        assert_eq!(
            normalized,
            [NormalizedSymlink {
                path: "LICENSE-APACHE".into(),
                target_digest: sha256(bytes),
            }]
        );
        assert!(!root.join("LICENSE-APACHE").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_filter_rejects_wanted_symlink_with_wrong_digest() {
        let root = unique_temp_root().unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("actual"), b"actual").unwrap();
        std::os::unix::fs::symlink("actual", root.join("wanted")).unwrap();

        let error = filter_checkout(&root, "", &frozen_files(&[("wanted", b"expected")]))
            .unwrap_err()
            .to_string();

        assert!(error.contains("target digest mismatch"));
        assert!(
            fs::symlink_metadata(root.join("wanted"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_filter_rejects_wanted_symlink_escaping_checkout() {
        let scratch = unique_temp_root().unwrap();
        let root = scratch.join("checkout");
        let outside = scratch.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside, b"expected").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("wanted")).unwrap();

        let error = filter_checkout(&root, "", &frozen_files(&[("wanted", b"expected")]))
            .unwrap_err()
            .to_string();

        assert!(error.contains("escapes the checkout root"));
        assert!(
            fs::symlink_metadata(root.join("wanted"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(scratch).unwrap();
    }

    #[test]
    fn checkout_filter_rejects_wanted_symlink_cycle() {
        let root = unique_temp_root().unwrap();
        fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink("second", root.join("wanted")).unwrap();
        std::os::unix::fs::symlink("wanted", root.join("second")).unwrap();

        let error = filter_checkout(&root, "", &frozen_files(&[("wanted", b"expected")]))
            .unwrap_err()
            .to_string();

        assert!(error.contains("failed to resolve frozen package symlink wanted"));
        assert!(
            fs::symlink_metadata(root.join("wanted"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_filter_rejects_wanted_symlink_to_directory() {
        let root = unique_temp_root().unwrap();
        fs::create_dir_all(root.join("actual")).unwrap();
        std::os::unix::fs::symlink("actual", root.join("wanted")).unwrap();

        let error = filter_checkout(&root, "", &frozen_files(&[("wanted", b"expected")]))
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid bounded file"));
        assert!(
            fs::symlink_metadata(root.join("wanted"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_filter_rejects_symlink_directory_ancestor_with_path() {
        let root = unique_temp_root().unwrap();
        fs::create_dir_all(root.join("actual-src")).unwrap();
        fs::write(root.join("actual-src/lib.rs"), "pub fn actual() {}\n").unwrap();
        std::os::unix::fs::symlink("actual-src", root.join("src")).unwrap();

        let error = filter_checkout(
            &root,
            "",
            &frozen_files(&[("src/lib.rs", b"pub fn actual() {}\n")]),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("src"));
        assert!(error.contains("ancestor of a frozen package file"));
        assert!(
            fs::symlink_metadata(root.join("src"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "explicit non-measured networked materialization-only smoke"]
    fn all_package_vcs_materializations_smoke() {
        let vendor = env::var_os("KIT_M005_W07_SMOKE_VENDOR")
            .map(PathBuf::from)
            .expect("KIT_M005_W07_SMOKE_VENDOR is required")
            .canonicalize()
            .unwrap();
        let root = workspace_root();
        let preregistration: crate::Preregistration = serde_json::from_slice(
            &read_bounded(&root.join(PREREG_PATH), crate::MAX_JSON_BYTES as u64).unwrap(),
        )
        .unwrap();
        let git = pinned_git(&preregistration).unwrap();
        let schedule: ScheduleManifest = serde_json::from_slice(
            &read_bounded(&root.join(MANIFEST_PATH), crate::MAX_JSON_BYTES as u64).unwrap(),
        )
        .unwrap();
        let scratch = unique_temp_root().unwrap();
        let temporary = scratch.join("temporary");
        let run_root = scratch.join("run");
        fs::create_dir(&scratch).unwrap();
        fs::create_dir(&temporary).unwrap();
        fs::create_dir(&run_root).unwrap();

        let (checkouts, _, count) =
            prepare_checkouts(&git, &vendor, &temporary, &run_root, &schedule).unwrap();
        for unit in &schedule.units {
            let destination = temporary.join(format!("smoke-{:02}", unit.schedule_index));
            clone_checkout(
                &git,
                &vendor,
                &checkouts[unit.schedule_index],
                &destination,
                unit,
            )
            .unwrap();
            fs::remove_dir_all(destination).unwrap();
        }
        assert_eq!(
            scratch.parent(),
            Some(env::temp_dir().canonicalize().unwrap().as_path())
        );
        fs::remove_dir_all(&scratch).unwrap();
        assert_eq!(count, crate::UNIT_COUNT);
    }

    #[test]
    fn trusted_state_is_one_way_and_cleanup_keeps_terminal_ledger() {
        let root = unique_temp_root().unwrap();
        fs::create_dir_all(root.join(Path::new(REPORT_PATH).parent().unwrap())).unwrap();
        fs::write(
            root.join(REPORT_PATH),
            serde_json::to_vec(&serde_json::json!({
                "kind": "m005_w07_status_report",
                "status": "NOT_RUN_PRECOMMIT",
                "measured_trials": 0
            }))
            .unwrap(),
        )
        .unwrap();
        require_pristine_not_run(&root).unwrap();
        let run = root.join(RUN_PATH);
        fs::create_dir_all(&run).unwrap();
        assert!(require_pristine_not_run(&root).is_err());
        let entry = SignedLedgerEntry {
            sequence: 1,
            event: LedgerEvent::Report,
            recorded_at: "2026-01-01T00:00:00Z".into(),
            previous_entry_digest: ZERO_DIGEST.into(),
            payload_path: "../retrieval-report.json".into(),
            payload_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            key_id: "ed25519-aaaaaaaaaaaaaaaaaaaaaaaa".into(),
            signature: "aa".repeat(64),
        };
        write_json(
            &run.join("signed-ledger.json"),
            &SignedLedger {
                schema_version: "2.0".into(),
                kind: "m005_w07_public_signed_ledger".into(),
                algorithm: "Ed25519".into(),
                public_key_digest:
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .into(),
                table_rows: vec![LedgerTableRow {
                    sequence: 1,
                    event: LedgerEvent::Report,
                    payload_path: entry.payload_path.clone(),
                    payload_digest: entry.payload_digest.clone(),
                }],
                entries: vec![entry],
            },
        )
        .unwrap();
        assert!(cleanup_failed_run_at(&root).is_err());
        assert!(run.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trusted_git_command_clears_ambient_git_and_path_environment() {
        let git = preregistration_git().unwrap();
        let command = trusted_git_command(&git, &workspace_root()).unwrap();
        let environment = command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect::<BTreeMap<_, _>>();
        for forbidden in [
            "PATH",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_EXEC_PATH",
            "KIT_M005_W07_SIGNING_KEY",
        ] {
            assert!(!environment.contains_key(std::ffi::OsStr::new(forbidden)));
        }
        assert_eq!(
            environment.get(std::ffi::OsStr::new("GIT_ALLOW_PROTOCOL")),
            Some(&std::ffi::OsStr::new("https"))
        );
    }
}
