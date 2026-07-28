#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use kit::workspace::edit::{
    ir::{EditLimits, EditOperation, ExecutableMode, RevisionToken},
    normalize::{ModelEditFormat, NormalizationContext, normalize},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_PATH_BYTES: usize = 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GraderBounds {
    pub max_patch_bytes: usize,
    pub max_source_bytes: usize,
    pub max_files: usize,
    pub max_checks: usize,
    pub max_check_bytes: usize,
    pub max_log_bytes: usize,
    pub max_artifact_bytes: usize,
    pub max_memory_bytes: usize,
    pub max_time_millis: u64,
}

impl GraderBounds {
    pub fn validate(&self) -> Result<(), GradeError> {
        if self.max_patch_bytes == 0
            || self.max_source_bytes == 0
            || self.max_files == 0
            || self.max_checks == 0
            || self.max_check_bytes == 0
            || self.max_log_bytes == 0
            || self.max_artifact_bytes == 0
            || self.max_memory_bytes == 0
            || self.max_time_millis == 0
            || self.max_patch_bytes > 16 * 1024 * 1024
            || self.max_source_bytes > 512 * 1024 * 1024
            || self.max_files > 100_000
            || self.max_checks > 10_000
            || self.max_check_bytes > 16 * 1024 * 1024
            || self.max_log_bytes > 16 * 1024 * 1024
            || self.max_artifact_bytes > 512 * 1024 * 1024
            || self.max_memory_bytes > 2 * 1024 * 1024 * 1024
            || self.max_time_millis > 4 * 60 * 60 * 1000
        {
            return Err(GradeError::InvalidBounds);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSnapshot {
    files: BTreeMap<String, Vec<u8>>,
    digest: String,
    bytes: usize,
}

impl SourceSnapshot {
    pub fn new(
        files: impl IntoIterator<Item = (String, Vec<u8>)>,
        bounds: &GraderBounds,
    ) -> Result<Self, GradeError> {
        bounds.validate()?;
        let mut canonical = BTreeMap::new();
        let mut bytes = 0_usize;
        for (path, contents) in files {
            validate_source_path(&path)?;
            bytes = bytes
                .checked_add(path.len())
                .and_then(|value| value.checked_add(contents.len()))
                .ok_or(GradeError::SourceBoundExceeded)?;
            if bytes > bounds.max_source_bytes || canonical.len() >= bounds.max_files {
                return Err(GradeError::SourceBoundExceeded);
            }
            if canonical.insert(path.clone(), contents).is_some() {
                return Err(GradeError::DuplicatePath(path));
            }
        }
        let digest = tree_digest(&canonical);
        Ok(Self {
            files: canonical,
            digest,
            bytes,
        })
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    pub fn files(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.files
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    #[serde(rename = "file_digest")]
    Digest {
        id: String,
        path: String,
        sha256: String,
    },
    #[serde(rename = "file_contains")]
    Contains {
        id: String,
        path: String,
        text: String,
    },
    #[serde(rename = "file_absent")]
    Absent { id: String, path: String },
}

impl Check {
    fn id(&self) -> &str {
        match self {
            Self::Digest { id, .. } | Self::Contains { id, .. } | Self::Absent { id, .. } => id,
        }
    }

    fn path(&self) -> &str {
        match self {
            Self::Digest { path, .. } | Self::Contains { path, .. } | Self::Absent { path, .. } => {
                path
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GradeOutcome {
    Success,
    Failure,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckEvidence {
    pub id: String,
    pub passed: bool,
    pub path: String,
    pub expected: String,
    pub actual: String,
    pub duration_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GradeReport {
    pub schema_version: u16,
    pub outcome: GradeOutcome,
    pub base_tree_digest: String,
    pub patch_digest: String,
    pub final_tree_digest: String,
    pub final_tree_artifact: Vec<u8>,
    pub checks: Vec<CheckEvidence>,
    pub hidden_checks: Vec<CheckSummary>,
    pub hidden: HiddenCheckAggregate,
    pub diagnostic: Option<String>,
    pub timing: GradeTiming,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSummary {
    pub id: String,
    pub passed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HiddenTestManifest {
    pub schema_version: u16,
    pub checks: Vec<Check>,
    pub canaries: Vec<String>,
}

impl HiddenTestManifest {
    pub fn validated_canaries(
        &self,
        bounds: &GraderBounds,
        public_checks: &[Check],
    ) -> Result<Vec<Vec<u8>>, GradeError> {
        if self.schema_version != 1 || self.canaries.is_empty() {
            return Err(GradeError::InvalidHiddenManifest);
        }
        let mut checks = public_checks.to_vec();
        checks.extend_from_slice(&self.checks);
        validate_checks(&checks, bounds)?;
        let mut unique = BTreeSet::new();
        let canaries = self
            .canaries
            .iter()
            .map(|canary| canary.as_bytes().to_vec())
            .collect::<Vec<_>>();
        if canaries.iter().any(|canary| {
            canary.is_empty() || canary.len() > 1024 || !unique.insert(canary.clone())
        }) {
            return Err(GradeError::InvalidHiddenManifest);
        }
        Ok(canaries)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HiddenCheckAggregate {
    pub verdict: GradeOutcome,
    pub count: usize,
    pub digest: String,
}

impl HiddenCheckAggregate {
    fn new(checks: &[CheckSummary]) -> Self {
        Self {
            verdict: if checks.iter().all(|check| check.passed) {
                GradeOutcome::Success
            } else {
                GradeOutcome::Failure
            },
            count: checks.len(),
            digest: sha256(&serde_json::to_vec(checks).expect("check summaries serialize")),
        }
    }

    fn error(checks: &[CheckSummary]) -> Self {
        Self {
            verdict: GradeOutcome::Error,
            count: checks.len(),
            digest: sha256(&serde_json::to_vec(checks).expect("hidden checks serialize")),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GradeTiming {
    pub wall_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GradeMetadata {
    pub schema_version: u16,
    pub outcome: GradeOutcome,
    pub base_tree_digest: String,
    pub patch_digest: String,
    pub final_tree_digest: String,
    pub diagnostic: Option<String>,
    pub timing: GradeTiming,
}

impl From<&GradeReport> for GradeMetadata {
    fn from(report: &GradeReport) -> Self {
        Self {
            schema_version: report.schema_version,
            outcome: report.outcome,
            base_tree_digest: report.base_tree_digest.clone(),
            patch_digest: report.patch_digest.clone(),
            final_tree_digest: report.final_tree_digest.clone(),
            diagnostic: report.diagnostic.clone(),
            timing: report.timing.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedChannel {
    pub class: String,
    pub handle: String,
    pub digest: String,
    pub length: u64,
    pub authentication: String,
}

pub fn channel_authentication(
    key: &[u8],
    class: &str,
    handle: &str,
    digest: &str,
    length: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"kit-core-grader-channel-v1\0");
    hasher.update(key);
    for value in [class, handle, digest] {
        hasher.update([0]);
        hasher.update(value.as_bytes());
    }
    hasher.update([0]);
    hasher.update(length.to_be_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

#[derive(Serialize)]
struct TreeEntry<'a> {
    path: &'a str,
    bytes: &'a [u8],
}

pub fn grade(
    source: &SourceSnapshot,
    patch: &[u8],
    checks: &[Check],
    bounds: &GraderBounds,
) -> Result<GradeReport, GradeError> {
    grade_with_hidden(source, patch, checks, &[], bounds)
}

pub fn grade_with_hidden(
    source: &SourceSnapshot,
    patch: &[u8],
    public_checks: &[Check],
    hidden_checks: &[Check],
    bounds: &GraderBounds,
) -> Result<GradeReport, GradeError> {
    let started = Instant::now();
    bounds.validate()?;
    if patch.len() > bounds.max_patch_bytes
        || source.bytes() > bounds.max_source_bytes
        || public_checks
            .len()
            .checked_add(hidden_checks.len())
            .is_none_or(|count| count > bounds.max_checks)
    {
        return Err(GradeError::ExecutionBoundExceeded);
    }
    let mut checks = Vec::with_capacity(public_checks.len() + hidden_checks.len());
    checks.extend_from_slice(public_checks);
    checks.extend_from_slice(hidden_checks);
    validate_checks(&checks, bounds)?;
    let patch_digest = sha256(patch);
    let mut files = source.files.clone();
    let apply = apply_unified_patch(&mut files, patch, bounds);
    if let Err(error) = apply {
        let policy_failure = matches!(error, PatchError::ProtectedPath(_));
        let hidden_checks = hidden_checks
            .iter()
            .map(|check| CheckSummary {
                id: check.id().to_owned(),
                passed: false,
            })
            .collect::<Vec<_>>();
        return Ok(GradeReport {
            schema_version: 1,
            outcome: if policy_failure {
                GradeOutcome::Failure
            } else {
                GradeOutcome::Error
            },
            base_tree_digest: source.digest.clone(),
            patch_digest,
            final_tree_digest: source.digest.clone(),
            final_tree_artifact: tree_artifact(&source.files),
            checks: Vec::new(),
            hidden: HiddenCheckAggregate::error(&hidden_checks),
            hidden_checks,
            diagnostic: Some(error.to_string()),
            timing: GradeTiming {
                wall_millis: elapsed_millis(started),
            },
        });
    }

    let mut evidence = Vec::with_capacity(checks.len());
    for check in &checks {
        let item = match check {
            Check::Digest { id, path, sha256 } => {
                let actual = files
                    .get(path)
                    .map_or_else(|| "missing".to_owned(), |bytes| self::sha256(bytes));
                CheckEvidence {
                    id: id.clone(),
                    passed: actual == *sha256,
                    path: path.clone(),
                    expected: sha256.clone(),
                    actual,
                    duration_micros: 0,
                }
            }
            Check::Contains { id, path, text } => {
                let actual = files.get(path).map_or("missing", |bytes| {
                    if bytes
                        .windows(text.len())
                        .any(|window| window == text.as_bytes())
                    {
                        "present"
                    } else {
                        "absent"
                    }
                });
                CheckEvidence {
                    id: id.clone(),
                    passed: actual == "present",
                    path: path.clone(),
                    expected: "present".to_owned(),
                    actual: actual.to_owned(),
                    duration_micros: 0,
                }
            }
            Check::Absent { id, path } => {
                let actual = if files.contains_key(path) {
                    "present"
                } else {
                    "absent"
                };
                CheckEvidence {
                    id: id.clone(),
                    passed: actual == "absent",
                    path: path.clone(),
                    expected: "absent".to_owned(),
                    actual: actual.to_owned(),
                    duration_micros: 0,
                }
            }
        };
        evidence.push(item);
    }
    let outcome = if evidence.iter().all(|item| item.passed) {
        GradeOutcome::Success
    } else {
        GradeOutcome::Failure
    };
    let final_tree_artifact = tree_artifact(&files);
    let hidden_checks = evidence[public_checks.len()..]
        .iter()
        .map(|check| CheckSummary {
            id: check.id.clone(),
            passed: check.passed,
        })
        .collect::<Vec<_>>();
    Ok(GradeReport {
        schema_version: 1,
        outcome,
        base_tree_digest: source.digest.clone(),
        patch_digest,
        final_tree_digest: sha256(&final_tree_artifact),
        final_tree_artifact,
        checks: evidence[..public_checks.len()].to_vec(),
        hidden: HiddenCheckAggregate::new(&hidden_checks),
        hidden_checks,
        diagnostic: None,
        timing: GradeTiming {
            wall_millis: elapsed_millis(started),
        },
    })
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub fn validate_checks(checks: &[Check], bounds: &GraderBounds) -> Result<(), GradeError> {
    let mut ids = BTreeSet::new();
    let mut bytes = 0_usize;
    for check in checks {
        if check.id().is_empty()
            || check.id().len() > 128
            || !check
                .id()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            || !ids.insert(check.id())
        {
            return Err(GradeError::InvalidCheck(check.id().to_owned()));
        }
        validate_source_path(check.path())?;
        bytes = bytes
            .checked_add(
                serde_json::to_vec(check)
                    .map_err(|error| GradeError::Serialization(error.to_string()))?
                    .len(),
            )
            .ok_or(GradeError::CheckBoundExceeded)?;
        if bytes > bounds.max_check_bytes {
            return Err(GradeError::CheckBoundExceeded);
        }
        if let Check::Digest { sha256, .. } = check
            && !valid_sha256(sha256)
        {
            return Err(GradeError::InvalidCheck(check.id().to_owned()));
        }
        if let Check::Contains { text, .. } = check
            && text.is_empty()
        {
            return Err(GradeError::InvalidCheck(check.id().to_owned()));
        }
    }
    Ok(())
}

#[derive(Debug)]
enum PatchError {
    InvalidUtf8,
    Malformed(&'static str),
    Parser(String),
    Mismatch(String),
    UnsafePath(String),
    ProtectedPath(String),
    BoundExceeded,
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 => formatter.write_str("patch is not UTF-8"),
            Self::Malformed(detail) => write!(formatter, "malformed patch: {detail}"),
            Self::Parser(detail) => write!(formatter, "malformed patch: {detail}"),
            Self::Mismatch(path) => write!(formatter, "patch does not apply to {path}"),
            Self::UnsafePath(path) => write!(formatter, "unsafe patch path: {path}"),
            Self::ProtectedPath(path) => write!(formatter, "protected grader path: {path}"),
            Self::BoundExceeded => formatter.write_str("patch execution bound exceeded"),
        }
    }
}

fn apply_unified_patch(
    files: &mut BTreeMap<String, Vec<u8>>,
    patch: &[u8],
    bounds: &GraderBounds,
) -> Result<(), PatchError> {
    if patch.is_empty() {
        return Ok(());
    }
    let limits = EditLimits {
        max_operations: bounds.max_patch_bytes.min(10_000),
        max_path_bytes: MAX_PATH_BYTES,
        max_content_bytes: bounds.max_source_bytes,
        max_input_bytes: bounds.max_patch_bytes,
        max_validation_memory_bytes: bounds.max_memory_bytes,
        max_validation_time: std::time::Duration::from_millis(bounds.max_time_millis),
        ..EditLimits::default()
    };
    let revision = RevisionToken::parse(format!("r:{}", "0".repeat(64)))
        .expect("static revision token is valid");
    let mut context = NormalizationContext::new(revision, limits);
    for (path, bytes) in files.iter() {
        context
            .insert_file(path, bytes, false)
            .map_err(|_| PatchError::Mismatch(path.clone()))?;
    }
    let edit = normalize(ModelEditFormat::UnifiedDiff, patch, &context)
        .map_err(|error| PatchError::Parser(error.to_string()))?;
    let mut replacements = BTreeMap::<String, Vec<(usize, usize, Vec<u8>, Vec<u8>)>>::new();
    for operation in edit.operations() {
        match operation.operation() {
            EditOperation::AddFile {
                path,
                content,
                executable,
            } => {
                validate_patch_path(path.as_str())?;
                if *executable || files.insert(path.to_string(), content.render()).is_some() {
                    return Err(PatchError::Malformed("unsupported add-file metadata"));
                }
            }
            EditOperation::DeleteFile { path, .. } => {
                validate_patch_path(path.as_str())?;
                if files.remove(path.as_str()).is_none() {
                    return Err(PatchError::Mismatch(path.to_string()));
                }
            }
            EditOperation::MoveFile { from, to, .. } => {
                validate_patch_path(from.as_str())?;
                validate_patch_path(to.as_str())?;
                return Err(PatchError::Malformed("renames are not supported"));
            }
            EditOperation::ReplaceRange {
                path,
                range,
                expected,
                replacement,
                executable,
                ..
            } => {
                validate_patch_path(path.as_str())?;
                if *executable != ExecutableMode::Preserve {
                    return Err(PatchError::Malformed("mode metadata is not supported"));
                }
                replacements.entry(path.to_string()).or_default().push((
                    usize::try_from(range.start).map_err(|_| PatchError::BoundExceeded)?,
                    usize::try_from(range.end).map_err(|_| PatchError::BoundExceeded)?,
                    expected.render(),
                    replacement.render(),
                ));
            }
        }
    }
    for (path, mut ranges) in replacements {
        let contents = files
            .get_mut(&path)
            .ok_or_else(|| PatchError::Mismatch(path.clone()))?;
        ranges.sort_by_key(|(start, _, _, _)| std::cmp::Reverse(*start));
        for (start, end, expected, replacement) in ranges {
            if contents.get(start..end) != Some(expected.as_slice()) {
                return Err(PatchError::Mismatch(path.clone()));
            }
            contents.splice(start..end, replacement);
        }
    }
    let tree_bytes = files.iter().try_fold(0_usize, |total, (path, contents)| {
        total.checked_add(path.len())?.checked_add(contents.len())
    });
    if files.len() > bounds.max_files
        || tree_bytes.is_none_or(|bytes| bytes > bounds.max_source_bytes)
    {
        return Err(PatchError::BoundExceeded);
    }
    Ok(())
}

fn validate_source_path(path: &str) -> Result<(), GradeError> {
    if unsafe_path(path) {
        return Err(GradeError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn validate_patch_path(path: &str) -> Result<(), PatchError> {
    if unsafe_path(path) {
        return Err(PatchError::UnsafePath(path.to_owned()));
    }
    if path.split('/').any(|component| {
        matches!(
            component,
            ".kit"
                | ".kit-eval"
                | "kit-trusted-input"
                | "hidden-tests"
                | "gold-patch"
                | "acceptance-rules"
                | "harness-config"
        )
    }) {
        return Err(PatchError::ProtectedPath(path.to_owned()));
    }
    Ok(())
}

fn unsafe_path(path: &str) -> bool {
    path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
}

fn tree_artifact(files: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    serde_json::to_vec(
        &files
            .iter()
            .map(|(path, bytes)| TreeEntry { path, bytes })
            .collect::<Vec<_>>(),
    )
    .expect("tree entries are infallibly serializable")
}

fn tree_digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    sha256(&tree_artifact(files))
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Debug, Eq, PartialEq)]
pub enum GradeError {
    InvalidBounds,
    InvalidHiddenManifest,
    SourceBoundExceeded,
    ExecutionBoundExceeded,
    CheckBoundExceeded,
    DuplicatePath(String),
    UnsafePath(String),
    InvalidCheck(String),
    Serialization(String),
}

impl std::fmt::Display for GradeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBounds => formatter.write_str("invalid grader bounds"),
            Self::InvalidHiddenManifest => formatter.write_str("invalid hidden-test manifest"),
            Self::SourceBoundExceeded => formatter.write_str("source snapshot bound exceeded"),
            Self::ExecutionBoundExceeded => formatter.write_str("grader execution bound exceeded"),
            Self::CheckBoundExceeded => formatter.write_str("check declaration bound exceeded"),
            Self::DuplicatePath(path) => write!(formatter, "duplicate source path: {path}"),
            Self::UnsafePath(path) => write!(formatter, "unsafe source path: {path}"),
            Self::InvalidCheck(id) => write!(formatter, "invalid check: {id}"),
            Self::Serialization(detail) => write!(formatter, "serialization failed: {detail}"),
        }
    }
}

impl std::error::Error for GradeError {}
