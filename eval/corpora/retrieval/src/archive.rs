use crate::{
    MeasuredReport, Preregistration, ProtocolError, RegistrationRecord, Result, canonical, sha256,
};
use ed25519_dalek::{
    VerifyingKey,
    pkcs8::{DecodePublicKey, EncodePublicKey},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    env, fs,
    io::{Cursor, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

const ARCHIVE_PATH: &str = "evidence/m005-w07/v6/manifest.json";
const EXPERIMENT_ID: &str = "m005-w07-rust-registry-v6";
const SOURCE_COMMIT: &str = "c6f00fe6dcd51ccfc6d571708d65da6d8fbb0dab";
const MAX_COMPRESSED: u64 = 50 * 1024 * 1024;
const MAX_DECODED: u64 = 512 * 1024 * 1024;
const MAX_WINDOW_LOG: u32 = 23;
const MAX_MANIFEST: u64 = 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const PATHS: [(&str, &str); 9] = [
    (
        "eval/reports/m005/source-semantics/retrieval-report.json",
        "payload/retrieval-report.json.zst",
    ),
    (
        "eval/reports/m005/source-semantics/retrieval-run/admissions.jsonl",
        "payload/retrieval-run/admissions.jsonl.zst",
    ),
    (
        "eval/reports/m005/source-semantics/retrieval-run/git-materialization.jsonl",
        "payload/retrieval-run/git-materialization.jsonl.zst",
    ),
    (
        "eval/reports/m005/source-semantics/retrieval-run/grades.jsonl",
        "payload/retrieval-run/grades.jsonl.zst",
    ),
    (
        "eval/reports/m005/source-semantics/retrieval-run/raw-trials.jsonl",
        "payload/retrieval-run/raw-trials.jsonl.zst",
    ),
    (
        "eval/reports/m005/source-semantics/retrieval-run/registration.json",
        "payload/retrieval-run/registration.json.zst",
    ),
    (
        "eval/reports/m005/source-semantics/retrieval-run/runtime.json",
        "payload/retrieval-run/runtime.json.zst",
    ),
    (
        "eval/reports/m005/source-semantics/retrieval-run/signed-ledger.json",
        "payload/retrieval-run/signed-ledger.json.zst",
    ),
    (
        "eval/reports/m005/source-semantics/retrieval-run/trial-bindings.jsonl",
        "payload/retrieval-run/trial-bindings.jsonl.zst",
    ),
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArchiveManifest {
    schema_version: String,
    kind: String,
    experiment_id: String,
    source_commit: String,
    preregistration: CanonicalInput,
    corpus_manifest: CanonicalInput,
    public_key: PublicKeyInput,
    compression: Compression,
    aggregate_uncompressed_size: u64,
    aggregate_compressed_size: u64,
    entries: Vec<ArchiveEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CanonicalInput {
    logical_path: String,
    size: u64,
    file_sha256: String,
    canonical_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PublicKeyInput {
    logical_path: String,
    size: u64,
    file_sha256: String,
    subject_public_key_info_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Compression {
    format: String,
    level: i32,
    frames_per_file: usize,
    checksum: bool,
    content_size: bool,
    dictionary: bool,
    decoder_crate: String,
    decoder_version: String,
    decoder_default_features: bool,
    maximum_window_size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArchiveEntry {
    logical_path: String,
    stored_path: String,
    uncompressed_size: u64,
    uncompressed_sha256: String,
    compressed_size: u64,
    compressed_sha256: String,
}

#[derive(Debug)]
struct DecodedSmall {
    report: Vec<u8>,
    registration: Vec<u8>,
}

struct ArchiveContext {
    root: PathBuf,
    path: PathBuf,
    manifest: ArchiveManifest,
    preregistration: Preregistration,
}

pub fn archive_check(path: &Path) -> Result<()> {
    let archive = load_archive(path)?;
    verify_archive(&archive, None)?;
    println!(
        "checked M005-W07 v6 archive structure and bytes; this is not the semantic/full W07 verifier"
    );
    Ok(())
}

pub fn archive_verify(path: &Path, vendor: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(
            ProtocolError("archive-verify requires the pinned macOS environment".into()).into(),
        );
    }
    let archive = load_archive(path)?;
    let root = &archive.root;
    let manifest = &archive.manifest;
    let vendor = crate::canonicalize_vendor_root(vendor)?;
    let git = PathBuf::from(&archive.preregistration.runtime_environment.git_path);
    check_pinned_git(&git, &archive.preregistration)?;

    let temporary_parent = env::temp_dir().canonicalize()?;
    crate::reject_symlink_components(&temporary_parent, false)?;
    let worktree = temporary_parent.join(unique_temporary_name("kit-w07-archive"));
    let added = command(
        &git,
        root,
        [
            "worktree",
            "add",
            "--detach",
            path_text(&worktree)?,
            &manifest.source_commit,
        ],
    )?;
    if !added.status.success() {
        return Err(command_error("detached worktree creation", &added).into());
    }

    let verification = (|| {
        let report = worktree.join(PATHS[0].0);
        let metadata = fs::symlink_metadata(&report)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(
                ProtocolError("historical report overlay target is not regular".into()).into(),
            );
        }
        fs::remove_file(&report)?;
        verify_archive(&archive, Some(&worktree))?;
        let output = command(
            Path::new("cargo"),
            &worktree,
            [
                "run",
                "--locked",
                "--manifest-path",
                "eval/corpora/retrieval/Cargo.toml",
                "--bin",
                "w07-retrieval",
                "--",
                "verify",
                path_text(&vendor)?,
            ],
        )?;
        if !output.status.success() {
            return Err(command_error("historical W07 verifier", &output).into());
        }
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        Ok(())
    })();

    let cleanup = command(
        &git,
        root,
        ["worktree", "remove", "--force", path_text(&worktree)?],
    );
    match (verification, cleanup) {
        (Ok(()), Ok(output)) if output.status.success() => {
            println!("archive and historical full M005-W07 verifier passed");
            Ok(())
        }
        (Err(error), Ok(output)) if output.status.success() => Err(error),
        (Ok(()), Ok(output)) => Err(command_error("detached worktree cleanup", &output).into()),
        (Err(error), Ok(output)) => Err(ProtocolError(format!(
            "historical verification failed: {error}; cleanup failed: {}",
            command_error("detached worktree cleanup", &output)
        ))
        .into()),
        (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(ProtocolError(format!(
            "historical verification failed: {primary}; cleanup failed: {cleanup}"
        ))
        .into()),
    }
}

pub fn evidence_size_check() -> Result<()> {
    let root = workspace_root()?;
    let output = command(Path::new("git"), &root, ["ls-files", "--stage", "-z", "--", "evidence"])?;
    if !output.status.success() {
        return Err(command_error("tracked evidence listing", &output).into());
    }
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record)?;
        let (object, path) = record
            .split_once('\t')
            .ok_or_else(|| ProtocolError("invalid tracked evidence index entry".into()))?;
        let mut fields = object.split_ascii_whitespace();
        let mode = fields.next().unwrap_or_default();
        let object = fields.next().unwrap_or_default();
        let stage = fields.next().unwrap_or_default();
        if !matches!(mode, "100644" | "100755") || stage != "0" || fields.next().is_some() {
            return Err(ProtocolError(format!("tracked evidence is not a regular blob: {path}")).into());
        }
        let kind = git_output(&root, ["cat-file", "-t", object])?;
        let size = git_output(&root, ["cat-file", "-s", object])?.parse::<u64>()?;
        if kind != "blob" || size > MAX_COMPRESSED {
            return Err(ProtocolError(format!(
                "tracked evidence blob exceeds 50 MiB: {path}"
            ))
            .into());
        }
    }

    let output = command(
        Path::new("git"),
        &root,
        ["ls-files", "--others", "--exclude-standard", "-z", "--", "evidence"],
    )?;
    if !output.status.success() {
        return Err(command_error("untracked evidence listing", &output).into());
    }
    for path in output.stdout.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
        let path = std::str::from_utf8(path)?;
        let full = root.join(path);
        crate::reject_symlink_components(&full, false)?;
        let metadata = fs::symlink_metadata(&full)?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_COMPRESSED {
            return Err(ProtocolError(format!(
                "untracked evidence blob is not regular or exceeds 50 MiB: {path}"
            ))
            .into());
        }
    }
    println!("tracked evidence blobs are at most 50 MiB");
    Ok(())
}

fn read_manifest(path: &Path) -> Result<ArchiveManifest> {
    let bytes = read_regular_bounded(path, MAX_MANIFEST)?;
    crate::protocol::validate_schema(
        include_bytes!("../schema/v2/archive-manifest.schema.json"),
        &bytes,
    )?;
    let manifest: ArchiveManifest = serde_json::from_slice(&bytes)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn load_archive(path: &Path) -> Result<ArchiveContext> {
    let root = workspace_root()?;
    let path = exact_manifest_path(&root, path)?;
    let manifest = read_manifest(&path)?;
    check_source_commit(&root, &manifest, &path)?;
    let preregistration = check_source_inputs(&root, &manifest)?;
    Ok(ArchiveContext {
        root,
        path,
        manifest,
        preregistration,
    })
}

fn verify_archive(archive: &ArchiveContext, output_root: Option<&Path>) -> Result<()> {
    let decoded = verify_payload(&archive.manifest, &archive.path, output_root)?;
    check_current_report(&archive.root, &archive.manifest, &decoded.report)
}

fn validate_manifest(manifest: &ArchiveManifest) -> Result<()> {
    let expected_paths = PATHS
        .iter()
        .map(|(logical, stored)| (*logical, *stored))
        .collect::<Vec<_>>();
    let actual_paths = manifest
        .entries
        .iter()
        .map(|entry| (entry.logical_path.as_str(), entry.stored_path.as_str()))
        .collect::<Vec<_>>();
    if manifest.schema_version != "1.0"
        || manifest.kind != "m005_w07_evidence_archive"
        || manifest.experiment_id != EXPERIMENT_ID
        || manifest.source_commit != SOURCE_COMMIT
        || !valid_commit(&manifest.source_commit)
        || manifest.preregistration.logical_path != "eval/preregistration/m005-w07.yaml"
        || manifest.corpus_manifest.logical_path
            != "eval/reports/m005/source-semantics/corpus-manifest.json"
        || manifest.public_key.logical_path != "eval/corpora/retrieval/public-key.pem"
        || manifest.compression.format != "zstd"
        || manifest.compression.level != 19
        || manifest.compression.frames_per_file != 1
        || !manifest.compression.checksum
        || !manifest.compression.content_size
        || manifest.compression.dictionary
        || manifest.compression.decoder_crate != "zstd"
        || manifest.compression.decoder_version != "0.13.3"
        || manifest.compression.decoder_default_features
        || manifest.compression.maximum_window_size != 1_u64 << MAX_WINDOW_LOG
        || actual_paths != expected_paths
        || manifest.entries.len() != PATHS.len()
        || manifest.entries.iter().any(|entry| {
            entry.uncompressed_size == 0
                || entry.compressed_size == 0
                || entry.compressed_size > MAX_COMPRESSED
                || !valid_sha256(&entry.uncompressed_sha256)
                || !valid_sha256(&entry.compressed_sha256)
        })
        || manifest.aggregate_uncompressed_size > MAX_DECODED
        || manifest.aggregate_uncompressed_size
            != manifest
                .entries
                .iter()
                .map(|entry| entry.uncompressed_size)
                .sum::<u64>()
        || manifest.aggregate_compressed_size
            != manifest
                .entries
                .iter()
                .map(|entry| entry.compressed_size)
                .sum::<u64>()
        || !input_digest_valid(&manifest.preregistration)
        || !input_digest_valid(&manifest.corpus_manifest)
        || manifest.public_key.size == 0
        || !valid_sha256(&manifest.public_key.file_sha256)
        || !valid_sha256(&manifest.public_key.subject_public_key_info_sha256)
    {
        return Err(ProtocolError("invalid M005-W07 archive manifest".into()).into());
    }
    Ok(())
}

fn verify_payload(
    manifest: &ArchiveManifest,
    manifest_path: &Path,
    output_root: Option<&Path>,
) -> Result<DecodedSmall> {
    let archive_root = manifest_path
        .parent()
        .ok_or_else(|| ProtocolError("archive manifest has no parent".into()))?;
    check_payload_tree(archive_root, manifest)?;
    let mut aggregate = 0_u64;
    let mut report = Vec::new();
    let mut registration = Vec::new();
    for entry in &manifest.entries {
        let capture = match entry.logical_path.as_str() {
            "eval/reports/m005/source-semantics/retrieval-report.json" => Some(&mut report),
            "eval/reports/m005/source-semantics/retrieval-run/registration.json" => {
                Some(&mut registration)
            }
            _ => None,
        };
        decode_entry(archive_root, entry, output_root, capture, &mut aggregate)?;
    }
    if aggregate != manifest.aggregate_uncompressed_size {
        return Err(ProtocolError("archive decoded aggregate size mismatch".into()).into());
    }
    let decoded = DecodedSmall {
        report,
        registration,
    };
    check_decoded_identities(
        manifest,
        &decoded,
    )?;
    Ok(decoded)
}

fn decode_entry(
    archive_root: &Path,
    entry: &ArchiveEntry,
    output_root: Option<&Path>,
    mut capture: Option<&mut Vec<u8>>,
    aggregate: &mut u64,
) -> Result<()> {
    let compressed_path = archive_root.join(&entry.stored_path);
    let compressed = read_regular_exact(&compressed_path, entry.compressed_size, MAX_COMPRESSED)?;
    if sha256(&compressed) != entry.compressed_sha256 {
        return Err(
            ProtocolError(format!("compressed digest mismatch: {}", entry.stored_path)).into(),
        );
    }
    check_frame(&compressed, entry.uncompressed_size)?;
    let frame_size = zstd::zstd_safe::find_frame_compressed_size(&compressed).map_err(|code| {
        ProtocolError(format!(
            "invalid zstd frame: {}",
            zstd::zstd_safe::get_error_name(code)
        ))
    })?;
    if frame_size != compressed.len() {
        return Err(ProtocolError("zstd trailing or concatenated data rejected".into()).into());
    }

    let mut decoder = zstd::stream::read::Decoder::new(Cursor::new(&compressed))?.single_frame();
    decoder.window_log_max(MAX_WINDOW_LOG)?;
    let mut output = output_root
        .map(|root| create_output(root, &entry.logical_path))
        .transpose()?;
    let mut digest = Sha256::new();
    let mut privacy = PrivacyScan::default();
    let mut decoded = 0_u64;
    let mut buffer =
        vec![0_u8; zstd::stream::read::Decoder::<Cursor<&[u8]>>::recommended_output_size()];
    loop {
        let count = decoder.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        decoded = decoded
            .checked_add(count as u64)
            .ok_or_else(|| ProtocolError("decoded size overflow".into()))?;
        *aggregate = aggregate
            .checked_add(count as u64)
            .ok_or_else(|| ProtocolError("decoded aggregate overflow".into()))?;
        if decoded > entry.uncompressed_size || *aggregate > MAX_DECODED {
            return Err(ProtocolError("decoded archive bound exceeded".into()).into());
        }
        let bytes = &buffer[..count];
        privacy.scan(bytes)?;
        digest.update(bytes);
        if let Some(capture) = capture.as_deref_mut() {
            if capture.len() + count > crate::MAX_JSON_BYTES {
                return Err(
                    ProtocolError("decoded identity file exceeds JSON bound".into()).into(),
                );
            }
            capture.extend_from_slice(bytes);
        }
        if let Some(file) = &mut output {
            file.write_all(bytes)?;
        }
    }
    if decoded != entry.uncompressed_size
        || format!("sha256:{:x}", digest.finalize()) != entry.uncompressed_sha256
    {
        return Err(ProtocolError(format!(
            "decoded size or digest mismatch: {}",
            entry.logical_path
        ))
        .into());
    }
    if let Some(file) = output {
        file.sync_all()?;
    }
    Ok(())
}

fn check_frame(compressed: &[u8], expected_size: u64) -> Result<()> {
    if compressed.len() < 6 || compressed[..4] != [0x28, 0xb5, 0x2f, 0xfd] {
        return Err(ProtocolError("not a standard zstd frame".into()).into());
    }
    let descriptor = compressed[4];
    if descriptor & 0b0001_1000 != 0 || descriptor & 0b11 != 0 || descriptor & 0b100 == 0 {
        return Err(
            ProtocolError("zstd frame must use checksum and no dictionary ID".into()).into(),
        );
    }
    let content_size = zstd::zstd_safe::get_frame_content_size(compressed)
        .map_err(|_| ProtocolError("invalid zstd frame content size".into()))?;
    if content_size != Some(expected_size) {
        return Err(ProtocolError("zstd frame content size mismatch".into()).into());
    }
    let single_segment = descriptor & 0b0010_0000 != 0;
    let window_size = if single_segment {
        expected_size
    } else {
        let descriptor = compressed[5];
        let base = 1_u64 << ((descriptor >> 3) as u32 + 10);
        base + (base / 8) * u64::from(descriptor & 7)
    };
    if window_size > 1_u64 << MAX_WINDOW_LOG {
        return Err(ProtocolError("zstd frame window exceeds 8 MiB".into()).into());
    }
    Ok(())
}

fn check_decoded_identities(manifest: &ArchiveManifest, decoded: &DecodedSmall) -> Result<()> {
    let report: MeasuredReport = serde_json::from_slice(&decoded.report)?;
    let registration: RegistrationRecord = serde_json::from_slice(&decoded.registration)?;
    if report.schema_version != "2.0"
        || report.kind != "m005_w07_measured_report"
        || report.experiment_id != manifest.experiment_id
        || registration.schema_version != "2.0"
        || registration.kind != "m005_w07_registration"
        || registration.experiment_id != manifest.experiment_id
        || registration.git_commit_sha != manifest.source_commit
        || registration.preregistration_digest != manifest.preregistration.canonical_sha256
        || registration.corpus_manifest_digest != manifest.corpus_manifest.canonical_sha256
        || report.preregistration_digest != manifest.preregistration.canonical_sha256
        || report.corpus_manifest_digest != manifest.corpus_manifest.canonical_sha256
        || report.registration_digest != sha256(&canonical(&registration)?)
    {
        return Err(ProtocolError("archive report/registration identity mismatch".into()).into());
    }
    Ok(())
}

fn check_current_report(root: &Path, manifest: &ArchiveManifest, archived: &[u8]) -> Result<()> {
    let report = &manifest.entries[0];
    let current = read_regular_exact(
        &root.join(&report.logical_path),
        report.uncompressed_size,
        crate::MAX_JSON_BYTES as u64,
    )?;
    if current != archived || sha256(&current) != report.uncompressed_sha256 {
        return Err(ProtocolError("current measured report differs from archive".into()).into());
    }
    Ok(())
}

fn check_source_inputs(root: &Path, manifest: &ArchiveManifest) -> Result<Preregistration> {
    let preregistration = read_git_blob_exact(
        root,
        &manifest.source_commit,
        &manifest.preregistration.logical_path,
        manifest.preregistration.size,
        crate::MAX_JSON_BYTES as u64,
    )?;
    check_canonical_input(&preregistration, &manifest.preregistration)?;
    let corpus = read_git_blob_exact(
        root,
        &manifest.source_commit,
        &manifest.corpus_manifest.logical_path,
        manifest.corpus_manifest.size,
        crate::MAX_JSON_BYTES as u64,
    )?;
    check_canonical_input(&corpus, &manifest.corpus_manifest)?;
    let public_key = read_git_blob_exact(
        root,
        &manifest.source_commit,
        &manifest.public_key.logical_path,
        manifest.public_key.size,
        64 * 1024,
    )?;
    check_public_key(&public_key, &manifest.public_key)?;
    Ok(serde_json::from_slice(&preregistration)?)
}

fn check_canonical_input(bytes: &[u8], input: &CanonicalInput) -> Result<()> {
    let canonical_digest = match input.logical_path.as_str() {
        "eval/preregistration/m005-w07.yaml" => sha256(&canonical(&serde_json::from_slice::<
            Preregistration,
        >(bytes)?)?),
        "eval/reports/m005/source-semantics/corpus-manifest.json" => sha256(&canonical(
            &serde_json::from_slice::<crate::CorpusManifest>(bytes)?,
        )?),
        _ => return Err(ProtocolError("unexpected canonical archive input path".into()).into()),
    };
    if sha256(bytes) != input.file_sha256 || canonical_digest != input.canonical_sha256 {
        return Err(ProtocolError(format!(
            "archived input identity mismatch: {}",
            input.logical_path
        ))
        .into());
    }
    Ok(())
}

fn check_public_key(bytes: &[u8], input: &PublicKeyInput) -> Result<()> {
    let pem = std::str::from_utf8(bytes)?;
    let key = VerifyingKey::from_public_key_pem(pem)?;
    let spki = key.to_public_key_der()?;
    if sha256(bytes) != input.file_sha256
        || sha256(spki.as_bytes()) != input.subject_public_key_info_sha256
    {
        return Err(ProtocolError("public key digest mismatch".into()).into());
    }
    Ok(())
}

fn check_source_commit(
    root: &Path,
    manifest: &ArchiveManifest,
    manifest_path: &Path,
) -> Result<()> {
    let resolved = git_output(
        root,
        [
            "rev-parse",
            &format!("{}^{{commit}}", manifest.source_commit),
        ],
    )?;
    if resolved != manifest.source_commit {
        return Err(ProtocolError("archive source commit does not resolve exactly".into()).into());
    }
    git_success(
        root,
        [
            "merge-base",
            "--is-ancestor",
            &manifest.source_commit,
            "HEAD",
        ],
        "archive source commit is not an ancestor of HEAD",
    )?;

    if let Some(base) = env::var_os("GOVERNANCE_BASE_REF").filter(|base| !base.is_empty()) {
        let base = base
            .into_string()
            .map_err(|_| ProtocolError("non-UTF-8 governance base ref".into()))?;
        let base = resolve_commit(root, &base)?;
        git_success(
            root,
            [
                "merge-base",
                "--is-ancestor",
                &manifest.source_commit,
                &base,
            ],
            "archive source commit is not an ancestor of governance base",
        )?;
        let relative = manifest_path
            .strip_prefix(root)?
            .to_str()
            .ok_or_else(|| ProtocolError("non-UTF-8 archive path".into()))?;
        let candidate = env::var("GOVERNANCE_CANDIDATE_SHA").unwrap_or_else(|_| "HEAD".into());
        check_append_only(root, relative, &base, &candidate)?;
    }
    Ok(())
}

fn check_append_only(root: &Path, manifest_path: &str, base: &str, candidate: &str) -> Result<()> {
    let base = resolve_commit(root, base)?;
    let candidate = resolve_commit(root, candidate)?;
    if git_tree(root, &base, manifest_path)?.is_empty() {
        return Ok(());
    }
    let archive_dir = Path::new(manifest_path)
        .parent()
        .ok_or_else(|| ProtocolError("archive path has no parent".into()))?
        .to_str()
        .ok_or_else(|| ProtocolError("non-UTF-8 archive directory".into()))?;
    let base_tree = git_tree(root, &base, archive_dir)?;
    let candidate_tree = git_tree(root, &candidate, archive_dir)?;
    if base_tree != candidate_tree {
        return Err(ProtocolError(
            "existing evidence archive is append-only; add a new archive".into(),
        )
        .into());
    }
    Ok(())
}

fn resolve_commit(root: &Path, reference: &str) -> Result<String> {
    git_output(
        root,
        [
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
}

fn git_tree(root: &Path, commit: &str, path: &str) -> Result<Vec<u8>> {
    let output = command(
        Path::new("git"),
        root,
        ["ls-tree", "-r", "-z", commit, "--", path],
    )?;
    if !output.status.success() {
        return Err(command_error("archive tree listing", &output).into());
    }
    Ok(output.stdout)
}

fn check_pinned_git(path: &Path, preregistration: &Preregistration) -> Result<()> {
    let bytes = read_regular_bounded(path, MAX_COMPRESSED)?;
    if sha256(&bytes) != preregistration.runtime_environment.git_executable_digest {
        return Err(ProtocolError("pinned Apple Git executable digest mismatch".into()).into());
    }
    let output = command(path, Path::new("/"), ["--version"])?;
    if !output.status.success()
        || String::from_utf8(output.stdout)?.trim() != preregistration.runtime_environment.git
    {
        return Err(ProtocolError("pinned Apple Git version mismatch".into()).into());
    }
    Ok(())
}

fn check_payload_tree(root: &Path, manifest: &ArchiveManifest) -> Result<()> {
    let payload = root.join("payload");
    crate::reject_symlink_components(&payload, false)?;
    let metadata = fs::symlink_metadata(&payload)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(ProtocolError("archive payload is not a regular directory".into()).into());
    }
    let expected = manifest
        .entries
        .iter()
        .map(|entry| entry.stored_path.clone())
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    collect_payload(root, &payload, &mut actual)?;
    if actual != expected {
        return Err(ProtocolError("archive payload contains missing or extra files".into()).into());
    }
    Ok(())
}

fn collect_payload(root: &Path, directory: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(ProtocolError("symlink in archive payload".into()).into());
        }
        if metadata.file_type().is_dir() {
            if path != root.join("payload/retrieval-run") {
                return Err(ProtocolError("extra archive payload directory".into()).into());
            }
            collect_payload(root, &path, files)?;
        } else if metadata.file_type().is_file() {
            files.insert(path.strip_prefix(root)?.to_string_lossy().into_owned());
        } else {
            return Err(ProtocolError("non-regular archive payload entry".into()).into());
        }
    }
    Ok(())
}

fn create_output(root: &Path, logical: &str) -> Result<fs::File> {
    let path = root.join(logical);
    let parent = path
        .parent()
        .ok_or_else(|| ProtocolError("decoded path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    crate::reject_symlink_components(parent, false)?;
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn exact_manifest_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    crate::reject_symlink_components(&path, false)?;
    let path = path.canonicalize()?;
    if path != root.join(ARCHIVE_PATH) {
        return Err(
            ProtocolError("archive command requires the exact v6 manifest path".into()).into(),
        );
    }
    Ok(path)
}

fn read_regular_exact(path: &Path, expected: u64, maximum: u64) -> Result<Vec<u8>> {
    let bytes = read_regular_bounded(path, maximum)?;
    if bytes.len() as u64 != expected {
        return Err(ProtocolError(format!("file size mismatch: {}", path.display())).into());
    }
    Ok(bytes)
}

fn read_regular_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    crate::reject_symlink_components(path, false)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let before = file.metadata()?;
    if !before.file_type().is_file() || before.len() == 0 || before.len() > maximum {
        return Err(
            ProtocolError(format!("bounded regular file rejected: {}", path.display())).into(),
        );
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    Read::by_ref(&mut file)
        .take(maximum.checked_add(1).ok_or_else(|| ProtocolError("file bound overflow".into()))?)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != before.len()
        || !after.file_type().is_file()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(ProtocolError("file changed while reading".into()).into());
    }
    Ok(bytes)
}

fn read_git_blob_exact(
    root: &Path,
    commit: &str,
    path: &str,
    expected: u64,
    maximum: u64,
) -> Result<Vec<u8>> {
    if expected == 0 || expected > maximum {
        return Err(ProtocolError(format!("Git blob size rejected: {path}")).into());
    }
    let object = format!("{commit}:{path}");
    let mut child = Command::new("git")
        .args(["--no-replace-objects", "show", "--format=", "--no-ext-diff", &object])
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProtocolError("Git blob stdout unavailable".into()))?;
    let mut bytes = Vec::with_capacity(expected as usize);
    stdout
        .by_ref()
        .take(maximum.checked_add(1).ok_or_else(|| ProtocolError("Git blob bound overflow".into()))?)
        .read_to_end(&mut bytes)?;
    drop(stdout);
    let status = child.wait()?;
    if !status.success() || bytes.len() as u64 != expected {
        return Err(ProtocolError(format!("pinned Git blob read failed: {path}")).into());
    }
    Ok(bytes)
}

fn unique_temporary_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn workspace_root() -> Result<PathBuf> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()?)
}

fn command<'a>(
    executable: &Path,
    current_dir: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
) -> Result<Output> {
    Ok(Command::new(executable)
        .args(arguments)
        .current_dir(current_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()?)
}

fn git_output<'a>(root: &Path, arguments: impl IntoIterator<Item = &'a str>) -> Result<String> {
    let output = command(Path::new("git"), root, arguments)?;
    if !output.status.success() {
        return Err(command_error("git command", &output).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn git_success<'a>(
    root: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    error: &str,
) -> Result<()> {
    let output = command(Path::new("git"), root, arguments)?;
    if !output.status.success() {
        return Err(ProtocolError(error.into()).into());
    }
    Ok(())
}

fn command_error(context: &str, output: &Output) -> ProtocolError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    ProtocolError(format!(
        "{context} failed: {}",
        stderr.lines().next().unwrap_or("no diagnostic")
    ))
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| ProtocolError("non-UTF-8 command path".into()).into())
}

