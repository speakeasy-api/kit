//! Durable checkpoint candidate/validated/rejected/promoted states (M009-W02).
//!
//! This module owns the versioned state contract only. Eviction, `yield`
//! packets, authoritative validation inputs, and atomic promotion remain
//! W03-W06 scope; no semantic compaction is enabled here.

use std::fmt;

use agentkit_loop::{MutationPoint, PostValidationCheckpointOutcome};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{
    agent::driver::restart::SafeBoundary,
    domain::{
        events::{ContentDigest, SchemaVersion},
        ids::{CheckpointId, RunId},
        lifecycle::{AttemptOwnership, StateVersion},
    },
    store::artifacts::{ArtifactDigest, ArtifactReference},
};

pub const MAX_DRIVER_LEASE_ID_BYTES: usize = 128;
pub const MAX_REJECTION_REASON_BYTES: usize = 1024;

/// Unique durable driver lease presenting a checkpoint operation.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DriverLeaseId(String);

impl DriverLeaseId {
    pub fn parse(value: &str) -> Result<Self, CheckpointStateError> {
        if value.is_empty()
            || value.len() > MAX_DRIVER_LEASE_ID_BYTES
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(CheckpointStateError::InvalidDriverLeaseId);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DriverLeaseId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Bounded rejection reason accepted from untrusted hook or validator input.
/// The accepted alphabet is visible ASCII plus ordinary spaces, so control,
/// bidirectional-override, and zero-width characters cannot spoof operators.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RejectionReason(String);

impl RejectionReason {
    pub fn parse(value: &str) -> Result<Self, CheckpointStateError> {
        if value.is_empty()
            || value.len() > MAX_REJECTION_REASON_BYTES
            || !value
                .chars()
                .all(|character| character == ' ' || character.is_ascii_graphic())
        {
            return Err(CheckpointStateError::InvalidRejectionReason);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RejectionReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Kit-owned closed set of loop points at which a checkpoint may be taken.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointMutationPoint {
    AfterToolResult,
    AfterTurnEnded,
}

impl CheckpointMutationPoint {
    /// Map the agentkit mutation point, failing closed on any future variant
    /// of the upstream `#[non_exhaustive]` enum.
    pub fn from_agentkit(point: MutationPoint) -> Result<Self, CheckpointStateError> {
        match point {
            MutationPoint::AfterToolResult => Ok(Self::AfterToolResult),
            MutationPoint::AfterTurnEnded => Ok(Self::AfterTurnEnded),
            other => Err(CheckpointStateError::UnsupportedMutationPoint(
                bounded_debug(&other),
            )),
        }
    }

    /// The M002 restart safe boundary this checkpoint point corresponds to.
    pub const fn safe_boundary(self) -> SafeBoundary {
        match self {
            Self::AfterToolResult => SafeBoundary::AfterToolOutcome,
            Self::AfterTurnEnded => SafeBoundary::TurnEnd,
        }
    }
}

/// Complete identity a versioned checkpoint record is bound to.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointBinding {
    pub schema_version: SchemaVersion,
    pub checkpoint_id: CheckpointId,
    pub run_id: RunId,
    #[serde(with = "owner_wire")]
    pub owner: AttemptOwnership,
    pub driver_lease_id: DriverLeaseId,
    pub operation_sequence: u64,
    pub expected_durable_head_sequence: u64,
    pub base_transcript_digest: ContentDigest,
    pub candidate_transcript_digest: ContentDigest,
    #[serde(with = "artifact_wire")]
    pub candidate_artifact_digest: ArtifactDigest,
    #[serde(with = "artifact_wire")]
    pub candidate_artifact_reference: ArtifactReference,
    pub mutation_point: CheckpointMutationPoint,
}

impl CheckpointBinding {
    pub fn validate(&self) -> Result<(), CheckpointStateError> {
        if self.operation_sequence <= self.expected_durable_head_sequence {
            return Err(CheckpointStateError::NonMonotonicSequence);
        }
        Ok(())
    }
}

/// The four durable checkpoint states.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointState {
    Candidate,
    Validated,
    Rejected,
    Promoted,
}

impl CheckpointState {
    pub const ALL: [Self; 4] = [
        Self::Candidate,
        Self::Validated,
        Self::Rejected,
        Self::Promoted,
    ];

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Rejected | Self::Promoted)
    }

    /// The explicit legal transition graph. `Validated` is Kit-authoritative
    /// validation eligibility, not agentkit transcript-pair validation.
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Candidate, Self::Validated)
                | (Self::Candidate, Self::Rejected)
                | (Self::Validated, Self::Promoted)
                | (Self::Validated, Self::Rejected)
        )
    }
}

