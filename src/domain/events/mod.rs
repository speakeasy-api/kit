use std::fmt;
use std::str::FromStr;

use serde::de::{DeserializeOwned, Error as _};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::config::{ConfigField, LayerKind};
use super::ids::{
    AgentLinkId, ApprovalId, ArtifactId, AttemptId, CheckpointId, CommandId, DaemonServiceId,
    EventId, ExperimentId, ExternalTaskId, ModelCallId, PrincipalId, ProcessId, ProjectId, RunId,
    TaskId, TerminalId, ThreadId, ToolCallId, TurnId, WorkspaceId,
};
use std::collections::BTreeMap;

pub use super::lifecycle::{
    AttemptState, AttemptTransition, RunState, RunTransition, TransitionError,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SchemaVersion {
    V1,
}

impl SchemaVersion {
    pub const CURRENT: Self = Self::V1;
}

impl From<SchemaVersion> for u16 {
    fn from(value: SchemaVersion) -> Self {
        match value {
            SchemaVersion::V1 => 1,
        }
    }
}

impl TryFrom<u16> for SchemaVersion {
    type Error = VersionError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::V1),
            _ => Err(VersionError(value)),
        }
    }
}

impl Serialize for SchemaVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u16((*self).into())
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(u16::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionError(u16);

impl fmt::Display for VersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported schema version {}", self.0)
    }
}

impl std::error::Error for VersionError {}

macro_rules! positive_position {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub fn new(value: u64) -> Result<Self, PositionError> {
                if value == 0 {
                    Err(PositionError($label))
                } else {
                    Ok(Self(value))
                }
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl TryFrom<u64> for $name {
            type Error = PositionError;

            fn try_from(value: u64) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_u64(self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(u64::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PositionError(&'static str);

impl fmt::Display for PositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} must be greater than zero", self.0)
    }
}

impl std::error::Error for PositionError {}

positive_position!(StreamSequence, "event sequence");
positive_position!(CommitPosition, "commit position");

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UtcDateTime(String);

impl UtcDateTime {
    pub fn parse(value: &str) -> Result<Self, TimestampError> {
        if valid_rfc3339_utc(value.as_bytes()) {
            Ok(Self(value.to_owned()))
        } else {
            Err(TimestampError)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn now() -> Result<Self, TimestampError> {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| TimestampError)?
            .as_secs();
        let days = i64::try_from(seconds / 86_400).map_err(|_| TimestampError)?;
        let second = seconds % 86_400;
        let (year, month, day) = civil_from_days(days);
        Self::parse(&format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            second / 3_600,
            second % 3_600 / 60,
            second % 60
        ))
    }
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

impl fmt::Display for UtcDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for UtcDateTime {
    type Err = TimestampError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for UtcDateTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for UtcDateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampError;

impl fmt::Display for TimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("timestamp must be a valid RFC 3339 UTC value ending in Z")
    }
}

impl std::error::Error for TimestampError {}

fn valid_rfc3339_utc(value: &[u8]) -> bool {
    if value.len() < 20
        || value[4] != b'-'
        || value[7] != b'-'
        || value[10] != b'T'
        || value[13] != b':'
        || value[16] != b':'
        || value[value.len() - 1] != b'Z'
    {
        return false;
    }
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    if digits.iter().any(|&index| !value[index].is_ascii_digit()) {
        return false;
    }
    if value.len() > 20
        && (value[19] != b'.'
            || value.len() == 21
            || value[20..value.len() - 1]
                .iter()
                .any(|byte| !byte.is_ascii_digit()))
    {
        return false;
    }

    let number = |start: usize, len: usize| -> u32 {
        value[start..start + len]
            .iter()
            .fold(0, |result, byte| result * 10 + u32::from(byte - b'0'))
    };
    let year = number(0, 4);
    let month = number(5, 2);
    let day = number(8, 2);
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year != 0
        && (1..=days).contains(&day)
        && number(11, 2) < 24
        && number(14, 2) < 60
        && number(17, 2) <= 60
}

macro_rules! entity_id {
    ($($variant:ident($id:ty)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub enum EntityId {
            $($variant($id)),+
        }

        impl fmt::Display for EntityId {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$variant(id) => id.fmt(f)),+
                }
            }
        }

        impl FromStr for EntityId {
            type Err = EntityIdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                $(
                    if value.strip_prefix(<$id>::PREFIX).is_some_and(|rest| rest.starts_with('_')) {
                        return <$id>::parse(value)
                            .map(Self::$variant)
                            .map_err(|_| EntityIdError);
                    }
                )+
                Err(EntityIdError)
            }
        }

        impl Serialize for EntityId {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for EntityId {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(D::Error::custom)
            }
        }

        $(impl From<$id> for EntityId {
            fn from(value: $id) -> Self {
                Self::$variant(value)
            }
        })+
    };
}

