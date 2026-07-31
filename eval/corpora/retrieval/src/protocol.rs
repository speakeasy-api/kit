use crate::{
    AnalysisPlan, Arm, ClassDefinition, CorpusManifest, CorpusUnit, ImmutableInputs,
    MAX_JSON_BYTES, MAX_SNAPSHOT_BYTES, MAX_SOURCE_FILE_BYTES, OraclePin, OracleSelection,
    PackagePin, Preregistration, PriorInvalidExperiment, ProtocolError, PublicReceiptKey,
    ReferenceEdit, RepositoryClass, Result, RuntimeEnvironment, StatusReport, SymbolPin, TaskPin,
    TrialProtocol, UNIT_COUNT, UNITS_PER_CLASS, canonical, sha256, valid_digest,
};
use ed25519_dalek::{
    VerifyingKey,
    pkcs8::{DecodePublicKey, EncodePublicKey},
};
use jsonschema::validator_for;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    io::{Read, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};
use tree_sitter::{Node, Parser};
use url::Url;

const MANIFEST_PATH: &str = "eval/reports/m005/source-semantics/corpus-manifest.json";
const PREREG_PATH: &str = "eval/preregistration/m005-w07.yaml";
const REPORT_PATH: &str = "eval/reports/m005/source-semantics/retrieval-report.json";
const PUBLIC_KEY_PATH: &str = "eval/corpora/retrieval/public-key.pem";
const V2_INCIDENT_PATH: &str =
    "eval/reports/m005/source-semantics/incidents/m005-w07-v2-worker-abort.json";
const V3_INCIDENT_PATH: &str =
    "eval/reports/m005/source-semantics/incidents/m005-w07-v3-sandbox-traversal.json";
const V4_INCIDENT_PATH: &str =
    "eval/reports/m005/source-semantics/incidents/m005-w07-v4-metadata-owner.json";
const RELEASE_EXECUTABLE_PATH: &str = "eval/corpora/retrieval/target/release/w07-retrieval";
const CHECKSUM_FILE: &str = ".cargo-checksum.json";
const VCS_INFO_FILE: &str = ".cargo_vcs_info.json";
const MAX_FILES: usize = 100_000;

#[derive(Deserialize)]
struct Lockfile {
    package: Vec<LockPackage>,
}

