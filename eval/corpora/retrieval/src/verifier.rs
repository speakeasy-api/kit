use crate::{
    AdmissionRecord, CorpusManifest, GitMaterializationReceipt, LedgerEvent, MeasuredReport,
    MeasuredRuntimeManifest, ProtocolError, RawTrial, RegistrationRecord, Result, SignedLedger,
    TrialBinding, TrialGrade, canonical, grade, sha256,
};
use ed25519_dalek::{Signature, VerifyingKey};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const RUN_PATH: &str = "eval/reports/m005/source-semantics/retrieval-run";
const MAX_LINE: usize = 16 * 1024 * 1024;
const MAX_AGGREGATE: usize = 512 * 1024 * 1024;
const ZERO_DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

pub(crate) fn verify_measured(
    root: &Path,
    vendor: &Path,
    manifest: &CorpusManifest,
    preregistration: &crate::Preregistration,
    public_key: &crate::protocol::PinnedPublicKey,
    report: &MeasuredReport,
) -> Result<()> {
    let vendor = crate::canonicalize_vendor_root(vendor)?;
    crate::run::verify_postcommit_git(root, preregistration)?;
    let run_root = root.join(RUN_PATH);
    let registration: RegistrationRecord =
        read_json(&run_root.join("registration.json"), MAX_LINE)?;
    let runtime: MeasuredRuntimeManifest = read_json(&run_root.join("runtime.json"), MAX_LINE)?;
    let ledger: SignedLedger = read_json(&run_root.join("signed-ledger.json"), MAX_LINE)?;
    let admissions: Vec<AdmissionRecord> = read_jsonl(&run_root.join("admissions.jsonl"), 504)?;
    let raws: Vec<RawTrial> = read_jsonl(&run_root.join("raw-trials.jsonl"), 504)?;
    let grades: Vec<TrialGrade> = read_jsonl(&run_root.join("grades.jsonl"), 504)?;
    let bindings: Vec<TrialBinding> = read_jsonl(&run_root.join("trial-bindings.jsonl"), 504)?;
    let receipt_path = run_root.join("git-materialization.jsonl");
    let receipt_bytes = read_bounded(&receipt_path, MAX_AGGREGATE as u64)?;
    let receipts: Vec<GitMaterializationReceipt> = parse_jsonl(&receipt_bytes, crate::UNIT_COUNT)?;
    if admissions.len() != 504
        || raws.len() != 504
        || grades.len() != 504
        || bindings.len() != 504
        || receipts.len() != crate::UNIT_COUNT
    {
        return Err(
            ProtocolError("measured tables do not contain the fixed 504 trials".into()).into(),
        );
    }
    crate::protocol::validate_schema(
        include_bytes!("../schema/v2/signed-ledger.schema.json"),
        &canonical(&ledger)?,
    )?;
    for raw in &raws {
        crate::protocol::validate_schema(
            include_bytes!("../schema/v2/raw-trial.schema.json"),
            &canonical(raw)?,
        )?;
    }
    for grade in &grades {
        crate::protocol::validate_schema(
            include_bytes!("../schema/v2/grade.schema.json"),
            &canonical(grade)?,
        )?;
    }
    let registration_digest = sha256(&canonical(&registration)?);
    let git = crate::run::pinned_git(preregistration)?;
    let head = crate::run::trusted_git_output(
        &git,
        root,
        &[
            "log",
            "-1",
            "--format=%H",
            "--",
            "eval/preregistration/m005-w07.yaml",
        ],
    )?;
    let head_time = crate::run::trusted_git_output(
        &git,
        root,
        &[
            "log",
            "-1",
            "--format=%cI",
            "--",
            "eval/preregistration/m005-w07.yaml",
        ],
    )?;
    let head_time = crate::run::normalize_timestamp(&head_time)?;
    if registration.preregistration_digest != sha256(&canonical(preregistration)?)
        || registration.corpus_manifest_digest != sha256(&canonical(manifest)?)
        || registration.immutable_inputs_digest
            != sha256(&canonical(&preregistration.immutable_inputs)?)
        || registration.runtime_manifest_digest != sha256(&canonical(&runtime)?)
        || report.registration_digest != registration_digest
        || report.runtime_manifest_digest != registration.runtime_manifest_digest
        || registration.materialization_receipt_digest != sha256(&receipt_bytes)
        || registration.materialization_receipt_count != receipts.len()
        || report.materialization_receipt_digest != registration.materialization_receipt_digest
        || report.materialization_receipt_count != registration.materialization_receipt_count
        || registration.route != crate::ExecutorEvidence::LocalSandboxNotTrusted
        || runtime.route != registration.route
        || report.route != registration.route
        || registration.git_commit_sha != head
        || registration.git_commit_time != head_time
        || crate::run::parse_timestamp(&registration.git_commit_time)?
            >= crate::run::parse_timestamp(&registration.registered_at)?
    {
        return Err(ProtocolError("registration/report immutable binding mismatch".into()).into());
    }
    verify_receipts(&vendor, manifest, preregistration, &receipts)?;
    verify_ledger(
        &ledger,
        public_key,
        &registration,
        &admissions,
        &bindings,
        report,
    )?;

    let scratch = unique_temp()?;
    fs::create_dir(&scratch)?;
    let verification = (|| {
        let mut recomputed = Vec::with_capacity(504);
        for index in 0..504 {
            let admission = &admissions[index];
            let raw = &raws[index];
            let retained_grade = &grades[index];
            let binding = &bindings[index];
            let unit = manifest
                .units
                .iter()
                .find(|unit| unit.unit_id == raw.unit_id)
                .ok_or_else(|| ProtocolError("raw unit is absent from manifest".into()))?;
            if admission.unit_id != raw.unit_id
                || admission.arm != raw.arm
                || admission.sequence_index != index
                || admission.source_digest != unit.rust_source_digest
                || raw.source_digest != unit.rust_source_digest
                || raw.admission_digest != sha256(&canonical(admission)?)
                || raw.worker_executable_digest != runtime.executable_digest
                || crate::run::parse_timestamp(&registration.registered_at)?
                    >= crate::run::parse_timestamp(&admission.admitted_at)?
                || crate::run::parse_timestamp(&admission.admitted_at)?
                    >= crate::run::parse_timestamp(&raw.measured_started_at)?
                || crate::run::parse_timestamp(&raw.measured_started_at)?
                    >= crate::run::parse_timestamp(&raw.measured_ended_at)?
            {
                return Err(ProtocolError("trial chronology/admission mismatch".into()).into());
            }
            let source = scratch.join(format!("source-{index:04}"));
            fs::create_dir(&source)?;
            crate::run::materialize_full(&vendor, &source, unit)?;
            let computed = grade(unit, raw, &source)?;
            fs::remove_dir_all(&source)?;
            if &computed != retained_grade
                || binding.unit_id != raw.unit_id
                || binding.arm != raw.arm
                || binding.raw_trial_digest != sha256(&canonical(raw)?)
                || binding.grade_digest != sha256(&canonical(&computed)?)
            {
                return Err(
                    ProtocolError("raw/grade/trial binding recomputation mismatch".into()).into(),
                );
            }
            recomputed.push(computed);
        }
        let expected = crate::run::measured_report(
            manifest,
            &recomputed,
            preregistration,
            registration_digest,
            report.runtime_manifest_digest.clone(),
            report.materialization_receipt_digest.clone(),
            report.materialization_receipt_count,
            raws.first()
                .expect("fixed table")
                .measured_started_at
                .clone(),
            raws.last().expect("fixed table").measured_ended_at.clone(),
            report.reported_at.clone(),
        )?;
        if &expected != report
            || crate::run::parse_timestamp(&report.measured_ended_at)?
                >= crate::run::parse_timestamp(&report.reported_at)?
        {
            return Err(ProtocolError("measured report recomputation mismatch".into()).into());
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(scratch);
    verification
}

fn verify_receipts(
    vendor: &Path,
    manifest: &CorpusManifest,
    preregistration: &crate::Preregistration,
    receipts: &[GitMaterializationReceipt],
) -> Result<()> {
    for (unit, receipt) in manifest.units.iter().zip(receipts) {
        let package = vendor.join(format!("{}-{}", unit.package.name, unit.package.version));
        let files = crate::protocol::validated_unit_files(&package, unit)?;
        let package_files = files.keys().cloned().collect::<Vec<_>>();
        let rust = files
            .iter()
            .filter(|(path, _)| path.ends_with(".rs"))
            .map(|(path, digest)| (path.clone(), digest.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut normalized_paths = std::collections::BTreeSet::new();
        let valid_normalizations = receipt.normalized_symlinks.iter().all(|normalization| {
            normalized_paths.insert(&normalization.path)
                && files
                    .get(&normalization.path)
                    .is_some_and(|digest| normalization.target_digest == format!("sha256:{digest}"))
        });
        if receipt.unit_id != unit.unit_id
            || receipt.git_path != preregistration.runtime_environment.git_path
            || receipt.git_digest != preregistration.runtime_environment.git_executable_digest
            || receipt.git_version != preregistration.runtime_environment.git
            || receipt.fetch_depth != 100
            || receipt.result != "FETCH_HEAD_DETACHED_EXACT"
            || receipt.repository_url != unit.package.normalized_repository_url
            || receipt.vcs_commit != unit.package.vcs_commit
            || receipt.path_in_vcs != unit.package.path_in_vcs
            || receipt.head != unit.package.vcs_commit
            || receipt.rust_source_digest != unit.rust_source_digest
            || receipt.rust_source_digest != sha256(&canonical(&rust)?)
            || receipt.package_file_set_digest != unit.source_digest
            || receipt.package_file_set_digest != sha256(&canonical(&files)?)
            || receipt.package_files != package_files
            || !valid_normalizations
            || receipt.commands.len() != 4
            || receipt.commands[0].first().map(String::as_str) != Some("init")
            || receipt.commands[1]
                != [
                    "remote".to_owned(),
                    "add".to_owned(),
                    "origin".to_owned(),
                    unit.package.normalized_repository_url.clone(),
                ]
            || receipt.commands[2]
                != [
                    "fetch".to_owned(),
                    "--quiet".to_owned(),
                    "--depth=100".to_owned(),
                    "origin".to_owned(),
                    unit.package.vcs_commit.clone(),
                ]
            || receipt.commands[3]
                != [
                    "checkout".to_owned(),
                    "--quiet".to_owned(),
                    "--detach".to_owned(),
                    "FETCH_HEAD".to_owned(),
                ]
        {
            return Err(ProtocolError("materialization receipt mismatch".into()).into());
        }
    }
    Ok(())
}

fn verify_ledger(
    ledger: &SignedLedger,
    public_key: &crate::protocol::PinnedPublicKey,
    registration: &RegistrationRecord,
    admissions: &[AdmissionRecord],
    bindings: &[TrialBinding],
    report: &MeasuredReport,
) -> Result<()> {
    if ledger.schema_version != "2.0"
        || ledger.kind != "m005_w07_public_signed_ledger"
        || ledger.algorithm != "Ed25519"
        || ledger.entries.len() != 2 + admissions.len() + bindings.len() + 1
        || ledger.entries.len() != ledger.table_rows.len()
        || ledger.public_key_digest != public_key.digest
        || ledger
            .entries
            .iter()
            .any(|entry| entry.key_id != public_key.key_id)
    {
        return Err(ProtocolError("invalid signed ledger cardinality".into()).into());
    }
    let mut previous_digest = ZERO_DIGEST.to_owned();
    let mut previous_time: Option<String> = None;
    for (index, entry) in ledger.entries.iter().enumerate() {
        let row = &ledger.table_rows[index];
        if entry.sequence != index as u64 + 1
            || row.sequence != entry.sequence
            || row.event != entry.event
            || row.payload_path != entry.payload_path
            || row.payload_digest != entry.payload_digest
            || entry.previous_entry_digest != previous_digest
            || previous_time.as_ref().is_some_and(|time| {
                crate::run::parse_timestamp(time).expect("validated previous timestamp")
                    >= crate::run::parse_timestamp(&entry.recorded_at).expect("validated timestamp")
            })
        {
            return Err(ProtocolError("signed ledger chain/table mismatch".into()).into());
        }
        let message = crate::run::SignatureMessage {
            domain: "m005-w07-ledger-entry-v1",
            sequence: entry.sequence,
            event: entry.event,
            recorded_at: &entry.recorded_at,
            previous_entry_digest: &entry.previous_entry_digest,
            payload_path: &entry.payload_path,
            payload_digest: &entry.payload_digest,
            key_id: &entry.key_id,
        };
        verify_signature(&public_key.key, &canonical(&message)?, &entry.signature)?;
        previous_digest = sha256(&canonical(entry)?);
        previous_time = Some(entry.recorded_at.clone());
    }
    let mut expected = Vec::with_capacity(ledger.entries.len());
    expected.push((
        LedgerEvent::Registration,
        "registration.json".to_owned(),
        sha256(&canonical(registration)?),
    ));
    expected.push((
        LedgerEvent::Materialization,
        "git-materialization.jsonl".to_owned(),
        registration.materialization_receipt_digest.clone(),
    ));
    expected.extend(admissions.iter().enumerate().map(|(index, value)| {
        (
            LedgerEvent::Admission,
            format!("admissions.jsonl#{}", index + 1),
            sha256(&canonical(value).expect("bounded admission")),
        )
    }));
    expected.extend(bindings.iter().enumerate().map(|(index, value)| {
        (
            LedgerEvent::Trial,
            format!(
                "trial-bindings.jsonl#{};grades.jsonl#{}",
                index + 1,
                index + 1
            ),
            sha256(&canonical(value).expect("bounded binding")),
        )
    }));
    expected.push((
        LedgerEvent::Report,
        "../retrieval-report.json".to_owned(),
        sha256(&canonical(report)?),
    ));
    if ledger
        .entries
        .iter()
        .map(|entry| {
            (
                entry.event,
                entry.payload_path.clone(),
                entry.payload_digest.clone(),
            )
        })
        .ne(expected)
        || ledger
            .entries
            .first()
            .map(|entry| entry.recorded_at.as_str())
            != Some(registration.registered_at.as_str())
        || ledger.entries.last().is_none_or(|entry| {
            crate::run::parse_timestamp(&entry.recorded_at).expect("validated ledger time")
                <= crate::run::parse_timestamp(&report.reported_at).expect("validated report time")
        })
    {
        return Err(
            ProtocolError("ledger does not exactly reconcile retained tables".into()).into(),
        );
    }
    for (index, admission) in admissions.iter().enumerate() {
        if ledger.entries[index + 2].recorded_at != admission.admitted_at {
            return Err(ProtocolError("signed admission time mismatch".into()).into());
        }
    }
    Ok(())
}

fn verify_signature(public: &VerifyingKey, message: &[u8], signature: &str) -> Result<()> {
    if signature.len() != 128 || !signature.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ProtocolError("invalid Ed25519 signature encoding".into()).into());
    }
    let signature_bytes = (0..signature.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&signature[index..index + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    verify_signature_with_key(public, message, &signature_bytes)
}

pub(crate) fn verify_signature_with_key(
    key: &VerifyingKey,
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    let signature = Signature::from_slice(signature)?;
    key.verify_strict(message, &signature)
        .map_err(|_| ProtocolError("Ed25519 signature verification failed".into()))?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, maximum: usize) -> Result<T> {
    Ok(serde_json::from_slice(&read_bounded(
        path,
        maximum as u64,
    )?)?)
}

fn read_jsonl<T: serde::de::DeserializeOwned>(
    path: &Path,
    maximum_records: usize,
) -> Result<Vec<T>> {
    parse_jsonl(&read_bounded(path, MAX_AGGREGATE as u64)?, maximum_records)
}

fn parse_jsonl<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    maximum_records: usize,
) -> Result<Vec<T>> {
    let mut reader = BufReader::with_capacity(64 * 1024, bytes);
    let mut records = Vec::new();
    let mut aggregate = 0_usize;
    loop {
        let mut line = Vec::new();
        let read = Read::by_ref(&mut reader)
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        aggregate = aggregate
            .checked_add(read)
            .ok_or_else(|| ProtocolError("JSONL aggregate overflow".into()))?;
        if read > MAX_LINE
            || aggregate > MAX_AGGREGATE
            || line.last() != Some(&b'\n')
            || records.len() == maximum_records
        {
            return Err(ProtocolError("JSONL line/record/aggregate bound exceeded".into()).into());
        }
        line.pop();
        if line.is_empty() || line.contains(&b'\n') {
            return Err(ProtocolError("invalid JSONL record framing".into()).into());
        }
        records.push(serde_json::from_slice(&line)?);
    }
    Ok(records)
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    crate::reject_symlink_components(path, false)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > maximum {
        return Err(ProtocolError("bounded JSON file rejected".into()).into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref().take(maximum + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != metadata.len()
        || after.len() != metadata.len()
        || after.modified()? != metadata.modified()?
    {
        return Err(ProtocolError("bounded file changed while read".into()).into());
    }
    Ok(bytes)
}

fn unique_temp() -> Result<PathBuf> {
    Ok(std::env::temp_dir().canonicalize()?.join(format!(
        "kit-m005-verify-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn streamed_jsonl_rejects_record_and_line_bounds() {
        let root = unique_temp().unwrap();
        fs::create_dir(&root).unwrap();
        let path = root.join("rows.jsonl");
        fs::write(
            &path,
            b"{\"task_id\":\"a\",\"query\":\"b\",\"query_digest\":\"c\"}\n",
        )
        .unwrap();
        let rows: Vec<crate::WorkerQuery> = read_jsonl(&path, 1).unwrap();
        assert_eq!(rows.len(), 1);
        fs::write(&path, vec![b'x'; MAX_LINE + 1]).unwrap();
        assert!(read_jsonl::<crate::WorkerQuery>(&path, 1).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parsed_public_key_is_immutable_across_path_replacement() {
        let root = unique_temp().unwrap();
        fs::create_dir(&root).unwrap();
        let path = root.join("public-key.pem");
        fs::write(&path, "original").unwrap();
        let signing = SigningKey::from_bytes(&[9; 32]);
        let pinned = signing.verifying_key();
        let message = b"ledger entry";
        let signature = signing.sign(message).to_bytes();
        fs::write(&path, "replacement").unwrap();
        verify_signature_with_key(&pinned, message, &signature).unwrap();
        assert!(verify_signature_with_key(&pinned, b"tampered", &signature).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn authenticated_jsonl_bytes_cannot_be_split_by_path_replacement() {
        let root = unique_temp().unwrap();
        fs::create_dir(&root).unwrap();
        let path = root.join("rows.jsonl");
        fs::write(
            &path,
            b"{\"task_id\":\"original\",\"query\":\"b\",\"query_digest\":\"c\"}\n",
        )
        .unwrap();
        let authenticated = read_bounded(&path, MAX_AGGREGATE as u64).unwrap();
        let digest = sha256(&authenticated);
        let replacement = root.join("replacement.jsonl");
        fs::write(
            &replacement,
            b"{\"task_id\":\"replacement\",\"query\":\"b\",\"query_digest\":\"c\"}\n",
        )
        .unwrap();
        fs::rename(replacement, &path).unwrap();
        let rows: Vec<crate::WorkerQuery> = parse_jsonl(&authenticated, 1).unwrap();
        assert_eq!(rows[0].task_id, "original");
        assert_eq!(digest, sha256(&authenticated));
        assert_ne!(digest, sha256(&fs::read(&path).unwrap()));
        fs::remove_dir_all(root).unwrap();
    }
}