entity_id!(
    Principal(PrincipalId),
    Project(ProjectId),
    Thread(ThreadId),
    Run(RunId),
    Attempt(AttemptId),
    Turn(TurnId),
    ModelCall(ModelCallId),
    ToolCall(ToolCallId),
    Task(TaskId),
    AgentLink(AgentLinkId),
    ExternalTask(ExternalTaskId),
    DaemonService(DaemonServiceId),
    Workspace(WorkspaceId),
    Process(ProcessId),
    Terminal(TerminalId),
    Approval(ApprovalId),
    Checkpoint(CheckpointId),
    Artifact(ArtifactId),
    Experiment(ExperimentId),
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntityIdError;

impl fmt::Display for EntityIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown or malformed entity identifier")
    }
}

impl std::error::Error for EntityIdError {}

macro_rules! validated_string {
    ($name:ident, $error:ident, $message:literal, $validator:expr) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: &str) -> Result<Self, $error> {
                if ($validator)(value) {
                    Ok(Self(value.to_owned()))
                } else {
                    Err($error)
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = $error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $error;

        impl fmt::Display for $error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str($message)
            }
        }

        impl std::error::Error for $error {}
    };
}

validated_string!(
    EventType,
    EventTypeError,
    "event type must be lowercase dotted identifiers",
    |value: &str| {
        let mut segments = value.split('.');
        let valid = |segment: &str| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        };
        segments.clone().count() >= 2 && segments.all(valid)
    }
);

validated_string!(
    TraceId,
    TraceIdError,
    "trace id must contain 1 to 128 visible ASCII characters",
    |value: &str| {
        !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic())
    }
);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentDigest(String);

impl ContentDigest {
    pub fn parse(value: &str) -> Result<Self, DigestError> {
        let Some((algorithm, digest)) = value.split_once(':') else {
            return Err(DigestError);
        };
        if matches!(algorithm, "blake3" | "sha256")
            && digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(DigestError)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_owned(value: String) -> Result<Self, DigestError> {
        let Some((algorithm, digest)) = value.split_once(':') else {
            return Err(DigestError);
        };
        if matches!(algorithm, "blake3" | "sha256")
            && digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(DigestError)
        }
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ContentDigest {
    type Err = DigestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ContentDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ContentDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DigestError;

impl fmt::Display for DigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("digest must use blake3 or sha256 with 64 lowercase hexadecimal digits")
    }
}

impl std::error::Error for DigestError {}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ArtifactRef(ContentDigest);

pub use super::ids::ArtifactId as ArtifactRecordId;

impl ArtifactRef {
    pub fn parse(value: &str) -> Result<Self, DigestError> {
        let digest = ContentDigest::parse(value)?;
        if value.starts_with("blake3:") {
            Ok(Self(digest))
        } else {
            Err(DigestError)
        }
    }