impl fmt::Display for CheckpointState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Candidate => "candidate",
            Self::Validated => "validated",
            Self::Rejected => "rejected",
            Self::Promoted => "promoted",
        })
    }
}

/// One requested state transition, bound to the full checkpoint identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointTransition {
    pub binding: CheckpointBinding,
    pub to: CheckpointState,
    pub rejection_reason: Option<RejectionReason>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AppliedTransition {
    to: CheckpointState,
    rejection_reason: Option<RejectionReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionApplied {
    Applied,
    Replayed,
}

/// Versioned durable record for one checkpoint candidate.
///
/// The record carries digests and references only; it never holds transcript
/// content, reasoning parts, or any other hidden model output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckpointStateRecord {
    binding: CheckpointBinding,
    state: CheckpointState,
    version: StateVersion,
    applied: Vec<AppliedTransition>,
}

impl CheckpointStateRecord {
    /// Record a new candidate. Creation never touches live context or the
    /// durable promoted head; only [`DurablePromotedHead::advance`] moves the
    /// head, and only for a `Promoted` record.
    pub fn new_candidate(binding: CheckpointBinding) -> Result<Self, CheckpointStateError> {
        binding.validate()?;
        Ok(Self {
            binding,
            state: CheckpointState::Candidate,
            version: StateVersion::INITIAL,
            applied: Vec::new(),
        })
    }

    pub fn binding(&self) -> &CheckpointBinding {
        &self.binding
    }

    pub fn state(&self) -> CheckpointState {
        self.state
    }

    pub fn version(&self) -> StateVersion {
        self.version
    }

    pub fn rejection_reason(&self) -> Option<&RejectionReason> {
        self.applied
            .iter()
            .find(|applied| applied.to == CheckpointState::Rejected)
            .and_then(|applied| applied.rejection_reason.as_ref())
    }

    /// Apply one transition with fail-closed replay semantics.
    ///
    /// An exact replay of an already-applied transition is idempotent and
    /// returns [`TransitionApplied::Replayed`]. The same logical transition
    /// with conflicting identity or data returns a typed error. Every error
    /// path returns before any mutation, so the record stays byte-identical.
    pub fn apply(
        &mut self,
        transition: &CheckpointTransition,
    ) -> Result<TransitionApplied, CheckpointStateError> {
        transition.binding.validate()?;
        if transition.binding != self.binding {
            return Err(CheckpointStateError::IdentityMismatch);
        }
        match (transition.to, transition.rejection_reason.is_some()) {
            (CheckpointState::Rejected, false) => {
                return Err(CheckpointStateError::MissingRejectionReason);
            }
            (CheckpointState::Rejected, true) => {}
            (_, true) => return Err(CheckpointStateError::UnexpectedRejectionReason),
            (_, false) => {}
        }
        if let Some(previous) = self
            .applied
            .iter()
            .find(|applied| applied.to == transition.to)
        {
            if previous.rejection_reason == transition.rejection_reason {
                return Ok(TransitionApplied::Replayed);
            }
            return Err(CheckpointStateError::ConflictingReplay {
                state: transition.to,
            });
        }
        if !self.state.can_transition_to(transition.to) {
            return Err(CheckpointStateError::IllegalTransition {
                from: self.state,
                to: transition.to,
            });
        }
        let version = self
            .version
            .increment()
            .ok_or(CheckpointStateError::VersionOverflow)?;
        self.applied.push(AppliedTransition {
            to: transition.to,
            rejection_reason: transition.rejection_reason.clone(),
        });
        self.state = transition.to;
        self.version = version;
        Ok(TransitionApplied::Applied)
    }
}