#[derive(Deserialize)]
struct LockPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoChecksum {
    files: BTreeMap<String, String>,
    package: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoVcsInfo {
    git: CargoVcsGit,
    path_in_vcs: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoVcsGit {
    sha1: String,
}

#[derive(Clone)]
struct Candidate {
    root: PathBuf,
    package: PackagePin,
    rust_sloc: u64,
    source_file_count: usize,
    source_bytes: u64,
    files: BTreeMap<String, String>,
    source_digest: String,
    rust_source_digest: String,
    checksum_manifest_digest: String,
    symbols: Vec<EligibleSymbol>,
}

#[derive(Clone)]
struct EligibleSymbol {
    pin: SymbolPin,
    docs: String,
    selection_hash: String,
}

pub(crate) struct PinnedPublicKey {
    pub(crate) key: VerifyingKey,
    pub(crate) digest: String,
    pub(crate) key_id: String,
}

pub fn prepare(vendor: &Path) -> Result<()> {
    let vendor = crate::canonicalize_vendor_root(vendor)?;
    let root = workspace_root();
    let mut candidates = discover_candidates(&root, &vendor)
        .map_err(|error| ProtocolError(format!("candidate discovery failed: {error}")))?;
    if candidates.len() < UNIT_COUNT {
        return Err(ProtocolError(format!(
            "only {} repository-unique eligible snapshots; 72 required",
            candidates.len()
        ))
        .into());
    }
    candidates.sort_by(|left, right| {
        (left.rust_sloc, &left.package.name, &left.package.version).cmp(&(
            right.rust_sloc,
            &right.package.name,
            &right.package.version,
        ))
    });

    let large_start = candidates.len() - UNITS_PER_CLASS;
    let selections = [
        (RepositoryClass::Small, &candidates[..UNITS_PER_CLASS]),
        (
            RepositoryClass::Medium,
            &candidates[UNITS_PER_CLASS..UNITS_PER_CLASS * 2],
        ),
        (RepositoryClass::Large, &candidates[large_start..]),
    ];
    let classes = selections
        .iter()
        .map(|(name, selected)| ClassDefinition {
            name: *name,
            minimum_rust_sloc: selected.first().expect("24 entries").rust_sloc,
            maximum_rust_sloc: selected.last().expect("24 entries").rust_sloc,
            units: UNITS_PER_CLASS,
            analysis_unit: "published_crate_source_snapshot".into(),
        })
        .collect::<Vec<_>>();
    let mut units = Vec::with_capacity(UNIT_COUNT);
    for (class, selected) in selections {
        for candidate in selected {
            units.push(freeze_unit(units.len(), class, candidate)?);
        }
    }
    let manifest = CorpusManifest {
        schema_version: "2.0".into(),
        kind: "m005_w07_registry_corpus".into(),
        selection_algorithm: "root Cargo.lock registry packages -> fresh cargo vendor --locked --versioned-dirs -> validate lock/package/file checksums and .cargo_vcs_info.json git sha/path_in_vcs -> normalize upstream URL -> lexicographically first package per URL -> require >=4 documented public Rust symbols -> sort (Rust SLOC,name,version) with deterministic name/version tie break -> first 24, next 24, last 24; require 72 unique upstream repositories after every filter".into(),
        candidate_repository_count: candidates.len(),
        classes,
        units,
    };
    validate_manifest(&manifest)?;
    let manifest_digest = sha256(&canonical(&manifest)?);
    let preregistration = make_preregistration(&root, manifest_digest.clone())
        .map_err(|error| ProtocolError(format!("preregistration pinning failed: {error}")))?;
    validate_preregistration(&preregistration, &manifest)?;
    let preregistration_digest = sha256(&canonical(&preregistration)?);
    let report = not_run_report(&manifest, preregistration_digest, manifest_digest);

    write_json(&root.join(MANIFEST_PATH), &manifest)?;
    write_json(&root.join(PREREG_PATH), &preregistration)?;
    write_json(&root.join(REPORT_PATH), &report)?;
    verify()?;
    println!(
        "prepared {} real snapshots from {} repository-unique eligible candidates",
        manifest.units.len(),
        manifest.candidate_repository_count
    );
    Ok(())
}

pub fn refresh_frozen() -> Result<()> {
    let root = workspace_root();
    let manifest: CorpusManifest = serde_json::from_slice(&read_bounded(
        &root.join(MANIFEST_PATH),
        MAX_JSON_BYTES as u64,
    )?)?;
    validate_manifest(&manifest)?;
    let manifest_digest = sha256(&canonical(&manifest)?);
    let preregistration = make_preregistration(&root, manifest_digest.clone())?;
    validate_preregistration(&preregistration, &manifest)?;
    let preregistration_digest = sha256(&canonical(&preregistration)?);
    let report = not_run_report(&manifest, preregistration_digest, manifest_digest);
    write_json(&root.join(PREREG_PATH), &preregistration)?;
    write_json(&root.join(REPORT_PATH), &report)?;
    verify()?;
    Ok(())
}

pub fn verify() -> Result<()> {
    verify_with_vendor(None)
}

pub fn verify_with_vendor(vendor: Option<&Path>) -> Result<()> {
    let root = workspace_root();
    let manifest_bytes = read_bounded(&root.join(MANIFEST_PATH), MAX_JSON_BYTES as u64)?;
    let preregistration_bytes = read_bounded(&root.join(PREREG_PATH), MAX_JSON_BYTES as u64)?;
    let report_bytes = read_bounded(&root.join(REPORT_PATH), 64 * 1024)?;
    validate_schema(
        include_bytes!("../schema/v2/corpus-manifest.schema.json"),
        &manifest_bytes,
    )?;
    validate_schema(
        include_bytes!("../schema/v2/preregistration.schema.json"),
        &preregistration_bytes,
    )?;
    let report_value: serde_json::Value = serde_json::from_slice(&report_bytes)?;
    validate_schema_definition(include_bytes!("../schema/v2/raw-trial.schema.json"))?;
    validate_schema_definition(include_bytes!("../schema/v2/grade.schema.json"))?;
    validate_schema_definition(include_bytes!("../schema/v2/measured-report.schema.json"))?;
    validate_schema_definition(include_bytes!("../schema/v2/blocked-report.schema.json"))?;
    validate_schema_definition(include_bytes!("../schema/v2/signed-ledger.schema.json"))?;
    for path in [V2_INCIDENT_PATH, V3_INCIDENT_PATH, V4_INCIDENT_PATH] {
        validate_schema(
            include_bytes!("../schema/v2/worker-abort-incident.schema.json"),
            &read_bounded(&root.join(path), 64 * 1024)?,
        )?;
    }
    serde_json::from_slice::<crate::PartialRunIncident>(&read_bounded(
        &root.join(V4_INCIDENT_PATH),
        64 * 1024,
    )?)?;
    let manifest: CorpusManifest = serde_json::from_slice(&manifest_bytes)?;
    let preregistration: Preregistration = serde_json::from_slice(&preregistration_bytes)?;
    validate_manifest(&manifest)?;
    validate_preregistration(&preregistration, &manifest)?;
    let public_key = load_public_key(&root, &preregistration)?;
    let manifest_digest = sha256(&canonical(&manifest)?);
    let preregistration_digest = sha256(&canonical(&preregistration)?);
    if report_value.get("kind").and_then(serde_json::Value::as_str)
        == Some("m005_w07_measured_report")
    {
        validate_schema(
            include_bytes!("../schema/v2/measured-report.schema.json"),
            &report_bytes,
        )?;
        let report: crate::MeasuredReport = serde_json::from_slice(&report_bytes)?;
        verify_input_pins_with_key(&root, &preregistration, &public_key)?;
        crate::verifier::verify_measured(
            &root,
            vendor.ok_or_else(|| ProtocolError("measured verify requires VENDOR_DIR".into()))?,
            &manifest,
            &preregistration,
            &public_key,
            &report,
        )?;
        println!("verified measured M005-W07 report: {}", report.status);
        return Ok(());
    }
    if report_value.get("kind").and_then(serde_json::Value::as_str)
        == Some("m005_w07_blocked_report")
    {
        validate_schema(
            include_bytes!("../schema/v2/blocked-report.schema.json"),
            &report_bytes,
        )?;
        let report: crate::BlockedReport = serde_json::from_slice(&report_bytes)?;
        let expected = crate::BlockedReport {
            schema_version: "2.0".into(),
            kind: "m005_w07_blocked_report".into(),
            experiment_id: "m005-w07-rust-registry-v5".into(),
            status: "BLOCKED_G03_G04".into(),
            gate_claim: "NONE_BLOCKED_EXTERNAL".into(),
            measured_trials: 0,
            blocker: "BLOCKED_G03_G04: M005-W07 trusted execution requires the unavailable pinned M004 production isolated adapter and satisfied G04; no trial was measured".into(),
        };
        if report != expected {
            return Err(ProtocolError("blocked report is not exactly derivable".into()).into());
        }
        if root
            .join("eval/reports/m005/source-semantics/retrieval-run")
            .exists()
        {
            return Err(ProtocolError(
                "blocked report cannot coexist with measured run files".into(),
            )
            .into());
        }
        verify_input_pins_with_key(&root, &preregistration, &public_key)?;
        println!("verified frozen M005-W07 report: BLOCKED_G03_G04");
        return Ok(());
    }
    validate_schema(
        include_bytes!("../../../reports/m005/schema/v2/status-report.schema.json"),
        &report_bytes,
    )?;
    let report: StatusReport = serde_json::from_slice(&report_bytes)?;
    if report != not_run_report(&manifest, preregistration_digest, manifest_digest) {
        return Err(ProtocolError("pre-run report is not exactly derivable".into()).into());
    }
    verify_input_pins_with_key(&root, &preregistration, &public_key)?;
    println!("verified frozen M005-W07 preregistration: NOT_RUN_PRECOMMIT");
    Ok(())
}

fn discover_candidates(root: &Path, vendor: &Path) -> Result<Vec<Candidate>> {
    let lock: Lockfile = toml::from_str(&read_text_bounded(&root.join("Cargo.lock"), 8 << 20)?)?;
    let locked = lock
        .package
        .into_iter()
        .filter(|package| {
            package
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with("registry+"))
        })
        .map(|package| ((package.name, package.version), package.checksum))
        .collect::<BTreeMap<_, _>>();
    let mut by_repository = BTreeMap::<String, Candidate>::new();
    for entry in sorted_directories(vendor)? {
        let cargo_toml = read_text_bounded(&entry.join("Cargo.toml"), 2 << 20)?;
        let value: toml::Value = toml::from_str(&cargo_toml)?;
        let package = value
            .get("package")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| ProtocolError("vendored Cargo.toml has no package table".into()))?;
        let name = string_field(package, "name")?;
        let version = string_field(package, "version")?;
        let Some(lock_checksum) = locked
            .get(&(name.clone(), version.clone()))
            .and_then(Clone::clone)
        else {
            continue;
        };
        let repository = package
            .get("repository")
            .and_then(toml::Value::as_str)
            .map(normalize_repository_url)
            .transpose()?;
        let license = package
            .get("license")
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_owned();
        let Some(repository) = repository else {
            continue;
        };
        if license.is_empty() {
            continue;
        }
        let (checksum, files, source_bytes) = validate_cargo_checksum(&entry, &lock_checksum)
            .map_err(|error| {
                ProtocolError(format!(
                    "vendored checksum validation failed for {name}: {error}"
                ))
            })?;
        let Ok(vcs) = cargo_vcs_info(&entry) else {
            continue;
        };
        let rust_sloc = rust_sloc(&entry, files.keys())
            .map_err(|error| ProtocolError(format!("Rust SLOC scan failed for {name}: {error}")))?;
        let source_digest = sha256(&canonical(&files)?);
        let rust_source_digest = rust_source_digest(&files)?;
        let symbols = eligible_symbols(&entry, files.keys(), &source_digest, &name, &version)
            .map_err(|error| ProtocolError(format!("symbol scan failed for {name}: {error}")))?;
        if symbols.len() < 4 {
            continue;
        }
        let candidate = Candidate {
            root: entry,
            package: PackagePin {
                name,
                version,
                normalized_repository_url: repository.clone(),
                vcs_commit: vcs.git.sha1,
                path_in_vcs: vcs.path_in_vcs,
                license,
                cargo_lock_checksum: format!("sha256:{lock_checksum}"),
                registry_source: "crates.io".into(),
            },
            rust_sloc,
            source_file_count: files.len(),
            source_bytes,
            files,
            source_digest,
            rust_source_digest,
            checksum_manifest_digest: checksum,
            symbols,
        };
        let key = (
            candidate.package.name.as_str(),
            candidate.package.version.as_str(),
            candidate.package.cargo_lock_checksum.as_str(),
        );
        match by_repository.get(&repository) {
            Some(existing)
                if (
                    existing.package.name.as_str(),
                    existing.package.version.as_str(),
                    existing.package.cargo_lock_checksum.as_str(),
                ) <= key => {}
            _ => {
                by_repository.insert(repository, candidate);
            }
        }
    }
    Ok(by_repository.into_values().collect())
}

