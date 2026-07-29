use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    store::artifacts::{ArtifactDigest, ArtifactReference, ArtifactRetention},
    workspace::revision::Revision,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::recover_pending;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use unix::{materialize, materialize_with_hook};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unavailable;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) use unavailable::recover_pending;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use unavailable::{materialize, materialize_with_hook};

pub const RECOVERY_MANIFEST_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryPosition {
    Base,
    Successor,
    Other,
}

#[derive(Clone, Debug)]
pub struct MaterializeOptions {
    pub retention: ArtifactRetention,
    pub max_preview_bytes: usize,
    pub max_actions: usize,
    pub max_path_bytes: usize,
    pub max_image_bytes: u64,
    pub max_manifest_bytes: usize,
    pub max_diff_bytes: u64,
    pub max_total_bytes: u64,
    pub max_time: Duration,
    pub cancellation: Option<Arc<AtomicBool>>,
}

impl MaterializeOptions {
    pub fn new(retention: ArtifactRetention) -> Self {
        Self {
            retention,
            max_preview_bytes: 256 * 1024,
            max_actions: 100_000,
            max_path_bytes: 256 * 1024 * 1024,
            max_image_bytes: 2 * 1024 * 1024 * 1024,
            max_manifest_bytes: 64 * 1024 * 1024,
            max_diff_bytes: 1024 * 1024 * 1024,
            max_total_bytes: 4 * 1024 * 1024 * 1024,
            max_time: Duration::from_secs(30),
            cancellation: None,
        }
    }

    pub fn with_cancellation(mut self, cancellation: Arc<AtomicBool>) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

#[derive(Clone, Debug)]
pub struct MaterializedEdit {
    transaction_id: String,
    revision: Revision,
    diff_artifact: ArtifactReference,
    diff_artifact_digest: ArtifactDigest,
    diff_preview: Vec<u8>,
    change_diff: Vec<u8>,
    change_diff_complete: bool,
    verification: crate::verify::profiles::VerificationReceipt,
    committed_with_cancel_race: bool,
}

impl MaterializedEdit {
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub fn revision(&self) -> &Revision {
        &self.revision
    }

    pub fn diff_artifact_reference(&self) -> ArtifactReference {
        self.diff_artifact
    }

    pub fn diff_artifact_digest(&self) -> ArtifactDigest {
        self.diff_artifact_digest
    }

    pub fn diff_preview(&self) -> &[u8] {
        &self.diff_preview
    }

    pub fn change_diff(&self) -> &[u8] {
        &self.change_diff
    }

    pub const fn change_diff_complete(&self) -> bool {
        self.change_diff_complete
    }

    pub fn verification_receipt(&self) -> &crate::verify::profiles::VerificationReceipt {
        &self.verification
    }

    pub const fn committed_with_cancel_race(&self) -> bool {
        self.committed_with_cancel_race
    }