    pub fn digest(&self) -> &ContentDigest {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl FromStr for ArtifactRef {
    type Err = DigestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for ArtifactRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    Denied,
}

pub trait EventPayload: Clone + fmt::Debug + Eq + Serialize + DeserializeOwned {
    const EVENT_TYPE: &'static str;
    const SCHEMA_VERSION: SchemaVersion;

    fn schema_version(&self) -> SchemaVersion;
}

macro_rules! event_payload {
    ($name:ident, $event_type:literal, {$($field:ident: $type:ty),* $(,)?}) => {
        #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
        pub struct $name {
            pub schema_version: SchemaVersion,
            $(pub $field: $type),*
        }

        impl $name {
            pub fn new($($field: $type),*) -> Self {
                Self {
                    schema_version: SchemaVersion::CURRENT,
                    $($field),*
                }
            }
        }

        impl EventPayload for $name {
            const EVENT_TYPE: &'static str = $event_type;
            const SCHEMA_VERSION: SchemaVersion = SchemaVersion::V1;

            fn schema_version(&self) -> SchemaVersion {
                self.schema_version
            }
        }
    };
}

event_payload!(ProjectCreated, "project.created", {
    project_id: ProjectId,
    principal_id: PrincipalId,
});
event_payload!(ThreadCreated, "thread.created", {
    thread_id: ThreadId,
    project_id: ProjectId,
});
event_payload!(RunQueued, "run.queued", {
    run_id: RunId,
    thread_id: ThreadId,
});
event_payload!(RunTransitioned, "run.transitioned", {
    run_id: RunId,
    transition: RunTransition,
    version: u64,
});
event_payload!(AttemptStarted, "attempt.started", {
    attempt_id: AttemptId,
    run_id: RunId,
    owner: super::lifecycle::AttemptOwnership,
});
event_payload!(AttemptTransitioned, "attempt.transitioned", {
    attempt_id: AttemptId,
    transition: AttemptTransition,
    version: u64,
});
event_payload!(RunConfigMaterialized, "run.config_materialized", {
    run_id: RunId,
    digest: ContentDigest,
    provenance: BTreeMap<ConfigField, LayerKind>,
    canonical_bytes: Vec<u8>,
});
event_payload!(ArtifactRegistered, "artifact.registered", {
    artifact_id: ArtifactRecordId,
    project_id: ProjectId,
    reference: ArtifactRef,
});
event_payload!(ApprovalRequested, "approval.requested", {
    approval_id: ApprovalId,
    run_id: RunId,
});
event_payload!(ApprovalResolved, "approval.resolved", {
    approval_id: ApprovalId,
    decision: ApprovalDecision,
});
event_payload!(ThreadArchived, "thread.archived", {
    thread_id: ThreadId,
    archived: bool,
});

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventEnvelope<P> {
    pub id: EventId,
    pub stream: EntityId,
    pub sequence: StreamSequence,
    pub commit_position: CommitPosition,
    event_type: EventType,
    schema_version: SchemaVersion,
    pub occurred_at: UtcDateTime,
    pub causation_id: CommandId,
    pub correlation_id: EntityId,
    pub attempt_id: Option<AttemptId>,
    pub trace_id: TraceId,
    pub payload: P,
    pub artifacts: Vec<ArtifactRef>,
}

impl<P: EventPayload> EventEnvelope<P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: EventId,
        stream: EntityId,
        sequence: StreamSequence,
        commit_position: CommitPosition,
        occurred_at: UtcDateTime,
        causation_id: CommandId,
        correlation_id: EntityId,
        attempt_id: Option<AttemptId>,
        trace_id: TraceId,
        payload: P,
        artifacts: Vec<ArtifactRef>,
    ) -> Result<Self, EnvelopeError> {
        if payload.schema_version() != P::SCHEMA_VERSION {
            return Err(EnvelopeError::PayloadVersion);
        }
        Ok(Self {
            id,
            stream,
            sequence,
            commit_position,
            event_type: EventType::parse(P::EVENT_TYPE)
                .expect("event payload constants are valid event types"),
            schema_version: SchemaVersion::CURRENT,
            occurred_at,
            causation_id,
            correlation_id,
            attempt_id,
            trace_id,
            payload,
            artifacts,
        })
    }

    pub fn event_type(&self) -> &EventType {
        &self.event_type
    }

    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }
}

impl<P: EventPayload> Serialize for EventEnvelope<P> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("EventEnvelope", 13)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("stream", &self.stream)?;
        state.serialize_field("sequence", &self.sequence)?;
        state.serialize_field("commit_position", &self.commit_position)?;
        state.serialize_field("type", &self.event_type)?;
        state.serialize_field("schema_version", &self.schema_version)?;
        state.serialize_field("occurred_at", &self.occurred_at)?;
        state.serialize_field("causation_id", &self.causation_id)?;
        state.serialize_field("correlation_id", &self.correlation_id)?;
        state.serialize_field("attempt_id", &self.attempt_id)?;
        state.serialize_field("trace_id", &self.trace_id)?;
        state.serialize_field("payload", &self.payload)?;
        state.serialize_field("artifacts", &self.artifacts)?;
        state.end()
    }
}

#[derive(Deserialize)]
struct EnvelopeWire<P> {
    id: EventId,
    stream: EntityId,
    sequence: StreamSequence,
    commit_position: CommitPosition,
    #[serde(rename = "type")]
    event_type: EventType,
    schema_version: SchemaVersion,
    occurred_at: UtcDateTime,
    causation_id: CommandId,
    correlation_id: EntityId,
    attempt_id: Option<AttemptId>,
    trace_id: TraceId,
    payload: P,
    artifacts: Vec<ArtifactRef>,
}

impl<'de, P: EventPayload> Deserialize<'de> for EventEnvelope<P> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = EnvelopeWire::<P>::deserialize(deserializer)?;
        if wire.event_type.as_str() != P::EVENT_TYPE {
            return Err(D::Error::custom("event type does not match payload"));
        }
        if wire.schema_version != SchemaVersion::CURRENT {
            return Err(D::Error::custom("unsupported event envelope version"));
        }
        Self::new(
            wire.id,
            wire.stream,
            wire.sequence,
            wire.commit_position,
            wire.occurred_at,
            wire.causation_id,
            wire.correlation_id,
            wire.attempt_id,
            wire.trace_id,
            wire.payload,
            wire.artifacts,
        )
        .map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeError {
    PayloadVersion,
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadVersion => {
                f.write_str("payload schema version does not match its event type")
            }
        }
    }
}

impl std::error::Error for EnvelopeError {}