fn validate_cargo_checksum(
    root: &Path,
    lock_checksum: &str,
) -> Result<(String, BTreeMap<String, String>, u64)> {
    let bytes = read_bounded(&root.join(CHECKSUM_FILE), 16 << 20)?;
    let checksum: CargoChecksum = serde_json::from_slice(&bytes)?;
    if checksum.package != lock_checksum
        || checksum.files.is_empty()
        || checksum.files.len() > MAX_FILES
    {
        return Err(
            ProtocolError("Cargo checksum manifest disagrees with Cargo.lock".into()).into(),
        );
    }
    let mut total = 0_u64;
    for (path, expected) in &checksum.files {
        validate_relative_path(path)?;
        let file = read_bounded_allow_empty(&root.join(path), MAX_SOURCE_FILE_BYTES)?;
        total = total
            .checked_add(file.len() as u64)
            .ok_or_else(|| ProtocolError("snapshot byte count overflow".into()))?;
        if total > MAX_SNAPSHOT_BYTES || hex_sha256(&file) != *expected {
            return Err(ProtocolError(format!("checksum mismatch for {path}")).into());
        }
    }
    Ok((sha256(&bytes), checksum.files, total))
}

fn cargo_vcs_info(root: &Path) -> Result<CargoVcsInfo> {
    let info: CargoVcsInfo =
        serde_json::from_slice(&read_bounded(&root.join(VCS_INFO_FILE), 64 * 1024)?)?;
    if info.git.sha1.len() != 40
        || !info.git.sha1.bytes().all(|byte| byte.is_ascii_hexdigit())
        || (!info.path_in_vcs.is_empty() && validate_relative_path(&info.path_in_vcs).is_err())
    {
        return Err(ProtocolError("invalid cargo VCS provenance".into()).into());
    }
    Ok(info)
}

fn rust_source_digest(files: &BTreeMap<String, String>) -> Result<String> {
    let rust = files
        .iter()
        .filter(|(path, _)| path.ends_with(".rs"))
        .collect::<BTreeMap<_, _>>();
    if rust.is_empty() {
        return Err(ProtocolError("snapshot contains no pinned Rust source".into()).into());
    }
    Ok(sha256(&canonical(&rust)?))
}

pub(crate) fn validated_unit_files(
    source: &Path,
    unit: &CorpusUnit,
) -> Result<BTreeMap<String, String>> {
    validated_pin_files(
        source,
        &unit.package.cargo_lock_checksum,
        &unit.checksum_manifest_digest,
        &unit.source_digest,
        unit.source_bytes,
        unit.source_file_count,
        unit.rust_sloc,
    )
}

pub(crate) fn validated_pin_files(
    source: &Path,
    cargo_lock_checksum: &str,
    checksum_manifest_digest: &str,
    source_digest: &str,
    expected_source_bytes: u64,
    expected_file_count: usize,
    expected_rust_sloc: u64,
) -> Result<BTreeMap<String, String>> {
    let lock_checksum = cargo_lock_checksum
        .strip_prefix("sha256:")
        .ok_or_else(|| ProtocolError("invalid unit lock checksum".into()))?;
    let (actual_checksum_digest, files, source_bytes) =
        validate_cargo_checksum(source, lock_checksum)?;
    let actual_rust_sloc = rust_sloc(source, files.keys())?;
    if actual_checksum_digest != checksum_manifest_digest
        || sha256(&canonical(&files)?) != source_digest
        || source_bytes != expected_source_bytes
        || files.len() != expected_file_count
        || actual_rust_sloc != expected_rust_sloc
    {
        return Err(ProtocolError("materialized package disagrees with frozen unit".into()).into());
    }
    Ok(files)
}

fn rust_sloc<'a>(root: &Path, paths: impl Iterator<Item = &'a String>) -> Result<u64> {
    let mut sloc = 0_u64;
    for path in paths.filter(|path| path.ends_with(".rs")) {
        let text = read_text_bounded(&root.join(path), MAX_SOURCE_FILE_BYTES)?;
        let mut block_comment = false;
        for line in text.lines() {
            let mut trimmed = line.trim();
            while !trimmed.is_empty() {
                if block_comment {
                    if let Some(end) = trimmed.find("*/") {
                        block_comment = false;
                        trimmed = trimmed[end + 2..].trim_start();
                    } else {
                        break;
                    }
                } else if trimmed.starts_with("//") {
                    break;
                } else if trimmed.starts_with("/*") {
                    block_comment = true;
                    trimmed = trimmed[2..].trim_start();
                } else {
                    sloc += 1;
                    break;
                }
            }
        }
    }
    Ok(sloc)
}

fn eligible_symbols<'a>(
    root: &Path,
    paths: impl Iterator<Item = &'a String>,
    source_digest: &str,
    package: &str,
    version: &str,
) -> Result<Vec<EligibleSymbol>> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let mut symbols = Vec::new();
    for path in paths.filter(|path| path.ends_with(".rs")) {
        let bytes = read_bounded(&root.join(path), MAX_SOURCE_FILE_BYTES)?;
        let source = std::str::from_utf8(&bytes)?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| ProtocolError(format!("tree-sitter failed for {path}")))?;
        collect_symbols(tree.root_node(), source, path, &mut symbols)?;
    }
    for symbol in &mut symbols {
        symbol.selection_hash = sha256(
            format!(
                "m005-w07-oracle-v2\0{source_digest}\0{package}\0{version}\0{}\0{}\0{}\0{}\0{}",
                symbol.pin.path,
                symbol.pin.start_byte,
                symbol.pin.end_byte,
                symbol.pin.symbol,
                symbol.docs
            )
            .as_bytes(),
        );
    }
    symbols.sort_by(|left, right| {
        (&left.selection_hash, &left.pin.path, left.pin.start_byte).cmp(&(
            &right.selection_hash,
            &right.pin.path,
            right.pin.start_byte,
        ))
    });
    Ok(symbols)
}