    pub(crate) fn mark_cancel_race(&mut self) {
        self.committed_with_cancel_race = true;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryPoint {
    LedgerTempWrite,
    LedgerFileSync,
    LedgerRename,
    LedgerDirectorySync,
    ManifestTempWrite,
    ManifestFileSync,
    ManifestRename,
    ManifestDirectorySync,
    TransactionMkdir,
    TransactionMarkerWrite,
    TransactionMarkerSync,
    TransactionBind,
    ImageWrite,
    ImageFileSync,
    ArtifactPromote,
    ArtifactLease,
    ArtifactLeaseTempCreate,
    ArtifactLeasePartialWrite,
    ArtifactLeaseFileSync,
    ArtifactLeaseRename,
    ArtifactLeaseDirectorySync,
    BeforeWorkspaceExchange,
    AfterWorkspaceExchange,
    RevisionStateWrite,
    RevisionStateFileSync,
    RevisionStateRename,
    RevisionStateDirectorySync,
    RevisionGuardWrite,
    RevisionGuardSync,
    CleanupProgress,
    CleanupItem,
    CleanupDirectory,
    CleanupManifestRemove,
    CleanupManifestDirectorySync,
    CleanupLedgerRemove,
    CleanupLedgerDirectorySync,
    QuarantineMkdir,
    QuarantineSentinelCreate,
    QuarantineSentinelSync,
    QuarantineExchange,
    QuarantinePostVerify,
    QuarantineSourceSentinelRetire,
    QuarantineItemUnlink,
    QuarantineDirectoryRemove,
    QuarantineParentSync,
    AfterUndoImageSync,
    AfterDiffArtifactSync,
    AfterStagedManifestSync,
    AfterDestinationTempSync,
    AfterPreparedManifestSync,
    BeforeAction,
    AfterSourceQuarantineSync,
    AfterActionSync,
    AfterMaterializedManifestSync,
    BeforeRevisionCommit,
    AfterRevisionCommit,
    AfterCommittedManifestSync,
    AfterCleanupManifestSync,
    DuringCleanup,
}

impl RecoveryPoint {
    pub const ALL: [Self; 14] = [
        Self::AfterUndoImageSync,
        Self::AfterDiffArtifactSync,
        Self::AfterStagedManifestSync,
        Self::AfterDestinationTempSync,
        Self::AfterPreparedManifestSync,
        Self::BeforeAction,
        Self::AfterSourceQuarantineSync,
        Self::AfterActionSync,
        Self::AfterMaterializedManifestSync,
        Self::BeforeRevisionCommit,
        Self::AfterRevisionCommit,
        Self::AfterCommittedManifestSync,
        Self::AfterCleanupManifestSync,
        Self::DuringCleanup,
    ];

    pub const CRASH_MATRIX: [Self; 59] = [
        Self::LedgerTempWrite,
        Self::LedgerFileSync,
        Self::LedgerRename,
        Self::LedgerDirectorySync,
        Self::ManifestTempWrite,
        Self::ManifestFileSync,
        Self::ManifestRename,
        Self::ManifestDirectorySync,
        Self::TransactionMkdir,
        Self::TransactionMarkerWrite,
        Self::TransactionMarkerSync,
        Self::TransactionBind,
        Self::ImageWrite,
        Self::ImageFileSync,
        Self::ArtifactPromote,
        Self::ArtifactLease,
        Self::ArtifactLeaseTempCreate,
        Self::ArtifactLeasePartialWrite,
        Self::ArtifactLeaseFileSync,
        Self::ArtifactLeaseRename,
        Self::ArtifactLeaseDirectorySync,
        Self::BeforeWorkspaceExchange,
        Self::AfterWorkspaceExchange,
        Self::RevisionStateWrite,
        Self::RevisionStateFileSync,
        Self::RevisionStateRename,
        Self::RevisionStateDirectorySync,
        Self::RevisionGuardWrite,
        Self::RevisionGuardSync,
        Self::CleanupProgress,
        Self::CleanupItem,
        Self::CleanupDirectory,
        Self::CleanupManifestRemove,
        Self::CleanupManifestDirectorySync,
        Self::CleanupLedgerRemove,
        Self::CleanupLedgerDirectorySync,
        Self::QuarantineMkdir,
        Self::QuarantineSentinelCreate,
        Self::QuarantineSentinelSync,
        Self::QuarantineExchange,
        Self::QuarantinePostVerify,
        Self::QuarantineSourceSentinelRetire,
        Self::QuarantineItemUnlink,
        Self::QuarantineDirectoryRemove,
        Self::QuarantineParentSync,
        Self::AfterUndoImageSync,
        Self::AfterDiffArtifactSync,
        Self::AfterStagedManifestSync,
        Self::AfterDestinationTempSync,
        Self::AfterPreparedManifestSync,
        Self::BeforeAction,
        Self::AfterSourceQuarantineSync,
        Self::AfterActionSync,
        Self::AfterMaterializedManifestSync,
        Self::BeforeRevisionCommit,
        Self::AfterRevisionCommit,
        Self::AfterCommittedManifestSync,
        Self::AfterCleanupManifestSync,
        Self::DuringCleanup,
    ];
}

#[cfg(debug_assertions)]
pub(crate) fn system_crash(point: RecoveryPoint, action: usize) {
    if !RECOVERY_CRASH_ARMED.load(Ordering::Acquire) {
        return;
    }
    if std::env::var("KIT_EDIT_CRASH_POINT").ok().as_deref() == Some(&format!("{point:?}"))
        && std::env::var("KIT_EDIT_CRASH_ACTION")
            .ok()
            .and_then(|value| value.parse().ok())
            == Some(action)
    {
        #[cfg(unix)]
        unsafe {
            if std::env::var_os("KIT_EDIT_CRASH_KILL").is_some() {
                libc::kill(libc::getpid(), libc::SIGKILL);
                loop {
                    libc::pause();
                }
            }
            libc::_exit(86)
        };
        #[cfg(not(unix))]
        std::process::exit(86);
    }
}

#[cfg(not(debug_assertions))]
pub(crate) fn system_crash(_point: RecoveryPoint, _action: usize) {}

#[cfg(debug_assertions)]
static RECOVERY_CRASH_ARMED: AtomicBool = AtomicBool::new(false);

pub(crate) struct SystemCrashArm {
    #[cfg(debug_assertions)]
    previously_armed: bool,
}

pub(crate) fn arm_system_crash() -> SystemCrashArm {
    #[cfg(debug_assertions)]
    let previously_armed = RECOVERY_CRASH_ARMED.swap(true, Ordering::AcqRel);
    SystemCrashArm {
        #[cfg(debug_assertions)]
        previously_armed,
    }
}

impl Drop for SystemCrashArm {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        RECOVERY_CRASH_ARMED.store(self.previously_armed, Ordering::Release);
    }
}

#[derive(Debug)]
pub enum RecoveryError {
    InvalidOptions,
    Conflict(String),
    CorruptManifest,
    UnsafeEntry(String),
    StageChanged,
    ChangeDiffMismatch {
        expected: String,
        actual: String,
        complete: bool,
    },
    Cancelled,
    Revision(crate::workspace::revision::RevisionError),
    Artifact(crate::store::artifacts::ArtifactError),
    Io(std::io::Error),
    InjectedCrash {
        point: RecoveryPoint,
        action: usize,
    },
    CommittedCleanup {
        result: Box<MaterializedEdit>,
        source: std::io::Error,
    },
    Unavailable,
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOptions => formatter.write_str("invalid edit materialization options"),
            Self::Conflict(path) => write!(formatter, "workspace changed at {path}"),
            Self::CorruptManifest => formatter.write_str("edit recovery manifest is corrupt"),
            Self::UnsafeEntry(path) => write!(formatter, "unsafe materialization entry at {path}"),
            Self::StageChanged => formatter.write_str("staged edit changed before materialization"),
            Self::ChangeDiffMismatch {
                expected,
                actual,
                complete,
            } => write!(
                formatter,
                "staged change diff does not match preview binding (expected {expected}, actual {actual}, complete={complete})"
            ),
            Self::Cancelled => formatter.write_str("edit cancelled before revision commit"),
            Self::Revision(error) => error.fmt(formatter),
            Self::Artifact(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "edit recovery I/O error: {error}"),
            Self::InjectedCrash { point, action } => {
                write!(
                    formatter,
                    "injected edit crash at {point:?} action {action}"
                )
            }
            Self::CommittedCleanup { source, .. } => {
                write!(
                    formatter,
                    "edit committed but durable cleanup failed: {source}"
                )
            }
            Self::Unavailable => formatter.write_str("safe edit materialization is unavailable"),
        }
    }
}