fn input_digest_valid(input: &CanonicalInput) -> bool {
    input.size > 0 && valid_sha256(&input.file_sha256) && valid_sha256(&input.canonical_sha256)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Default)]
struct PrivacyScan {
    tail: Vec<u8>,
}

impl PrivacyScan {
    fn scan(&mut self, bytes: &[u8]) -> Result<()> {
        const FORBIDDEN: [&[u8]; 6] = [
            b"/Users/",
            b"/private/var/folders/",
            b"/var/folders/",
            b"-----BEGIN PRIVATE KEY-----",
            b"-----BEGIN OPENSSH PRIVATE KEY-----",
            b"KIT_M005_W07_SIGNING_KEY=",
        ];
        let mut window = Vec::with_capacity(self.tail.len() + bytes.len());
        window.extend_from_slice(&self.tail);
        window.extend_from_slice(bytes);
        if FORBIDDEN
            .iter()
            .any(|pattern| window.windows(pattern.len()).any(|part| part == *pattern))
        {
            return Err(ProtocolError("decoded archive failed privacy scan".into()).into());
        }
        let retain = FORBIDDEN
            .iter()
            .map(|pattern| pattern.len())
            .max()
            .unwrap_or(1)
            - 1;
        self.tail = window[window.len().saturating_sub(retain)..].to_vec();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(contents: &[u8]) -> (PathBuf, ArchiveManifest) {
        let root = env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(unique_temporary_name("kit-w07-archive-test"));
        fs::create_dir_all(root.join("payload/retrieval-run")).unwrap();
        let mut entries = Vec::new();
        for (logical, stored) in PATHS {
            let path = root.join(stored);
            let compressed = compress(contents, true);
            fs::write(&path, &compressed).unwrap();
            entries.push(ArchiveEntry {
                logical_path: logical.into(),
                stored_path: stored.into(),
                uncompressed_size: contents.len() as u64,
                uncompressed_sha256: sha256(contents),
                compressed_size: compressed.len() as u64,
                compressed_sha256: sha256(&compressed),
            });
        }
        let manifest = ArchiveManifest {
            schema_version: "1.0".into(),
            kind: "m005_w07_evidence_archive".into(),
            experiment_id: EXPERIMENT_ID.into(),
            source_commit: SOURCE_COMMIT.into(),
            preregistration: dummy_input("eval/preregistration/m005-w07.yaml"),
            corpus_manifest: dummy_input("eval/reports/m005/source-semantics/corpus-manifest.json"),
            public_key: PublicKeyInput {
                logical_path: "eval/corpora/retrieval/public-key.pem".into(),
                size: 1,
                file_sha256: digest(),
                subject_public_key_info_sha256: digest(),
            },
            compression: Compression {
                format: "zstd".into(),
                level: 19,
                frames_per_file: 1,
                checksum: true,
                content_size: true,
                dictionary: false,
                decoder_crate: "zstd".into(),
                decoder_version: "0.13.3".into(),
                decoder_default_features: false,
                maximum_window_size: 1 << MAX_WINDOW_LOG,
            },
            aggregate_uncompressed_size: entries.iter().map(|entry| entry.uncompressed_size).sum(),
            aggregate_compressed_size: entries.iter().map(|entry| entry.compressed_size).sum(),
            entries,
        };
        (root, manifest)
    }

    fn dummy_input(path: &str) -> CanonicalInput {
        CanonicalInput {
            logical_path: path.into(),
            size: 1,
            file_sha256: digest(),
            canonical_sha256: digest(),
        }
    }

    fn digest() -> String {
        format!("sha256:{}", "0".repeat(64))
    }

    fn compress(contents: &[u8], checksum: bool) -> Vec<u8> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 19).unwrap();
        encoder.include_checksum(checksum).unwrap();
        encoder.include_contentsize(true).unwrap();
        encoder.include_dictid(false).unwrap();
        encoder.window_log(MAX_WINDOW_LOG).unwrap();
        encoder
            .set_pledged_src_size(Some(contents.len() as u64))
            .unwrap();
        encoder.write_all(contents).unwrap();
        encoder.finish().unwrap()
    }

    fn refresh_entry(root: &Path, manifest: &mut ArchiveManifest, index: usize) {
        let bytes = fs::read(root.join(&manifest.entries[index].stored_path)).unwrap();
        let old = manifest.entries[index].compressed_size;
        manifest.entries[index].compressed_size = bytes.len() as u64;
        manifest.entries[index].compressed_sha256 = sha256(&bytes);
        manifest.aggregate_compressed_size =
            manifest.aggregate_compressed_size - old + bytes.len() as u64;
    }

    fn replace_entry(
        root: &Path,
        manifest: &mut ArchiveManifest,
        index: usize,
        contents: &[u8],
    ) {
        let old_uncompressed = manifest.entries[index].uncompressed_size;
        let compressed = compress(contents, true);
        fs::write(root.join(&manifest.entries[index].stored_path), compressed).unwrap();
        manifest.entries[index].uncompressed_size = contents.len() as u64;
        manifest.entries[index].uncompressed_sha256 = sha256(contents);
        manifest.aggregate_uncompressed_size =
            manifest.aggregate_uncompressed_size - old_uncompressed + contents.len() as u64;
        refresh_entry(root, manifest, index);
    }

    #[test]
    fn rejects_compressed_tamper() {
        let (root, manifest) = fixture(b"{}");
        let path = root.join(&manifest.entries[0].stored_path);
        let mut bytes = fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        fs::write(path, bytes).unwrap();
        assert!(
            verify_payload(&manifest, &root.join("manifest.json"), None)
                .unwrap_err()
                .to_string()
                .contains("compressed digest mismatch")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_trailing_or_concatenated_frame() {
        let (root, mut manifest) = fixture(b"{}");
        let path = root.join(&manifest.entries[0].stored_path);
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(&bytes.clone());
        fs::write(&path, bytes).unwrap();
        refresh_entry(&root, &mut manifest, 0);
        assert!(
            verify_payload(&manifest, &root.join("manifest.json"), None)
                .unwrap_err()
                .to_string()
                .contains("trailing or concatenated")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_missing_frame_checksum() {
        let (root, mut manifest) = fixture(b"{}");
        let path = root.join(&manifest.entries[0].stored_path);
        fs::write(&path, compress(b"{}", false)).unwrap();
        refresh_entry(&root, &mut manifest, 0);
        assert!(
            verify_payload(&manifest, &root.join("manifest.json"), None)
                .unwrap_err()
                .to_string()
                .contains("checksum and no dictionary")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_path_substitution() {
        let (root, mut manifest) = fixture(b"{}");
        manifest.entries[0].logical_path = "eval/reports/m005/other.json".into();
        assert!(validate_manifest(&manifest).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_compressed_oversize() {
        let (root, mut manifest) = fixture(b"{}");
        let old = manifest.entries[0].compressed_size;
        manifest.entries[0].compressed_size = MAX_COMPRESSED + 1;
        manifest.aggregate_compressed_size =
            manifest.aggregate_compressed_size - old + MAX_COMPRESSED + 1;
        assert!(validate_manifest(&manifest).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_privacy_leak() {
        let (root, mut manifest) = fixture(b"{}");
        replace_entry(&root, &mut manifest, 0, b"/Users/private/evidence");
        let error = verify_payload(&manifest, &root.join("manifest.json"), None).unwrap_err();
        assert!(
            error.to_string().contains("privacy scan"),
            "unexpected error: {error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_report_is_mandatory_and_exact() {
        let archived = br#"{"experiment_id":"archive"}"#;
        let (root, manifest) = fixture(archived);
        assert!(check_current_report(&root, &manifest, archived).is_err());

        let report = root.join(&manifest.entries[0].logical_path);
        fs::create_dir_all(report.parent().unwrap()).unwrap();
        fs::write(&report, vec![b'x'; archived.len()]).unwrap();
        assert!(check_current_report(&root, &manifest, archived).is_err());
        fs::write(&report, br#"{"experiment_id":"current"}"#).unwrap();
        assert!(check_current_report(&root, &manifest, archived).is_err());
        fs::write(&report, archived).unwrap();
        check_current_report(&root, &manifest, archived).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_archive_rejects_blob_mutation_from_base() {
        let root = env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(unique_temporary_name("kit-w07-append-test"));
        fs::create_dir(&root).unwrap();
        test_git(&root, &["init", "-q"]);
        fs::write(root.join("README"), b"base").unwrap();
        test_git(&root, &["add", "README"]);
        test_git(&root, &["commit", "-qm", "base"]);
        let base = test_git(&root, &["rev-parse", "HEAD"]);

        fs::create_dir_all(root.join("evidence/m005-w07/v6/payload")).unwrap();
        fs::write(root.join("evidence/m005-w07/v6/manifest.json"), b"manifest").unwrap();
        fs::write(root.join("evidence/m005-w07/v6/payload/report.zst"), b"payload").unwrap();
        test_git(&root, &["add", "evidence"]);
        test_git(&root, &["commit", "-qm", "archive"]);
        let archive = test_git(&root, &["rev-parse", "HEAD"]);
        check_append_only(
            &root,
            "evidence/m005-w07/v6/manifest.json",
            &base,
            &archive,
        )
        .unwrap();

        fs::write(root.join("evidence/m005-w07/v6/payload/report.zst"), b"changed").unwrap();
        test_git(&root, &["add", "evidence"]);
        test_git(&root, &["commit", "-qm", "mutate"]);
        let mutation = test_git(&root, &["rev-parse", "HEAD"]);
        assert!(
            check_append_only(
                &root,
                "evidence/m005-w07/v6/manifest.json",
                &archive,
                &mutation,
            )
            .unwrap_err()
            .to_string()
            .contains("append-only")
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn test_git(root: &Path, arguments: &[&str]) -> String {
        let mut configured = vec![
            "-c",
            "user.name=archive-test",
            "-c",
            "user.email=archive-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ];
        configured.extend_from_slice(arguments);
        let output = command(Path::new("git"), root, configured).unwrap();
        assert!(output.status.success(), "{}", command_error("test Git", &output));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn manifest_denies_unknown_fields() {
        let (root, manifest) = fixture(b"{}");
        let mut value = serde_json::to_value(manifest).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), true.into());
        assert!(serde_json::from_value::<ArchiveManifest>(value).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