fn collect_symbols(
    node: Node<'_>,
    source: &str,
    path: &str,
    output: &mut Vec<EligibleSymbol>,
) -> Result<()> {
    const ITEM_KINDS: &[&str] = &[
        "function_item",
        "struct_item",
        "enum_item",
        "trait_item",
        "type_item",
        "const_item",
        "static_item",
        "mod_item",
    ];
    if ITEM_KINDS.contains(&node.kind()) {
        let item = &source[node.byte_range()];
        let name = node.child_by_field_name("name");
        if let Some(name) = name {
            let prefix = &source[node.start_byte()..name.start_byte()];
            let docs = preceding_docs(source, node.start_position().row);
            if public_prefix(prefix) && !docs.is_empty() {
                let symbol = &source[name.byte_range()];
                output.push(EligibleSymbol {
                    pin: SymbolPin {
                        path: path.to_owned(),
                        symbol: symbol.to_owned(),
                        symbol_kind: node.kind().trim_end_matches("_item").to_owned(),
                        start_byte: node.start_byte(),
                        end_byte: node.end_byte(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        doc_digest: sha256(docs.as_bytes()),
                    },
                    docs,
                    selection_hash: String::new(),
                });
            }
        }
        let _ = item;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(child, source, path, output)?;
    }
    Ok(())
}

fn preceding_docs(source: &str, start_row: usize) -> String {
    let lines = source.lines().collect::<Vec<_>>();
    let mut docs = Vec::new();
    let mut row = start_row;
    while row > 0 {
        row -= 1;
        let line = lines.get(row).map_or("", |line| line.trim());
        if let Some(doc) = line.strip_prefix("///") {
            docs.push(doc.trim().to_owned());
        } else if docs.is_empty() && (line.is_empty() || line.starts_with("#[")) {
            continue;
        } else {
            break;
        }
    }
    docs.reverse();
    docs.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn public_prefix(prefix: &str) -> bool {
    prefix
        .split_whitespace()
        .any(|token| token == "pub" || token.starts_with("pub("))
}

fn freeze_unit(index: usize, class: RepositoryClass, candidate: &Candidate) -> Result<CorpusUnit> {
    let target = candidate
        .symbols
        .first()
        .ok_or_else(|| ProtocolError("eligible symbol set became empty".into()))?;
    let query_text = query_from_docs(&target.docs);
    let query = format!("Locate the public Rust item documented as: \"{query_text}\"");
    let insertion = "#[doc = \"M005-W07 downstream localization check.\"]\n";
    let target_bytes = read_bounded(
        &candidate.root.join(&target.pin.path),
        MAX_SOURCE_FILE_BYTES,
    )?;
    if target.pin.start_byte > target_bytes.len() {
        return Err(ProtocolError("target byte range exceeds source".into()).into());
    }
    let mut edited = target_bytes;
    edited.splice(
        target.pin.start_byte..target.pin.start_byte,
        insertion.bytes(),
    );
    let mut edited_files = candidate.files.clone();
    edited_files.insert(target.pin.path.clone(), hex_sha256(&edited));
    let arm_order = balanced_arm_order(&target.selection_hash, index)?;
    let class_name = match class {
        RepositoryClass::Small => "small",
        RepositoryClass::Medium => "medium",
        RepositoryClass::Large => "large",
    };
    Ok(CorpusUnit {
        schedule_index: index,
        unit_id: format!("rust-{class_name}-{:02}", index % UNITS_PER_CLASS + 1),
        repository_class: class,
        package: candidate.package.clone(),
        rust_sloc: candidate.rust_sloc,
        source_file_count: candidate.source_file_count,
        source_bytes: candidate.source_bytes,
        source_digest: candidate.source_digest.clone(),
        rust_source_digest: candidate.rust_source_digest.clone(),
        checksum_manifest_digest: candidate.checksum_manifest_digest.clone(),
        task: TaskPin {
            task_id: format!("rust-{class_name}-task-{:02}", index % UNITS_PER_CLASS + 1),
            query_digest: sha256(query.as_bytes()),
            query,
        },
        oracle: OraclePin {
            selection_hash: target.selection_hash.clone(),
            target: target.pin.clone(),
            decoys: candidate.symbols[1..4]
                .iter()
                .map(|symbol| symbol.pin.clone())
                .collect(),
            reference_edit: ReferenceEdit {
                operation: "insert_utf8_before_registered_target_after_context_authorization"
                    .into(),
                utf8_text: insertion.into(),
            },
            expected_post_edit_tree_digest: sha256(&canonical(&edited_files)?),
        },
        arm_order,
    })
}

fn balanced_arm_order(hash: &str, schedule_index: usize) -> Result<Vec<Arm>> {
    let mut arms = vec![Arm::L, Arm::C, Arm::F, Arm::FS, Arm::FP, Arm::FG, Arm::FH];
    let first = u8::from_str_radix(&hash[7..9], 16)? as usize;
    let count = arms.len();
    arms.rotate_left((first + schedule_index) % count);
    Ok(arms)
}

fn query_from_docs(docs: &str) -> String {
    let sentence = docs
        .find(['.', '!', '?'])
        .map_or(docs, |index| &docs[..=index]);
    sentence.chars().take(240).collect()
}

fn make_preregistration(root: &Path, corpus_manifest_digest: String) -> Result<Preregistration> {
    let immutable_inputs = immutable_inputs(root)?;
    let git = crate::run::preregistration_git()?;
    let git_path = crate::run::git_path(&git).to_path_buf();
    let uname_path = crate::run::resolve_executable("uname")?;
    let sw_vers_path = crate::run::resolve_executable("sw_vers")?;
    let rustc_path = crate::run::resolve_executable("rustc")?;
    let cargo_path = crate::run::resolve_executable("cargo")?;
    let sandbox_exec_path = Path::new("/usr/bin/sandbox-exec").canonicalize()?;
    let mut runtime_environment = RuntimeEnvironment {
        manifest_digest: String::new(),
        route: crate::ExecutorEvidence::LocalSandboxNotTrusted,
        uname_path: uname_path.to_string_lossy().into_owned(),
        sw_vers_path: sw_vers_path.to_string_lossy().into_owned(),
        rustc_path: rustc_path.to_string_lossy().into_owned(),
        cargo_path: cargo_path.to_string_lossy().into_owned(),
        os: command_output("uname", &["-s"])?,
        architecture: command_output("uname", &["-m"])?,
        os_version: command_output("sw_vers", &["-productVersion"])?,
        rustc: command_output("rustc", &["--version", "--verbose"])?,
        cargo: command_output("cargo", &["--version", "--verbose"])?,
        git: command_output("git", &["--version"])?,
        git_executable_digest: crate::run::git_digest(&git).to_owned(),
        uname_executable_digest: executable_digest("uname", 4 << 20)?,
        sw_vers_executable_digest: executable_digest("sw_vers", 4 << 20)?,
        rustc_executable_digest: executable_digest("rustc", 256 << 20)?,
        cargo_executable_digest: executable_digest("cargo", 256 << 20)?,
        git_path: git_path.to_string_lossy().into_owned(),
        sandbox_exec_path: sandbox_exec_path.to_string_lossy().into_owned(),
        sandbox_exec: crate::run::sandbox_exec_version(&sandbox_exec_path)?,
        sandbox_exec_executable_digest: sha256(&read_bounded(&sandbox_exec_path, 4 << 20)?),
        profile: "release".into(),
        opt_level: "3".into(),
        debug: "false".into(),
        debug_assertions: false,
    };
    runtime_environment.manifest_digest = sha256(&canonical(&runtime_environment)?);
    let public_key_digest = public_key_der_digest(root)?;
    Ok(Preregistration {
        schema_version: "2.0".into(),
        kind: "m005_w07_preregistration".into(),
        experiment_id: "m005-w07-rust-registry-v5".into(),
        state: "PRE_RUN_FROZEN".into(),
        language: "rust".into(),
        corpus_manifest_digest,
        prior_invalid_experiments: prior_invalid_experiments(root)?,
        immutable_inputs,
        runtime_environment,
        public_receipt_key: PublicReceiptKey {
            algorithm: "Ed25519".into(),
            key_id: format!("ed25519-{}", &public_key_digest[7..31]),
            subject_public_key_info_sha256: public_key_digest,
            pem_path: PUBLIC_KEY_PATH.into(),
        },
        oracle_selection: OracleSelection {
            algorithm: "minimum SHA-256 of domain, source digest, package/version, path, exact item byte range, symbol, and normalized existing doc comment; next three hashes are decoys".into(),
            eligibility: "tree-sitter-rust public function/struct/enum/trait/type/const/static/mod item with a contiguous non-empty /// doc comment; snapshot needs at least four".into(),
            query_derivation: "first normalized existing documentation sentence, capped at 240 Unicode scalar values, embedded in a fixed locate-public-item request".into(),
            decoy_count: 3,
            outcome_independent: true,
        },
        trial_protocol: TrialProtocol {
            trusted_executor: "existing M004 production isolated trial executor; only its TrustedContainerHelper/TrustedWindowsComposite routes may produce trusted evidence".into(),
            local_executor: "implemented macOS sandbox-exec deny-default backend is local-only and must label all receipts LOCAL_SANDBOX_NOT_TRUSTED; it may not satisfy G03/G04/G05".into(),
            worker_visible_inputs: vec!["fresh source snapshot".into(), "task query".into(), "arm configuration".into(), "single arm-specific output path".into()],
            worker_forbidden_inputs: vec!["oracle".into(), "corpus manifest".into(), "report".into(), "preregistration".into(), "other arm outputs or caches".into(), "authority/signing environment".into()],
            fresh_process_per_non_oracle_arm: true,
            fresh_cache_per_non_oracle_arm: true,
            oracle_is_grader_only: true,
            syntax_free_arm: "F-S".into(),
            syntax_free_sources: vec!["lexical".into(), "filesystem_metadata".into(), "cargo_metadata_without_source_parse".into(), "git_path_history".into()],
            arm_source_sets: [Arm::L, Arm::C, Arm::F, Arm::FS, Arm::FP, Arm::FG, Arm::FH]
                .into_iter()
                .map(|arm| (arm, crate::ArmConfig::frozen(arm).enabled_sources))
                .collect(),
            source_limits: crate::SourceLimits::FROZEN,
            normalization: crate::ArmConfig::frozen(Arm::L).normalization,
            lexical_context_rule: "lexical candidates are the UTF-8 bounded window of up to 1024 bytes before and after an actual literal lexical match; range and snippet are that fixed lexical context, never the match substring or a syntax-derived item".into(),
            localization_relevance: "a candidate localizes a registered symbol iff it has exact_item semantics and exactly equals the registered item range, or it has lexical_context semantics on the same path and contains the registered declaration start byte; other_context never localizes".into(),
            history_materialization_rule: "before measurement use the exact W06b-selected trusted system Git executable pinned by absolute path, SHA-256, and version for init plus remote plus fetch --depth=100 of the exact .cargo_vcs_info.json 40-hex commit and detach FETCH_HEAD, verify exact HEAD/remote and every VCS-present frozen Rust byte, remove every worktree file outside the frozen package file set, materialize the exact checksum-validated published package snapshot including registry-only files, and retain separately sandboxed genuine .git object metadata; any failure is terminal".into(),
            premeasurement_canary_rule: "PREMEASUREMENT_CANARY: use cargo run --release --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- canary VENDOR_DIR; after all 72 materializations and before signed registration or admission, run all seven arms for root-level unit 0 and all seven arms for the first nested-package unit in frozen balanced order, each with a fresh checkout, cache, input, output, temporary root, and process and the exact production command, release executable, Git pin, and sandbox policy; require exactly 14 exits at zero, schema-valid complete raw output with exact bindings, exactly the enabled source observations in order, no source error or available truncated observation, F-S syntax initialization zero, and history Git implementation SHA-256 equal preregistration, allowing only typed terminal-unavailable sources; capture bounded sanitized source code/message and stderr, require delete and prune success for each arm before the next, record zero measured or admission rows, and fail INVALID_HARNESS with cleanup on any error".into(),
            measured_timestamp_rule: "signed registration and signed admission must precede CLOCK_REALTIME start; each trial stores measured start/end plus monotonic elapsed; otherwise terminal failure".into(),
            raw_observation_rule: "persist every source response candidate/range/snippet/provenance, source start/end, timing, truncation, bounded sanitized variant-specific source error code/message, structural pattern attempt/success counts, and the actual W06b Git executable SHA-256 for successful history observations before deterministic top-k projection; lexical, map, graph, structural, and history errors distinguish time limit, deterministic bound, invalid request, invalid index, and invalid contract as applicable; history API errors are SourceStatus::Error while Git pin mismatch is harness-fatal; structural partial success is available only with successful_pattern_count > 0 and a bounded diagnostic, zero success is Error, and successful empty searches remain available".into(),
            verification_rule: "verify schemas, every immutable preregistered runtime/tool/profile field, the public key, canonical W06b Git path/SHA-256/version, materialization receipt digest/count, the exact manifest-derived ordered 504-trial unit/task/class/arm schedule across admissions/raw/grades/bindings/ledger, raw history implementation pins, and Ed25519 ledger/table reconciliation in Rust; recompute the frozen Rust-tree source digest, reconstruct top-k only from raw observations, grade hidden target/decoys, and compare the complete expected edited tree digest".into(),
            build_rule: "measured run-local and standalone canary require the compile-time Cargo marker profile=release, opt-level=3, DEBUG=false, and disabled debug assertions, with a canonical release target path as a secondary check; runtime.json records that marker, exact OS/architecture and rustc/cargo/Git/sandbox executable paths/digests/versions, and the SHA-256 of the actual copied worker executable; workers can read only the exact checksum-validated published package snapshot overlaid on the filtered pinned worktree plus separate exact Git object metadata; a nested package builds a separate repository-root lexical history index and charges that build to index_latency_ms, while root/index/search contain no sibling or unpublished VCS worktree files, and no synthetic history or answer-bearing paths are permitted".into(),
        },
        analysis: analysis_plan(),
        external_blockers: vec!["G03 production executor availability".into(), "G04 prerequisite gate".into(), "BLK-14 production LSP pins".into(), "EXT-15 trusted-model credentials/spend".into()],
    })
}

fn prior_invalid_experiments(root: &Path) -> Result<Vec<PriorInvalidExperiment>> {
    [
        ("m005-w07-rust-registry-v2", V2_INCIDENT_PATH),
        ("m005-w07-rust-registry-v3", V3_INCIDENT_PATH),
        ("m005-w07-rust-registry-v4", V4_INCIDENT_PATH),
    ]
    .into_iter()
    .map(|(experiment_id, incident_path)| {
        let bytes = read_bounded(&root.join(incident_path), 64 * 1024)?;
        validate_schema(
            include_bytes!("../schema/v2/worker-abort-incident.schema.json"),
            &bytes,
        )?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        Ok(PriorInvalidExperiment {
            experiment_id: experiment_id.into(),
            status: "INVALID_HARNESS".into(),
            incident_path: incident_path.into(),
            incident_digest: sha256(&canonical(&value)?),
        })
    })
    .collect()
}

fn analysis_plan() -> AnalysisPlan {
    AnalysisPlan {
        contrast: "C-L".into(),
        estimand: "paired target-localization success risk difference within each preregistered Rust SLOC class".into(),
        k: 10,
        familywise_alpha: 0.05,
        class_alpha: 0.05 / 3.0,
        interval: "exact core finite-sample paired binary interval with Bonferroni alpha allocation; no duplicate statistics implementation".into(),
        acceptance: "PASS iff the C-L lower confidence bound is strictly greater than zero in all three classes and every C/F/F-S/F-P/F-G/F-H treatment-arm guardrail passes; L guardrail failures are permitted and contribute only to the baseline localization outcome".into(),
        stopping: "all 72 units x 7 non-oracle arms; no optional stopping".into(),
        exclusions: Vec::new(),
        replacement: false,
        retry: false,
        missing_and_error: "terminal localization failure".into(),
        guardrails: BTreeMap::from([
            ("downstream_mechanical".into(), "100% in candidate/full treatment arms; relevant retrieved context authorizes the frozen edit at the exact hidden target in a fresh tree, followed by complete expected digest equality".into()),
            ("freshness".into(), "zero stale results".into()),
            ("latency".into(), "index <=10000 ms and query <=3000 ms per terminal trial".into()),
            ("provenance".into(), "100% of projected candidates".into()),
            ("token_budget".into(), "hard 2048 tokens per arm".into()),
            ("wrong_decoy".into(), "100% in candidate/full treatment arms: target localized and no registered decoy localized in reconstructed top-k; not required for L".into()),
        ]),
    }
}

fn immutable_inputs(root: &Path) -> Result<ImmutableInputs> {
    Ok(ImmutableInputs {
        build_inputs: digest_build_inputs(root)?,
        release_executable_digest: release_executable_digest(root)?,
    })
}

fn release_executable_digest(root: &Path) -> Result<String> {
    let path = root.join(RELEASE_EXECUTABLE_PATH);
    crate::reject_symlink_components(&path, false)?;
    if !fs::symlink_metadata(&path)?.file_type().is_file() {
        return Err(ProtocolError(
            "canonical W07 release executable is not a regular file; build --release first".into(),
        )
        .into());
    }
    file_digest(&path, 256 << 20)
}

fn validate_manifest(manifest: &CorpusManifest) -> Result<()> {
    if manifest.schema_version != "2.0"
        || manifest.kind != "m005_w07_registry_corpus"
        || manifest.units.len() != UNIT_COUNT
        || manifest.classes.len() != 3
        || manifest.candidate_repository_count < UNIT_COUNT
    {
        return Err(ProtocolError("invalid corpus identity or cardinality".into()).into());
    }
    let mut repositories = BTreeSet::new();
    let mut packages = BTreeSet::new();
    let mut checksums = BTreeSet::new();
    let mut sources = BTreeSet::new();
    for (index, unit) in manifest.units.iter().enumerate() {
        let class = [
            RepositoryClass::Small,
            RepositoryClass::Medium,
            RepositoryClass::Large,
        ][index / UNITS_PER_CLASS];
        let definition = &manifest.classes[index / UNITS_PER_CLASS];
        if unit.schedule_index != index
            || unit.repository_class != class
            || definition.name != class
            || definition.units != UNITS_PER_CLASS
            || !(definition.minimum_rust_sloc..=definition.maximum_rust_sloc)
                .contains(&unit.rust_sloc)
            || unit.source_file_count == 0
            || unit.source_file_count > MAX_FILES
            || unit.source_bytes == 0
            || unit.source_bytes > MAX_SNAPSHOT_BYTES
            || unit.oracle.decoys.len() != 3
            || unit.arm_order.len() != 7
            || unit
                .arm_order
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != 7
            || !repositories.insert(&unit.package.normalized_repository_url)
            || !packages.insert((&unit.package.name, &unit.package.version))
            || !checksums.insert(&unit.package.cargo_lock_checksum)
            || !sources.insert(&unit.source_digest)
            || unit.package.vcs_commit.len() != 40
            || !unit
                .package
                .vcs_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || (!unit.package.path_in_vcs.is_empty()
                && validate_relative_path(&unit.package.path_in_vcs).is_err())
            || unit.task.query_digest != sha256(unit.task.query.as_bytes())
            || unit.oracle.target.start_byte >= unit.oracle.target.end_byte
            || unit.oracle.reference_edit.operation
                != "insert_utf8_before_registered_target_after_context_authorization"
            || [
                &unit.package.cargo_lock_checksum,
                &unit.source_digest,
                &unit.rust_source_digest,
                &unit.checksum_manifest_digest,
                &unit.task.query_digest,
                &unit.oracle.selection_hash,
                &unit.oracle.expected_post_edit_tree_digest,
            ]
            .into_iter()
            .any(|digest| !valid_digest(digest))
        {
            return Err(ProtocolError(format!("invalid frozen unit at index {index}")).into());
        }
    }
    Ok(())
}

fn validate_preregistration(plan: &Preregistration, manifest: &CorpusManifest) -> Result<()> {
    let mut runtime = plan.runtime_environment.clone();
    let runtime_digest = runtime.manifest_digest.clone();
    runtime.manifest_digest.clear();
    if plan.schema_version != "2.0"
        || plan.kind != "m005_w07_preregistration"
        || plan.experiment_id != "m005-w07-rust-registry-v5"
        || plan.state != "PRE_RUN_FROZEN"
        || plan.corpus_manifest_digest != sha256(&canonical(manifest)?)
        || plan.prior_invalid_experiments != prior_invalid_experiments(&workspace_root())?
        || plan.public_receipt_key.algorithm != "Ed25519"
        || !plan.oracle_selection.outcome_independent
        || plan.analysis.contrast != "C-L"
        || plan.analysis.k != 10
        || plan.analysis.retry
        || plan.analysis.replacement
        || plan.analysis.class_alpha != 0.05 / 3.0
        || !valid_digest(&plan.immutable_inputs.release_executable_digest)
        || !plan.trial_protocol.oracle_is_grader_only
        || !plan.trial_protocol.fresh_process_per_non_oracle_arm
        || !plan.trial_protocol.fresh_cache_per_non_oracle_arm
        || plan.trial_protocol.syntax_free_arm != "F-S"
        || plan.runtime_environment.route != crate::ExecutorEvidence::LocalSandboxNotTrusted
        || plan.runtime_environment.profile != "release"
        || plan.runtime_environment.opt_level != "3"
        || plan.runtime_environment.debug != "false"
        || plan.runtime_environment.debug_assertions
        || [
            &plan.runtime_environment.uname_path,
            &plan.runtime_environment.sw_vers_path,
            &plan.runtime_environment.rustc_path,
            &plan.runtime_environment.cargo_path,
            &plan.runtime_environment.git_path,
            &plan.runtime_environment.sandbox_exec_path,
        ]
        .into_iter()
        .any(|path| !Path::new(path).is_absolute())
        || runtime_digest != sha256(&canonical(&runtime)?)
        || [
            &plan.runtime_environment.git_executable_digest,
            &plan.runtime_environment.rustc_executable_digest,
            &plan.runtime_environment.cargo_executable_digest,
            &plan.runtime_environment.uname_executable_digest,
            &plan.runtime_environment.sw_vers_executable_digest,
            &plan.runtime_environment.sandbox_exec_executable_digest,
        ]
        .into_iter()
        .any(|digest| !valid_digest(digest))
        || plan.trial_protocol.normalization != crate::ArmConfig::frozen(Arm::L).normalization
        || plan.trial_protocol.source_limits != crate::SourceLimits::FROZEN
        || [Arm::L, Arm::C, Arm::F, Arm::FS, Arm::FP, Arm::FG, Arm::FH]
            .into_iter()
            .any(|arm| {
                plan.trial_protocol.arm_source_sets.get(&arm)
                    != Some(&crate::ArmConfig::frozen(arm).enabled_sources)
            })
    {
        return Err(ProtocolError("invalid preregistered protocol semantics".into()).into());
    }
    Ok(())
}

pub(crate) fn verify_input_pins(root: &Path, plan: &Preregistration) -> Result<()> {
    let key = load_public_key(root, plan)?;
    verify_input_pins_with_key(root, plan, &key)
}

fn verify_input_pins_with_key(
    root: &Path,
    plan: &Preregistration,
    key: &PinnedPublicKey,
) -> Result<()> {
    if digest_build_inputs(root)? != plan.immutable_inputs.build_inputs {
        return Err(ProtocolError("immutable code/content pin mismatch".into()).into());
    }
    if key.digest != plan.public_receipt_key.subject_public_key_info_sha256
        || key.key_id != plan.public_receipt_key.key_id
    {
        return Err(ProtocolError("public Ed25519 key pin mismatch".into()).into());
    }
    Ok(())
}

fn load_public_key(root: &Path, plan: &Preregistration) -> Result<PinnedPublicKey> {
    if plan.public_receipt_key.pem_path != PUBLIC_KEY_PATH {
        return Err(ProtocolError("public key path is not frozen".into()).into());
    }
    let pem = read_text_bounded(&root.join(PUBLIC_KEY_PATH), 4096)?;
    let key = VerifyingKey::from_public_key_pem(&pem)?;
    let digest = sha256(key.to_public_key_der()?.as_bytes());
    Ok(PinnedPublicKey {
        key,
        digest,
        key_id: plan.public_receipt_key.key_id.clone(),
    })
}

fn not_run_report(
    manifest: &CorpusManifest,
    preregistration_digest: String,
    corpus_manifest_digest: String,
) -> StatusReport {
    let mut class_counts = BTreeMap::new();
    for unit in &manifest.units {
        *class_counts.entry(unit.repository_class).or_insert(0) += 1;
    }
    StatusReport {
        schema_version: "2.0".into(),
        kind: "m005_w07_status_report".into(),
        experiment_id: "m005-w07-rust-registry-v5".into(),
        status: "NOT_RUN_PRECOMMIT".into(),
        statistical_verdict: None,
        gate_claim: "NONE; C-L and G05 are not claimed".into(),
        chronology: "uncommitted preregistration cannot prove pre-measurement chronology".into(),
        preregistration_digest,
        corpus_manifest_digest,
        units: manifest.units.len(),
        class_counts,
        measured_trials: 0,
        external_blockers: vec!["G03".into(), "G04".into(), "BLK-14".into(), "EXT-15".into()],
    }
}

fn normalize_repository_url(value: &str) -> Result<String> {
    let mut url = Url::parse(value)?;
    url.set_query(None);
    url.set_fragment(None);
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let mut segments = url
        .path_segments()
        .ok_or_else(|| ProtocolError("repository URL is not hierarchical".into()))?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if matches!(host.as_str(), "github.com" | "gitlab.com" | "codeberg.org") && segments.len() >= 2
    {
        segments.truncate(2);
    }
    if let Some(last) = segments.last_mut() {
        *last = last.strip_suffix(".git").unwrap_or(last);
    }
    url.set_path(&format!("/{}", segments.join("/")));
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let normalized = url.as_str().trim_end_matches('/').to_ascii_lowercase();
    if normalized.len() > 2048 || url.scheme() != "https" {
        return Err(ProtocolError("unsupported repository URL".into()).into());
    }
    Ok(normalized)
}

fn public_key_der_digest(root: &Path) -> Result<String> {
    let pem = read_text_bounded(&root.join(PUBLIC_KEY_PATH), 4096)?;
    let key = VerifyingKey::from_public_key_pem(&pem)?;
    Ok(sha256(key.to_public_key_der()?.as_bytes()))
}

pub(crate) fn validate_schema(schema: &[u8], instance: &[u8]) -> Result<()> {
    if schema.len() > 1024 * 1024 || instance.len() > MAX_JSON_BYTES {
        return Err(ProtocolError("schema or instance exceeds bound".into()).into());
    }
    let schema: serde_json::Value = serde_json::from_slice(schema)?;
    let instance: serde_json::Value = serde_json::from_slice(instance)?;
    validator_for(&schema)?
        .validate(&instance)
        .map_err(|error| ProtocolError(format!("schema validation failed: {error}")))?;
    Ok(())
}

fn validate_schema_definition(schema: &[u8]) -> Result<()> {
    if schema.len() > 1024 * 1024 {
        return Err(ProtocolError("schema exceeds bound".into()).into());
    }
    let schema: serde_json::Value = serde_json::from_slice(schema)?;
    validator_for(&schema)?;
    Ok(())
}

fn digest_build_inputs(root: &Path) -> Result<String> {
    let mut pins = BTreeMap::new();
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "build.rs",
        "src",
        "docs",
        "eval",
        "ci/lanes",
    ] {
        let path = root.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(_) => collect_build_inputs(root, &path, &mut pins)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut package_roots = fs::read_dir(root)?.collect::<std::result::Result<Vec<_>, _>>()?;
    package_roots.sort_by_key(fs::DirEntry::file_name);
    for entry in package_roots {
        let path = entry.path();
        if path.is_dir() && path.join("Cargo.toml").is_file() {
            collect_build_inputs(root, &path, &mut pins)?;
        }
    }
    let mut visited = BTreeSet::new();
    collect_path_dependencies(root, &root.join("Cargo.toml"), &mut visited, &mut pins)?;
    Ok(sha256(&canonical(&pins)?))
}

fn collect_path_dependencies(
    root: &Path,
    manifest: &Path,
    visited: &mut BTreeSet<PathBuf>,
    pins: &mut BTreeMap<String, String>,
) -> Result<()> {
    let manifest = manifest.canonicalize()?;
    if !manifest.starts_with(root) || !visited.insert(manifest.clone()) {
        return Ok(());
    }
    let value: toml::Value = toml::from_str(&read_text_bounded(&manifest, 2 << 20)?)?;
    let mut paths = Vec::new();
    collect_toml_paths(&value, &mut paths);
    let parent = manifest
        .parent()
        .ok_or_else(|| ProtocolError("Cargo manifest has no parent".into()))?;
    for relative in paths {
        let package = parent.join(relative).canonicalize()?;
        if !package.starts_with(root) {
            return Err(ProtocolError("path dependency escapes workspace root".into()).into());
        }
        collect_build_inputs(root, &package, pins)?;
        let dependency_manifest = package.join("Cargo.toml");
        collect_path_dependencies(root, &dependency_manifest, visited, pins)?;
        for ancestor in package.ancestors().skip(1).take_while(|path| *path != root) {
            let workspace_manifest = ancestor.join("Cargo.toml");
            if workspace_manifest.is_file() {
                collect_build_inputs(root, &workspace_manifest, pins)?;
            }
        }
    }
    Ok(())
}

fn collect_toml_paths(value: &toml::Value, output: &mut Vec<String>) {
    match value {
        toml::Value::Table(table) => {
            if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
                output.push(path.to_owned());
            }
            for child in table.values() {
                collect_toml_paths(child, output);
            }
        }
        toml::Value::Array(values) => {
            for child in values {
                collect_toml_paths(child, output);
            }
        }
        _ => {}
    }
}

fn collect_build_inputs(
    root: &Path,
    path: &Path,
    pins: &mut BTreeMap<String, String>,
) -> Result<()> {
    let relative = path_string(path.strip_prefix(root)?);
    if build_pin_excluded(&relative) {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(ProtocolError(format!(
            "symlink in build input tree is forbidden: {}",
            path.display()
        ))
        .into());
    }
    if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?.collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            collect_build_inputs(root, &entry.path(), pins)?;
        }
    } else if metadata.is_file() {
        pins.insert(
            relative,
            sha256(&read_bounded_allow_empty(path, MAX_SNAPSHOT_BYTES)?),
        );
        if pins.len() > MAX_FILES {
            return Err(ProtocolError("build input file bound exceeded".into()).into());
        }
    } else {
        return Err(ProtocolError("build input is not a regular file".into()).into());
    }
    Ok(())
}

