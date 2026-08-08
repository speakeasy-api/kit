use std::{
    fmt,
    io::{self, Read, Write},
    mem::size_of,
    path::{Component, PathBuf},
    time::{Duration, Instant},
};

use serde::Serialize;
use subtle::ConstantTimeEq;

use crate::{
    store::artifacts::{
        ArtifactClass, ArtifactDigest, ArtifactError, ArtifactMetadata, ArtifactRetention,
        ArtifactStore, StagedArtifact,
    },
    workspace::{
        index::meta::MetadataIndex,
        revision::{
            EntryKind, EpochId, FileReadRange, LimitKind, ManagedWorkspace, RevisionError,
            RevisionId,
        },
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadRange {
    Full,
    Bytes { start: usize, end: usize },
    Lines { start: usize, end: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadRequest {
    pub expected_revision: RevisionId,
    pub path: PathBuf,
    pub range: ReadRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadOptions {
    pub max_read_bytes: usize,
    pub max_inline_bytes: usize,
    pub max_artifact_bytes: usize,
    pub max_result_bytes: usize,
    pub max_time: Duration,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            max_read_bytes: 64 * 1024 * 1024,
            max_inline_bytes: 64 * 1024,
            max_artifact_bytes: 64 * 1024 * 1024 + 4 * 1024,
            max_result_bytes: 256 * 1024,
            max_time: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactContext {
    pub principal: String,
    pub project: String,
    pub retention: ArtifactRetention,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactResolveOptions {
    pub max_envelope_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_time: Duration,
}

impl Default for ArtifactResolveOptions {
    fn default() -> Self {
        Self {
            max_envelope_bytes: 64 * 1024 * 1024 + 4 * 1024,
            max_payload_bytes: 64 * 1024 * 1024,
            max_time: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Utf8,
    Binary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewlineStyle {
    None,
    Lf,
    Crlf,
    Mixed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FileMode {
    pub executable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GapMarker {
    pub byte_start: usize,
    pub byte_end: usize,
    pub omitted_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkspaceArtifactHandle {
    pub id: String,
    pub path: PathBuf,
    pub path_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReadResponse {
    pub revision: RevisionId,
    pub path: PathBuf,
    /// Size of the authoritative source file.
    pub file_bytes: usize,
    /// Returned range coordinates, projected only when `source_offset` is present.
    pub byte_start: usize,
    pub byte_end: usize,
    /// Authoritative source coordinate for a range whose projection changed length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_offset: Option<usize>,
    /// Byte length of the selected authoritative source range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_length: Option<usize>,
    /// BLAKE3 digest of the selected authoritative source range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<String>,
    /// Authoritative whole-file size for a range whose projection changed length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file_size: Option<usize>,
    /// Offset within the independently projected selected range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_offset: Option<usize>,
    /// Total projected length of the selected source range, not the whole source file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_length: Option<usize>,
    /// BLAKE3 digest of the projected selected range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_digest: Option<String>,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
    pub encoding: Encoding,
    pub newline: NewlineStyle,
    pub final_newline: bool,
    pub mode: FileMode,
    pub content: Vec<u8>,
    pub gap: Option<GapMarker>,
    pub artifact: Option<WorkspaceArtifactHandle>,
    pub truncated: bool,
    pub result_bytes: usize,
}

impl ReadResponse {
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Debug)]
pub enum ReadError {
    Revision(RevisionError),
    Artifact(ArtifactError),
    TimeLimit,
    UnsafePath(PathBuf),
    NotIndexed(PathBuf),
    InvalidRange(&'static str),
    InvalidOptions(&'static str),
    ArtifactTooLarge { required: usize, max: usize },
    ArtifactAuthorization,
    Serialization(serde_json::Error),
}

impl From<RevisionError> for ReadError {
    fn from(value: RevisionError) -> Self {
        match value {
            RevisionError::LimitExceeded(LimitKind::Time) => Self::TimeLimit,
            RevisionError::InvalidRange(reason) => Self::InvalidRange(reason),
            value => Self::Revision(value),
        }
    }
}

impl From<ArtifactError> for ReadError {
    fn from(value: ArtifactError) -> Self {
        Self::Artifact(value)
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::Artifact(error) => error.fmt(formatter),
            Self::TimeLimit => formatter.write_str("focused read time limit exceeded"),
            Self::UnsafePath(path) => {
                write!(formatter, "unsafe focused read path: {}", path.display())
            }
            Self::NotIndexed(path) => write!(
                formatter,
                "workspace path is not in the authorized index: {}",
                path.display()
            ),
            Self::InvalidRange(reason) => write!(formatter, "invalid focused read range: {reason}"),
            Self::InvalidOptions(reason) => {
                write!(formatter, "invalid focused read options: {reason}")
            }
            Self::ArtifactTooLarge { required, max } => write!(
                formatter,
                "required focused-read artifact size {required} exceeds bound {max}"
            ),
            Self::ArtifactAuthorization => {
                formatter.write_str("workspace artifact is unavailable for this context")
            }
            Self::Serialization(error) => write!(formatter, "serialize focused read: {error}"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            Self::Artifact(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

pub fn read(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    artifacts: &ArtifactStore,
    context: &ArtifactContext,
    request: &ReadRequest,
    options: &ReadOptions,
) -> Result<ReadResponse, ReadError> {
    read_with_publish_hook_inner(
        workspace,
        index,
        artifacts,
        context,
        request,
        options,
        None,
        None,
        |_| {},
    )
}

pub fn read_projected(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    artifacts: &ArtifactStore,
    context: &ArtifactContext,
    request: &ReadRequest,
    options: &ReadOptions,
    custody: &crate::domain::secret::SecretCustody,
) -> Result<ReadResponse, ReadError> {
    read_projected_with_state(
        workspace,
        index,
        artifacts,
        context,
        request,
        options,
        custody,
        &mut crate::domain::secret::JsonProjectionState::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn read_projected_with_state(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    artifacts: &ArtifactStore,
    context: &ArtifactContext,
    request: &ReadRequest,
    options: &ReadOptions,
    custody: &crate::domain::secret::SecretCustody,
    projection_state: &mut crate::domain::secret::JsonProjectionState,
) -> Result<ReadResponse, ReadError> {
    read_with_publish_hook_inner(
        workspace,
        index,
        artifacts,
        context,
        request,
        options,
        Some(custody),
        Some(projection_state),
        |_| {},
    )
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadPublishPoint {
    ProvisionalSynced,
    VerifiedSynced,
    IssuedSynced,
}

#[doc(hidden)]
pub fn read_with_stage_hook(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    artifacts: &ArtifactStore,
    context: &ArtifactContext,
    request: &ReadRequest,
    options: &ReadOptions,
    after_stage: impl FnOnce(),
) -> Result<ReadResponse, ReadError> {
    let mut after_stage = Some(after_stage);
    read_with_publish_hook_inner(
        workspace,
        index,
        artifacts,
        context,
        request,
        options,
        None,
        None,
        |point| {
            if point == ReadPublishPoint::ProvisionalSynced
                && let Some(after_stage) = after_stage.take()
            {
                after_stage();
            }
        },
    )
}

#[doc(hidden)]
pub fn read_with_publish_hook(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    artifacts: &ArtifactStore,
    context: &ArtifactContext,
    request: &ReadRequest,
    options: &ReadOptions,
    publish_hook: impl FnMut(ReadPublishPoint),
) -> Result<ReadResponse, ReadError> {
    read_with_publish_hook_inner(
        workspace,
        index,
        artifacts,
        context,
        request,
        options,
        None,
        None,
        publish_hook,
    )
}

#[allow(clippy::too_many_arguments)]
fn read_with_publish_hook_inner(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    artifacts: &ArtifactStore,
    context: &ArtifactContext,
    request: &ReadRequest,
    options: &ReadOptions,
    custody: Option<&crate::domain::secret::SecretCustody>,
    mut projection_state: Option<&mut crate::domain::secret::JsonProjectionState>,
    mut publish_hook: impl FnMut(ReadPublishPoint),
) -> Result<ReadResponse, ReadError> {
    let started = Instant::now();
    let deadline = started.checked_add(options.max_time).unwrap_or(started);
    validate_request(request, options, context, deadline)?;
    let mut guard = workspace.stable_read_guard_before(request.expected_revision, deadline)?;
    let revision = guard.validate_before(deadline)?;
    if index.revision() != request.expected_revision {
        return Err(ReadError::NotIndexed(request.path.clone()));
    }
    let entry = index
        .entries()
        .iter()
        .find(|entry| entry.path == request.path && entry.kind == EntryKind::File)
        .ok_or_else(|| ReadError::NotIndexed(request.path.clone()))?;
    let range = match request.range {
        ReadRange::Full => FileReadRange::Full,
        ReadRange::Bytes { start, end } => FileReadRange::Bytes { start, end },
        ReadRange::Lines { start, end } => FileReadRange::Lines { start, end },
    };
    let file =
        guard.read_file_range_before(&request.path, range, options.max_read_bytes, deadline)?;
    check_deadline(deadline)?;
    let source_path = request.path.as_os_str().as_encoded_bytes();
    let display_path = custody.map_or_else(
        || request.path.clone(),
        |custody| {
            projected_path(project_read_component(
                custody,
                &mut projection_state,
                source_path,
            ))
        },
    );
    let source_start = file.byte_start;
    let requested_end = file.byte_end;
    let projected;
    let selected = if let Some(custody) = custody {
        projected = project_read_component(custody, &mut projection_state, &file.bytes);
        projected.as_slice()
    } else {
        file.bytes.as_slice()
    };
    let encoding = if std::str::from_utf8(selected).is_ok() && !selected.contains(&0) {
        Encoding::Utf8
    } else {
        Encoding::Binary
    };
    let must_artifact = encoding == Encoding::Binary
        || selected.len() > options.max_inline_bytes
        || matches!(request.range, ReadRange::Full)
            && request
                .path
                .extension()
                .is_some_and(|extension| extension == "log");
    let artifact_path = must_artifact.then(|| {
        custody.map_or_else(
            || request.path.clone(),
            |custody| {
                projected_path(project_read_component(
                    custody,
                    &mut projection_state,
                    source_path,
                ))
            },
        )
    });
    let prepared = artifact_path
        .as_ref()
        .map(|artifact_path| {
            prepare_artifact(
                artifacts,
                context,
                revision.epoch(),
                request,
                source_start,
                &file.bytes,
                selected,
                artifact_path,
                encoding,
                options.max_artifact_bytes,
                deadline,
            )
        })
        .transpose()?;
    let artifact = prepared.as_ref().map(|prepared| prepared.handle.clone());
    let pending = prepared
        .map(|prepared| prepared.staged.promote_pending_before(deadline))
        .transpose()
        .map_err(map_artifact_error)?;
    if pending.is_some() {
        publish_hook(ReadPublishPoint::ProvisionalSynced);
    }
    let projected_coordinates = custody.is_some() && selected != file.bytes;
    let start = if projected_coordinates {
        0
    } else {
        source_start
    };
    let end = if projected_coordinates {
        selected.len()
    } else {
        requested_end
    };
    let (has_lf, has_crlf) = projected_newlines(selected);
    let source_digest = blake3::hash(&file.bytes);
    let projected_digest = blake3::hash(selected);
    let mut response = ReadResponse {
        revision: request.expected_revision,
        path: display_path,
        file_bytes: file.file_bytes,
        byte_start: start,
        byte_end: end,
        source_offset: projected_coordinates.then_some(source_start),
        source_length: projected_coordinates.then_some(file.bytes.len()),
        source_digest: projected_coordinates
            .then(|| format!("blake3:{}", hex(source_digest.as_bytes()))),
        source_file_size: projected_coordinates.then_some(file.file_bytes),
        projected_offset: projected_coordinates.then_some(0),
        projected_length: projected_coordinates.then_some(selected.len()),
        projected_digest: projected_coordinates
            .then(|| format!("blake3:{}", hex(projected_digest.as_bytes()))),
        line_start: file.line_start,
        line_end: file.line_end,
        encoding,
        newline: if projected_coordinates {
            match (has_lf, has_crlf) {
                (false, false) => NewlineStyle::None,
                (true, false) => NewlineStyle::Lf,
                (false, true) => NewlineStyle::Crlf,
                (true, true) => NewlineStyle::Mixed,
            }
        } else {
            match (file.has_lf, file.has_crlf) {
                (false, false) => NewlineStyle::None,
                (true, false) => NewlineStyle::Lf,
                (false, true) => NewlineStyle::Crlf,
                (true, true) => NewlineStyle::Mixed,
            }
        },
        final_newline: if projected_coordinates {
            selected.last() == Some(&b'\n')
        } else {
            file.final_newline
        },
        mode: FileMode {
            executable: entry.executable,
        },
        content: Vec::new(),
        gap: None,
        artifact,
        truncated: file.truncated,
        result_bytes: 0,
    };
    let prepared_response = (|| {
        check_deadline(deadline)?;
        fit_content(
            &mut response,
            &selected[..selected.len().min(options.max_inline_bytes)],
            start,
            end,
            options.max_result_bytes,
            deadline,
        )?;
        guard.validate_before(deadline)?;
        Ok::<_, ReadError>(response)
    })();
    let response = match prepared_response {
        Ok(response) => response,
        Err(error) => {
            if let Some(pending) = pending {
                pending.rollback();
            }
            return Err(error);
        }
    };
    if let Some(pending) = pending {
        let mut committed = pending
            .commit_unissued_before(deadline)
            .map_err(map_artifact_error)?;
        publish_hook(ReadPublishPoint::VerifiedSynced);
        if let Err(error) = guard.validate_before(deadline) {
            committed.rollback();
            return Err(error.into());
        }
        if let Err(error) = committed.issue_workspace_before(deadline) {
            committed.rollback();
            return Err(map_artifact_error(error));
        }
        publish_hook(ReadPublishPoint::IssuedSynced);
        if let Err(error) = guard.validate_before(deadline) {
            committed.rollback();
            return Err(error.into());
        }
        committed.finish().map_err(map_artifact_error)?;
    }
    Ok(response)
}

fn projected_newlines(bytes: &[u8]) -> (bool, bool) {
    let crlf = bytes.windows(2).any(|pair| pair == b"\r\n");
    let lf = bytes.iter().enumerate().any(|(index, byte)| {
        *byte == b'\n' && index.checked_sub(1).and_then(|at| bytes.get(at)) != Some(&b'\r')
    });
    (lf, crlf)
}

fn project_read_component(
    custody: &crate::domain::secret::SecretCustody,
    state: &mut Option<&mut crate::domain::secret::JsonProjectionState>,
    bytes: &[u8],
) -> Vec<u8> {
    state.as_deref_mut().map_or_else(
        || {
            custody
                .project(
                    crate::telemetry::redact::CaptureBoundary::WorkspaceMetadata,
                    bytes,
                )
                .bytes()
                .expect("finished workspace projection")
                .to_vec()
        },
        |state| {
            custody.project_bytes_stream(
                crate::telemetry::redact::CaptureBoundary::WorkspaceMetadata,
                bytes,
                state,
            )
        },
    )
}

fn projected_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn resolve_artifact(
    workspace: &ManagedWorkspace,
    store: &ArtifactStore,
    context: &ArtifactContext,
    expected: &ReadRequest,
    handle: &WorkspaceArtifactHandle,
    options: &ArtifactResolveOptions,
) -> Result<Vec<u8>, ReadError> {
    if options.max_envelope_bytes == 0
        || options.max_payload_bytes == 0
        || options.max_time.is_zero()
    {
        return Err(ReadError::InvalidOptions(
            "artifact resolver bounds must be nonzero",
        ));
    }
    let deadline = Instant::now()
        .checked_add(options.max_time)
        .unwrap_or_else(Instant::now);
    validate_artifact_context(context, deadline)?;
    validate_path_and_range(expected)?;
    let revision = workspace.validate_revision_until(expected.expected_revision, deadline)?;
    let (version, digest) = opaque_digest(&handle.id, context, expected, deadline)
        .map_err(|_| ReadError::ArtifactAuthorization)?;
    if !store
        .workspace_artifact_is_issued(digest)
        .map_err(|_| ReadError::ArtifactAuthorization)?
    {
        return Err(ReadError::ArtifactAuthorization);
    }
    let epoch_text = revision.epoch().to_string();
    let revision_text = expected.expected_revision.to_string();
    let expected_path = expected.path.as_os_str().as_encoded_bytes();
    let (expected_range_tag, expected_range_start, expected_range_end) =
        range_binding(expected.range)?;
    let resolved = store
        .with_verified_reader_before(
            digest,
            options.max_envelope_bytes,
            deadline,
            |manifest, file| {
                let mut file = DeadlineReader {
                    inner: file,
                    deadline,
                };
                let mut magic = [0_u8; 26];
                file.read_exact(&mut magic)?;
                if magic != version.envelope_magic() {
                    return Err(ArtifactError::InvalidManifest("unknown workspace envelope"));
                }
                let epoch = read_frame(&mut file, 80)?;
                let bound_revision = read_frame(&mut file, 80)?;
                let path = read_frame(&mut file, options.max_envelope_bytes)?;
                let (projected_path, projected_path_digest) =
                    if version == WorkspaceArtifactVersion::V4 {
                        let projected_path = read_frame(&mut file, options.max_envelope_bytes)?;
                        let mut projected_path_digest = [0_u8; 32];
                        file.read_exact(&mut projected_path_digest)?;
                        (projected_path, projected_path_digest)
                    } else {
                        (path.clone(), *blake3::hash(&path).as_bytes())
                    };
                let mut range_tag = [0_u8; 1];
                file.read_exact(&mut range_tag)?;
                let range_start = read_u64(&mut file)?;
                let range_end = read_u64(&mut file)?;
                let first_coordinate = read_u64(&mut file)?;
                let second_coordinate = read_u64(&mut file)?;
                let mut source_digest = [0_u8; 32];
                if version == WorkspaceArtifactVersion::V4 {
                    file.read_exact(&mut source_digest)?;
                }
                let media_type = read_frame(&mut file, 255)?;
                let mut binding = [0_u8; 32];
                file.read_exact(&mut binding)?;
                let mut retention_tag = [0_u8; 1];
                file.read_exact(&mut retention_tag)?;
                let retention = match retention_tag[0] {
                    0 => ArtifactRetention::Forever,
                    1 => ArtifactRetention::UntilUnixMicros(read_i64(&mut file)?),
                    _ => return Err(ArtifactError::InvalidManifest("invalid envelope retention")),
                };
                let mut projected_digest = [0_u8; 32];
                file.read_exact(&mut projected_digest)?;
                let projected_length = read_u64(&mut file)?;
                let computed_binding = match version {
                    WorkspaceArtifactVersion::V3 => auth_binding_v3_u64(
                        context,
                        &epoch,
                        &bound_revision,
                        &path,
                        range_tag[0],
                        range_start,
                        range_end,
                        first_coordinate,
                        second_coordinate,
                        &projected_digest,
                    ),
                    WorkspaceArtifactVersion::V4 => auth_binding_u64(
                        context,
                        &epoch,
                        &bound_revision,
                        &path,
                        range_tag[0],
                        range_start,
                        range_end,
                        first_coordinate,
                        second_coordinate,
                        &source_digest,
                        &projected_digest,
                        projected_length,
                        &projected_path,
                        &projected_path_digest,
                    ),
                };
                let canonical_path = !path.is_empty()
                    && path[0] != b'/'
                    && path
                        .split(|byte| *byte == b'/')
                        .all(|part| !part.is_empty() && part != b"." && part != b"..");
                let canonical_projected_path = !projected_path.is_empty()
                    && projected_path[0] != b'/'
                    && projected_path
                        .split(|byte| *byte == b'/')
                        .all(|part| !part.is_empty() && part != b"." && part != b"..");
                let expected_projected_path_digest = blake3::hash(&projected_path);
                let projected_path_digest_text = format!("blake3:{}", hex(&projected_path_digest));
                let authorized =
                    fixed_eq(manifest.principal.as_bytes(), context.principal.as_bytes())
                        & fixed_eq(manifest.project.as_bytes(), context.project.as_bytes())
                        & fixed_eq(&epoch, epoch_text.as_bytes())
                        & fixed_eq(&bound_revision, revision_text.as_bytes())
                        & fixed_eq(&path, expected_path)
                        & fixed_eq(&projected_path, handle.path.as_os_str().as_encoded_bytes())
                        & fixed_eq(
                            projected_path_digest_text.as_bytes(),
                            handle.path_digest.as_bytes(),
                        )
                        & bool::from(
                            projected_path_digest.ct_eq(expected_projected_path_digest.as_bytes()),
                        )
                        & (range_tag[0] == expected_range_tag)
                        & (range_start == expected_range_start)
                        & (range_end == expected_range_end)
                        & bool::from(binding.ct_eq(&computed_binding))
                        & fixed_eq(
                            manifest.media_type.as_bytes(),
                            ENVELOPE_MEDIA_TYPE.as_bytes(),
                        )
                        & (manifest.class == ArtifactClass::File)
                        & (manifest.retention == retention)
                        & (manifest.stored_at_unix_micros == 0)
                        & match version {
                            WorkspaceArtifactVersion::V3 => {
                                first_coordinate <= second_coordinate
                                    && second_coordinate.checked_sub(first_coordinate)
                                        == Some(projected_length)
                            }
                            WorkspaceArtifactVersion::V4 => {
                                first_coordinate.checked_add(second_coordinate).is_some()
                            }
                        }
                        & canonical_path
                        & canonical_projected_path
                        & std::str::from_utf8(&media_type).is_ok();
                if !authorized {
                    return Err(ArtifactError::InvalidManifest(
                        "workspace artifact authorization failed",
                    ));
                }
                if projected_length > options.max_payload_bytes as u64
                    || projected_length > usize::MAX as u64
                {
                    return Err(ArtifactError::TooLarge {
                        size: projected_length,
                        max: options.max_payload_bytes as u64,
                    });
                }
                let mut payload = Vec::new();
                payload
                    .try_reserve_exact(projected_length as usize)
                    .map_err(|_| ArtifactError::TooLarge {
                        size: projected_length,
                        max: options.max_payload_bytes as u64,
                    })?;
                let mut hash = blake3::Hasher::new();
                let mut remaining = projected_length as usize;
                let mut buffer = [0_u8; 64 * 1024];
                while remaining != 0 {
                    if Instant::now() >= deadline {
                        return Err(ArtifactError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "artifact resolver time limit exceeded",
                        )));
                    }
                    let requested = remaining.min(buffer.len());
                    let count = file.read(&mut buffer[..requested])?;
                    if count == 0 {
                        return Err(ArtifactError::InvalidManifest("truncated artifact payload"));
                    }
                    hash.update(&buffer[..count]);
                    payload.extend_from_slice(&buffer[..count]);
                    remaining -= count;
                }
                let mut trailing = [0_u8; 1];
                if file.read(&mut trailing)? != 0 {
                    return Err(ArtifactError::InvalidManifest(
                        "trailing artifact payload bytes",
                    ));
                }
                let computed_projected_digest = *hash.finalize().as_bytes();
                if !bool::from(projected_digest.ct_eq(&computed_projected_digest)) {
                    return Err(ArtifactError::InvalidManifest(
                        "workspace projected payload digest mismatch",
                    ));
                }
                Ok(payload)
            },
        )
        .map_err(|error| match error {
            ArtifactError::Io(ref io) if io.kind() == std::io::ErrorKind::TimedOut => {
                ReadError::TimeLimit
            }
            _ => ReadError::ArtifactAuthorization,
        })?;
    workspace.validate_revision_until(expected.expected_revision, deadline)?;
    Ok(resolved)
}

fn validate_request(
    request: &ReadRequest,
    options: &ReadOptions,
    context: &ArtifactContext,
    deadline: Instant,
) -> Result<(), ReadError> {
    if options.max_read_bytes == 0
        || options.max_inline_bytes == 0
        || options.max_artifact_bytes == 0
        || options.max_result_bytes == 0
        || options.max_time.is_zero()
    {
        return Err(ReadError::InvalidOptions("all bounds must be nonzero"));
    }
    validate_path_and_range(request)?;
    validate_artifact_context(context, deadline)
}

fn validate_path_and_range(request: &ReadRequest) -> Result<(), ReadError> {
    if request.path.as_os_str().is_empty()
        || request.path.is_absolute()
        || !request
            .path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(ReadError::UnsafePath(request.path.clone()));
    }
    match request.range {
        ReadRange::Bytes { start, end } if start >= end => {
            return Err(ReadError::InvalidRange(
                "byte end must be greater than start",
            ));
        }
        ReadRange::Lines { start, end } if start == 0 || start > end => {
            return Err(ReadError::InvalidRange("lines are one-based and ordered"));
        }
        _ => {}
    }
    Ok(())
}

fn validate_artifact_context(
    context: &ArtifactContext,
    deadline: Instant,
) -> Result<(), ReadError> {
    check_deadline(deadline)?;
    if !valid_auth_field(&context.principal) || !valid_auth_field(&context.project) {
        return Err(ReadError::InvalidOptions(
            "artifact authorization fields must be 1..=128 printable ASCII bytes",
        ));
    }
    check_deadline(deadline)?;
    Ok(())
}

fn valid_auth_field(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ' || byte == b'\t')
}

const ENVELOPE_MEDIA_TYPE: &str = "application/vnd.kit.workspace-read-envelope";

#[derive(Clone, Copy, Eq, PartialEq)]
enum WorkspaceArtifactVersion {
    V3,
    V4,
}

impl WorkspaceArtifactVersion {
    const fn envelope_magic(self) -> [u8; 26] {
        match self {
            Self::V3 => *b"kit-workspace-artifact-v3\0",
            Self::V4 => *b"kit-workspace-artifact-v4\0",
        }
    }

    const fn handle_prefix(self) -> &'static str {
        match self {
            Self::V3 => "kit-workspace-artifact:v3:",
            Self::V4 => "kit-workspace-artifact:v4:",
        }
    }
}

struct PreparedArtifact<'a> {
    handle: WorkspaceArtifactHandle,
    staged: StagedArtifact<'a>,
}

#[allow(clippy::too_many_arguments)]
fn prepare_artifact<'a>(
    store: &'a ArtifactStore,
    context: &ArtifactContext,
    epoch: EpochId,
    request: &ReadRequest,
    source_offset: usize,
    source: &[u8],
    projected: &[u8],
    projected_path: &std::path::Path,
    encoding: Encoding,
    max_artifact_bytes: usize,
    deadline: Instant,
) -> Result<PreparedArtifact<'a>, ReadError> {
    check_deadline(deadline)?;
    let source_digest = blake3::hash(source);
    let projected_digest = blake3::hash(projected);
    let epoch = epoch.to_string();
    let revision = request.expected_revision.to_string();
    let path = request.path.as_os_str().as_encoded_bytes();
    let projected_path_value = projected_path.to_path_buf();
    let projected_path = projected_path.as_os_str().as_encoded_bytes();
    let projected_path_digest = blake3::hash(projected_path);
    let payload_media_type = if encoding == Encoding::Utf8 {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    let (range_tag, range_start, range_end) = range_binding(request.range)?;
    let binding = auth_binding(
        context,
        epoch.as_bytes(),
        revision.as_bytes(),
        path,
        range_tag,
        range_start,
        range_end,
        source_offset,
        source.len(),
        source_digest.as_bytes(),
        projected_digest.as_bytes(),
        projected.len(),
        projected_path,
        projected_path_digest.as_bytes(),
    )?;
    let retention_bytes = match context.retention {
        ArtifactRetention::Forever => 1,
        ArtifactRetention::UntilUnixMicros(_) => 1 + size_of::<i64>(),
    };
    let required = b"kit-workspace-artifact-v4\0"
        .len()
        .checked_add(framed_size(epoch.len())?)
        .and_then(|size| size.checked_add(framed_size(revision.len()).ok()?))
        .and_then(|size| size.checked_add(framed_size(path.len()).ok()?))
        .and_then(|size| size.checked_add(framed_size(projected_path.len()).ok()?))
        .and_then(|size| size.checked_add(projected_path_digest.as_bytes().len()))
        .and_then(|size| size.checked_add(1 + size_of::<u64>() * 4))
        .and_then(|size| size.checked_add(framed_size(payload_media_type.len()).ok()?))
        .and_then(|size| size.checked_add(binding.len()))
        .and_then(|size| size.checked_add(retention_bytes))
        .and_then(|size| size.checked_add(source_digest.as_bytes().len()))
        .and_then(|size| size.checked_add(projected_digest.as_bytes().len()))
        .and_then(|size| size.checked_add(size_of::<u64>()))
        .and_then(|size| size.checked_add(projected.len()))
        .ok_or(ReadError::InvalidOptions("artifact envelope size overflow"))?;
    if required > max_artifact_bytes {
        return Err(ReadError::ArtifactTooLarge {
            required,
            max: max_artifact_bytes,
        });
    }
    let header_size = required
        .checked_sub(projected.len())
        .ok_or(ReadError::InvalidOptions("artifact envelope size overflow"))?;
    let mut envelope_header = Vec::new();
    envelope_header
        .try_reserve_exact(header_size)
        .map_err(|_| ReadError::InvalidOptions("artifact envelope allocation failed"))?;
    envelope_header.extend_from_slice(b"kit-workspace-artifact-v4\0");
    frame(&mut envelope_header, epoch.as_bytes())?;
    frame(&mut envelope_header, revision.as_bytes())?;
    frame(&mut envelope_header, path)?;
    frame(&mut envelope_header, projected_path)?;
    envelope_header.extend_from_slice(projected_path_digest.as_bytes());
    envelope_header.push(range_tag);
    envelope_header.extend_from_slice(&range_start.to_le_bytes());
    envelope_header.extend_from_slice(&range_end.to_le_bytes());
    envelope_header.extend_from_slice(
        &u64::try_from(source_offset)
            .map_err(|_| ReadError::InvalidRange("source offset is out of range"))?
            .to_le_bytes(),
    );
    envelope_header.extend_from_slice(
        &u64::try_from(source.len())
            .map_err(|_| ReadError::InvalidRange("source length is out of range"))?
            .to_le_bytes(),
    );
    envelope_header.extend_from_slice(source_digest.as_bytes());
    frame(&mut envelope_header, payload_media_type.as_bytes())?;
    envelope_header.extend_from_slice(&binding);
    match context.retention {
        ArtifactRetention::Forever => envelope_header.push(0),
        ArtifactRetention::UntilUnixMicros(value) => {
            envelope_header.push(1);
            envelope_header.extend_from_slice(&value.to_le_bytes());
        }
    }
    envelope_header.extend_from_slice(projected_digest.as_bytes());
    envelope_header.extend_from_slice(
        &u64::try_from(projected.len())
            .map_err(|_| ReadError::InvalidOptions("artifact payload size overflow"))?
            .to_le_bytes(),
    );
    debug_assert_eq!(envelope_header.len(), header_size);
    let staged = store
        .stage_chunks_before(
            [envelope_header.as_slice(), projected],
            required,
            ArtifactMetadata::new(
                ENVELOPE_MEDIA_TYPE,
                ArtifactClass::File,
                context.principal.clone(),
                context.project.clone(),
                context.retention,
                0,
            )?,
            deadline,
        )
        .map_err(map_artifact_error)?;
    check_deadline(deadline)?;
    let id = opaque_id(staged.digest(), context, request, deadline)?;
    Ok(PreparedArtifact {
        handle: WorkspaceArtifactHandle {
            id,
            path: projected_path_value,
            path_digest: format!("blake3:{}", hex(projected_path_digest.as_bytes())),
        },
        staged,
    })
}

fn framed_size(bytes: usize) -> Result<usize, ReadError> {
    size_of::<u64>()
        .checked_add(bytes)
        .ok_or(ReadError::InvalidOptions("artifact envelope size overflow"))
}

fn frame(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ReadError> {
    output.extend_from_slice(
        &u64::try_from(bytes.len())
            .map_err(|_| ReadError::InvalidOptions("artifact frame size overflow"))?
            .to_le_bytes(),
    );
    output.extend_from_slice(bytes);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn auth_binding(
    context: &ArtifactContext,
    epoch: &[u8],
    revision: &[u8],
    path: &[u8],
    range_tag: u8,
    range_start: u64,
    range_end: u64,
    source_offset: usize,
    source_length: usize,
    source_digest: &[u8; 32],
    projected_digest: &[u8; 32],
    projected_length: usize,
    projected_path: &[u8],
    projected_path_digest: &[u8; 32],
) -> Result<[u8; 32], ReadError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-workspace-artifact-auth-v4\0");
    for value in [
        context.principal.as_bytes(),
        context.project.as_bytes(),
        epoch,
        revision,
        path,
    ] {
        hash.update(
            &u64::try_from(value.len())
                .map_err(|_| ReadError::InvalidOptions("artifact binding size overflow"))?
                .to_le_bytes(),
        );
        hash.update(value);
    }
    hash.update(&[range_tag]);
    hash.update(&range_start.to_le_bytes());
    hash.update(&range_end.to_le_bytes());
    hash.update(
        &u64::try_from(source_offset)
            .map_err(|_| ReadError::InvalidRange("source offset is out of range"))?
            .to_le_bytes(),
    );
    hash.update(
        &u64::try_from(source_length)
            .map_err(|_| ReadError::InvalidRange("source length is out of range"))?
            .to_le_bytes(),
    );
    hash.update(source_digest);
    hash.update(projected_digest);
    hash.update(
        &u64::try_from(projected_length)
            .map_err(|_| ReadError::InvalidRange("projected length is out of range"))?
            .to_le_bytes(),
    );
    hash.update(
        &u64::try_from(projected_path.len())
            .map_err(|_| ReadError::InvalidRange("projected path is out of range"))?
            .to_le_bytes(),
    );
    hash.update(projected_path);
    hash.update(projected_path_digest);
    Ok(*hash.finalize().as_bytes())
}

#[allow(clippy::too_many_arguments)]
fn auth_binding_u64(
    context: &ArtifactContext,
    epoch: &[u8],
    revision: &[u8],
    path: &[u8],
    range_tag: u8,
    range_start: u64,
    range_end: u64,
    source_offset: u64,
    source_length: u64,
    source_digest: &[u8; 32],
    projected_digest: &[u8; 32],
    projected_length: u64,
    projected_path: &[u8],
    projected_path_digest: &[u8; 32],
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-workspace-artifact-auth-v4\0");
    for value in [
        context.principal.as_bytes(),
        context.project.as_bytes(),
        epoch,
        revision,
        path,
    ] {
        hash.update(&(value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    hash.update(&[range_tag]);
    hash.update(&range_start.to_le_bytes());
    hash.update(&range_end.to_le_bytes());
    hash.update(&source_offset.to_le_bytes());
    hash.update(&source_length.to_le_bytes());
    hash.update(source_digest);
    hash.update(projected_digest);
    hash.update(&projected_length.to_le_bytes());
    hash.update(&(projected_path.len() as u64).to_le_bytes());
    hash.update(projected_path);
    hash.update(projected_path_digest);
    *hash.finalize().as_bytes()
}

#[allow(clippy::too_many_arguments)]
fn auth_binding_v3_u64(
    context: &ArtifactContext,
    epoch: &[u8],
    revision: &[u8],
    path: &[u8],
    range_tag: u8,
    range_start: u64,
    range_end: u64,
    start: u64,
    end: u64,
    payload_digest: &[u8; 32],
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-workspace-artifact-auth-v3\0");
    for value in [
        context.principal.as_bytes(),
        context.project.as_bytes(),
        epoch,
        revision,
        path,
    ] {
        hash.update(&(value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    hash.update(&[range_tag]);
    hash.update(&range_start.to_le_bytes());
    hash.update(&range_end.to_le_bytes());
    hash.update(&start.to_le_bytes());
    hash.update(&end.to_le_bytes());
    hash.update(payload_digest);
    *hash.finalize().as_bytes()
}

fn range_binding(range: ReadRange) -> Result<(u8, u64, u64), ReadError> {
    match range {
        ReadRange::Full => Ok((0, 0, 0)),
        ReadRange::Bytes { start, end } => Ok((
            1,
            u64::try_from(start)
                .map_err(|_| ReadError::InvalidRange("byte start is out of range"))?,
            u64::try_from(end).map_err(|_| ReadError::InvalidRange("byte end is out of range"))?,
        )),
        ReadRange::Lines { start, end } => Ok((
            2,
            u64::try_from(start)
                .map_err(|_| ReadError::InvalidRange("line start is out of range"))?,
            u64::try_from(end).map_err(|_| ReadError::InvalidRange("line end is out of range"))?,
        )),
    }
}

fn handle_mask(
    version: WorkspaceArtifactVersion,
    context: &ArtifactContext,
    request: &ReadRequest,
    deadline: Instant,
) -> Result<[u8; 32], ReadError> {
    check_deadline(deadline)?;
    let (tag, start, end) = range_binding(request.range)?;
    let revision = request.expected_revision.to_string();
    let mut hash = blake3::Hasher::new();
    hash.update(match version {
        WorkspaceArtifactVersion::V3 => b"kit-workspace-artifact-handle-v3\0",
        WorkspaceArtifactVersion::V4 => b"kit-workspace-artifact-handle-v4\0",
    });
    for value in [
        context.principal.as_bytes(),
        context.project.as_bytes(),
        revision.as_bytes(),
        request.path.as_os_str().as_encoded_bytes(),
    ] {
        hash.update(&(value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    hash.update(&[tag]);
    hash.update(&start.to_le_bytes());
    hash.update(&end.to_le_bytes());
    check_deadline(deadline)?;
    Ok(*hash.finalize().as_bytes())
}

fn opaque_id(
    digest: ArtifactDigest,
    context: &ArtifactContext,
    request: &ReadRequest,
    deadline: Instant,
) -> Result<String, ReadError> {
    let version = WorkspaceArtifactVersion::V4;
    let mask = handle_mask(version, context, request, deadline)?;
    let mut opaque = digest.as_bytes();
    for (byte, mask) in opaque.iter_mut().zip(mask) {
        *byte ^= mask;
    }
    Ok(format!("{}{}", version.handle_prefix(), hex(&opaque)))
}

fn opaque_digest(
    handle: &str,
    context: &ArtifactContext,
    request: &ReadRequest,
    deadline: Instant,
) -> Result<(WorkspaceArtifactVersion, ArtifactDigest), ArtifactError> {
    let (version, encoded) = [WorkspaceArtifactVersion::V4, WorkspaceArtifactVersion::V3]
        .into_iter()
        .find_map(|version| {
            handle
                .strip_prefix(version.handle_prefix())
                .map(|encoded| (version, encoded))
        })
        .ok_or(ArtifactError::InvalidArtifactDigest)?;
    let opaque = ArtifactDigest::parse(&format!("blake3:{encoded}"))?.as_bytes();
    let mask = handle_mask(version, context, request, deadline)
        .map_err(|_| ArtifactError::InvalidArtifactDigest)?;
    let mut digest = opaque;
    for (byte, mask) in digest.iter_mut().zip(mask) {
        *byte ^= mask;
    }
    ArtifactDigest::parse(&format!("blake3:{}", hex(&digest))).map(|digest| (version, digest))
}

fn read_frame(reader: &mut impl std::io::Read, max: usize) -> Result<Vec<u8>, ArtifactError> {
    let length = read_u64(reader)?;
    if length > max as u64 || length > usize::MAX as u64 {
        return Err(ArtifactError::TooLarge {
            size: length,
            max: max as u64,
        });
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length as usize)
        .map_err(|_| ArtifactError::TooLarge {
            size: length,
            max: max as u64,
        })?;
    bytes.resize(length as usize, 0);
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_u64(reader: &mut impl std::io::Read) -> Result<u64, ArtifactError> {
    let mut bytes = [0_u8; size_of::<u64>()];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_i64(reader: &mut impl std::io::Read) -> Result<i64, ArtifactError> {
    let mut bytes = [0_u8; size_of::<i64>()];
    reader.read_exact(&mut bytes)?;
    Ok(i64::from_le_bytes(bytes))
}

fn fixed_eq(left: &[u8], right: &[u8]) -> bool {
    bool::from(
        blake3::hash(left)
            .as_bytes()
            .ct_eq(blake3::hash(right).as_bytes()),
    )
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn fit_content(
    response: &mut ReadResponse,
    selected: &[u8],
    start: usize,
    requested_end: usize,
    max_result_bytes: usize,
    deadline: Instant,
) -> Result<(), ReadError> {
    let base_truncated = response.truncated;
    set_content_metadata(response, 0, start, requested_end, base_truncated);
    settle_size(response, &[], deadline)?;
    if response.result_bytes > max_result_bytes {
        return Err(ReadError::InvalidOptions(
            "result byte bound is smaller than response metadata",
        ));
    }

    set_content_metadata(
        response,
        selected.len(),
        start,
        requested_end,
        base_truncated,
    );
    settle_size(response, selected, deadline)?;
    let chosen = if response.result_bytes <= max_result_bytes {
        selected.len()
    } else {
        let mut low = 0;
        let mut high = selected.len();
        while low < high {
            check_deadline(deadline)?;
            let middle = low + (high - low).div_ceil(2);
            set_content_metadata(response, middle, start, requested_end, base_truncated);
            settle_size(response, &selected[..middle], deadline)?;
            if response.result_bytes <= max_result_bytes {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        low
    };
    set_content_metadata(response, chosen, start, requested_end, base_truncated);
    settle_size(response, &selected[..chosen], deadline)?;
    check_deadline(deadline)?;
    response
        .content
        .try_reserve_exact(chosen)
        .map_err(|_| ReadError::InvalidOptions("response content allocation failed"))?;
    response.content.extend_from_slice(&selected[..chosen]);
    check_deadline(deadline)?;
    Ok(())
}

fn set_content_metadata(
    response: &mut ReadResponse,
    content_bytes: usize,
    start: usize,
    requested_end: usize,
    base_truncated: bool,
) {
    let omitted_start = start + content_bytes;
    response.gap = (omitted_start < requested_end).then_some(GapMarker {
        byte_start: omitted_start,
        byte_end: requested_end,
        omitted_bytes: requested_end - omitted_start,
    });
    response.truncated = base_truncated || response.gap.is_some();
}

fn settle_size(
    response: &mut ReadResponse,
    content: &[u8],
    deadline: Instant,
) -> Result<(), ReadError> {
    for _ in 0..usize::MAX.to_string().len() + 2 {
        check_deadline(deadline)?;
        let size = serialized_bytes(response, content, deadline)?;
        if size == response.result_bytes {
            return Ok(());
        }
        response.result_bytes = size;
    }
    Err(ReadError::InvalidOptions(
        "serialized result size did not converge",
    ))
}

#[derive(Serialize)]
struct BorrowedReadResponse<'a> {
    revision: RevisionId,
    path: &'a std::path::Path,
    file_bytes: usize,
    byte_start: usize,
    byte_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_digest: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_file_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    projected_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    projected_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    projected_digest: Option<&'a str>,
    line_start: Option<usize>,
    line_end: Option<usize>,
    encoding: Encoding,
    newline: NewlineStyle,
    final_newline: bool,
    mode: FileMode,
    content: &'a [u8],
    gap: &'a Option<GapMarker>,
    artifact: &'a Option<WorkspaceArtifactHandle>,
    truncated: bool,
    result_bytes: usize,
}

fn serialized_bytes(
    response: &ReadResponse,
    content: &[u8],
    deadline: Instant,
) -> Result<usize, ReadError> {
    let projected = BorrowedReadResponse {
        revision: response.revision,
        path: &response.path,
        file_bytes: response.file_bytes,
        byte_start: response.byte_start,
        byte_end: response.byte_end,
        source_offset: response.source_offset,
        source_length: response.source_length,
        source_digest: response.source_digest.as_deref(),
        source_file_size: response.source_file_size,
        projected_offset: response.projected_offset,
        projected_length: response.projected_length,
        projected_digest: response.projected_digest.as_deref(),
        line_start: response.line_start,
        line_end: response.line_end,
        encoding: response.encoding,
        newline: response.newline,
        final_newline: response.final_newline,
        mode: response.mode,
        content,
        gap: &response.gap,
        artifact: &response.artifact,
        truncated: response.truncated,
        result_bytes: response.result_bytes,
    };
    let mut writer = CountingWriter { bytes: 0, deadline };
    serde_json::to_writer(&mut writer, &projected).map_err(|error| {
        if error.io_error_kind() == Some(io::ErrorKind::TimedOut) {
            ReadError::TimeLimit
        } else {
            ReadError::Serialization(error)
        }
    })?;
    Ok(writer.bytes)
}

struct CountingWriter {
    bytes: usize,
    deadline: Instant,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "focused read time limit exceeded",
            ));
        }
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("serialized focused read length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DeadlineReader<R> {
    inner: R,
    deadline: Instant,
}

impl<R: Read> Read for DeadlineReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "artifact resolver time limit exceeded",
            ));
        }
        let read = self.inner.read(bytes)?;
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "artifact resolver time limit exceeded",
            ));
        }
        Ok(read)
    }
}

fn map_artifact_error(error: ArtifactError) -> ReadError {
    match error {
        ArtifactError::Io(ref io) if io.kind() == io::ErrorKind::TimedOut => ReadError::TimeLimit,
        error => ReadError::Artifact(error),
    }
}

fn check_deadline(deadline: Instant) -> Result<(), ReadError> {
    if Instant::now() >= deadline {
        Err(ReadError::TimeLimit)
    } else {
        Ok(())
    }
}