impl std::error::Error for RecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            Self::Artifact(error) => Some(error),
            Self::Io(error) | Self::CommittedCleanup { source: error, .. } => Some(error),
            _ => None,
        }
    }
}

impl From<crate::workspace::revision::RevisionError> for RecoveryError {
    fn from(error: crate::workspace::revision::RevisionError) -> Self {
        Self::Revision(error)
    }
}

impl From<crate::store::artifacts::ArtifactError> for RecoveryError {
    fn from(error: crate::store::artifacts::ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<std::io::Error> for RecoveryError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub type RecoveryHook<'a> = &'a mut dyn FnMut(RecoveryPoint, usize) -> bool;

#[allow(clippy::too_many_arguments)]
pub(crate) fn result(
    transaction_id: String,
    revision: Revision,
    diff_artifact: ArtifactReference,
    diff_artifact_digest: ArtifactDigest,
    diff_preview: Vec<u8>,
    change_diff: Vec<u8>,
    change_diff_complete: bool,
    verification: crate::verify::profiles::VerificationReceipt,
) -> MaterializedEdit {
    MaterializedEdit {
        transaction_id,
        revision,
        diff_artifact,
        diff_artifact_digest,
        diff_preview,
        change_diff,
        change_diff_complete,
        verification,
        committed_with_cancel_race: false,
    }
}