fn build_pin_excluded(relative: &str) -> bool {
    Path::new(relative).components().any(|component| {
        matches!(
            component,
            Component::Normal(value)
                if matches!(value.to_str(), Some(".git" | "target" | ".kit"))
        )
    }) || relative == PREREG_PATH
        || relative == REPORT_PATH
        || relative == MANIFEST_PATH
        || relative == PUBLIC_KEY_PATH
        || relative.starts_with("eval/reports/m005/source-semantics/retrieval-run/")
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn file_digest(path: &Path, maximum: u64) -> Result<String> {
    Ok(sha256(&read_bounded(path, maximum)?))
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| ProtocolError("output has no parent".into()))?;
    crate::reject_symlink_components(parent, true)?;
    fs::create_dir_all(parent)?;
    crate::reject_symlink_components(path, true)?;
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_JSON_BYTES {
        return Err(ProtocolError("output exceeds JSON bound".into()).into());
    }
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let bytes = read_bounded_allow_empty(path, maximum)?;
    if bytes.is_empty() {
        return Err(ProtocolError(format!("empty bounded file: {}", path.display())).into());
    }
    Ok(bytes)
}

pub(crate) fn read_bounded_allow_empty(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    crate::reject_symlink_components(path, false)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() > maximum {
        return Err(ProtocolError(format!("invalid bounded file: {}", path.display())).into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != metadata.len()
        || after.len() != metadata.len()
        || after.modified()? != metadata.modified()?
    {
        return Err(ProtocolError(format!("file changed while read: {}", path.display())).into());
    }
    Ok(bytes)
}

fn read_text_bounded(path: &Path, maximum: u64) -> Result<String> {
    Ok(String::from_utf8(read_bounded(path, maximum)?)?)
}

fn sorted_directories(root: &Path) -> Result<Vec<PathBuf>> {
    let mut directories = fs::read_dir(root)?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| entry.path())
        })
        .collect::<Vec<_>>();
    directories.sort();
    Ok(directories)
}