/// Untrusted wire payloads are replayed through [`CheckpointStateRecord::apply`]
/// so every structural invariant is re-proven before a record exists: a payload
/// whose state, version, or applied transitions are mutually inconsistent, or
/// whose sequence is non-monotonic, is rejected. This is validation, not
/// authentication — a self-consistent payload decodes regardless of origin, and
/// [`DurablePromotedHead::advance`] still gates any head movement.
impl<'de> Deserialize<'de> for CheckpointStateRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            binding: CheckpointBinding,
            state: CheckpointState,
            version: StateVersion,
            applied: Vec<AppliedTransition>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let mut record =
            CheckpointStateRecord::new_candidate(wire.binding).map_err(D::Error::custom)?;
        for applied in wire.applied {
            let transition = CheckpointTransition {
                binding: record.binding.clone(),
                to: applied.to,
                rejection_reason: applied.rejection_reason,
            };
            match record.apply(&transition).map_err(D::Error::custom)? {
                TransitionApplied::Applied => {}
                TransitionApplied::Replayed => {
                    return Err(D::Error::custom("duplicate applied checkpoint transition"));
                }
            }
        }
        if record.state != wire.state {
            return Err(D::Error::custom(
                "checkpoint state does not match its applied transitions",
            ));
        }
        if record.version != wire.version {
            return Err(D::Error::custom(
                "checkpoint version does not match its applied transitions",
            ));
        }
        Ok(record)
    }
}

/// The durable promoted transcript head, bound to the single run it serves.
/// A rejected checkpoint retains the prior head; only a promoted record from
/// the head's own run whose expected head sequence and base transcript digest
/// both match the current head advances it, and only the exact binding of the
/// promotion this head itself recorded replays as a no-op.
///
/// The head is bound to the run, not to an attempt: it outlives any single
/// driver attempt within the run, so attempt ownership and fencing are
/// authorized per promotion (W06), not pinned at head construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurablePromotedHead {
    run_id: RunId,
    sequence: u64,
    transcript_digest: ContentDigest,
    last_promotion: Option<CheckpointBinding>,
}

impl DurablePromotedHead {
    /// A head with no known promotion history. It never acknowledges a
    /// historical promotion it did not perform: any promoted record claiming
    /// the current sequence is a conflict, not a replay.
    pub const fn new(run_id: RunId, sequence: u64, transcript_digest: ContentDigest) -> Self {
        Self {
            run_id,
            sequence,
            transcript_digest,
            last_promotion: None,
        }
    }

    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn transcript_digest(&self) -> &ContentDigest {
        &self.transcript_digest
    }

    /// Advance the head; the sole mutation path for a promoted record. A
    /// record from a foreign run is rejected before any sequence, digest, or
    /// replay consideration.
    pub fn advance(&mut self, record: &CheckpointStateRecord) -> Result<(), CheckpointStateError> {
        if record.state != CheckpointState::Promoted {
            return Err(CheckpointStateError::NotPromoted {
                state: record.state,
            });
        }
        if record.binding.run_id != self.run_id {
            return Err(CheckpointStateError::HeadRunMismatch {
                expected: self.run_id,
                actual: record.binding.run_id,
            });
        }
        if self.last_promotion.as_ref() == Some(&record.binding) {
            return Ok(());
        }
        if record.binding.expected_durable_head_sequence != self.sequence {
            return Err(CheckpointStateError::HeadSequenceMismatch {
                expected: record.binding.expected_durable_head_sequence,
                actual: self.sequence,
            });
        }
        if record.binding.base_transcript_digest != self.transcript_digest {
            return Err(CheckpointStateError::HeadDigestMismatch {
                expected: record.binding.base_transcript_digest.clone(),
                actual: self.transcript_digest.clone(),
            });
        }
        self.sequence = record.binding.operation_sequence;
        self.transcript_digest = record.binding.candidate_transcript_digest.clone();
        self.last_promotion = Some(record.binding.clone());
        Ok(())
    }
}

