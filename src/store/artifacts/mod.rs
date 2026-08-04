use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactDigest([u8; 32]);

impl ArtifactDigest {
    pub fn digest(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub fn parse(value: &str) -> Result<Self, ArtifactError> {
        let hex = value
            .strip_prefix("blake3:")
            .ok_or(ArtifactError::InvalidArtifactDigest)?;
        if hex.len() != 64 {
            return Err(ArtifactError::InvalidArtifactDigest);
        }
        let mut digest = [0; 32];
        for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
            digest[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
        }
        Ok(Self(digest))
    }

    pub fn as_bytes(self) -> [u8; 32] {
        self.0
    }

    fn hex(self) -> String {
        let mut result = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            write!(result, "{byte:02x}").expect("writing to a string cannot fail");
        }
        result
    }
}

impl fmt::Display for ArtifactDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blake3:{}", self.hex())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactReference([u8; 32]);

impl ArtifactReference {
    pub fn parse(value: &str) -> Result<Self, ArtifactError> {
        let hex = value
            .strip_prefix("artifact-ref:")
            .ok_or(ArtifactError::InvalidArtifactReference)?;
        if hex.len() != 64 {
            return Err(ArtifactError::InvalidArtifactReference);
        }
        let mut reference = [0; 32];
        for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
            reference[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
        }
        Ok(Self(reference))
    }

    fn generate() -> Result<Self, ArtifactError> {
        let mut reference = [0; 32];
        getrandom::fill(&mut reference)
            .map_err(|_| ArtifactError::InvalidManifest("secure randomness failed"))?;
        Ok(Self(reference))
    }

    pub(crate) fn derive(domain: &[u8], identity: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(domain);
        hasher.update(&[0]);
        hasher.update(identity);
        Self(*hasher.finalize().as_bytes())
    }

    fn compatibility(digest: ArtifactDigest) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"kit-artifact-compatibility-reference-v1");
        hasher.update(&digest.as_bytes());
        Self(*hasher.finalize().as_bytes())
    }

    fn hex(self) -> String {
        hex_bytes(&self.0)
    }
}

impl fmt::Display for ArtifactReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "artifact-ref:{}", self.hex())
    }
}

// Kept for existing backup consumers; new artifact-reference code uses the
// content-specific name so it cannot be confused with an artifact record ID.
#[doc(hidden)]
pub type ArtifactId = ArtifactDigest;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactClass {
    Log,
    Diff,
    File,
    Index,
    Image,
    Report,
    RestrictedEncrypted,
}