fn validate_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 4096
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, Component::Normal(_))
                || component.as_os_str() == OsStr::new(CHECKSUM_FILE)
        })
    {
        return Err(ProtocolError(format!("invalid checksum path: {value}")).into());
    }
    Ok(())
}

fn string_field(table: &toml::map::Map<String, toml::Value>, name: &str) -> Result<String> {
    table
        .get(name)
        .and_then(toml::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .map(str::to_owned)
        .ok_or_else(|| ProtocolError(format!("invalid Cargo package {name}")).into())
}

fn command_output(program: &str, arguments: &[&str]) -> Result<String> {
    if program == "git" {
        let git = crate::run::preregistration_git()?;
        return crate::run::trusted_git_output(&git, &workspace_root(), arguments);
    }
    let executable = crate::run::resolve_executable(program)?;
    let mut command = Command::new(&executable);
    command.arg0(program);
    let output = command.args(arguments).stdin(Stdio::null()).output()?;
    if !output.status.success() || output.stdout.is_empty() || output.stdout.len() > 16 * 1024 {
        return Err(ProtocolError(format!("failed to identify {program}")).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn executable_digest(program: &str, maximum: u64) -> Result<String> {
    let path = crate::run::resolve_executable(program)?;
    file_digest(&path, maximum)
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_urls_are_normalized_to_upstream_identity() {
        assert_eq!(
            normalize_repository_url(
                "https://GitHub.com/wasm-bindgen/wasm-bindgen/tree/main/crates/web-sys.git?x=1"
            )
            .unwrap(),
            "https://github.com/wasm-bindgen/wasm-bindgen"
        );
    }

    #[test]
    fn docs_produce_semantic_query_without_generated_symbol_hint() {
        assert_eq!(
            query_from_docs("Reads one bounded value. More details follow."),
            "Reads one bounded value."
        );
    }

    #[test]
    fn build_pin_covers_included_assets_and_path_dependency_bytes() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-w07-build-pin-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("docs/api")).unwrap();
        fs::create_dir_all(root.join("path-dep/src")).unwrap();
        fs::create_dir_all(root.join("eval/preregistration")).unwrap();
        fs::create_dir_all(root.join("eval/reports/m005/source-semantics")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='root'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "const ASSET: &str = include_str!(\"asset.txt\");\n",
        )
        .unwrap();
        fs::write(root.join("src/asset.txt"), "one").unwrap();
        fs::write(root.join("docs/api/openapi.yaml"), "openapi: one\n").unwrap();
        fs::write(
            root.join("path-dep/Cargo.toml"),
            "[package]\nname='dep'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("path-dep/src/lib.rs"),
            "pub fn value() -> u8 { 1 }\n",
        )
        .unwrap();
        fs::write(root.join(PREREG_PATH), "prereg one\n").unwrap();
        fs::write(root.join(REPORT_PATH), "report one\n").unwrap();
        let original = digest_build_inputs(&root).unwrap();
        fs::write(root.join(PREREG_PATH), "prereg two\n").unwrap();
        fs::write(root.join(REPORT_PATH), "report two\n").unwrap();
        assert_eq!(original, digest_build_inputs(&root).unwrap());
        let release = root.join(RELEASE_EXECUTABLE_PATH);
        fs::create_dir_all(release.parent().unwrap()).unwrap();
        fs::write(&release, "release one").unwrap();
        assert_eq!(original, digest_build_inputs(&root).unwrap());
        fs::rename(&release, release.with_extension("removed")).unwrap();
        assert_eq!(original, digest_build_inputs(&root).unwrap());
        fs::write(root.join("src/asset.txt"), "two").unwrap();
        let asset_changed = digest_build_inputs(&root).unwrap();
        fs::write(root.join("docs/api/openapi.yaml"), "openapi: two\n").unwrap();
        let openapi_changed = digest_build_inputs(&root).unwrap();
        fs::write(
            root.join("path-dep/src/lib.rs"),
            "pub fn value() -> u8 { 2 }\n",
        )
        .unwrap();
        let dependency_changed = digest_build_inputs(&root).unwrap();
        assert_ne!(original, asset_changed);
        assert_ne!(asset_changed, openapi_changed);
        assert_ne!(openapi_changed, dependency_changed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_preregistered_release_digest_fails_semantics() {
        let manifest: CorpusManifest = serde_json::from_slice(include_bytes!(
            "../../../reports/m005/source-semantics/corpus-manifest.json"
        ))
        .unwrap();
        let mut preregistration: Preregistration =
            serde_json::from_slice(include_bytes!("../../../preregistration/m005-w07.yaml"))
                .unwrap();
        validate_preregistration(&preregistration, &manifest).unwrap();
        preregistration.immutable_inputs.release_executable_digest = "sha256:tampered".into();
        assert!(validate_preregistration(&preregistration, &manifest).is_err());
    }
}