/// Kit-side disposition of an agentkit post-validation hook outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookOutcomeAction {
    /// `Committed`: the exact candidate is durable; the record stays
    /// `Candidate` until Kit-authoritative validation runs.
    RecordDurableCandidate,
    /// `NotCommitted`: the candidate definitely did not commit and may be
    /// rejected with the bounded reason.
    RejectCandidate(RejectionReason),
    /// `Unknown`: the operation stays pending and must be reconciled under
    /// the same checkpoint identity. Never maps to `Rejected` or `Promoted`.
    AwaitReconciliation,
}

pub fn hook_outcome_action(
    outcome: &PostValidationCheckpointOutcome,
) -> Result<HookOutcomeAction, CheckpointStateError> {
    match outcome {
        PostValidationCheckpointOutcome::Committed => Ok(HookOutcomeAction::RecordDurableCandidate),
        PostValidationCheckpointOutcome::NotCommitted(reason) => {
            RejectionReason::parse(reason).map(HookOutcomeAction::RejectCandidate)
        }
        PostValidationCheckpointOutcome::Unknown(_) => Ok(HookOutcomeAction::AwaitReconciliation),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckpointStateError {
    InvalidDriverLeaseId,
    InvalidRejectionReason,
    UnsupportedMutationPoint(String),
    NonMonotonicSequence,
    IdentityMismatch,
    MissingRejectionReason,
    UnexpectedRejectionReason,
    IllegalTransition {
        from: CheckpointState,
        to: CheckpointState,
    },
    ConflictingReplay {
        state: CheckpointState,
    },
    NotPromoted {
        state: CheckpointState,
    },
    HeadRunMismatch {
        expected: RunId,
        actual: RunId,
    },
    HeadSequenceMismatch {
        expected: u64,
        actual: u64,
    },
    HeadDigestMismatch {
        expected: ContentDigest,
        actual: ContentDigest,
    },
    VersionOverflow,
}

impl fmt::Display for CheckpointStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDriverLeaseId => write!(
                f,
                "driver lease id must contain 1 to {MAX_DRIVER_LEASE_ID_BYTES} visible ASCII characters"
            ),
            Self::InvalidRejectionReason => write!(
                f,
                "rejection reason must contain 1 to {MAX_REJECTION_REASON_BYTES} visible ASCII characters or spaces"
            ),
            Self::UnsupportedMutationPoint(point) => {
                write!(f, "unsupported checkpoint mutation point {point}")
            }
            Self::NonMonotonicSequence => {
                f.write_str("operation sequence must exceed the expected durable head sequence")
            }
            Self::IdentityMismatch => {
                f.write_str("transition identity does not match the checkpoint record")
            }
            Self::MissingRejectionReason => f.write_str("rejection requires a bounded reason"),
            Self::UnexpectedRejectionReason => {
                f.write_str("only a rejection carries a rejection reason")
            }
            Self::IllegalTransition { from, to } => {
                write!(f, "illegal checkpoint transition {from} -> {to}")
            }
            Self::ConflictingReplay { state } => {
                write!(f, "conflicting replay of the {state} transition")
            }
            Self::NotPromoted { state } => {
                write!(
                    f,
                    "a {state} checkpoint cannot advance the durable promoted head"
                )
            }
            Self::HeadRunMismatch { expected, actual } => write!(
                f,
                "promotion from run {actual} cannot advance the durable head bound to run {expected}"
            ),
            Self::HeadSequenceMismatch { expected, actual } => write!(
                f,
                "promotion expected durable head sequence {expected} but the head is at {actual}"
            ),
            Self::HeadDigestMismatch { expected, actual } => write!(
                f,
                "promotion expected durable head transcript digest {expected} but the head is at {actual}"
            ),
            Self::VersionOverflow => f.write_str("checkpoint record version overflow"),
        }
    }
}