impl ArtifactClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Diff => "diff",
            Self::File => "file",
            Self::Index => "index",
            Self::Image => "image",
            Self::Report => "report",
            Self::RestrictedEncrypted => "restricted_encrypted",
        }
    }

    fn parse(value: &str) -> Result<Self, ArtifactError> {
        match value {
            "log" => Ok(Self::Log),
            "diff" => Ok(Self::Diff),
            "file" => Ok(Self::File),
            "index" => Ok(Self::Index),
            "image" => Ok(Self::Image),
            "report" => Ok(Self::Report),
            "restricted_encrypted" => Ok(Self::RestrictedEncrypted),
            _ => Err(ArtifactError::InvalidManifest("unknown artifact class")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactRetention {
    UntilUnixMicros(i64),
    Forever,
}

impl ArtifactRetention {
    fn blocks_at(self, now_unix_micros: i64) -> bool {
        match self {
            Self::UntilUnixMicros(expires_at) => now_unix_micros < expires_at,
            Self::Forever => true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactMetadata {
    pub media_type: String,
    pub class: ArtifactClass,
    pub principal: String,
    pub project: String,
    pub retention: ArtifactRetention,
    pub stored_at_unix_micros: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactEnvelopeBinding {
    pub principal: String,
    pub project: String,
    pub run: String,
    pub purpose: String,
    pub invocation_id: Option<String>,
    pub callback_id: Option<String>,
}

impl ArtifactEnvelopeBinding {
    fn validate(&self) -> Result<(), ArtifactError> {
        if !valid_field(&self.principal, 128)
            || !valid_field(&self.project, 128)
            || !valid_field(&self.run, 128)
            || !valid_field(&self.purpose, 128)
            || self.invocation_id.is_some() == self.callback_id.is_some()
            || self
                .invocation_id
                .as_deref()
                .or(self.callback_id.as_deref())
                .is_none_or(|id| !valid_field(id, 256))
        {
            return Err(ArtifactError::InvalidManifest(
                "invalid artifact envelope binding",
            ));
        }
        Ok(())
    }

    pub fn seal(&self, payload: &[u8]) -> Result<Vec<u8>, ArtifactError> {
        self.validate()?;
        let mut bytes = b"kit-artifact-envelope-v1\0".to_vec();
        for field in [
            self.principal.as_bytes(),
            self.project.as_bytes(),
            self.run.as_bytes(),
            self.purpose.as_bytes(),
            self.invocation_id.as_deref().unwrap_or("").as_bytes(),
            self.callback_id.as_deref().unwrap_or("").as_bytes(),
            payload,
        ] {
            bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
            bytes.extend_from_slice(field);
        }
        Ok(bytes)
    }

    pub fn open<'a>(&self, envelope: &'a [u8]) -> Result<&'a [u8], ArtifactError> {
        self.validate()?;
        let mut rest = envelope.strip_prefix(b"kit-artifact-envelope-v1\0").ok_or(
            ArtifactError::InvalidManifest("unknown artifact envelope version"),
        )?;
        let mut fields = Vec::with_capacity(7);
        for _ in 0..7 {
            let length = rest
                .get(..8)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u64::from_be_bytes)
                .and_then(|length| usize::try_from(length).ok())
                .ok_or(ArtifactError::InvalidManifest(
                    "invalid artifact envelope length",
                ))?;
            rest = &rest[8..];
            let (field, remaining) =
                rest.split_at_checked(length)
                    .ok_or(ArtifactError::InvalidManifest(
                        "truncated artifact envelope",
                    ))?;
            fields.push(field);
            rest = remaining;
        }
        if !rest.is_empty()
            || fields[..6]
                != [
                    self.principal.as_bytes(),
                    self.project.as_bytes(),
                    self.run.as_bytes(),
                    self.purpose.as_bytes(),
                    self.invocation_id.as_deref().unwrap_or("").as_bytes(),
                    self.callback_id.as_deref().unwrap_or("").as_bytes(),
                ]
        {
            return Err(ArtifactError::InvalidManifest(
                "artifact envelope binding mismatch",
            ));
        }
        Ok(fields[6])
    }

    pub(crate) fn matches(&self, envelope: &[u8]) -> Result<bool, ArtifactError> {
        match self.open(envelope) {
            Ok(_) => Ok(true),
            Err(ArtifactError::InvalidManifest(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

impl ArtifactMetadata {
    pub fn new(
        media_type: impl Into<String>,
        class: ArtifactClass,
        principal: impl Into<String>,
        project: impl Into<String>,
        retention: ArtifactRetention,
        stored_at_unix_micros: i64,
    ) -> Result<Self, ArtifactError> {
        let metadata = Self {
            media_type: media_type.into(),
            class,
            principal: principal.into(),
            project: project.into(),
            retention,
            stored_at_unix_micros,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    fn validate(&self) -> Result<(), ArtifactError> {
        if !valid_field(&self.media_type, 255) || !self.media_type.contains('/') {
            return Err(ArtifactError::InvalidManifest("invalid media type"));
        }
        if !valid_field(&self.principal, 128) {
            return Err(ArtifactError::InvalidManifest("invalid principal"));
        }
        if !valid_field(&self.project, 128) {
            return Err(ArtifactError::InvalidManifest("invalid project"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactManifest {
    pub size: u64,
    pub media_type: String,
    pub class: ArtifactClass,
    pub principal: String,
    pub project: String,
    pub retention: ArtifactRetention,
    pub stored_at_unix_micros: i64,
}

impl ArtifactManifest {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let retention = match self.retention {
            ArtifactRetention::UntilUnixMicros(value) => format!("until:{value}"),
            ArtifactRetention::Forever => "forever".to_owned(),
        };
        format!(
            "kit-artifact-manifest-v1\nsize={}\nmedia={}\nclass={}\nprincipal={}\nproject={}\nretention={}\nstored_at={}\n",
            self.size,
            self.media_type,
            self.class.as_str(),
            self.principal,
            self.project,
            retention,
            self.stored_at_unix_micros,
        )
        .into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, ArtifactError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| ArtifactError::InvalidManifest("manifest is not UTF-8"))?;
        let mut lines = text.lines();
        if lines.next() != Some("kit-artifact-manifest-v1") {
            return Err(ArtifactError::InvalidManifest("unknown manifest version"));
        }
        let size = field(&mut lines, "size=")?
            .parse()
            .map_err(|_| ArtifactError::InvalidManifest("invalid size"))?;
        let media_type = field(&mut lines, "media=")?.to_owned();
        let class = ArtifactClass::parse(field(&mut lines, "class=")?)?;
        let principal = field(&mut lines, "principal=")?.to_owned();
        let project = field(&mut lines, "project=")?.to_owned();
        let retention = match field(&mut lines, "retention=")? {
            "forever" => ArtifactRetention::Forever,
            value => ArtifactRetention::UntilUnixMicros(
                value
                    .strip_prefix("until:")
                    .ok_or(ArtifactError::InvalidManifest("invalid retention"))?
                    .parse()
                    .map_err(|_| ArtifactError::InvalidManifest("invalid retention"))?,
            ),
        };
        let stored_at_unix_micros = field(&mut lines, "stored_at=")?
            .parse()
            .map_err(|_| ArtifactError::InvalidManifest("invalid stored time"))?;
        if lines.next().is_some() {
            return Err(ArtifactError::InvalidManifest("unexpected manifest field"));
        }
        let metadata = ArtifactMetadata::new(
            media_type.clone(),
            class,
            principal.clone(),
            project.clone(),
            retention,
            stored_at_unix_micros,
        )?;
        Ok(Self {
            size,
            media_type: metadata.media_type,
            class: metadata.class,
            principal: metadata.principal,
            project: metadata.project,
            retention: metadata.retention,
            stored_at_unix_micros: metadata.stored_at_unix_micros,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArtifactReferenceManifest {
    digest: ArtifactDigest,
    artifact: ArtifactManifest,
}

impl ArtifactReferenceManifest {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = format!(
            "kit-artifact-reference-manifest-v1\ndigest={}\n",
            self.digest
        )
        .into_bytes();
        bytes.extend_from_slice(&self.artifact.canonical_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self, ArtifactError> {
        let prefix = b"kit-artifact-reference-manifest-v1\ndigest=";
        let rest = bytes
            .strip_prefix(prefix)
            .ok_or(ArtifactError::InvalidManifest(
                "unknown artifact reference manifest version",
            ))?;
        let newline =
            rest.iter()
                .position(|byte| *byte == b'\n')
                .ok_or(ArtifactError::InvalidManifest(
                    "artifact reference digest is missing",
                ))?;
        let digest = std::str::from_utf8(&rest[..newline])
            .map_err(|_| ArtifactError::InvalidManifest("reference digest is not UTF-8"))?;
        Ok(Self {
            digest: ArtifactDigest::parse(digest)?,
            artifact: ArtifactManifest::decode(&rest[newline + 1..])?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedArtifact {
    digest: ArtifactDigest,
    reference: ArtifactReference,
    manifest: ArtifactManifest,
}

impl VerifiedArtifact {
    pub fn digest(&self) -> ArtifactDigest {
        self.digest
    }

    pub fn reference(&self) -> ArtifactReference {
        self.reference
    }

    #[doc(hidden)]
    pub fn id(&self) -> ArtifactDigest {
        self.digest
    }

    pub fn manifest(&self) -> &ArtifactManifest {
        &self.manifest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrashPoint {
    AfterTempCreated,
    AfterBytesWritten,
    AfterFileSynced,
    AfterHashVerified,
    AfterObjectRenamed,
    AfterObjectDirectorySynced,
    AfterManifestWritten,
    AfterManifestSynced,
    AfterManifestRenamed,
    AfterManifestDirectorySynced,
    BeforeEventReference,
    AfterEventReference,
    AfterLeaseTempCreated,
    AfterLeasePartialWrite,
    AfterLeaseFileSynced,
    AfterLeaseRenamed,
    AfterLeaseDirectorySynced,
}

impl CrashPoint {
    pub const ALL: [Self; 12] = [
        Self::AfterTempCreated,
        Self::AfterBytesWritten,
        Self::AfterFileSynced,
        Self::AfterHashVerified,
        Self::AfterObjectRenamed,
        Self::AfterObjectDirectorySynced,
        Self::AfterManifestWritten,
        Self::AfterManifestSynced,
        Self::AfterManifestRenamed,
        Self::AfterManifestDirectorySynced,
        Self::BeforeEventReference,
        Self::AfterEventReference,
    ];

    pub const LEASE_PUBLICATION: [Self; 5] = [
        Self::AfterLeaseTempCreated,
        Self::AfterLeasePartialWrite,
        Self::AfterLeaseFileSynced,
        Self::AfterLeaseRenamed,
        Self::AfterLeaseDirectorySynced,
    ];
}

#[derive(Debug)]
pub enum ArtifactError {
    Io(std::io::Error),
    InvalidArtifactDigest,
    InvalidArtifactReference,
    AccessDenied,
    InvalidManifest(&'static str),
    UnsafePath(PathBuf),
    Missing(ArtifactDigest),
    DigestMismatch(ArtifactDigest),
    ManifestConflict(ArtifactDigest),
    TooLarge { size: u64, max: u64 },
    InjectedCrash(CrashPoint),
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "artifact store I/O error: {error}"),
            Self::InvalidArtifactDigest => f.write_str("invalid BLAKE3 artifact digest"),
            Self::InvalidArtifactReference => f.write_str("invalid opaque artifact reference"),
            Self::AccessDenied => f.write_str("artifact reference access denied"),
            Self::InvalidManifest(message) => write!(f, "invalid artifact manifest: {message}"),
            Self::UnsafePath(path) => write!(f, "unsafe artifact store path: {}", path.display()),
            Self::Missing(id) => write!(f, "artifact {id} is missing"),
            Self::DigestMismatch(id) => write!(f, "artifact {id} failed digest verification"),
            Self::ManifestConflict(id) => write!(f, "artifact {id} has conflicting metadata"),
            Self::TooLarge { size, max } => {
                write!(f, "artifact size {size} exceeds caller bound {max}")
            }
            Self::InjectedCrash(point) => write!(f, "injected artifact crash at {point:?}"),
        }
    }
}

impl std::error::Error for ArtifactError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ArtifactError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub enum ReferenceError<E> {
    Artifact(ArtifactError),
    Commit(E),
}

#[derive(Clone, Debug, Default)]
pub struct Reachability {
    pub now_unix_micros: i64,
    pub orphan_grace_micros: u64,
    pub retained: BTreeSet<ArtifactDigest>,
    pub legal_holds: BTreeSet<ArtifactDigest>,
    pub shared_references: BTreeSet<ArtifactDigest>,
    pub backup_inventory: BTreeSet<ArtifactDigest>,
}

impl Reachability {
    fn is_referenced(&self, digest: ArtifactDigest) -> bool {
        self.retained.contains(&digest)
            || self.legal_holds.contains(&digest)
            || self.shared_references.contains(&digest)
            || self.backup_inventory.contains(&digest)
    }

    pub fn is_reachable(&self, digest: ArtifactDigest, manifest: &ArtifactManifest) -> bool {
        manifest.retention.blocks_at(self.now_unix_micros) || self.is_referenced(digest)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    pub deleted_artifacts: BTreeSet<ArtifactDigest>,
    pub deleted_staged_files: usize,
    pub deleted_orphan_manifests: usize,
    pub skipped_unsafe_entries: usize,
}

pub struct ArtifactStore {
    root: PathBuf,
    reference_gate: Mutex<()>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactLease {
    digest: ArtifactDigest,
    id: String,
    owner: String,
}

impl ArtifactLease {
    pub fn digest(&self) -> ArtifactDigest {
        self.digest
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

pub struct StagedArtifact<'a> {
    store: &'a ArtifactStore,
    digest: ArtifactDigest,
    reference: ArtifactReference,
    manifest: ArtifactManifest,
    object_temp: PathBuf,
    manifest_temp: PathBuf,
    complete: bool,
}

#[derive(Clone, Debug)]
pub struct ArtifactPublication {
    reference: ArtifactReference,
    digest: ArtifactDigest,
    manifest: ArtifactManifest,
}

impl ArtifactPublication {
    pub fn reference(&self) -> ArtifactReference {
        self.reference
    }

    pub fn digest(&self) -> ArtifactDigest {
        self.digest
    }

    pub fn manifest(&self) -> &ArtifactManifest {
        &self.manifest
    }
}

pub struct PendingArtifact<'a> {
    store: &'a ArtifactStore,
    digest: ArtifactDigest,
    reference: ArtifactReference,
    manifest: ArtifactManifest,
    pending_path: PathBuf,
    already_committed: bool,
    created_object: bool,
    created_pending: bool,
}

pub struct CommittedArtifact<'a> {
    store: &'a ArtifactStore,
    _reference_guard: MutexGuard<'a, ()>,
    artifact: VerifiedArtifact,
    created_object: bool,
    created_verified: bool,
    created_issued: bool,
    created_reference: bool,
}

impl<'a> StagedArtifact<'a> {
    pub fn digest(&self) -> ArtifactDigest {
        self.digest
    }

    pub fn reference(&self) -> ArtifactReference {
        self.reference
    }

    pub fn manifest(&self) -> &ArtifactManifest {
        &self.manifest
    }

    pub fn promote(self) -> Result<VerifiedArtifact, ArtifactError> {
        self.promote_pending()?.commit()
    }

    pub fn promote_pending(self) -> Result<PendingArtifact<'a>, ArtifactError> {
        self.promote_pending_until(None)
    }

    pub fn promote_pending_before(
        self,
        deadline: Instant,
    ) -> Result<PendingArtifact<'a>, ArtifactError> {
        self.promote_pending_until(Some(deadline))
    }

    fn promote_pending_until(
        mut self,
        deadline: Option<Instant>,
    ) -> Result<PendingArtifact<'a>, ArtifactError> {
        check_artifact_deadline(deadline)?;
        let _guard = self
            .store
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let object_path = self.store.object_path(self.digest)?;
        check_artifact_deadline(deadline)?;
        let manifest_path = self.store.manifest_path(self.digest)?;
        check_artifact_deadline(deadline)?;
        let pending_path = self.store.pending_manifest_path(self.digest)?;
        check_artifact_deadline(deadline)?;
        let manifest_bytes = self.manifest.canonical_bytes();
        let mut created_object = false;
        let mut created_pending = false;
        let result = (|| {
            check_artifact_deadline(deadline)?;
            if object_path.exists() {
                verify_file_until(&object_path, self.digest, self.manifest.size, deadline)?;
                check_artifact_deadline(deadline)?;
                self.store
                    .quarantine_remove_file(&self.object_temp, None, deadline)?;
            } else {
                rename_until(&self.object_temp, &object_path, deadline)?;
                created_object = true;
                sync_directory_until(object_path.parent().expect("object has parent"), deadline)?;
            }
            check_artifact_deadline(deadline)?;
            let already_committed = if manifest_path.exists() {
                check_artifact_deadline(deadline)?;
                self.store
                    .quarantine_remove_file(&self.manifest_temp, None, deadline)?;
                true
            } else if pending_path.exists() {
                if read_regular_file_bounded_until(&pending_path, 4096, deadline)? != manifest_bytes
                {
                    return Err(ArtifactError::ManifestConflict(self.digest));
                }
                check_artifact_deadline(deadline)?;
                self.store
                    .quarantine_remove_file(&self.manifest_temp, None, deadline)?;
                false
            } else {
                rename_until(&self.manifest_temp, &pending_path, deadline)?;
                created_pending = true;
                sync_directory_until(
                    pending_path.parent().expect("manifest has parent"),
                    deadline,
                )?;
                false
            };
            check_artifact_deadline(deadline)?;
            self.complete = true;
            Ok(PendingArtifact {
                store: self.store,
                digest: self.digest,
                reference: self.reference,
                manifest: self.manifest.clone(),
                pending_path: pending_path.clone(),
                already_committed,
                created_object,
                created_pending,
            })
        })();
        if result.is_err() {
            if created_pending {
                let _ = self.store.quarantine_remove_file(&pending_path, None, None);
                let _ = sync_directory(pending_path.parent().expect("manifest has parent"));
            }
            if created_object {
                let _ = self.store.quarantine_remove_file(&object_path, None, None);
                let _ = sync_directory(object_path.parent().expect("object has parent"));
            }
        }
        result
    }
}

impl<'a> PendingArtifact<'a> {
    pub fn digest(&self) -> ArtifactDigest {
        self.digest
    }

    pub fn commit(self) -> Result<VerifiedArtifact, ArtifactError> {
        self.commit_until(None)
    }

    pub fn commit_before(self, deadline: Instant) -> Result<VerifiedArtifact, ArtifactError> {
        self.commit_until(Some(deadline))
    }

    fn commit_until(self, deadline: Option<Instant>) -> Result<VerifiedArtifact, ArtifactError> {
        self.commit_unissued_until(deadline)?.finish_until(deadline)
    }

    pub fn commit_unissued_before(
        self,
        deadline: Instant,
    ) -> Result<CommittedArtifact<'a>, ArtifactError> {
        self.commit_unissued_until(Some(deadline))
    }

    fn commit_unissued_until(
        self,
        deadline: Option<Instant>,
    ) -> Result<CommittedArtifact<'a>, ArtifactError> {
        check_artifact_deadline(deadline)?;
        let reference_guard = self
            .store
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let object_path = self.store.object_path(self.digest)?;
        check_artifact_deadline(deadline)?;
        verify_file_until(&object_path, self.digest, self.manifest.size, deadline)?;
        let manifest_path = self.store.manifest_path(self.digest)?;
        check_artifact_deadline(deadline)?;
        let manifest_bytes = self.manifest.canonical_bytes();
        let mut created_verified = false;
        let result = (|| {
            if self.already_committed || manifest_path.exists() {
                if self.created_pending && self.pending_path.exists() {
                    check_artifact_deadline(deadline)?;
                    self.store
                        .quarantine_remove_file(&self.pending_path, None, deadline)?;
                }
            } else {
                if read_regular_file_bounded_until(&self.pending_path, 4096, deadline)?
                    != manifest_bytes
                {
                    return Err(ArtifactError::ManifestConflict(self.digest));
                }
                rename_until(&self.pending_path, &manifest_path, deadline)?;
                created_verified = self.created_pending;
            }
            sync_directory_until(
                manifest_path.parent().expect("manifest has parent"),
                deadline,
            )?;
            self.store.write_reference_manifest(
                self.reference,
                self.digest,
                &self.manifest,
                deadline,
            )?;
            Ok(CommittedArtifact {
                store: self.store,
                _reference_guard: reference_guard,
                artifact: VerifiedArtifact {
                    digest: self.digest,
                    reference: self.reference,
                    manifest: self.manifest.clone(),
                },
                created_object: self.created_object,
                created_verified,
                created_issued: false,
                created_reference: true,
            })
        })();
        if result.is_err() {
            if created_verified {
                let _ = self
                    .store
                    .quarantine_remove_file(&manifest_path, None, None);
            }
            self.rollback();
        }
        result
    }

    pub fn rollback(self) {
        if self.created_pending {
            let _ = self
                .store
                .quarantine_remove_file(&self.pending_path, None, None);
        }
        if self.created_object {
            self.store.remove_object_if_unclaimed(self.digest);
        }
    }
}

impl CommittedArtifact<'_> {
    pub fn issue_workspace_before(&mut self, deadline: Instant) -> Result<(), ArtifactError> {
        let issued_path = self.store.issued_path(self.artifact.digest)?;
        if issued_path.exists() {
            let bytes = read_regular_file_bounded_until(&issued_path, 128, Some(deadline))?;
            if bytes != b"kit-workspace-artifact-issued-v1\n" {
                return Err(ArtifactError::InvalidManifest(
                    "invalid workspace artifact issue state",
                ));
            }
            return Ok(());
        }
        let temp = self.store.temp_path("issued");
        let result = (|| {
            let mut file = secure_create_new(&temp)?;
            file.write_all(b"kit-workspace-artifact-issued-v1\n")?;
            sync_file_until(&file, Some(deadline))?;
            rename_until(&temp, &issued_path, Some(deadline))?;
            self.created_issued = true;
            sync_directory_until(
                issued_path.parent().expect("issue state has parent"),
                Some(deadline),
            )
        })();
        if result.is_err() {
            let _ = self.store.quarantine_remove_file(&temp, None, None);
            if self.created_issued {
                let _ = self.store.quarantine_remove_file(&issued_path, None, None);
                self.created_issued = false;
            }
        }
        result
    }

    pub fn finish(self) -> Result<VerifiedArtifact, ArtifactError> {
        self.finish_until(None)
    }

    fn finish_until(self, deadline: Option<Instant>) -> Result<VerifiedArtifact, ArtifactError> {
        if self.created_reference
            && let Err(error) = self
                .store
                .publish_reference_manifest(self.artifact.reference, deadline)
        {
            self.rollback();
            return Err(error);
        }
        Ok(self.artifact)
    }

    pub fn rollback(self) {
        if self.created_reference {
            for path in [
                self.store
                    .pending_reference_manifest_path(self.artifact.reference),
                self.store.reference_manifest_path(self.artifact.reference),
            ]
            .into_iter()
            .flatten()
            {
                let _ = self.store.quarantine_remove_file(&path, None, None);
            }
        }
        if self.created_issued
            && let Ok(path) = self.store.issued_path(self.artifact.digest)
        {
            let _ = self.store.quarantine_remove_file(&path, None, None);
        }
        if self.created_verified
            && let Ok(path) = self.store.manifest_path(self.artifact.digest)
        {
            let _ = self.store.quarantine_remove_file(&path, None, None);
        }
        if self.created_object {
            self.store.remove_object_if_unclaimed(self.artifact.digest);
        }
    }
}

impl Drop for StagedArtifact<'_> {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self
                .store
                .quarantine_remove_file(&self.object_temp, None, None);
            let _ = self
                .store
                .quarantine_remove_file(&self.manifest_temp, None, None);
        }
    }
}

impl ArtifactStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ArtifactError> {
        let root = root.as_ref();
        if root.as_os_str().is_empty() {
            return Err(ArtifactError::UnsafePath(root.to_owned()));
        }
        ensure_directory(root)?;
        for name in [
            "objects",
            "manifests",
            "records",
            "staging",
            "leases",
            "references",
        ] {
            ensure_directory(&root.join(name))?;
        }
        sync_directory(root)?;
        Ok(Self {
            root: root.to_owned(),
            reference_gate: Mutex::new(()),
        })
    }

    pub fn put(
        &self,
        bytes: &[u8],
        metadata: ArtifactMetadata,
    ) -> Result<VerifiedArtifact, ArtifactError> {
        self.stage(bytes, metadata)?.promote()
    }

    pub fn stage_publication(
        &self,
        bytes: &[u8],
        metadata: ArtifactMetadata,
        reference: ArtifactReference,
    ) -> Result<ArtifactPublication, ArtifactError> {
        metadata.validate()?;
        if !metadata.retention.blocks_at(now_unix_micros()?) {
            return Err(ArtifactError::InvalidManifest(
                "expired artifact publication",
            ));
        }
        self.check_layout()?;
        let digest = ArtifactDigest::digest(bytes);
        let manifest = ArtifactManifest {
            size: bytes.len() as u64,
            media_type: metadata.media_type,
            class: metadata.class,
            principal: metadata.principal,
            project: metadata.project,
            retention: metadata.retention,
            stored_at_unix_micros: metadata.stored_at_unix_micros,
        };
        let publication = ArtifactPublication {
            reference,
            digest,
            manifest,
        };
        let path = self.publication_stage_path(reference)?;
        let record = publication_stage_bytes(&publication, bytes);
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if path.exists() {
            if read_regular_file_bounded(&path, record.len())? == record {
                return Ok(publication);
            }
            return Err(ArtifactError::InvalidManifest(
                "artifact publication stage collision",
            ));
        }
        let temp = self.temp_path("publication-stage");
        let result = (|| {
            let mut file = secure_create_new(&temp)?;
            file.write_all(&record)?;
            file.sync_all()?;
            rename_until(&temp, &path, None)?;
            sync_directory(path.parent().expect("publication stage has parent"))
        })();
        if result.is_err() {
            let _ = self.quarantine_remove_file(&temp, None, None);
        }
        result.map(|()| publication)
    }

    pub fn promote_publication(
        &self,
        publication: &ArtifactPublication,
    ) -> Result<VerifiedArtifact, ArtifactError> {
        if !publication.manifest.retention.blocks_at(now_unix_micros()?) {
            return Err(ArtifactError::InvalidManifest(
                "expired artifact publication",
            ));
        }
        let path = self.publication_stage_path(publication.reference)?;
        let maximum = usize::try_from(publication.manifest.size)
            .ok()
            .and_then(|size| size.checked_add(8192))
            .ok_or(ArtifactError::TooLarge {
                size: publication.manifest.size,
                max: usize::MAX as u64,
            })?;
        let bytes = read_regular_file_bounded(&path, maximum)?;
        let (found, payload) = decode_publication_stage(&bytes)?;
        if found.reference != publication.reference
            || found.digest != publication.digest
            || found.manifest != publication.manifest
            || ArtifactDigest::digest(payload) != publication.digest
        {
            return Err(ArtifactError::InvalidManifest(
                "artifact publication stage binding mismatch",
            ));
        }
        let metadata = ArtifactMetadata::new(
            publication.manifest.media_type.clone(),
            publication.manifest.class,
            publication.manifest.principal.clone(),
            publication.manifest.project.clone(),
            publication.manifest.retention,
            publication.manifest.stored_at_unix_micros,
        )?;
        let artifact = self.put_with_reference(payload, metadata, publication.reference)?;
        self.remove_publication_stage(publication.reference)?;
        Ok(artifact)
    }

    pub fn publication(
        &self,
        reference: ArtifactReference,
        digest: ArtifactDigest,
    ) -> Result<ArtifactPublication, ArtifactError> {
        let publication = self.staged_publication(reference)?;
        let bytes =
            read_regular_file_bounded(&self.publication_stage_path(reference)?, 64 * 1024 * 1024)?;
        let (_, payload) = decode_publication_stage(&bytes)?;
        if publication.reference != reference
            || publication.digest != digest
            || ArtifactDigest::digest(payload) != digest
        {
            return Err(ArtifactError::InvalidManifest(
                "artifact publication journal binding mismatch",
            ));
        }
        Ok(publication)
    }

    pub fn staged_publication(
        &self,
        reference: ArtifactReference,
    ) -> Result<ArtifactPublication, ArtifactError> {
        let bytes =
            read_regular_file_bounded(&self.publication_stage_path(reference)?, 64 * 1024 * 1024)?;
        let (publication, payload) = decode_publication_stage(&bytes)?;
        if publication.reference != reference
            || ArtifactDigest::digest(payload) != publication.digest
        {
            return Err(ArtifactError::InvalidManifest(
                "artifact publication stage binding mismatch",
            ));
        }
        Ok(publication)
    }

    pub fn read_staged_publication(
        &self,
        publication: &ArtifactPublication,
        binding: &ArtifactEnvelopeBinding,
        max_payload_bytes: usize,
    ) -> Result<Vec<u8>, ArtifactError> {
        let maximum = max_payload_bytes
            .checked_add(16 * 1024)
            .ok_or(ArtifactError::TooLarge {
                size: u64::MAX,
                max: max_payload_bytes as u64,
            })?;
        let bytes = read_regular_file_bounded(
            &self.publication_stage_path(publication.reference)?,
            maximum,
        )?;
        let (found, envelope) = decode_publication_stage(&bytes)?;
        if found.reference != publication.reference
            || found.digest != publication.digest
            || found.manifest != publication.manifest
        {
            return Err(ArtifactError::InvalidManifest(
                "artifact publication stage binding mismatch",
            ));
        }
        let payload = binding.open(envelope)?;
        if payload.len() > max_payload_bytes {
            return Err(ArtifactError::TooLarge {
                size: payload.len() as u64,
                max: max_payload_bytes as u64,
            });
        }
        Ok(payload.to_vec())
    }

    pub fn remove_publication_stage(
        &self,
        reference: ArtifactReference,
    ) -> Result<bool, ArtifactError> {
        self.quarantine_remove_file(&self.publication_stage_path(reference)?, None, None)
    }

    pub(crate) fn callback_references(
        &self,
        principal: &str,
        project: &str,
        run: &str,
        callback_id: &str,
    ) -> Result<Vec<ArtifactReference>, ArtifactError> {
        let mut references = BTreeSet::new();
        for reference in self.staged_publications()? {
            let bytes = read_regular_file_bounded(
                &self.publication_stage_path(reference)?,
                64 * 1024 * 1024,
            )?;
            let (publication, envelope) = decode_publication_stage(&bytes)?;
            if publication.manifest.principal == principal
                && publication.manifest.project == project
                && callback_binding(principal, project, run, callback_id).matches(envelope)?
            {
                references.insert(reference);
            }
        }
        for shard in fs::read_dir(self.root.join("records"))? {
            let shard = shard?;
            let Some(shard_name) = shard.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !fs::symlink_metadata(shard.path())?.file_type().is_dir()
                || !valid_hex(&shard_name, 2)
            {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let Some(rest) = entry.file_name().to_str().and_then(|name| {
                    name.strip_suffix(".reference")
                        .or_else(|| name.strip_suffix(".pending"))
                        .map(str::to_owned)
                }) else {
                    continue;
                };
                if !valid_hex(&rest, 62)
                    || !fs::symlink_metadata(entry.path())?.file_type().is_file()
                {
                    continue;
                }
                let record = ArtifactReferenceManifest::decode(&read_regular_file_bounded(
                    &entry.path(),
                    8192,
                )?)?;
                if record.artifact.principal != principal || record.artifact.project != project {
                    continue;
                }
                let Ok(envelope) = self.open_bytes_bounded(record.digest, 64 * 1024 * 1024) else {
                    continue;
                };
                if callback_binding(principal, project, run, callback_id).matches(&envelope)? {
                    references.insert(ArtifactReference::parse(&format!(
                        "artifact-ref:{shard_name}{rest}"
                    ))?);
                }
            }
        }
        Ok(references.into_iter().collect())
    }

    pub(crate) fn erase_owned_reference(
        &self,
        reference: ArtifactReference,
        principal: &str,
        project: &str,
    ) -> Result<(), ArtifactError> {
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut digests = BTreeSet::new();
        let stage_path = self.publication_stage_path(reference)?;
        if stage_path.exists() {
            let bytes = read_regular_file_bounded(&stage_path, 64 * 1024 * 1024)?;
            let (publication, _) = decode_publication_stage(&bytes)?;
            if publication.manifest.principal != principal
                || publication.manifest.project != project
            {
                return Err(ArtifactError::AccessDenied);
            }
            digests.insert(publication.digest);
            self.quarantine_remove_file(&stage_path, None, None)?;
        }
        for path in [
            self.pending_reference_manifest_path(reference)?,
            self.reference_manifest_path(reference)?,
        ] {
            if path.exists() {
                let record =
                    ArtifactReferenceManifest::decode(&read_regular_file_bounded(&path, 8192)?)?;
                if record.artifact.principal != principal || record.artifact.project != project {
                    return Err(ArtifactError::AccessDenied);
                }
                digests.insert(record.digest);
                self.quarantine_remove_file(&path, None, None)?;
            }
        }
        for digest in digests {
            if self.any_reference_manifest(digest)? || self.has_persistent_owner(digest)? {
                continue;
            }
            for path in [
                self.object_path(digest)?,
                self.manifest_path(digest)?,
                self.pending_manifest_path(digest)?,
                self.issued_path(digest)?,
            ] {
                self.quarantine_remove_file(&path, None, None)?;
            }
        }
        Ok(())
    }

    pub(crate) fn erase_callback_reference(
        &self,
        reference: ArtifactReference,
        binding: &ArtifactEnvelopeBinding,
    ) -> Result<bool, ArtifactError> {
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stage_path = self.publication_stage_path(reference)?;
        let mut found = false;
        if stage_path.exists() {
            let bytes = read_regular_file_bounded(&stage_path, 64 * 1024 * 1024)?;
            let (publication, envelope) = decode_publication_stage(&bytes)?;
            if publication.manifest.principal != binding.principal
                || publication.manifest.project != binding.project
                || !binding.matches(envelope)?
            {
                return Ok(false);
            }
            found = true;
        }
        for path in [
            self.pending_reference_manifest_path(reference)?,
            self.reference_manifest_path(reference)?,
        ] {
            if path.exists() {
                let record =
                    ArtifactReferenceManifest::decode(&read_regular_file_bounded(&path, 8192)?)?;
                if record.artifact.principal != binding.principal
                    || record.artifact.project != binding.project
                {
                    return Ok(false);
                }
                let envelope = self.open_bytes_bounded(record.digest, 64 * 1024 * 1024)?;
                if !binding.matches(&envelope)? {
                    return Ok(false);
                }
                found = true;
            }
        }
        if !found {
            return Ok(false);
        }
        let mut digests = BTreeSet::new();
        if stage_path.exists() {
            let bytes = read_regular_file_bounded(&stage_path, 64 * 1024 * 1024)?;
            let (publication, _) = decode_publication_stage(&bytes)?;
            digests.insert(publication.digest);
            self.quarantine_remove_file(&stage_path, None, None)?;
        }
        for path in [
            self.pending_reference_manifest_path(reference)?,
            self.reference_manifest_path(reference)?,
        ] {
            if path.exists() {
                let record =
                    ArtifactReferenceManifest::decode(&read_regular_file_bounded(&path, 8192)?)?;
                digests.insert(record.digest);
                self.quarantine_remove_file(&path, None, None)?;
            }
        }
        for digest in digests {
            if self.any_reference_manifest(digest)? || self.has_persistent_owner(digest)? {
                continue;
            }
            for path in [
                self.object_path(digest)?,
                self.manifest_path(digest)?,
                self.pending_manifest_path(digest)?,
                self.issued_path(digest)?,
            ] {
                self.quarantine_remove_file(&path, None, None)?;
            }
        }
        Ok(true)
    }

    pub fn staged_publications(&self) -> Result<Vec<ArtifactReference>, ArtifactError> {
        let mut publications = Vec::new();
        for entry in fs::read_dir(self.root.join("staging"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(hex) = name
                .to_str()
                .and_then(|name| name.strip_prefix("publication-"))
                .and_then(|name| name.strip_suffix(".stage"))
            else {
                continue;
            };
            if fs::symlink_metadata(entry.path())?.file_type().is_file() && valid_hex(hex, 64) {
                publications.push(ArtifactReference::parse(&format!("artifact-ref:{hex}"))?);
            }
        }
        publications.sort_unstable();
        Ok(publications)
    }

    pub(crate) fn put_with_reference(
        &self,
        bytes: &[u8],
        metadata: ArtifactMetadata,
        reference: ArtifactReference,
    ) -> Result<VerifiedArtifact, ArtifactError> {
        let reference_path = self.reference_manifest_path(reference)?;
        if reference_path.exists() {
            let artifact = self.open_reference(reference)?;
            let expected = ArtifactManifest {
                size: bytes.len() as u64,
                media_type: metadata.media_type.clone(),
                class: metadata.class,
                principal: metadata.principal.clone(),
                project: metadata.project.clone(),
                retention: metadata.retention,
                stored_at_unix_micros: metadata.stored_at_unix_micros,
            };
            if artifact.digest == ArtifactDigest(*blake3::hash(bytes).as_bytes())
                && artifact.manifest == expected
            {
                return Ok(artifact);
            }
            return Err(ArtifactError::InvalidManifest(
                "artifact reference collision",
            ));
        }
        self.stage_chunks_until([bytes], bytes.len(), metadata, None, Some(reference))?
            .promote()
    }

    pub fn stage(
        &self,
        bytes: &[u8],
        metadata: ArtifactMetadata,
    ) -> Result<StagedArtifact<'_>, ArtifactError> {
        self.stage_chunks([bytes], bytes.len(), metadata)
    }

    pub fn stage_chunks<'a>(
        &self,
        chunks: impl IntoIterator<Item = &'a [u8]>,
        size: usize,
        metadata: ArtifactMetadata,
    ) -> Result<StagedArtifact<'_>, ArtifactError> {
        self.stage_chunks_until(chunks, size, metadata, None, None)
    }

    pub fn stage_chunks_before<'a>(
        &self,
        chunks: impl IntoIterator<Item = &'a [u8]>,
        size: usize,
        metadata: ArtifactMetadata,
        deadline: Instant,
    ) -> Result<StagedArtifact<'_>, ArtifactError> {
        self.stage_chunks_until(chunks, size, metadata, Some(deadline), None)
    }

    pub(crate) fn stage_chunks_with_reference_before<'a>(
        &self,
        chunks: impl IntoIterator<Item = &'a [u8]>,
        size: usize,
        metadata: ArtifactMetadata,
        reference: ArtifactReference,
        deadline: Instant,
    ) -> Result<StagedArtifact<'_>, ArtifactError> {
        self.stage_chunks_until(chunks, size, metadata, Some(deadline), Some(reference))
    }

    pub fn stage_reader_before(
        &self,
        reader: &mut impl Read,
        size: u64,
        metadata: ArtifactMetadata,
        deadline: Instant,
    ) -> Result<StagedArtifact<'_>, ArtifactError> {
        check_artifact_deadline(Some(deadline))?;
        metadata.validate()?;
        self.check_layout()?;
        let reference = ArtifactReference::generate()?;
        let object_temp = self.temp_path("bytes");
        let manifest_temp = self.temp_path("manifest");
        let staged = (|| {
            let mut file = secure_create_new(&object_temp)?;
            let mut hash = blake3::Hasher::new();
            let mut written = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                check_artifact_deadline(Some(deadline))?;
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                written = written
                    .checked_add(count as u64)
                    .ok_or(ArtifactError::InvalidManifest("artifact size overflow"))?;
                if written > size {
                    return Err(ArtifactError::InvalidManifest(
                        "artifact exceeds staged size",
                    ));
                }
                file.write_all(&buffer[..count])?;
                hash.update(&buffer[..count]);
            }
            if written != size {
                return Err(ArtifactError::InvalidManifest(
                    "artifact is shorter than staged size",
                ));
            }
            sync_file_until(&file, Some(deadline))?;
            let digest = ArtifactDigest(*hash.finalize().as_bytes());
            verify_file_until(&object_temp, digest, size, Some(deadline))?;
            let manifest = ArtifactManifest {
                size,
                media_type: metadata.media_type,
                class: metadata.class,
                principal: metadata.principal,
                project: metadata.project,
                retention: metadata.retention,
                stored_at_unix_micros: metadata.stored_at_unix_micros,
            };
            let mut file = secure_create_new(&manifest_temp)?;
            file.write_all(&manifest.canonical_bytes())?;
            sync_file_until(&file, Some(deadline))?;
            Ok(StagedArtifact {
                store: self,
                digest,
                reference,
                manifest,
                object_temp: object_temp.clone(),
                manifest_temp: manifest_temp.clone(),
                complete: false,
            })
        })();
        if staged.is_err() {
            let _ = self.quarantine_remove_file(&object_temp, None, None);
            let _ = self.quarantine_remove_file(&manifest_temp, None, None);
        }
        staged
    }

    fn stage_chunks_until<'a>(
        &self,
        chunks: impl IntoIterator<Item = &'a [u8]>,
        size: usize,
        metadata: ArtifactMetadata,
        deadline: Option<Instant>,
        reference: Option<ArtifactReference>,
    ) -> Result<StagedArtifact<'_>, ArtifactError> {
        check_artifact_deadline(deadline)?;
        metadata.validate()?;
        self.check_layout()?;
        let reference = reference.map_or_else(ArtifactReference::generate, Ok)?;
        check_artifact_deadline(deadline)?;
        let object_temp = self.temp_path("bytes");
        let manifest_temp = self.temp_path("manifest");
        let staged = (|| {
            check_artifact_deadline(deadline)?;
            let mut file = secure_create_new(&object_temp)?;
            let mut hash = blake3::Hasher::new();
            let mut written = 0_usize;
            for part in chunks {
                for chunk in part.chunks(64 * 1024) {
                    check_artifact_deadline(deadline)?;
                    written = written
                        .checked_add(chunk.len())
                        .ok_or(ArtifactError::InvalidManifest("artifact size overflow"))?;
                    if written > size {
                        return Err(ArtifactError::InvalidManifest(
                            "artifact exceeds staged size",
                        ));
                    }
                    file.write_all(chunk)?;
                    check_artifact_deadline(deadline)?;
                    hash.update(chunk);
                }
            }
            if written != size {
                return Err(ArtifactError::InvalidManifest(
                    "artifact is shorter than staged size",
                ));
            }
            check_artifact_deadline(deadline)?;
            sync_file_until(&file, deadline)?;
            let digest = ArtifactDigest(*hash.finalize().as_bytes());
            verify_file_until(&object_temp, digest, size as u64, deadline)?;
            let manifest = ArtifactManifest {
                size: size as u64,
                media_type: metadata.media_type,
                class: metadata.class,
                principal: metadata.principal,
                project: metadata.project,
                retention: metadata.retention,
                stored_at_unix_micros: metadata.stored_at_unix_micros,
            };
            check_artifact_deadline(deadline)?;
            let mut file = secure_create_new(&manifest_temp)?;
            file.write_all(&manifest.canonical_bytes())?;
            check_artifact_deadline(deadline)?;
            sync_file_until(&file, deadline)?;
            Ok(StagedArtifact {
                store: self,
                digest,
                reference,
                manifest,
                object_temp: object_temp.clone(),
                manifest_temp: manifest_temp.clone(),
                complete: false,
            })
        })();
        if staged.is_err() {
            let _ = self.quarantine_remove_file(&object_temp, None, None);
            let _ = self.quarantine_remove_file(&manifest_temp, None, None);
        }
        staged
    }

    pub fn put_with_hook(
        &self,
        bytes: &[u8],
        metadata: ArtifactMetadata,
        mut crash: impl FnMut(CrashPoint) -> bool,
    ) -> Result<VerifiedArtifact, ArtifactError> {
        metadata.validate()?;
        self.check_layout()?;
        let reference = ArtifactReference::generate()?;
        let temp_path = self.temp_path("bytes");
        let mut temp = secure_create_new(&temp_path)?;
        inject(&mut crash, CrashPoint::AfterTempCreated)?;
        let mut hash = blake3::Hasher::new();
        for chunk in bytes.chunks(64 * 1024) {
            temp.write_all(chunk)?;
            hash.update(chunk);
        }
        inject(&mut crash, CrashPoint::AfterBytesWritten)?;
        temp.sync_all()?;
        inject(&mut crash, CrashPoint::AfterFileSynced)?;

        let digest = ArtifactDigest(*hash.finalize().as_bytes());
        verify_file(&temp_path, digest, bytes.len() as u64)?;
        inject(&mut crash, CrashPoint::AfterHashVerified)?;

        let manifest = ArtifactManifest {
            size: bytes.len() as u64,
            media_type: metadata.media_type,
            class: metadata.class,
            principal: metadata.principal,
            project: metadata.project,
            retention: metadata.retention,
            stored_at_unix_micros: metadata.stored_at_unix_micros,
        };
        let object_path = self.object_path(digest)?;
        if object_path.exists() {
            verify_file(&object_path, digest, manifest.size)?;
            self.quarantine_remove_file(&temp_path, None, None)?;
        } else {
            rename_until(&temp_path, &object_path, None)?;
        }
        inject(&mut crash, CrashPoint::AfterObjectRenamed)?;
        sync_directory(object_path.parent().expect("object has parent"))?;
        inject(&mut crash, CrashPoint::AfterObjectDirectorySynced)?;

        let manifest_bytes = manifest.canonical_bytes();
        let manifest_temp = self.temp_path("manifest");
        let mut file = secure_create_new(&manifest_temp)?;
        file.write_all(&manifest_bytes)?;
        inject(&mut crash, CrashPoint::AfterManifestWritten)?;
        file.sync_all()?;
        inject(&mut crash, CrashPoint::AfterManifestSynced)?;
        let manifest_path = self.manifest_path(digest)?;
        if manifest_path.exists() {
            self.quarantine_remove_file(&manifest_temp, None, None)?;
        } else {
            rename_until(&manifest_temp, &manifest_path, None)?;
        }
        inject(&mut crash, CrashPoint::AfterManifestRenamed)?;
        sync_directory(manifest_path.parent().expect("manifest has parent"))?;
        inject(&mut crash, CrashPoint::AfterManifestDirectorySynced)?;
        self.write_reference_manifest(reference, digest, &manifest, None)?;
        self.publish_reference_manifest(reference, None)?;
        Ok(VerifiedArtifact {
            digest,
            reference,
            manifest,
        })
    }

    pub fn open_bytes(&self, digest: ArtifactDigest) -> Result<Vec<u8>, ArtifactError> {
        self.open_bytes_bounded(digest, usize::MAX)
    }

    pub fn open_bytes_bounded(
        &self,
        digest: ArtifactDigest,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ArtifactError> {
        self.with_verified_reader(digest, max_bytes, |manifest, file| {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(manifest.size as usize)
                .map_err(|_| ArtifactError::TooLarge {
                    size: manifest.size,
                    max: max_bytes as u64,
                })?;
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
    }

    pub fn with_verified_reader<T>(
        &self,
        digest: ArtifactDigest,
        max_bytes: usize,
        read: impl FnOnce(&ArtifactManifest, &mut File) -> Result<T, ArtifactError>,
    ) -> Result<T, ArtifactError> {
        self.with_verified_reader_until(digest, max_bytes, None, read)
    }

    pub fn with_verified_reader_before<T>(
        &self,
        digest: ArtifactDigest,
        max_bytes: usize,
        deadline: Instant,
        read: impl FnOnce(&ArtifactManifest, &mut File) -> Result<T, ArtifactError>,
    ) -> Result<T, ArtifactError> {
        self.with_verified_reader_until(digest, max_bytes, Some(deadline), read)
    }

    fn with_verified_reader_until<T>(
        &self,
        digest: ArtifactDigest,
        max_bytes: usize,
        deadline: Option<Instant>,
        read: impl FnOnce(&ArtifactManifest, &mut File) -> Result<T, ArtifactError>,
    ) -> Result<T, ArtifactError> {
        check_artifact_deadline(deadline)?;
        self.check_layout()?;
        let manifest_path = self.manifest_path(digest)?;
        check_artifact_deadline(deadline)?;
        if !manifest_path.exists() {
            return Err(ArtifactError::Missing(digest));
        }
        let manifest = ArtifactManifest::decode(&read_regular_file_bounded_until(
            &manifest_path,
            4096,
            deadline,
        )?)?;
        if manifest.size > max_bytes as u64 || manifest.size > usize::MAX as u64 {
            return Err(ArtifactError::TooLarge {
                size: manifest.size,
                max: max_bytes as u64,
            });
        }
        let object_path = self.object_path(digest)?;
        check_artifact_deadline(deadline)?;
        if !object_path.exists() {
            return Err(ArtifactError::Missing(digest));
        }
        verify_file_until(&object_path, digest, manifest.size, deadline)?;
        check_artifact_deadline(deadline)?;
        let mut file = secure_open_read(&object_path)?;
        check_artifact_deadline(deadline)?;
        file.seek(SeekFrom::Start(0))?;
        check_artifact_deadline(deadline)?;
        read(&manifest, &mut file)
    }

    pub fn open_verified(&self, digest: ArtifactDigest) -> Result<VerifiedArtifact, ArtifactError> {
        self.check_layout()?;
        let manifest_path = self.manifest_path(digest)?;
        if !manifest_path.exists() {
            return Err(ArtifactError::Missing(digest));
        }
        let manifest = ArtifactManifest::decode(&read_regular_file_bounded(&manifest_path, 4096)?)?;
        let object_path = self.object_path(digest)?;
        if !object_path.exists() {
            return Err(ArtifactError::Missing(digest));
        }
        verify_file(&object_path, digest, manifest.size)?;
        Ok(VerifiedArtifact {
            digest,
            reference: ArtifactReference::compatibility(digest),
            manifest,
        })
    }

    pub fn resolve_reference(
        &self,
        authenticated: &crate::api::auth::contract::AuthenticatedPrincipal,
        reference: ArtifactReference,
    ) -> Result<VerifiedArtifact, ArtifactError> {
        let grants = authenticated.grant_snapshot();
        if !grants
            .grants()
            .contains(&crate::domain::config::Grant::WorkspaceRead)
        {
            return Err(ArtifactError::AccessDenied);
        }
        let artifact = self.open_reference(reference)?;
        if artifact.manifest.principal != grants.principal_id().to_string()
            || artifact.manifest.project != grants.project_id().to_string()
        {
            return Err(ArtifactError::AccessDenied);
        }
        Ok(artifact)
    }

    pub(crate) fn open_reference(
        &self,
        reference: ArtifactReference,
    ) -> Result<VerifiedArtifact, ArtifactError> {
        self.check_layout()?;
        let record = ArtifactReferenceManifest::decode(&read_regular_file_bounded(
            &self.reference_manifest_path(reference)?,
            8192,
        )?)?;
        verify_file(
            &self.object_path(record.digest)?,
            record.digest,
            record.artifact.size,
        )?;
        Ok(VerifiedArtifact {
            digest: record.digest,
            reference,
            manifest: record.artifact,
        })
    }

    pub(crate) fn open_reference_optional(
        &self,
        reference: ArtifactReference,
    ) -> Result<Option<VerifiedArtifact>, ArtifactError> {
        if !self.reference_manifest_path(reference)?.exists() {
            return Ok(None);
        }
        self.open_reference(reference).map(Some)
    }

    pub(crate) fn verify_content(
        &self,
        digest: ArtifactDigest,
        size: u64,
    ) -> Result<(), ArtifactError> {
        self.check_layout()?;
        verify_file(&self.object_path(digest)?, digest, size)
    }

    pub(crate) fn with_reference_reader<T>(
        &self,
        reference: ArtifactReference,
        read: impl FnOnce(&ArtifactManifest, &mut File) -> Result<T, ArtifactError>,
    ) -> Result<T, ArtifactError> {
        let artifact = self.open_reference(reference)?;
        let mut file = secure_open_read(&self.object_path(artifact.digest)?)?;
        read(&artifact.manifest, &mut file)
    }

    pub(crate) fn reference_manifests(
        &self,
    ) -> Result<Vec<(ArtifactReference, Vec<u8>)>, ArtifactError> {
        self.check_layout()?;
        let mut references = Vec::new();
        for shard in fs::read_dir(self.root.join("records"))? {
            let shard = shard?;
            let shard_name = shard.file_name();
            let shard_name = shard_name
                .to_str()
                .ok_or_else(|| ArtifactError::UnsafePath(shard.path()))?;
            if !fs::symlink_metadata(shard.path())?.file_type().is_dir()
                || !valid_hex(shard_name, 2)
            {
                return Err(ArtifactError::UnsafePath(shard.path()));
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| ArtifactError::UnsafePath(entry.path()))?;
                let Some(rest) = name.strip_suffix(".reference") else {
                    if name.ends_with(".pending") {
                        continue;
                    }
                    return Err(ArtifactError::UnsafePath(entry.path()));
                };
                if !fs::symlink_metadata(entry.path())?.file_type().is_file()
                    || !valid_hex(rest, 62)
                {
                    return Err(ArtifactError::UnsafePath(entry.path()));
                }
                let reference =
                    ArtifactReference::parse(&format!("artifact-ref:{shard_name}{rest}"))?;
                let bytes = read_regular_file_bounded(&entry.path(), 8192)?;
                let record = ArtifactReferenceManifest::decode(&bytes)?;
                if record.canonical_bytes() != bytes {
                    return Err(ArtifactError::InvalidManifest(
                        "artifact reference manifest is not canonical",
                    ));
                }
                verify_file(
                    &self.object_path(record.digest)?,
                    record.digest,
                    record.artifact.size,
                )?;
                references.push((reference, bytes));
            }
        }
        references.sort_by_key(|(reference, _)| *reference);
        Ok(references)
    }

    pub(crate) fn restore_reference_manifest(
        &self,
        reference: ArtifactReference,
        bytes: &[u8],
    ) -> Result<VerifiedArtifact, ArtifactError> {
        self.check_layout()?;
        let record = ArtifactReferenceManifest::decode(bytes)?;
        if record.canonical_bytes() != bytes {
            return Err(ArtifactError::InvalidManifest(
                "artifact reference manifest is not canonical",
            ));
        }
        verify_file(
            &self.object_path(record.digest)?,
            record.digest,
            record.artifact.size,
        )?;
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let published = self.reference_manifest_path(reference)?;
        if published.exists() {
            if read_regular_file_bounded(&published, 8192)? != bytes {
                return Err(ArtifactError::InvalidManifest(
                    "artifact reference collision",
                ));
            }
        } else {
            self.write_reference_manifest(reference, record.digest, &record.artifact, None)?;
            self.publish_reference_manifest(reference, None)?;
        }
        self.open_reference(reference)
    }

    fn write_reference_manifest(
        &self,
        reference: ArtifactReference,
        digest: ArtifactDigest,
        manifest: &ArtifactManifest,
        deadline: Option<Instant>,
    ) -> Result<(), ArtifactError> {
        let path = self.pending_reference_manifest_path(reference)?;
        let expected = ArtifactReferenceManifest {
            digest,
            artifact: manifest.clone(),
        }
        .canonical_bytes();
        if path.exists() {
            if read_regular_file_bounded_until(&path, 8192, deadline)? == expected {
                return Ok(());
            }
            return Err(ArtifactError::InvalidManifest(
                "artifact reference collision",
            ));
        }
        let temp = self.temp_path("reference-manifest");
        let result = (|| {
            let mut file = secure_create_new(&temp)?;
            file.write_all(&expected)?;
            sync_file_until(&file, deadline)?;
            rename_until(&temp, &path, deadline)?;
            sync_directory_until(path.parent().expect("reference has parent"), deadline)
        })();
        if result.is_err() {
            let _ = self.quarantine_remove_file(&temp, None, None);
        }
        result
    }

    fn publish_reference_manifest(
        &self,
        reference: ArtifactReference,
        deadline: Option<Instant>,
    ) -> Result<(), ArtifactError> {
        let pending = self.pending_reference_manifest_path(reference)?;
        let published = self.reference_manifest_path(reference)?;
        if published.exists() {
            if read_regular_file_bounded_until(&pending, 8192, deadline)?
                != read_regular_file_bounded_until(&published, 8192, deadline)?
            {
                return Err(ArtifactError::InvalidManifest(
                    "artifact reference collision",
                ));
            }
            self.quarantine_remove_file(&pending, None, deadline)?;
            return sync_directory_until(
                published.parent().expect("reference has parent"),
                deadline,
            );
        }
        rename_until(&pending, &published, deadline)?;
        sync_directory_until(published.parent().expect("reference has parent"), deadline)
    }

    pub fn recover_verified_before(
        &self,
        digest: ArtifactDigest,
        deadline: Instant,
    ) -> Result<VerifiedArtifact, ArtifactError> {
        check_artifact_deadline(Some(deadline))?;
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let manifest_path = self.manifest_path(digest)?;
        if manifest_path.exists() {
            return self.open_verified(digest);
        }
        let pending = self.pending_manifest_path(digest)?;
        let manifest = ArtifactManifest::decode(&read_regular_file_bounded_until(
            &pending,
            4096,
            Some(deadline),
        )?)?;
        verify_file_until(
            &self.object_path(digest)?,
            digest,
            manifest.size,
            Some(deadline),
        )?;
        rename_until(&pending, &manifest_path, Some(deadline))?;
        sync_directory_until(
            manifest_path.parent().expect("manifest has parent"),
            Some(deadline),
        )?;
        Ok(VerifiedArtifact {
            digest,
            reference: ArtifactReference::compatibility(digest),
            manifest,
        })
    }

    pub fn acquire_lease_before(
        &self,
        digest: ArtifactDigest,
        owner: &str,
        deadline: Instant,
    ) -> Result<ArtifactLease, ArtifactError> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|_| ArtifactError::InvalidManifest("secure randomness failed"))?;
        let id = hex_bytes(&nonce);
        self.acquire_lease_with_id_before(digest, &id, owner, deadline)
    }

    pub fn acquire_lease_with_id_before(
        &self,
        digest: ArtifactDigest,
        id: &str,
        owner: &str,
        deadline: Instant,
    ) -> Result<ArtifactLease, ArtifactError> {
        self.acquire_lease_with_id_before_with_hook(digest, id, owner, deadline, |_| false)
    }

    #[doc(hidden)]
    pub fn acquire_lease_with_id_before_with_hook(
        &self,
        digest: ArtifactDigest,
        id: &str,
        owner: &str,
        deadline: Instant,
        mut crash: impl FnMut(CrashPoint) -> bool,
    ) -> Result<ArtifactLease, ArtifactError> {
        check_artifact_deadline(Some(deadline))?;
        if !valid_hex(id, 32) || !valid_field(owner, 255) {
            return Err(ArtifactError::InvalidManifest("invalid artifact lease"));
        }
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.object_path(digest)?.exists()
            || (!self.manifest_path(digest)?.exists()
                && !self.pending_manifest_path(digest)?.exists())
        {
            return Err(ArtifactError::Missing(digest));
        }
        let path = self.ownership_path("leases", digest, id, ".lease")?;
        let temp = self.ownership_path("leases", digest, id, ".lease.tmp")?;
        let expected = format!("kit-artifact-lease-v1\ndigest={digest}\nowner={owner}\n");
        if expected.len() > 512 {
            return Err(ArtifactError::InvalidManifest(
                "artifact lease is too large",
            ));
        }
        loop {
            match read_regular_file_bounded_until(&path, 512, Some(deadline)) {
                Ok(bytes) if bytes == expected.as_bytes() => {
                    self.remove_authorized_lease_file(&temp, digest, id, Some(deadline))?;
                    break;
                }
                Ok(bytes) => {
                    if lease_record_well_formed(&bytes) {
                        return Err(ArtifactError::InvalidManifest(
                            "artifact lease binding mismatch",
                        ));
                    }
                    self.remove_authorized_lease_file(&path, digest, id, Some(deadline))?;
                }
                Err(ArtifactError::TooLarge { .. }) => {
                    self.remove_authorized_lease_file(&path, digest, id, Some(deadline))?;
                }
                Err(ArtifactError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }

            match read_regular_file_bounded_until(&temp, 512, Some(deadline)) {
                Ok(bytes) if bytes == expected.as_bytes() => {}
                Ok(bytes) => {
                    if lease_record_well_formed(&bytes) {
                        return Err(ArtifactError::InvalidManifest(
                            "artifact lease binding mismatch",
                        ));
                    }
                    self.remove_authorized_lease_file(&temp, digest, id, Some(deadline))?;
                    continue;
                }
                Err(ArtifactError::TooLarge { .. }) => {
                    self.remove_authorized_lease_file(&temp, digest, id, Some(deadline))?;
                    continue;
                }
                Err(ArtifactError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    let mut file = secure_create_new(&temp)?;
                    inject(&mut crash, CrashPoint::AfterLeaseTempCreated)?;
                    let split = expected.len().div_ceil(2);
                    file.write_all(&expected.as_bytes()[..split])?;
                    inject(&mut crash, CrashPoint::AfterLeasePartialWrite)?;
                    file.write_all(&expected.as_bytes()[split..])?;
                    sync_file_until(&file, Some(deadline))?;
                    inject(&mut crash, CrashPoint::AfterLeaseFileSynced)?;
                }
                Err(error) => return Err(error),
            }
            match rename_until(&temp, &path, Some(deadline)) {
                Ok(()) => {}
                Err(ArtifactError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
            inject(&mut crash, CrashPoint::AfterLeaseRenamed)?;
            sync_directory_until(path.parent().expect("lease has parent"), Some(deadline))?;
            inject(&mut crash, CrashPoint::AfterLeaseDirectorySynced)?;
            break;
        }
        Ok(ArtifactLease {
            digest,
            id: id.to_owned(),
            owner: owner.to_owned(),
        })
    }

    pub fn open_lease(
        &self,
        digest: ArtifactDigest,
        id: &str,
        owner: &str,
    ) -> Result<ArtifactLease, ArtifactError> {
        if !valid_hex(id, 32) || !valid_field(owner, 255) {
            return Err(ArtifactError::InvalidManifest("invalid artifact lease"));
        }
        let path = self.ownership_path("leases", digest, id, ".lease")?;
        let expected = format!("kit-artifact-lease-v1\ndigest={digest}\nowner={owner}\n");
        if read_regular_file_bounded(&path, 512)? != expected.as_bytes() {
            return Err(ArtifactError::InvalidManifest(
                "artifact lease binding mismatch",
            ));
        }
        Ok(ArtifactLease {
            digest,
            id: id.to_owned(),
            owner: owner.to_owned(),
        })
    }

    pub fn release_lease_before(
        &self,
        lease: &ArtifactLease,
        deadline: Instant,
    ) -> Result<(), ArtifactError> {
        self.release_lease_with_id_before(lease.digest, &lease.id, &lease.owner, deadline)
    }

    pub fn release_lease_with_id_before(
        &self,
        digest: ArtifactDigest,
        id: &str,
        owner: &str,
        deadline: Instant,
    ) -> Result<(), ArtifactError> {
        check_artifact_deadline(Some(deadline))?;
        if !valid_hex(id, 32) || !valid_field(owner, 255) {
            return Err(ArtifactError::InvalidManifest("invalid artifact lease"));
        }
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = self.ownership_path("leases", digest, id, ".lease")?;
        let temp = self.ownership_path("leases", digest, id, ".lease.tmp")?;
        let expected = format!("kit-artifact-lease-v1\ndigest={digest}\nowner={owner}\n");
        match read_regular_file_bounded_until(&path, 512, Some(deadline)) {
            Ok(bytes) if bytes == expected.as_bytes() => {}
            Ok(bytes) if lease_record_well_formed(&bytes) => {
                return Err(ArtifactError::InvalidManifest(
                    "artifact lease binding mismatch",
                ));
            }
            Ok(_) | Err(ArtifactError::TooLarge { .. }) => {}
            Err(ArtifactError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.remove_authorized_lease_file(&path, digest, id, Some(deadline))?;
        self.remove_authorized_lease_file(&temp, digest, id, Some(deadline))?;
        Ok(())
    }

    pub fn transfer_lease_to_reference_before(
        &self,
        lease: &ArtifactLease,
        owner: &str,
        deadline: Instant,
    ) -> Result<(), ArtifactError> {
        check_artifact_deadline(Some(deadline))?;
        if !valid_field(owner, 255) {
            return Err(ArtifactError::InvalidManifest(
                "invalid artifact reference owner",
            ));
        }
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lease_path = self.ownership_path("leases", lease.digest, &lease.id, ".lease")?;
        if !lease_path.exists() {
            return Err(ArtifactError::InvalidManifest("artifact lease is missing"));
        }
        let reference_id = blake3::hash(owner.as_bytes()).to_hex().to_string();
        let reference =
            self.ownership_path("references", lease.digest, &reference_id, ".reference")?;
        if !reference.exists() {
            let temp = self.temp_path("reference");
            let mut file = secure_create_new(&temp)?;
            write!(
                file,
                "kit-artifact-reference-v1\ndigest={}\nowner={owner}\n",
                lease.digest
            )?;
            sync_file_until(&file, Some(deadline))?;
            rename_until(&temp, &reference, Some(deadline))?;
            sync_directory_until(
                reference.parent().expect("reference has parent"),
                Some(deadline),
            )?;
        }
        let expected = format!(
            "kit-artifact-lease-v1\ndigest={}\nowner={}\n",
            lease.digest, lease.owner
        );
        self.quarantine_remove_file(&lease_path, Some(expected.as_bytes()), Some(deadline))?;
        Ok(())
    }

    pub fn reference_exists(
        &self,
        digest: ArtifactDigest,
        owner: &str,
    ) -> Result<bool, ArtifactError> {
        if !valid_field(owner, 255) {
            return Err(ArtifactError::InvalidManifest(
                "invalid artifact reference owner",
            ));
        }
        let id = blake3::hash(owner.as_bytes()).to_hex().to_string();
        let path = self.ownership_path("references", digest, &id, ".reference")?;
        let expected = format!("kit-artifact-reference-v1\ndigest={digest}\nowner={owner}\n");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                Ok(read_regular_file_bounded(&path, 512)? == expected.as_bytes())
            }
            Ok(_) => Err(ArtifactError::UnsafePath(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn release_reference_before(
        &self,
        digest: ArtifactDigest,
        owner: &str,
        deadline: Instant,
    ) -> Result<(), ArtifactError> {
        check_artifact_deadline(Some(deadline))?;
        if !valid_field(owner, 255) {
            return Err(ArtifactError::InvalidManifest(
                "invalid artifact reference owner",
            ));
        }
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = blake3::hash(owner.as_bytes()).to_hex().to_string();
        let path = self.ownership_path("references", digest, &id, ".reference")?;
        let expected = format!("kit-artifact-reference-v1\ndigest={digest}\nowner={owner}\n");
        self.quarantine_remove_file(&path, Some(expected.as_bytes()), Some(deadline))?;
        Ok(())
    }

    pub fn workspace_artifact_is_issued(
        &self,
        digest: ArtifactDigest,
    ) -> Result<bool, ArtifactError> {
        let path = self.issued_path(digest)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                Ok(read_regular_file_bounded(&path, 128)? == b"kit-workspace-artifact-issued-v1\n")
            }
            Ok(_) => Err(ArtifactError::UnsafePath(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn commit_reference<T, E>(
        &self,
        artifact: &VerifiedArtifact,
        commit: impl FnOnce(ArtifactDigest) -> Result<T, E>,
    ) -> Result<T, ReferenceError<E>> {
        self.commit_reference_with_hook(artifact, commit, |_| false)
    }

    pub fn commit_reference_with_hook<T, E>(
        &self,
        artifact: &VerifiedArtifact,
        commit: impl FnOnce(ArtifactDigest) -> Result<T, E>,
        mut crash: impl FnMut(CrashPoint) -> bool,
    ) -> Result<T, ReferenceError<E>> {
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = if artifact.reference == ArtifactReference::compatibility(artifact.digest) {
            self.open_verified(artifact.digest)
        } else {
            self.open_reference(artifact.reference)
        }
        .map_err(ReferenceError::Artifact)?;
        if current.manifest != artifact.manifest {
            return Err(ReferenceError::Artifact(ArtifactError::ManifestConflict(
                artifact.digest,
            )));
        }
        inject(&mut crash, CrashPoint::BeforeEventReference).map_err(ReferenceError::Artifact)?;
        let result = commit(artifact.digest).map_err(ReferenceError::Commit)?;
        inject(&mut crash, CrashPoint::AfterEventReference).map_err(ReferenceError::Artifact)?;
        Ok(result)
    }

    pub fn collect_garbage(&self, reachability: &Reachability) -> Result<GcReport, ArtifactError> {
        let _guard = self
            .reference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.check_layout()?;
        let mut report = GcReport::default();
        self.collect_staging(reachability, &mut report)?;
        self.collect_reference_manifests(reachability, &mut report)?;
        self.collect_objects(reachability, &mut report)?;
        self.collect_orphan_manifests(reachability, &mut report)?;
        Ok(report)
    }

    fn collect_staging(
        &self,
        reachability: &Reachability,
        report: &mut GcReport,
    ) -> Result<(), ArtifactError> {
        let staging = self.root.join("staging");
        for entry in fs::read_dir(&staging)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.file_type().is_file() {
                report.skipped_unsafe_entries += 1;
                continue;
            }
            if old_enough(&metadata, reachability)?
                && self.quarantine_remove_file(&entry.path(), None, None)?
            {
                report.deleted_staged_files += 1;
            }
        }
        if report.deleted_staged_files != 0 {
            sync_directory(&staging)?;
        }
        Ok(())
    }

    fn collect_objects(
        &self,
        reachability: &Reachability,
        report: &mut GcReport,
    ) -> Result<(), ArtifactError> {
        let objects = self.root.join("objects");
        for shard in fs::read_dir(&objects)? {
            let shard = shard?;
            let shard_path = shard.path();
            let shard_metadata = fs::symlink_metadata(&shard_path)?;
            let shard_name = shard.file_name();
            let Some(shard_name) = shard_name.to_str() else {
                report.skipped_unsafe_entries += 1;
                continue;
            };
            if !shard_metadata.file_type().is_dir() || !valid_hex(shard_name, 2) {
                report.skipped_unsafe_entries += 1;
                continue;
            }
            let mut removed = false;
            for entry in fs::read_dir(&shard_path)? {
                let entry = entry?;
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path)?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    report.skipped_unsafe_entries += 1;
                    continue;
                };
                let Some(rest) = name.strip_suffix(".blob") else {
                    report.skipped_unsafe_entries += 1;
                    continue;
                };
                if !metadata.file_type().is_file() || !valid_hex(rest, 62) {
                    report.skipped_unsafe_entries += 1;
                    continue;
                }
                let digest = ArtifactDigest::parse(&format!("blake3:{shard_name}{rest}"))?;
                let manifest_path = self.manifest_path(digest)?;
                let should_delete = if self.has_persistent_owner(digest)?
                    || reachability.is_referenced(digest)
                    || self.has_live_reference_manifest(digest, reachability.now_unix_micros)?
                {
                    false
                } else {
                    match fs::symlink_metadata(&manifest_path) {
                        Ok(metadata) if metadata.file_type().is_file() => {
                            let manifest = ArtifactManifest::decode(&read_regular_file_bounded(
                                &manifest_path,
                                4096,
                            )?)?;
                            if manifest.media_type == "application/vnd.kit.workspace-read-envelope"
                                && !reachability.is_referenced(digest)
                            {
                                let lease_metadata =
                                    match fs::symlink_metadata(self.issued_path(digest)?) {
                                        Ok(issued) if issued.file_type().is_file() => issued,
                                        Ok(_) => {
                                            report.skipped_unsafe_entries += 1;
                                            continue;
                                        }
                                        Err(error)
                                            if error.kind() == std::io::ErrorKind::NotFound =>
                                        {
                                            metadata
                                        }
                                        Err(error) => return Err(error.into()),
                                    };
                                old_enough(&lease_metadata, reachability)?
                            } else {
                                true
                            }
                        }
                        Ok(_) => {
                            report.skipped_unsafe_entries += 1;
                            continue;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            old_enough(&metadata, reachability)?
                        }
                        Err(error) => return Err(error.into()),
                    }
                };
                if should_delete {
                    self.quarantine_remove_file(&path, None, None)?;
                    if manifest_path.exists()
                        && fs::symlink_metadata(&manifest_path)?.file_type().is_file()
                    {
                        self.quarantine_remove_file(&manifest_path, None, None)?;
                    }
                    let issued_path = self.issued_path(digest)?;
                    if issued_path.exists()
                        && fs::symlink_metadata(&issued_path)?.file_type().is_file()
                    {
                        self.quarantine_remove_file(&issued_path, None, None)?;
                    }
                    let pending_path = self.pending_manifest_path(digest)?;
                    if pending_path.exists()
                        && fs::symlink_metadata(&pending_path)?.file_type().is_file()
                    {
                        self.quarantine_remove_file(&pending_path, None, None)?;
                        report.deleted_orphan_manifests += 1;
                    }
                    self.remove_reference_manifests(digest, report)?;
                    report.deleted_artifacts.insert(digest);
                    removed = true;
                }
            }
            if removed {
                sync_directory(&shard_path)?;
            }
        }
        Ok(())
    }

    fn collect_reference_manifests(
        &self,
        reachability: &Reachability,
        report: &mut GcReport,
    ) -> Result<(), ArtifactError> {
        let records = self.root.join("records");
        for shard in fs::read_dir(&records)? {
            let shard = shard?;
            let shard_path = shard.path();
            let shard_name = shard.file_name();
            if !fs::symlink_metadata(&shard_path)?.file_type().is_dir()
                || !shard_name.to_str().is_some_and(|name| valid_hex(name, 2))
            {
                report.skipped_unsafe_entries += 1;
                continue;
            }
            let mut removed = false;
            for entry in fs::read_dir(&shard_path)? {
                let entry = entry?;
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path)?;
                let file_name = entry.file_name();
                let final_reference = file_name.to_str().and_then(|name| {
                    name.strip_suffix(".reference")
                        .map(|id| (id, true))
                        .or_else(|| name.strip_suffix(".pending").map(|id| (id, false)))
                });
                if !metadata.file_type().is_file()
                    || !final_reference.is_some_and(|(id, _)| valid_hex(id, 62))
                {
                    report.skipped_unsafe_entries += 1;
                    continue;
                }
                let record =
                    ArtifactReferenceManifest::decode(&read_regular_file_bounded(&path, 8192)?)?;
                if ((!final_reference.expect("validated reference name").1
                    && old_enough(&metadata, reachability)?)
                    || (!record
                        .artifact
                        .retention
                        .blocks_at(reachability.now_unix_micros)
                        && !reachability.is_referenced(record.digest)
                        && !self.has_persistent_owner(record.digest)?))
                    && self.quarantine_remove_file(&path, None, None)?
                {
                    report.deleted_orphan_manifests += 1;
                    removed = true;
                }
            }
            if removed {
                sync_directory(&shard_path)?;
            }
        }
        Ok(())
    }

    fn has_live_reference_manifest(
        &self,
        digest: ArtifactDigest,
        now_unix_micros: i64,
    ) -> Result<bool, ArtifactError> {
        let records = self.root.join("records");
        for shard in fs::read_dir(records)? {
            let shard = shard?;
            if !fs::symlink_metadata(shard.path())?.file_type().is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                if !fs::symlink_metadata(entry.path())?.file_type().is_file()
                    || !entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.ends_with(".reference"))
                {
                    continue;
                }
                let record = ArtifactReferenceManifest::decode(&read_regular_file_bounded(
                    &entry.path(),
                    8192,
                )?)?;
                if record.digest == digest && record.artifact.retention.blocks_at(now_unix_micros) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn remove_reference_manifests(
        &self,
        digest: ArtifactDigest,
        report: &mut GcReport,
    ) -> Result<(), ArtifactError> {
        let records = self.root.join("records");
        for shard in fs::read_dir(records)? {
            let shard = shard?;
            if !fs::symlink_metadata(shard.path())?.file_type().is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let path = entry.path();
                if fs::symlink_metadata(&path)?.file_type().is_file()
                    && ArtifactReferenceManifest::decode(&read_regular_file_bounded(&path, 8192)?)?
                        .digest
                        == digest
                    && self.quarantine_remove_file(&path, None, None)?
                {
                    report.deleted_orphan_manifests += 1;
                }
            }
        }
        Ok(())
    }

    fn collect_orphan_manifests(
        &self,
        reachability: &Reachability,
        report: &mut GcReport,
    ) -> Result<(), ArtifactError> {
        let manifests = self.root.join("manifests");
        for shard in fs::read_dir(&manifests)? {
            let shard = shard?;
            let shard_path = shard.path();
            let metadata = fs::symlink_metadata(&shard_path)?;
            let shard_name = shard.file_name();
            let Some(shard_name) = shard_name.to_str() else {
                report.skipped_unsafe_entries += 1;
                continue;
            };
            if !metadata.file_type().is_dir() || !valid_hex(shard_name, 2) {
                report.skipped_unsafe_entries += 1;
                continue;
            }
            let mut removed = false;
            for entry in fs::read_dir(&shard_path)? {
                let entry = entry?;
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path)?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    report.skipped_unsafe_entries += 1;
                    continue;
                };
                let (rest, orphan) = if let Some(rest) = name.strip_suffix(".manifest") {
                    (rest, false)
                } else if let Some(rest) = name.strip_suffix(".pending") {
                    (rest, true)
                } else if let Some(rest) = name.strip_suffix(".issued") {
                    (rest, true)
                } else {
                    report.skipped_unsafe_entries += 1;
                    continue;
                };
                if !metadata.file_type().is_file() || !valid_hex(rest, 62) {
                    report.skipped_unsafe_entries += 1;
                    continue;
                }
                let digest = ArtifactDigest::parse(&format!("blake3:{shard_name}{rest}"))?;
                let issued_without_manifest =
                    name.ends_with(".issued") && !self.manifest_path(digest)?.exists();
                if ((orphan && !name.ends_with(".issued"))
                    || issued_without_manifest
                    || !self.object_path(digest)?.exists())
                    && old_enough(&metadata, reachability)?
                    && self.quarantine_remove_file(&path, None, None)?
                {
                    report.deleted_orphan_manifests += 1;
                    removed = true;
                }
            }
            if removed {
                sync_directory(&shard_path)?;
            }
        }
        Ok(())
    }

    fn check_layout(&self) -> Result<(), ArtifactError> {
        check_directory(&self.root)?;
        for name in [
            "objects",
            "manifests",
            "records",
            "staging",
            "leases",
            "references",
        ] {
            check_directory(&self.root.join(name))?;
        }
        Ok(())
    }

    fn object_path(&self, digest: ArtifactDigest) -> Result<PathBuf, ArtifactError> {
        self.content_path("objects", digest, ".blob")
    }

    fn manifest_path(&self, digest: ArtifactDigest) -> Result<PathBuf, ArtifactError> {
        self.content_path("manifests", digest, ".manifest")
    }

    fn pending_manifest_path(&self, digest: ArtifactDigest) -> Result<PathBuf, ArtifactError> {
        self.content_path("manifests", digest, ".pending")
    }

    fn issued_path(&self, digest: ArtifactDigest) -> Result<PathBuf, ArtifactError> {
        self.content_path("manifests", digest, ".issued")
    }

    fn reference_manifest_path(
        &self,
        reference: ArtifactReference,
    ) -> Result<PathBuf, ArtifactError> {
        let hex = reference.hex();
        let parent = self.root.join("records");
        check_directory(&parent)?;
        let shard = parent.join(&hex[..2]);
        if !shard.exists() {
            fs::create_dir(&shard)?;
            sync_directory(&parent)?;
        }
        check_directory(&shard)?;
        Ok(shard.join(format!("{}.reference", &hex[2..])))
    }

    fn pending_reference_manifest_path(
        &self,
        reference: ArtifactReference,
    ) -> Result<PathBuf, ArtifactError> {
        let mut path = self.reference_manifest_path(reference)?;
        path.set_extension("pending");
        Ok(path)
    }

    fn publication_stage_path(
        &self,
        reference: ArtifactReference,
    ) -> Result<PathBuf, ArtifactError> {
        let staging = self.root.join("staging");
        check_directory(&staging)?;
        Ok(staging.join(format!("publication-{}.stage", reference.hex())))
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn ownership_path(
        &self,
        directory: &str,
        digest: ArtifactDigest,
        id: &str,
        suffix: &str,
    ) -> Result<PathBuf, ArtifactError> {
        if !valid_hex(id, id.len()) || id.is_empty() {
            return Err(ArtifactError::InvalidManifest("invalid ownership id"));
        }
        let hex = digest.hex();
        let parent = self.root.join(directory);
        check_directory(&parent)?;
        let shard = parent.join(&hex[..2]);
        if !shard.exists() {
            fs::create_dir(&shard)?;
            sync_directory(&parent)?;
        }
        check_directory(&shard)?;
        Ok(shard.join(format!("{}.{}{suffix}", &hex[2..], id)))
    }

    fn has_persistent_owner(&self, digest: ArtifactDigest) -> Result<bool, ArtifactError> {
        let digest_hex = digest.hex();
        let prefix = format!("{}.", &digest_hex[2..]);
        for (directory, suffix) in [("leases", ".lease"), ("references", ".reference")] {
            let shard = self.root.join(directory).join(&digest_hex[..2]);
            let entries = match fs::read_dir(&shard) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                if name
                    .to_str()
                    .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(suffix))
                    && fs::symlink_metadata(entry.path())?.file_type().is_file()
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn remove_object_if_unclaimed(&self, digest: ArtifactDigest) {
        let Ok(object) = self.object_path(digest) else {
            return;
        };
        let claimed = self.manifest_path(digest).is_ok_and(|path| path.exists())
            || self
                .pending_manifest_path(digest)
                .is_ok_and(|path| path.exists())
            || self.issued_path(digest).is_ok_and(|path| path.exists())
            || self.any_reference_manifest(digest).unwrap_or(true)
            || self.has_persistent_owner(digest).unwrap_or(true);
        if !claimed {
            let _ = self.quarantine_remove_file(&object, None, None);
        }
    }

    fn any_reference_manifest(&self, digest: ArtifactDigest) -> Result<bool, ArtifactError> {
        let records = self.root.join("records");
        for shard in fs::read_dir(records)? {
            let shard = shard?;
            if !fs::symlink_metadata(shard.path())?.file_type().is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                if fs::symlink_metadata(entry.path())?.file_type().is_file()
                    && ArtifactReferenceManifest::decode(&read_regular_file_bounded(
                        &entry.path(),
                        8192,
                    )?)?
                    .digest
                        == digest
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn quarantine_remove_file(
        &self,
        path: &Path,
        expected: Option<&[u8]>,
        deadline: Option<Instant>,
    ) -> Result<bool, ArtifactError> {
        check_artifact_deadline(deadline)?;
        let mut source = match secure_open_read(path) {
            Ok(source) => source,
            Err(ArtifactError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let identity = file_identity(&source.metadata()?);
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let quarantine = self
            .root
            .join("staging")
            .join(format!(".quarantine-{}-{sequence}", std::process::id()));
        create_private_directory(&quarantine)?;
        let item = quarantine.join("item");
        rename_noreplace_path(path, &item)?;
        if file_identity(&fs::symlink_metadata(&item)?) != identity
            || file_identity(&source.metadata()?) != identity
        {
            return Err(ArtifactError::UnsafePath(item));
        }
        if let Some(expected) = expected {
            source.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            source
                .take(expected.len().saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes != expected {
                return Err(ArtifactError::InvalidManifest(
                    "quarantined artifact binding mismatch",
                ));
            }
        }
        fs::remove_file(&item)?;
        fs::remove_dir(&quarantine)?;
        sync_directory_until(path.parent().expect("removed file has parent"), deadline)?;
        sync_directory_until(&self.root.join("staging"), deadline)?;
        Ok(true)
    }

    fn remove_authorized_lease_file(
        &self,
        path: &Path,
        digest: ArtifactDigest,
        id: &str,
        deadline: Option<Instant>,
    ) -> Result<bool, ArtifactError> {
        check_artifact_deadline(deadline)?;
        let source = match secure_open_read(path) {
            Ok(source) => source,
            Err(ArtifactError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let identity = file_identity(&source.metadata()?);
        let quarantine = self
            .root
            .join("staging")
            .join(format!(".lease-{}-{id}.quarantine", digest.hex()));
        match fs::symlink_metadata(&quarantine) {
            Ok(metadata) if metadata.file_type().is_file() => {
                fs::remove_file(&quarantine)?;
                sync_directory_until(&self.root.join("staging"), deadline)?;
            }
            Ok(_) => return Err(ArtifactError::UnsafePath(quarantine)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        rename_until(path, &quarantine, deadline)?;
        if file_identity(&fs::symlink_metadata(&quarantine)?) != identity
            || file_identity(&source.metadata()?) != identity
        {
            return Err(ArtifactError::UnsafePath(quarantine));
        }
        fs::remove_file(&quarantine)?;
        sync_directory_until(path.parent().expect("lease has parent"), deadline)?;
        sync_directory_until(&self.root.join("staging"), deadline)?;
        Ok(true)
    }

    fn content_path(
        &self,
        directory: &str,
        digest: ArtifactDigest,
        suffix: &str,
    ) -> Result<PathBuf, ArtifactError> {
        let hex = digest.hex();
        let parent = self.root.join(directory);
        check_directory(&parent)?;
        let shard = parent.join(&hex[..2]);
        if !shard.exists() {
            fs::create_dir(&shard)?;
            sync_directory(&parent)?;
        }
        check_directory(&shard)?;
        Ok(shard.join(format!("{}{suffix}", &hex[2..])))
    }

    fn temp_path(&self, kind: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.root
            .join("staging")
            .join(format!(".{kind}-{}-{sequence}.tmp", std::process::id()))
    }
}

fn publication_stage_bytes(publication: &ArtifactPublication, payload: &[u8]) -> Vec<u8> {
    let mut bytes = b"kit-artifact-publication-stage-v1\0".to_vec();
    for field in [
        publication.reference.to_string().into_bytes(),
        publication.digest.to_string().into_bytes(),
        publication.manifest.canonical_bytes(),
        payload.to_vec(),
    ] {
        bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&field);
    }
    bytes
}

fn decode_publication_stage(bytes: &[u8]) -> Result<(ArtifactPublication, &[u8]), ArtifactError> {
    let mut rest = bytes
        .strip_prefix(b"kit-artifact-publication-stage-v1\0")
        .ok_or(ArtifactError::InvalidManifest(
            "unknown artifact publication stage version",
        ))?;
    let mut fields = Vec::with_capacity(4);
    for _ in 0..4 {
        let length = rest
            .get(..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_be_bytes)
            .and_then(|length| usize::try_from(length).ok())
            .ok_or(ArtifactError::InvalidManifest(
                "invalid artifact publication stage length",
            ))?;
        rest = &rest[8..];
        let (field, remaining) =
            rest.split_at_checked(length)
                .ok_or(ArtifactError::InvalidManifest(
                    "truncated artifact publication stage",
                ))?;
        fields.push(field);
        rest = remaining;
    }
    if !rest.is_empty() {
        return Err(ArtifactError::InvalidManifest(
            "trailing artifact publication stage bytes",
        ));
    }
    let reference = ArtifactReference::parse(
        std::str::from_utf8(fields[0])
            .map_err(|_| ArtifactError::InvalidManifest("invalid publication reference"))?,
    )?;
    let digest = ArtifactDigest::parse(
        std::str::from_utf8(fields[1])
            .map_err(|_| ArtifactError::InvalidManifest("invalid publication digest"))?,
    )?;
    let manifest = ArtifactManifest::decode(fields[2])?;
    if manifest.size != fields[3].len() as u64 {
        return Err(ArtifactError::InvalidManifest(
            "artifact publication payload size mismatch",
        ));
    }
    Ok((
        ArtifactPublication {
            reference,
            digest,
            manifest,
        },
        fields[3],
    ))
}

fn callback_binding(
    principal: &str,
    project: &str,
    run: &str,
    callback_id: &str,
) -> ArtifactEnvelopeBinding {
    ArtifactEnvelopeBinding {
        principal: principal.to_owned(),
        project: project.to_owned(),
        run: run.to_owned(),
        purpose: "mcp_callback_content".to_owned(),
        invocation_id: None,
        callback_id: Some(callback_id.to_owned()),
    }
}

fn verify_file(path: &Path, expected: ArtifactDigest, size: u64) -> Result<(), ArtifactError> {
    verify_file_until(path, expected, size, None)
}

fn verify_file_until(
    path: &Path,
    expected: ArtifactDigest,
    size: u64,
    deadline: Option<Instant>,
) -> Result<(), ArtifactError> {
    check_artifact_deadline(deadline)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ArtifactError::Missing(expected)
        } else {
            ArtifactError::Io(error)
        }
    })?;
    if !metadata.file_type().is_file() {
        return Err(ArtifactError::UnsafePath(path.to_owned()));
    }
    if metadata.len() != size {
        return Err(ArtifactError::DigestMismatch(expected));
    }
    let mut file = secure_open_read(path)?;
    let mut hash = blake3::Hasher::new();
    let mut actual = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_artifact_deadline(deadline)?;
        let count = file.read(&mut buffer)?;
        check_artifact_deadline(deadline)?;
        if count == 0 {
            break;
        }
        actual = actual
            .checked_add(count as u64)
            .ok_or(ArtifactError::DigestMismatch(expected))?;
        if actual > size {
            return Err(ArtifactError::DigestMismatch(expected));
        }
        hash.update(&buffer[..count]);
    }
    if actual != size || ArtifactDigest(*hash.finalize().as_bytes()) != expected {
        return Err(ArtifactError::DigestMismatch(expected));
    }
    if !file.metadata()?.file_type().is_file() {
        return Err(ArtifactError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn read_regular_file_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ArtifactError> {
    read_regular_file_bounded_until(path, max_bytes, None)
}

fn read_regular_file_bounded_until(
    path: &Path,
    max_bytes: usize,
    deadline: Option<Instant>,
) -> Result<Vec<u8>, ArtifactError> {
    check_artifact_deadline(deadline)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(ArtifactError::UnsafePath(path.to_owned()));
    }
    if metadata.len() > max_bytes as u64 || metadata.len() > usize::MAX as u64 {
        return Err(ArtifactError::TooLarge {
            size: metadata.len(),
            max: max_bytes as u64,
        });
    }
    let mut file = secure_open_read(path)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(metadata.len() as usize)
        .map_err(|_| ArtifactError::TooLarge {
            size: metadata.len(),
            max: max_bytes as u64,
        })?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_artifact_deadline(deadline)?;
        let count = file.read(&mut buffer)?;
        check_artifact_deadline(deadline)?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > max_bytes {
            return Err(ArtifactError::TooLarge {
                size: bytes.len().saturating_add(count) as u64,
                max: max_bytes as u64,
            });
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() as u64 != metadata.len() {
        return Err(ArtifactError::InvalidManifest(
            "artifact size changed while reading",
        ));
    }
    if !file.metadata()?.file_type().is_file() {
        return Err(ArtifactError::UnsafePath(path.to_owned()));
    }
    Ok(bytes)
}

fn ensure_directory(path: &Path) -> Result<(), ArtifactError> {
    if path.exists() {
        check_directory(path)
    } else {
        fs::create_dir(path)?;
        check_directory(path)
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;

    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| {
            duration.as_nanos().min(u128::from(u64::MAX)) as u64
        });
    (metadata.len(), modified)
}

fn check_directory(path: &Path) -> Result<(), ArtifactError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(ArtifactError::UnsafePath(path.to_owned()))
    }
}

fn sync_directory(path: &Path) -> Result<(), ArtifactError> {
    sync_directory_until(path, None)
}

fn sync_directory_until(path: &Path, deadline: Option<Instant>) -> Result<(), ArtifactError> {
    check_artifact_deadline(deadline)?;
    File::open(path)?.sync_all()?;
    check_artifact_deadline(deadline)?;
    Ok(())
}

fn sync_file_until(file: &File, deadline: Option<Instant>) -> Result<(), ArtifactError> {
    check_artifact_deadline(deadline)?;
    file.sync_all()?;
    check_artifact_deadline(deadline)
}

fn rename_until(from: &Path, to: &Path, deadline: Option<Instant>) -> Result<(), ArtifactError> {
    check_artifact_deadline(deadline)?;
    rename_noreplace_path(from, to)?;
    check_artifact_deadline(deadline)
}

#[cfg(target_os = "linux")]
fn rename_noreplace_path(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_noreplace_path(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    if unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_noreplace_path(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}

fn check_artifact_deadline(deadline: Option<Instant>) -> Result<(), ArtifactError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(ArtifactError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "artifact operation time limit exceeded",
        )))
    } else {
        Ok(())
    }
}

fn secure_create_new(path: &Path) -> Result<File, ArtifactError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    no_follow(&mut options);
    Ok(options.open(path)?)
}

fn secure_open_read(path: &Path) -> Result<File, ArtifactError> {
    let mut options = OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    Ok(options.open(path)?)
}

#[cfg(unix)]
fn no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(target_os = "macos")]
    const O_NOFOLLOW: i32 = 0x0000_0100;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const O_NOFOLLOW: i32 = 0x0002_0000;
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    const O_NOFOLLOW: i32 = 0;
    options.custom_flags(O_NOFOLLOW);
}

#[cfg(not(unix))]
fn no_follow(_: &mut OpenOptions) {}

fn old_enough(metadata: &fs::Metadata, reachability: &Reachability) -> Result<bool, ArtifactError> {
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            ArtifactError::InvalidManifest("filesystem timestamp predates the Unix epoch")
        })?;
    let modified_micros = i64::try_from(modified.as_micros()).unwrap_or(i64::MAX);
    let grace = i64::try_from(reachability.orphan_grace_micros).unwrap_or(i64::MAX);
    Ok(modified_micros
        .checked_add(grace)
        .is_some_and(|expires| reachability.now_unix_micros >= expires))
}

pub fn now_unix_micros() -> Result<i64, ArtifactError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ArtifactError::InvalidManifest("system clock predates the Unix epoch"))?;
    i64::try_from(elapsed.as_micros())
        .map_err(|_| ArtifactError::InvalidManifest("system clock is out of range"))
}

fn inject(
    crash: &mut impl FnMut(CrashPoint) -> bool,
    point: CrashPoint,
) -> Result<(), ArtifactError> {
    if crash(point) {
        Err(ArtifactError::InjectedCrash(point))
    } else {
        Ok(())
    }
}

fn field<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    prefix: &'static str,
) -> Result<&'a str, ArtifactError> {
    lines
        .next()
        .and_then(|line| line.strip_prefix(prefix))
        .ok_or(ArtifactError::InvalidManifest("missing or reordered field"))
}

fn valid_field(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ' || byte == b'\t')
}

fn lease_record_well_formed(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut lines = text.lines();
    lines.next() == Some("kit-artifact-lease-v1")
        && lines
            .next()
            .and_then(|line| line.strip_prefix("digest="))
            .is_some_and(|digest| ArtifactDigest::parse(digest).is_ok())
        && lines
            .next()
            .and_then(|line| line.strip_prefix("owner="))
            .is_some_and(|owner| valid_field(owner, 255))
        && lines.next().is_none()
        && bytes.ends_with(b"\n")
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        write!(value, "{byte:02x}").expect("writing to a string cannot fail");
    }
    value
}

fn hex_digit(byte: u8) -> Result<u8, ArtifactError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(ArtifactError::InvalidArtifactDigest),
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    #[test]
    fn bound_envelopes_separate_owners_and_purposes_but_retry_exactly() {
        let payload = br#"{"same":true}"#;
        let binding = ArtifactEnvelopeBinding {
            principal: "principal_a".to_owned(),
            project: "project_a".to_owned(),
            run: "run_a".to_owned(),
            purpose: "mcp_invocation_result".to_owned(),
            invocation_id: Some("invocation_a".to_owned()),
            callback_id: None,
        };
        let retry = binding.seal(payload).unwrap();
        assert_eq!(retry, binding.seal(payload).unwrap());
        assert_eq!(binding.open(&retry).unwrap(), payload);

        let mut other_owner = binding.clone();
        other_owner.principal = "principal_b".to_owned();
        let mut other_purpose = binding.clone();
        other_purpose.purpose = "mcp_callback_content".to_owned();
        other_purpose.invocation_id = None;
        other_purpose.callback_id = Some("callback_a".to_owned());
        assert_ne!(
            ArtifactDigest::digest(&retry),
            ArtifactDigest::digest(&other_owner.seal(payload).unwrap())
        );
        assert_ne!(
            ArtifactDigest::digest(&retry),
            ArtifactDigest::digest(&other_purpose.seal(payload).unwrap())
        );
        assert!(other_owner.open(&retry).is_err());
        assert!(other_purpose.open(&retry).is_err());

        let root = std::env::temp_dir().join(format!(
            "kit-artifact-shared-ref-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        let store = ArtifactStore::open(&root).unwrap();
        let metadata = ArtifactMetadata::new(
            "application/vnd.kit.artifact-envelope",
            ArtifactClass::File,
            "principal_a",
            "project_a",
            ArtifactRetention::UntilUnixMicros(i64::MAX),
            1,
        )
        .unwrap();
        let first = ArtifactReference::derive(b"shared-ref-test", b"first");
        let second = ArtifactReference::derive(b"shared-ref-test", b"second");
        let first_publication = store
            .stage_publication(&retry, metadata.clone(), first)
            .unwrap();
        let second_publication = store.stage_publication(&retry, metadata, second).unwrap();
        let digest = store
            .promote_publication(&first_publication)
            .unwrap()
            .digest();
        store.promote_publication(&second_publication).unwrap();
        store
            .erase_owned_reference(first, "principal_a", "project_a")
            .unwrap();
        assert!(store.open_reference(first).is_err());
        assert!(store.open_reference(second).is_ok());
        assert!(store.open_bytes(digest).is_ok());
        assert!(matches!(
            store.erase_owned_reference(second, "principal_b", "project_a"),
            Err(ArtifactError::AccessDenied)
        ));
        assert!(store.open_reference(second).is_ok());
        store
            .erase_owned_reference(second, "principal_a", "project_a")
            .unwrap();
        assert!(store.open_bytes(digest).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