impl std::error::Error for CheckpointStateError {}

/// Bound and sanitize `Debug` text destined for operator-visible errors: every
/// character outside visible ASCII plus ordinary spaces is replaced, so a
/// hostile upstream `Debug` impl cannot smuggle control, bidirectional-override,
/// or zero-width characters into diagnostics.
fn bounded_debug<T: fmt::Debug>(value: &T) -> String {
    format!("{value:?}")
        .chars()
        .map(|character| {
            if character == ' ' || character.is_ascii_graphic() {
                character
            } else {
                '?'
            }
        })
        .take(64)
        .collect()
}

/// Strict wire form for [`AttemptOwnership`] inside a [`CheckpointBinding`]:
/// the shared lifecycle type derives a lenient `Deserialize`, so this module
/// re-encodes the same three fields with `deny_unknown_fields` while reusing
/// the shared constructors and types rather than a parallel ownership model.
mod owner_wire {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::domain::{
        ids::{AttemptId, PrincipalId},
        lifecycle::{AttemptOwnership, FencingToken},
    };

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        attempt_id: AttemptId,
        principal_id: PrincipalId,
        fencing_token: FencingToken,
    }

    pub fn serialize<S>(owner: &AttemptOwnership, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Wire {
            attempt_id: owner.attempt_id,
            principal_id: owner.principal_id,
            fencing_token: owner.fencing_token,
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<AttemptOwnership, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = Wire::deserialize(deserializer)?;
        Ok(AttemptOwnership::new(
            wire.attempt_id,
            wire.principal_id,
            wire.fencing_token,
        ))
    }
}

/// Canonical-string wire form shared by both artifact identifier types.
mod artifact_wire {
    use std::fmt::Display;

    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    use crate::store::artifacts::{ArtifactDigest, ArtifactError, ArtifactReference};

    pub trait ArtifactWire: Display + Sized {
        fn parse_wire(value: &str) -> Result<Self, ArtifactError>;
    }

    impl ArtifactWire for ArtifactDigest {
        fn parse_wire(value: &str) -> Result<Self, ArtifactError> {
            Self::parse(value)
        }
    }

    impl ArtifactWire for ArtifactReference {
        fn parse_wire(value: &str) -> Result<Self, ArtifactError> {
            Self::parse(value)
        }
    }

    pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: ArtifactWire,
        S: Serializer,
    {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
    where
        T: ArtifactWire,
        D: Deserializer<'de>,
    {
        T::parse_wire(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::bounded_debug;

    #[test]
    fn bounded_debug_truncates_on_character_boundaries() {
        let multibyte = "é".repeat(80);
        let bounded = bounded_debug(&multibyte);
        assert_eq!(bounded.chars().count(), 64);
        assert_eq!(bounded_debug(&"short"), "\"short\"");
    }

    #[test]
    fn bounded_debug_sanitizes_hostile_debug_output() {
        struct Hostile;
        impl fmt::Debug for Hostile {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("evil\u{202E}\u{200B}\u{0007}\t\nplain text\u{00E9}")
            }
        }

        let bounded = bounded_debug(&Hostile);
        assert_eq!(bounded, "evil?????plain text?");
        assert!(
            bounded
                .chars()
                .all(|character| character == ' ' || character.is_ascii_graphic())
        );

        struct HostileLong;
        impl fmt::Debug for HostileLong {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&"\u{202E}".repeat(80))
            }
        }
        let bounded = bounded_debug(&HostileLong);
        assert_eq!(bounded, "?".repeat(64));
    }
}
