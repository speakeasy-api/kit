use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{
    verify::lsp::{
        facts::{
            NormalizedConfidence, RepositoryFactClassification, RepositoryFactProvenance,
            SemanticFact, SemanticRelationKind,
        },
        session::PositionEncoding,
    },
    workspace::{
        edit::ir::RootRelativePath,
        index::meta::{ContentState, MetadataEntry, MetadataIndex},
        revision::{EntryKind, LimitKind, ManagedWorkspace, RevisionError, RevisionId},
        syntax::{
            FactSource, SourceRange, SyntacticFacts, SyntacticProvenance, SyntacticSymbolKind,
            SyntacticSymbolRecord, UnavailableReason,
        },
    },
};
use serde::{Serialize, Serializer};

const MAP_POLICY: &str = "kit-repository-map-v1";
pub const MAP_POLICY_RANK_VERSION: &str = MAP_POLICY;
const MAP_CURSOR_PREFIX: &str = "kitmap1_";
const MAP_CURSOR_PAYLOAD_BYTES: usize = 200;
pub const MAP_CURSOR_TOKEN_LENGTH: usize = MAP_CURSOR_PREFIX.len() + MAP_CURSOR_PAYLOAD_BYTES * 2;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeclarationId([u8; 32]);

impl DeclarationId {
    pub const fn new(value: [u8; 32]) -> Self {
        Self(value)
    }

    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }

    pub fn parse(value: &str) -> Option<Self> {
        decode_hex::<32>(value).map(Self)
    }
}

impl From<[u8; 32]> for DeclarationId {
    fn from(value: [u8; 32]) -> Self {
        Self(value)
    }
}

impl Serialize for DeclarationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize_hex(&self.0, serializer)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StackFrame {
    pub path: PathBuf,
    pub symbol: Option<String>,
    pub line: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Personalization {
    pub task_terms: Vec<String>,
    pub exact_declaration_ids: Vec<DeclarationId>,
    pub stack_frames: Vec<StackFrame>,
    pub recently_read_paths: Vec<PathBuf>,
    pub current_edit_paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapBudget {
    pub max_items: usize,
    pub max_estimated_tokens: usize,
    pub max_hops: usize,
    pub max_degree: usize,
    pub max_result_bytes: usize,
}

impl Default for MapBudget {
    fn default() -> Self {
        Self {
            max_items: 200,
            max_estimated_tokens: 16_384,
            max_hops: 4,
            max_degree: 64,
            max_result_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapLimits {
    pub max_items: usize,
    pub max_estimated_tokens: usize,
    pub max_hops: usize,
    pub max_degree: usize,
    pub max_result_bytes: usize,
    pub max_task_terms: usize,
    pub max_exact_ids: usize,
    pub max_stack_frames: usize,
    pub max_recent_paths: usize,
    pub max_current_edit_paths: usize,
    pub max_path_filters: usize,
    pub max_language_filters: usize,
    pub max_expansion_seeds: usize,
    pub max_expansion_paths: usize,
    pub max_expansion_symbols: usize,
    pub max_relationship_kinds: usize,
    pub max_semantic_relationships: usize,
    pub max_input_bytes: usize,
    pub max_work: usize,
    pub max_candidates: usize,
    pub max_highlight_bytes: usize,
    pub max_cursor_frontier: usize,
    pub max_time: Duration,
}

impl Default for MapLimits {
    fn default() -> Self {
        Self {
            max_items: 10_000,
            max_estimated_tokens: 1_000_000,
            max_hops: 32,
            max_degree: 10_000,
            max_result_bytes: 4 * 1024 * 1024,
            max_task_terms: 128,
            max_exact_ids: 1_024,
            max_stack_frames: 1_024,
            max_recent_paths: 4_096,
            max_current_edit_paths: 4_096,
            max_path_filters: 1_024,
            max_language_filters: 128,
            max_expansion_seeds: 1_024,
            max_expansion_paths: 1_024,
            max_expansion_symbols: 1_024,
            max_relationship_kinds: 32,
            max_semantic_relationships: 100_000,
            max_input_bytes: 8 * 1024 * 1024,
            max_work: 10_000_000,
            max_candidates: 100_000,
            max_highlight_bytes: 4 * 1024,
            max_cursor_frontier: 100_000,
            max_time: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    Contains,
    ContainedBy,
    SemanticDeclaration,
    SemanticDefinition,
    SemanticTypeDefinition,
    SemanticImplementation,
    SemanticReference,
}

impl RelationshipKind {
    fn semantic(relation: SemanticRelationKind) -> Self {
        match relation {
            SemanticRelationKind::Declaration => Self::SemanticDeclaration,
            SemanticRelationKind::Definition => Self::SemanticDefinition,
            SemanticRelationKind::TypeDefinition => Self::SemanticTypeDefinition,
            SemanticRelationKind::Implementation => Self::SemanticImplementation,
            SemanticRelationKind::Reference => Self::SemanticReference,
        }
    }

    pub(crate) fn is_semantic(self) -> bool {
        !matches!(self, Self::Contains | Self::ContainedBy)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpansionPurpose {
    Dependencies,
    Dependents,
    #[default]
    Neighborhood,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpansionRequest {
    pub seeds: Vec<DeclarationId>,
    pub paths: Vec<RootRelativePath>,
    pub symbols: Vec<String>,
    pub score_band: Option<ScoreBand>,
    pub purpose: ExpansionPurpose,
    pub relationships: Vec<RelationshipKind>,
}

/// Inclusive `u64` rank interval under [`MAP_POLICY_RANK_VERSION`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScoreBand {
    pub min: u64,
    pub max: u64,
}

impl Default for ExpansionRequest {
    fn default() -> Self {
        Self {
            seeds: Vec::new(),
            paths: Vec::new(),
            symbols: Vec::new(),
            score_band: None,
            purpose: ExpansionPurpose::Neighborhood,
            relationships: vec![RelationshipKind::Contains, RelationshipKind::ContainedBy],
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepositoryMapRequest {
    pub personalization: Personalization,
    pub budget: MapBudget,
    pub expansion: ExpansionRequest,
    pub path_prefixes: Vec<PathBuf>,
    pub languages: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct SemanticRelationship<'a> {
    pub source_declaration: DeclarationId,
    pub fact: &'a SemanticFact,
}

impl<'a> SemanticRelationship<'a> {
    pub const fn new(source_declaration: DeclarationId, fact: &'a SemanticFact) -> Self {
        Self {
            source_declaration,
            fact,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapBound {
    Items,
    EstimatedTokens,
    Hops,
    Degree,
    ResultBytes,
}

#[derive(Debug)]
pub enum MapError {
    BoundExceeded(MapBound),
    InvalidRequest(&'static str),
    InvalidLimits(&'static str),
    InvalidIndex(&'static str),
    InvalidFact(&'static str),
    SelectorNoMatch(&'static str),
    StaleFact,
    SemanticEvidenceUnavailable,
    CursorMismatch,
    Revision(RevisionError),
    TimeLimit,
    Serialization(serde_json::Error),
}

impl From<RevisionError> for MapError {
    fn from(value: RevisionError) -> Self {
        match value {
            RevisionError::LimitExceeded(LimitKind::Time) => Self::TimeLimit,
            value => Self::Revision(value),
        }
    }
}

impl fmt::Display for MapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BoundExceeded(bound) => {
                write!(formatter, "repository map {bound:?} bound exceeded")
            }
            Self::InvalidRequest(reason) => {
                write!(formatter, "invalid repository map request: {reason}")
            }
            Self::InvalidLimits(reason) => {
                write!(formatter, "invalid repository map limits: {reason}")
            }
            Self::InvalidIndex(reason) => {
                write!(formatter, "invalid repository map index: {reason}")
            }
            Self::InvalidFact(reason) => write!(formatter, "invalid repository map fact: {reason}"),
            Self::SelectorNoMatch(selector) => {
                write!(
                    formatter,
                    "repository map {selector} selector matched no indexed items"
                )
            }
            Self::StaleFact => formatter.write_str("repository map semantic fact is stale"),
            Self::SemanticEvidenceUnavailable => {
                formatter.write_str("repository map semantic evidence is unavailable")
            }
            Self::CursorMismatch => {
                formatter.write_str("repository map cursor does not match the request")
            }
            Self::Revision(error) => error.fmt(formatter),
            Self::TimeLimit => formatter.write_str("repository map time limit exceeded"),
            Self::Serialization(error) => write!(formatter, "serialize repository map: {error}"),
        }
    }
}

impl std::error::Error for MapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FactClassification {
    Syntactic,
    Semantic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MapFactSource {
    TreeSitter,
    Lsp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PathKind {
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PathContentState {
    Directory,
    Text,
    Binary,
    InvalidUtf8,
    TooLarge,
    IndexLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PathFactSource {
    RepositoryTree,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct PathProvenance {
    pub classification: FactClassification,
    pub source: PathFactSource,
    pub revision: RevisionId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RepositoryPathNode {
    pub path: String,
    pub kind: PathKind,
    pub language: Option<String>,
    pub content_state: PathContentState,
    pub size: u64,
    pub revision: RevisionId,
    pub provenance: PathProvenance,
    pub degree: usize,
    pub hops: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RepositoryPathEdge {
    pub source_path: String,
    pub target_path: String,
    pub relationship: RelationshipKind,
    pub hops: usize,
    pub provenance: PathProvenance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct MapFactProvenance {
    pub classification: FactClassification,
    pub source: MapFactSource,
    pub revision: RevisionId,
    pub confidence_millis: u16,
    #[serde(serialize_with = "serialize_hex")]
    pub grammar_identity: [u8; 32],
    #[serde(serialize_with = "serialize_hex")]
    pub query_set_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MapSymbolKind {
    Function,
    Struct,
    Enum,
    Union,
    TypeAlias,
    Trait,
    Module,
    Constant,
    Static,
    Macro,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct MapSourceRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}

impl From<SourceRange> for MapSourceRange {
    fn from(value: SourceRange) -> Self {
        Self {
            start_byte: value.start_byte,
            end_byte: value.end_byte,
            start_line: value.start_line,
            end_line: value.end_line,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SourceLine {
    pub line: usize,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RelationshipState {
    Available,
    UnavailableNotExtracted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RelationshipAvailability {
    pub containment: RelationshipState,
    pub imports: RelationshipState,
    pub exports: RelationshipState,
    pub definitions: RelationshipState,
    pub references: RelationshipState,
    pub callers: RelationshipState,
    pub callees: RelationshipState,
    pub tests: RelationshipState,
    pub documentation: RelationshipState,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum RankingReason {
    ExactDeclarationId,
    StackPath,
    StackSymbol,
    StackLine,
    CurrentEdit,
    RecentlyRead,
    ExplicitSemanticEvidence,
    ExactTaskTerm,
    SubstringTaskTerm,
    ExpansionSeed,
    ExpansionNeighbor,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RepositoryMapEntry {
    pub declaration_id: DeclarationId,
    pub path: String,
    pub language: String,
    pub qualified_name: String,
    pub display_name: String,
    pub kind: MapSymbolKind,
    pub signature: String,
    pub source_range: MapSourceRange,
    pub source_lines: Vec<SourceLine>,
    pub provenance: MapFactProvenance,
    pub relationships: RelationshipAvailability,
    pub rank: u64,
    pub degree: usize,
    pub hops: Option<usize>,
    pub reasons: Vec<RankingReason>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct LspEdgeProvenance {
    pub classification: FactClassification,
    pub source: MapFactSource,
    pub revision: RevisionId,
    pub confidence_millis: u16,
    pub origin_uri: String,
    pub document_version: i32,
    pub request_generation: u64,
    pub request_id: u32,
    pub server_artifact: String,
    pub server_configuration: String,
    pub position_encoding: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum EdgeProvenance {
    TreeSitter { fact: MapFactProvenance },
    Lsp { fact: LspEdgeProvenance },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RepositoryMapEdge {
    pub source_declaration: DeclarationId,
    pub target_declaration: DeclarationId,
    pub relationship: RelationshipKind,
    pub hops: usize,
    pub source_range: MapSourceRange,
    pub target_range: MapSourceRange,
    pub fact_range: MapByteRange,
    pub semantic_source_range: Option<MapByteRange>,
    pub semantic_target_range: Option<MapByteRange>,
    pub provenance: EdgeProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct MapByteRange {
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct MapOmissions {
    pub ranked_entries: usize,
    pub index_entries: usize,
    pub syntax_declarations: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MapCompleteness {
    Complete,
    RankedEntriesOmitted,
    IndexIncomplete,
    SyntaxIncomplete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MapCursor {
    revision: RevisionId,
    index_digest: [u8; 32],
    policy_digest: [u8; 32],
    context_digest: [u8; 32],
    page: usize,
    mandatory_entries: usize,
    mandatory_edges: usize,
    frontier: usize,
    digest: [u8; 32],
}

impl MapCursor {
    pub fn to_token(&self) -> String {
        let mut payload = Vec::with_capacity(MAP_CURSOR_PAYLOAD_BYTES);
        payload.extend_from_slice(self.revision.as_bytes());
        payload.extend_from_slice(&self.index_digest);
        payload.extend_from_slice(&self.policy_digest);
        payload.extend_from_slice(&self.context_digest);
        payload.extend_from_slice(&(self.page as u64).to_be_bytes());
        payload.extend_from_slice(&(self.mandatory_entries as u64).to_be_bytes());
        payload.extend_from_slice(&(self.mandatory_edges as u64).to_be_bytes());
        payload.extend_from_slice(&(self.frontier as u64).to_be_bytes());
        payload.extend_from_slice(&[0; 8]);
        payload.extend_from_slice(&self.digest);
        debug_assert_eq!(payload.len(), MAP_CURSOR_PAYLOAD_BYTES);
        format!("{MAP_CURSOR_PREFIX}{}", encode_hex(&payload))
    }

    pub fn from_token(token: &str) -> Result<Self, MapCursorTokenError> {
        if token.len() != MAP_CURSOR_TOKEN_LENGTH {
            return Err(MapCursorTokenError);
        }
        let encoded = token
            .strip_prefix(MAP_CURSOR_PREFIX)
            .ok_or(MapCursorTokenError)?;
        let payload = decode_hex::<MAP_CURSOR_PAYLOAD_BYTES>(encoded).ok_or(MapCursorTokenError)?;
        let revision = RevisionId::parse(&format!("r:{}", encode_hex(&payload[0..32])))
            .ok_or(MapCursorTokenError)?;
        let index_digest = payload[32..64]
            .try_into()
            .map_err(|_| MapCursorTokenError)?;
        let policy_digest = payload[64..96]
            .try_into()
            .map_err(|_| MapCursorTokenError)?;
        let context_digest = payload[96..128]
            .try_into()
            .map_err(|_| MapCursorTokenError)?;
        let page = usize::try_from(u64::from_be_bytes(
            payload[128..136]
                .try_into()
                .map_err(|_| MapCursorTokenError)?,
        ))
        .map_err(|_| MapCursorTokenError)?;
        let mandatory_entries = usize::try_from(u64::from_be_bytes(
            payload[136..144]
                .try_into()
                .map_err(|_| MapCursorTokenError)?,
        ))
        .map_err(|_| MapCursorTokenError)?;
        let mandatory_edges = usize::try_from(u64::from_be_bytes(
            payload[144..152]
                .try_into()
                .map_err(|_| MapCursorTokenError)?,
        ))
        .map_err(|_| MapCursorTokenError)?;
        let frontier = usize::try_from(u64::from_be_bytes(
            payload[152..160]
                .try_into()
                .map_err(|_| MapCursorTokenError)?,
        ))
        .map_err(|_| MapCursorTokenError)?;
        if payload[160..168] != [0; 8] {
            return Err(MapCursorTokenError);
        }
        let digest = payload[168..200]
            .try_into()
            .map_err(|_| MapCursorTokenError)?;
        let cursor = Self {
            revision,
            index_digest,
            policy_digest,
            context_digest,
            page,
            mandatory_entries,
            mandatory_edges,
            frontier,
            digest,
        };
        if cursor.digest
            != cursor_digest(
                cursor.revision,
                cursor.index_digest,
                cursor.policy_digest,
                cursor.context_digest,
                cursor.page,
                cursor.mandatory_entries,
                cursor.mandatory_edges,
                cursor.frontier,
            )
        {
            return Err(MapCursorTokenError);
        }
        Ok(cursor)
    }
}

impl Serialize for MapCursor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_token())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapCursorTokenError;

impl fmt::Display for MapCursorTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid repository map cursor token")
    }
}

impl std::error::Error for MapCursorTokenError {}

/// Stable serialized repository-map response. Its wire fields remain private;
/// use canonical JSON at API boundaries and the scalar accessors in core code.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryMap {
    revision: RevisionId,
    #[serde(serialize_with = "serialize_hex")]
    index_digest: [u8; 32],
    #[serde(serialize_with = "serialize_hex")]
    policy_digest: [u8; 32],
    #[serde(serialize_with = "serialize_hex")]
    neighborhood_digest: [u8; 32],
    #[serde(serialize_with = "serialize_hex")]
    evidence_digest: [u8; 32],
    #[serde(serialize_with = "serialize_hex")]
    options_digest: [u8; 32],
    path_nodes: Vec<RepositoryPathNode>,
    path_edges: Vec<RepositoryPathEdge>,
    entries: Vec<RepositoryMapEntry>,
    edges: Vec<RepositoryMapEdge>,
    omissions: MapOmissions,
    completeness: MapCompleteness,
    item_count: usize,
    estimated_tokens: usize,
    result_bytes: usize,
    truncated: bool,
    cursor: Option<MapCursor>,
}

impl RepositoryMap {
    pub const fn item_count(&self) -> usize {
        self.item_count
    }

    pub const fn estimated_tokens(&self) -> usize {
        self.estimated_tokens
    }

    pub const fn result_bytes(&self) -> usize {
        self.result_bytes
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub const fn cursor(&self) -> Option<&MapCursor> {
        self.cursor.as_ref()
    }

    pub fn to_canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

struct Candidate<'a> {
    entry: &'a MetadataEntry,
    record: &'a SyntacticSymbolRecord,
    path: &'a str,
    rank: u64,
    degree: usize,
    reasons: Vec<RankingReason>,
}

struct GraphEdge<'a> {
    source: DeclarationId,
    target: DeclarationId,
    relationship: RelationshipKind,
    provenance: GraphProvenance<'a>,
}

enum GraphProvenance<'a> {
    Syntax(&'a SyntacticProvenance),
    Semantic(&'a SemanticFact),
}

#[derive(Clone, Copy)]
struct PathGraphEdge {
    source: usize,
    target: usize,
    relationship: RelationshipKind,
}

struct PathExpansion {
    nodes: BTreeSet<usize>,
    edges: BTreeSet<usize>,
    hops: BTreeMap<usize, usize>,
    degree: BTreeMap<usize, usize>,
    graph: Vec<PathGraphEdge>,
}

type ExpansionOutput = (
    BTreeSet<DeclarationId>,
    BTreeSet<usize>,
    BTreeMap<DeclarationId, usize>,
);

type Adjacency = BTreeMap<DeclarationId, Vec<(usize, DeclarationId)>>;
type TargetIndex<'a> = BTreeMap<&'a str, Vec<(usize, usize, DeclarationId)>>;

pub fn build_repository_map(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    request: &RepositoryMapRequest,
    evidence: &[SemanticRelationship<'_>],
    limits: MapLimits,
    cursor: Option<&MapCursor>,
) -> Result<RepositoryMap, MapError> {
    let started = Instant::now();
    let deadline = started.checked_add(limits.max_time).unwrap_or(started);
    let mut work = 0_usize;
    validate_request(request, evidence, limits, &mut work, deadline)?;
    let current = workspace.validate_revision_until(index.revision(), deadline)?;
    if current.epoch() != index.epoch() {
        return Err(MapError::InvalidIndex(
            "workspace epoch does not match index",
        ));
    }

    let policy_digest = *blake3::hash(MAP_POLICY.as_bytes()).as_bytes();
    let neighborhood_digest = digest_request(request, &mut work, limits.max_work, deadline)?;
    let evidence_digest = digest_evidence(evidence, &mut work, limits.max_work, deadline)?;
    let options_digest = digest_options(request.budget, limits, deadline)?;
    validate_cursor(
        cursor,
        index,
        policy_digest,
        options_digest,
        neighborhood_digest,
        evidence_digest,
        limits.max_cursor_frontier,
    )?;
    let frontier = cursor.map_or(0, |cursor| cursor.frontier);

    let mut all = BTreeMap::<DeclarationId, Candidate<'_>>::new();
    for entry in index.entries() {
        check_deadline(deadline)?;
        let path = entry
            .path
            .to_str()
            .ok_or(MapError::InvalidIndex("syntax path is not UTF-8"))?;
        for record in entry.syntax_records.iter() {
            charge_work(&mut work, 1, limits.max_work)?;
            validate_record(index, entry, record)?;
            let id = DeclarationId::from(record.declaration_id());
            if all.contains_key(&id) {
                return Err(MapError::InvalidIndex("duplicate declaration id"));
            }
            if all.len() == limits.max_candidates {
                return Err(MapError::InvalidRequest("candidate limit exceeded"));
            }
            all.insert(
                id,
                Candidate {
                    entry,
                    record,
                    path,
                    rank: 0,
                    degree: 0,
                    reasons: Vec::new(),
                },
            );
        }
    }
    if request
        .personalization
        .exact_declaration_ids
        .iter()
        .any(|id| !all.contains_key(id))
    {
        return Err(MapError::InvalidRequest("exact declaration id is unknown"));
    }

    let mut selected_ids = BTreeSet::new();
    for (id, candidate) in &all {
        check_deadline(deadline)?;
        if selected(candidate, request, &mut work, limits.max_work, deadline)? {
            selected_ids.insert(*id);
        }
    }
    let selected = selected_ids;
    let semantic_requested = request
        .expansion
        .relationships
        .iter()
        .any(|relationship| relationship.is_semantic());
    let target_index = (semantic_requested && !evidence.is_empty())
        .then(|| build_target_index(&all, &mut work, limits.max_work, deadline))
        .transpose()?;
    let mut graph = syntax_edges(&all, &selected, request, &mut work, limits, deadline)?;
    if let Some(target_index) = &target_index {
        add_semantic_edges(
            &mut graph,
            &all,
            target_index,
            &selected,
            evidence,
            index.revision(),
            request,
            &mut work,
            limits,
            deadline,
        )?;
    }
    charge_sort_work(&mut work, graph.len(), limits.max_work, deadline)?;
    graph.sort_by(compare_graph_edges);
    check_deadline(deadline)?;
    graph.dedup_by(|left, right| {
        left.source == right.source
            && left.target == right.target
            && left.relationship == right.relationship
    });

    let mut adjacency = Adjacency::new();
    for (edge_index, edge) in graph.iter().enumerate() {
        check_deadline(deadline)?;
        charge_work(&mut work, 1, limits.max_work)?;
        adjacency
            .entry(edge.source)
            .or_default()
            .push((edge_index, edge.target));
        if request.expansion.purpose == ExpansionPurpose::Neighborhood
            && edge.relationship.is_semantic()
            && edge.source != edge.target
        {
            charge_work(&mut work, 1, limits.max_work)?;
            adjacency
                .entry(edge.target)
                .or_default()
                .push((edge_index, edge.source));
        }
    }
    for (id, candidate) in &mut all {
        check_deadline(deadline)?;
        charge_work(&mut work, 1, limits.max_work)?;
        candidate.degree = adjacency.get(id).map_or(0, Vec::len);
    }

    let mut semantic_ids = BTreeSet::new();
    for edge in graph.iter().filter(|edge| edge.relationship.is_semantic()) {
        check_deadline(deadline)?;
        charge_work(&mut work, 2, limits.max_work)?;
        semantic_ids.extend([edge.source, edge.target]);
    }
    rank_candidates(
        &mut all,
        request,
        &semantic_ids,
        &mut work,
        limits.max_work,
        deadline,
    )?;

    let path_expansion = expand_paths(index, request, &mut work, limits, deadline)?;
    let (mandatory_ids, mandatory_edges, hops) = expand(
        &all, &selected, &adjacency, request, &mut work, limits, deadline,
    )?;
    let cursor_mandatory_entries = mandatory_ids
        .len()
        .saturating_add(path_expansion.nodes.len());
    let cursor_mandatory_edges = mandatory_edges
        .len()
        .saturating_add(path_expansion.edges.len());
    if cursor_mandatory_entries.saturating_add(cursor_mandatory_edges) > request.budget.max_items {
        return Err(MapError::BoundExceeded(MapBound::Items));
    }
    if let Some(cursor) = cursor
        && (cursor.page == 0
            || cursor.mandatory_entries != cursor_mandatory_entries
            || cursor.mandatory_edges != cursor_mandatory_edges)
    {
        return Err(MapError::CursorMismatch);
    }

    let mut ranked = selected.iter().copied().collect::<Vec<_>>();
    charge_sort_work(&mut work, ranked.len(), limits.max_work, deadline)?;
    ranked.sort_by(|left, right| compare_candidates(&all[left], &all[right]));
    check_deadline(deadline)?;
    if frontier > ranked.len() {
        return Err(MapError::CursorMismatch);
    }

    let syntax_omitted = index
        .entries()
        .iter()
        .map(|entry| entry.syntax_omitted)
        .sum::<usize>();
    let index_incomplete = index.source_truncated();
    let syntax_incomplete = index.entries().iter().any(|entry| entry.syntax_truncated);
    let mut response = RepositoryMap {
        revision: index.revision(),
        index_digest: *index.index_digest(),
        policy_digest,
        neighborhood_digest,
        evidence_digest,
        options_digest,
        path_nodes: Vec::new(),
        path_edges: Vec::new(),
        entries: Vec::new(),
        edges: Vec::new(),
        omissions: MapOmissions {
            ranked_entries: 0,
            index_entries: usize::from(index_incomplete),
            syntax_declarations: syntax_omitted,
        },
        completeness: MapCompleteness::Complete,
        item_count: 0,
        estimated_tokens: 0,
        result_bytes: 0,
        truncated: false,
        cursor: None,
    };

    if cursor.is_none() {
        precharge_mandatory(
            &all,
            &graph,
            &mandatory_ids,
            &mandatory_edges,
            index,
            &path_expansion,
            request.budget,
            limits.max_highlight_bytes,
            &mut work,
            limits.max_work,
            deadline,
        )?;
        for entry_index in &path_expansion.nodes {
            response.path_nodes.push(render_path_node(
                &index.entries()[*entry_index],
                path_expansion.hops[entry_index],
                path_expansion.degree[entry_index],
                index.revision(),
                &mut work,
                limits.max_work,
                deadline,
            )?);
        }
        for edge_index in &path_expansion.edges {
            let edge = path_expansion.graph[*edge_index];
            response.path_edges.push(render_path_edge(
                edge,
                index,
                path_expansion.hops[&edge.source].max(path_expansion.hops[&edge.target]),
            )?);
        }
        for id in &mandatory_ids {
            response.entries.push(render_entry(
                &all[id],
                hops.get(id).copied(),
                true,
                limits.max_highlight_bytes,
                &mut work,
                limits.max_work,
                deadline,
            )?);
        }
        for edge_index in &mandatory_edges {
            let edge = &graph[*edge_index];
            response.edges.push(render_edge(
                edge,
                &all,
                hops.get(&edge.source)
                    .copied()
                    .unwrap_or(0)
                    .max(hops.get(&edge.target).copied().unwrap_or(0)),
            ));
        }
    }
    charge_sort_work(&mut work, response.entries.len(), limits.max_work, deadline)?;
    charge_sort_work(
        &mut work,
        response.path_nodes.len(),
        limits.max_work,
        deadline,
    )?;
    response.path_nodes.sort_by(|left, right| {
        left.hops
            .cmp(&right.hops)
            .then_with(|| left.path.cmp(&right.path))
    });
    charge_sort_work(
        &mut work,
        response.path_edges.len(),
        limits.max_work,
        deadline,
    )?;
    response.path_edges.sort_by(|left, right| {
        left.hops
            .cmp(&right.hops)
            .then_with(|| left.source_path.cmp(&right.source_path))
            .then_with(|| left.target_path.cmp(&right.target_path))
            .then_with(|| left.relationship.cmp(&right.relationship))
    });
    response.entries.sort_by(|left, right| {
        left.hops
            .cmp(&right.hops)
            .then_with(|| right.rank.cmp(&left.rank))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| {
                left.source_range
                    .start_byte
                    .cmp(&right.source_range.start_byte)
            })
            .then_with(|| left.declaration_id.cmp(&right.declaration_id))
    });
    check_deadline(deadline)?;
    charge_sort_work(&mut work, response.edges.len(), limits.max_work, deadline)?;
    response.edges.sort_by(compare_output_edges);
    check_deadline(deadline)?;
    response.item_count = response.path_nodes.len()
        + response.path_edges.len()
        + response.entries.len()
        + response.edges.len();
    if !settle_size(&mut response, request.budget.max_result_bytes, deadline)? {
        return Err(MapError::BoundExceeded(MapBound::ResultBytes));
    }
    enforce_serialized_budget(&response, request.budget)?;

    let mandatory_item_count = response.item_count;
    let available = request
        .budget
        .max_items
        .saturating_sub(mandatory_item_count);
    let mut base_frontier = frontier;
    while base_frontier < ranked.len() && mandatory_ids.contains(&ranked[base_frontier]) {
        base_frontier += 1;
    }
    let mut ranked_page = Vec::with_capacity(available.min(ranked.len() - base_frontier));
    let mut ranked_retained = 0_usize;
    let mut position = base_frontier;
    while position < ranked.len() && ranked_page.len() < available {
        check_deadline(deadline)?;
        let id = ranked[position];
        position += 1;
        if mandatory_ids.contains(&id) {
            continue;
        }
        let retained = retained_entry_bytes(&all[&id], limits.max_highlight_bytes);
        let next_retained = ranked_retained.saturating_add(retained);
        if !ranked_page.is_empty()
            && (next_retained > request.budget.max_result_bytes
                || next_retained.div_ceil(4) > request.budget.max_estimated_tokens)
        {
            break;
        }
        ranked_retained = next_retained;
        while position < ranked.len() && mandatory_ids.contains(&ranked[position]) {
            position += 1;
        }
        ranked_page.push((id, position));
    }

    let mut rendered = VecDeque::with_capacity(ranked_page.len());
    for (id, _) in &ranked_page {
        rendered.push_back(render_entry(
            &all[id],
            None,
            false,
            limits.max_highlight_bytes,
            &mut work,
            limits.max_work,
            deadline,
        )?);
    }

    let mandatory_entry_count = response.entries.len();
    let mut low = 0;
    let mut high = ranked_page.len();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        move_ranked_prefix(
            &mut response.entries,
            &mut rendered,
            mandatory_entry_count,
            middle,
        );
        let consumed = if middle == 0 {
            base_frontier
        } else {
            ranked_page[middle - 1].1
        };
        update_page_state(
            &mut response,
            ranked.len(),
            consumed,
            index_incomplete,
            syntax_incomplete,
            resumable_cursor(
                cursor,
                index,
                policy_digest,
                options_digest,
                neighborhood_digest,
                evidence_digest,
                cursor_mandatory_entries,
                cursor_mandatory_edges,
                consumed,
                ranked.len(),
                limits.max_cursor_frontier,
                index_incomplete,
                syntax_incomplete,
            ),
        );
        let serialized = settle_size(&mut response, request.budget.max_result_bytes, deadline)?;
        check_deadline(deadline)?;
        if !serialized || exceeds_serialized_budget(&response, request.budget) {
            high = middle - 1;
        } else {
            low = middle;
        }
    }

    move_ranked_prefix(
        &mut response.entries,
        &mut rendered,
        mandatory_entry_count,
        low,
    );
    let consumed = if low == 0 {
        base_frontier
    } else {
        ranked_page[low - 1].1
    };
    update_page_state(
        &mut response,
        ranked.len(),
        consumed,
        index_incomplete,
        syntax_incomplete,
        resumable_cursor(
            cursor,
            index,
            policy_digest,
            options_digest,
            neighborhood_digest,
            evidence_digest,
            cursor_mandatory_entries,
            cursor_mandatory_edges,
            consumed,
            ranked.len(),
            limits.max_cursor_frontier,
            index_incomplete,
            syntax_incomplete,
        ),
    );
    if !settle_size(&mut response, request.budget.max_result_bytes, deadline)? {
        return Err(MapError::BoundExceeded(MapBound::ResultBytes));
    }
    if response.truncated
        && response.cursor.is_some()
        && response.entries.len() == mandatory_entry_count
        && consumed == frontier
        && (cursor.is_some() || mandatory_item_count == 0)
    {
        return if response.estimated_tokens > request.budget.max_estimated_tokens {
            Err(MapError::BoundExceeded(MapBound::EstimatedTokens))
        } else {
            Err(MapError::BoundExceeded(MapBound::ResultBytes))
        };
    }
    enforce_serialized_budget(&response, request.budget)?;
    workspace.validate_revision_until(index.revision(), deadline)?;
    Ok(response)
}

fn validate_request(
    request: &RepositoryMapRequest,
    evidence: &[SemanticRelationship<'_>],
    limits: MapLimits,
    work: &mut usize,
    deadline: Instant,
) -> Result<(), MapError> {
    if request
        .expansion
        .relationships
        .iter()
        .any(|relationship| relationship.is_semantic())
        && evidence.is_empty()
    {
        return Err(MapError::SemanticEvidenceUnavailable);
    }
    if limits.max_items == 0
        || limits.max_estimated_tokens == 0
        || limits.max_degree == 0
        || limits.max_result_bytes == 0
        || limits.max_task_terms == 0
        || limits.max_exact_ids == 0
        || limits.max_stack_frames == 0
        || limits.max_recent_paths == 0
        || limits.max_current_edit_paths == 0
        || limits.max_path_filters == 0
        || limits.max_language_filters == 0
        || limits.max_expansion_seeds == 0
        || limits.max_expansion_paths == 0
        || limits.max_expansion_symbols == 0
        || limits.max_relationship_kinds == 0
        || limits.max_semantic_relationships == 0
        || limits.max_input_bytes == 0
        || limits.max_work == 0
        || limits.max_candidates == 0
        || limits.max_highlight_bytes == 0
        || limits.max_cursor_frontier == 0
        || limits.max_time.is_zero()
    {
        return Err(MapError::InvalidLimits("all finite limits must be nonzero"));
    }
    for (actual, maximum, bound) in [
        (request.budget.max_items, limits.max_items, MapBound::Items),
        (
            request.budget.max_estimated_tokens,
            limits.max_estimated_tokens,
            MapBound::EstimatedTokens,
        ),
        (request.budget.max_hops, limits.max_hops, MapBound::Hops),
        (
            request.budget.max_degree,
            limits.max_degree,
            MapBound::Degree,
        ),
        (
            request.budget.max_result_bytes,
            limits.max_result_bytes,
            MapBound::ResultBytes,
        ),
    ] {
        if actual > maximum {
            return Err(MapError::BoundExceeded(bound));
        }
    }
    if request.budget.max_items == 0 {
        return Err(MapError::BoundExceeded(MapBound::Items));
    }
    if request.budget.max_estimated_tokens == 0 {
        return Err(MapError::BoundExceeded(MapBound::EstimatedTokens));
    }
    if request.budget.max_degree == 0 {
        return Err(MapError::BoundExceeded(MapBound::Degree));
    }
    if request.budget.max_result_bytes == 0 {
        return Err(MapError::BoundExceeded(MapBound::ResultBytes));
    }

    let personalization = &request.personalization;
    for (actual, maximum, reason) in [
        (
            personalization.task_terms.len(),
            limits.max_task_terms,
            "too many task terms",
        ),
        (
            personalization.exact_declaration_ids.len(),
            limits.max_exact_ids,
            "too many exact ids",
        ),
        (
            personalization.stack_frames.len(),
            limits.max_stack_frames,
            "too many stack frames",
        ),
        (
            personalization.recently_read_paths.len(),
            limits.max_recent_paths,
            "too many recent paths",
        ),
        (
            personalization.current_edit_paths.len(),
            limits.max_current_edit_paths,
            "too many edit paths",
        ),
        (
            request.path_prefixes.len(),
            limits.max_path_filters,
            "too many path filters",
        ),
        (
            request.languages.len(),
            limits.max_language_filters,
            "too many language filters",
        ),
        (
            request.expansion.seeds.len(),
            limits.max_expansion_seeds,
            "too many expansion seeds",
        ),
        (
            request.expansion.paths.len(),
            limits.max_expansion_paths,
            "too many expansion paths",
        ),
        (
            request.expansion.symbols.len(),
            limits.max_expansion_symbols,
            "too many expansion symbols",
        ),
        (
            request.expansion.relationships.len(),
            limits.max_relationship_kinds,
            "too many relationship filters",
        ),
        (
            evidence.len(),
            limits.max_semantic_relationships,
            "too many semantic relationships",
        ),
    ] {
        if actual > maximum {
            return Err(MapError::InvalidRequest(reason));
        }
    }
    let fixed_ids = personalization
        .exact_declaration_ids
        .len()
        .checked_add(request.expansion.seeds.len())
        .and_then(|count| count.checked_add(evidence.len()))
        .and_then(|count| count.checked_mul(32))
        .ok_or(MapError::InvalidRequest("input size overflow"))?;
    let mut bytes = fixed_ids
        .checked_add(
            personalization
                .stack_frames
                .len()
                .checked_mul(std::mem::size_of::<usize>())
                .ok_or(MapError::InvalidRequest("input size overflow"))?,
        )
        .and_then(|total| total.checked_add(request.expansion.relationships.len()))
        .and_then(|total| {
            total.checked_add(usize::from(request.expansion.score_band.is_some()) * 16)
        })
        .ok_or(MapError::InvalidRequest("input size overflow"))?;
    for value in personalization
        .task_terms
        .iter()
        .chain(
            personalization
                .stack_frames
                .iter()
                .filter_map(|frame| frame.symbol.as_ref()),
        )
        .chain(request.languages.iter())
        .chain(request.expansion.symbols.iter())
    {
        check_deadline(deadline)?;
        charge_work(work, 1, limits.max_work)?;
        if value.is_empty() {
            return Err(MapError::InvalidRequest("empty text filter"));
        }
        bytes = bytes
            .checked_add(value.len())
            .ok_or(MapError::InvalidRequest("input size overflow"))?;
    }
    for path in personalization
        .stack_frames
        .iter()
        .map(|frame| &frame.path)
        .chain(personalization.recently_read_paths.iter())
        .chain(personalization.current_edit_paths.iter())
        .chain(request.path_prefixes.iter())
    {
        check_deadline(deadline)?;
        charge_work(work, 1, limits.max_work)?;
        validate_relative_path(path)?;
        bytes = bytes
            .checked_add(path.as_os_str().as_encoded_bytes().len())
            .ok_or(MapError::InvalidRequest("input size overflow"))?;
    }
    for path in &request.expansion.paths {
        check_deadline(deadline)?;
        charge_work(work, 1, limits.max_work)?;
        bytes = bytes
            .checked_add(path.as_str().len())
            .ok_or(MapError::InvalidRequest("input size overflow"))?;
    }
    if request
        .expansion
        .score_band
        .is_some_and(|band| band.min > band.max)
    {
        return Err(MapError::InvalidRequest(
            "expansion score band minimum exceeds maximum",
        ));
    }
    for relationship in evidence {
        check_deadline(deadline)?;
        charge_work(work, 1, limits.max_work)?;
        let fact = relationship.fact;
        bytes = bytes
            .checked_add(fact.path().as_path().as_str().len())
            .and_then(|total| total.checked_add(fact.provenance().origin().uri().len()))
            .and_then(|total| {
                total.checked_add(fact.provenance().server().server_artifact.as_str().len())
            })
            .and_then(|total| {
                total.checked_add(fact.provenance().server().configuration.as_str().len())
            })
            .and_then(|total| total.checked_add(6 * std::mem::size_of::<u64>()))
            .ok_or(MapError::InvalidRequest("input size overflow"))?;
    }
    if bytes > limits.max_input_bytes {
        return Err(MapError::InvalidRequest("input byte limit exceeded"));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), MapError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        || path.to_str().is_none()
    {
        Err(MapError::InvalidRequest(
            "path must be a UTF-8 root-relative path",
        ))
    } else {
        Ok(())
    }
}

fn validate_record(
    index: &MetadataIndex,
    entry: &MetadataEntry,
    record: &SyntacticSymbolRecord,
) -> Result<(), MapError> {
    if record.workspace_revision() != index.revision() {
        return Err(MapError::InvalidIndex(
            "syntax record revision does not match index",
        ));
    }
    if record.canonical_path() != entry.path {
        return Err(MapError::InvalidIndex(
            "syntax record path does not match entry",
        ));
    }
    if entry.language.as_deref() != Some(record.language().as_str()) {
        return Err(MapError::InvalidIndex(
            "syntax record language does not match entry",
        ));
    }
    let range = record.range();
    if range.start_byte >= range.end_byte
        || range.start_line == 0
        || range.start_line > range.end_line
    {
        return Err(MapError::InvalidIndex(
            "syntax record has an invalid source range",
        ));
    }
    Ok(())
}

fn selected(
    candidate: &Candidate<'_>,
    request: &RepositoryMapRequest,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<bool, MapError> {
    let mut path_selected = request.path_prefixes.is_empty();
    for prefix in &request.path_prefixes {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        if candidate.entry.path.starts_with(prefix) {
            path_selected = true;
            break;
        }
    }
    if !path_selected {
        return Ok(false);
    }
    let mut language_selected = request.languages.is_empty();
    for language in &request.languages {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        if language == candidate.record.language().as_str() {
            language_selected = true;
            break;
        }
    }
    Ok(language_selected)
}

fn build_target_index<'a>(
    all: &BTreeMap<DeclarationId, Candidate<'a>>,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<TargetIndex<'a>, MapError> {
    let mut index = TargetIndex::new();
    for (id, candidate) in all {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        let range = candidate.record.range();
        index
            .entry(candidate.path)
            .or_default()
            .push((range.start_byte, range.end_byte, *id));
    }
    for ranges in index.values_mut() {
        charge_sort_work(work, ranges.len(), max_work, deadline)?;
        ranges.sort_unstable_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        check_deadline(deadline)?;
    }
    Ok(index)
}

#[allow(dead_code)]
pub(crate) fn validate_semantic_evidence(
    index: &MetadataIndex,
    evidence: &[SemanticRelationship<'_>],
    limits: MapLimits,
) -> Result<(), MapError> {
    let started = Instant::now();
    let deadline = started.checked_add(limits.max_time).unwrap_or(started);
    let mut work = 0_usize;
    if evidence.len() > limits.max_semantic_relationships {
        return Err(MapError::InvalidRequest("too many semantic relationships"));
    }
    let mut all = BTreeMap::<DeclarationId, Candidate<'_>>::new();
    for entry in index.entries() {
        let path = entry
            .path
            .to_str()
            .ok_or(MapError::InvalidIndex("syntax path is not UTF-8"))?;
        for record in entry.syntax_records.iter() {
            check_deadline(deadline)?;
            charge_work(&mut work, 1, limits.max_work)?;
            validate_record(index, entry, record)?;
            let id = DeclarationId::from(record.declaration_id());
            if all.len() == limits.max_candidates {
                return Err(MapError::InvalidRequest("candidate limit exceeded"));
            }
            if all
                .insert(
                    id,
                    Candidate {
                        entry,
                        record,
                        path,
                        rank: 0,
                        degree: 0,
                        reasons: Vec::new(),
                    },
                )
                .is_some()
            {
                return Err(MapError::InvalidIndex("duplicate declaration id"));
            }
        }
    }
    let target_index = build_target_index(&all, &mut work, limits.max_work, deadline)?;
    for relationship in evidence {
        validate_semantic_relationship(
            &all,
            &target_index,
            relationship,
            index.revision(),
            &mut work,
            limits.max_work,
            deadline,
        )?;
    }
    Ok(())
}

fn syntax_edges<'a>(
    all: &BTreeMap<DeclarationId, Candidate<'a>>,
    selected: &BTreeSet<DeclarationId>,
    request: &RepositoryMapRequest,
    work: &mut usize,
    limits: MapLimits,
    deadline: Instant,
) -> Result<Vec<GraphEdge<'a>>, MapError> {
    let relationships = request
        .expansion
        .relationships
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut edges = Vec::new();
    for child in selected {
        check_deadline(deadline)?;
        let candidate = &all[child];
        let Some(enclosing) = candidate.record.enclosing_symbol() else {
            continue;
        };
        charge_work(work, 1, limits.max_work)?;
        let parent = DeclarationId::from(*enclosing.value());
        if !selected.contains(&parent) {
            continue;
        }
        if relationships.contains(&RelationshipKind::Contains)
            && request.expansion.purpose != ExpansionPurpose::Dependents
        {
            edges.push(GraphEdge {
                source: parent,
                target: *child,
                relationship: RelationshipKind::Contains,
                provenance: GraphProvenance::Syntax(enclosing.provenance()),
            });
        }
        if relationships.contains(&RelationshipKind::ContainedBy)
            && request.expansion.purpose != ExpansionPurpose::Dependencies
        {
            edges.push(GraphEdge {
                source: *child,
                target: parent,
                relationship: RelationshipKind::ContainedBy,
                provenance: GraphProvenance::Syntax(enclosing.provenance()),
            });
        }
    }
    Ok(edges)
}

#[allow(clippy::too_many_arguments)]
fn add_semantic_edges<'a>(
    graph: &mut Vec<GraphEdge<'a>>,
    all: &BTreeMap<DeclarationId, Candidate<'a>>,
    target_index: &TargetIndex<'_>,
    selected: &BTreeSet<DeclarationId>,
    evidence: &'a [SemanticRelationship<'a>],
    revision: RevisionId,
    request: &RepositoryMapRequest,
    work: &mut usize,
    limits: MapLimits,
    deadline: Instant,
) -> Result<(), MapError> {
    let allowed = request
        .expansion
        .relationships
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    for relationship in evidence {
        let target = validate_semantic_relationship(
            all,
            target_index,
            relationship,
            revision,
            work,
            limits.max_work,
            deadline,
        )?;
        let fact = relationship.fact;
        let kind = RelationshipKind::semantic(fact.relation());
        if !allowed.contains(&kind)
            || !selected.contains(&relationship.source_declaration)
            || !selected.contains(&target)
        {
            continue;
        }
        graph.push(
            if request.expansion.purpose == ExpansionPurpose::Dependents {
                GraphEdge {
                    source: target,
                    target: relationship.source_declaration,
                    relationship: kind,
                    provenance: GraphProvenance::Semantic(fact),
                }
            } else {
                GraphEdge {
                    source: relationship.source_declaration,
                    target,
                    relationship: kind,
                    provenance: GraphProvenance::Semantic(fact),
                }
            },
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_semantic_relationship(
    all: &BTreeMap<DeclarationId, Candidate<'_>>,
    target_index: &TargetIndex<'_>,
    relationship: &SemanticRelationship<'_>,
    revision: RevisionId,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<DeclarationId, MapError> {
    check_deadline(deadline)?;
    charge_work(work, 1, max_work)?;
    let fact = relationship.fact;
    if fact.provenance().revision() != revision {
        return Err(MapError::StaleFact);
    }
    if fact.provenance().classification() != RepositoryFactClassification::Semantic
        || fact.provenance().source() != RepositoryFactProvenance::Lsp
        || fact.provenance().confidence() != NormalizedConfidence::ExactSource
    {
        return Err(MapError::InvalidFact(
            "semantic fact provenance is not exact LSP evidence",
        ));
    }
    let source = all
        .get(&relationship.source_declaration)
        .ok_or(MapError::InvalidFact("source declaration is unknown"))?;
    validate_semantic_source(
        all,
        relationship.source_declaration,
        source,
        fact,
        revision,
        work,
        max_work,
        deadline,
    )?;
    let path = fact.path().as_path().as_str();
    let start = fact.range().start();
    let end = fact.range().end();
    if start >= end {
        return Err(MapError::InvalidFact("semantic fact range is empty"));
    }
    let ranges = target_index.get(path).ok_or(MapError::InvalidFact(
        "semantic target does not identify an indexed declaration",
    ))?;
    let boundary = ranges.partition_point(|range| range.0 <= start);
    let mut target = None;
    for &(candidate_start, candidate_end, id) in ranges[..boundary].iter().rev() {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        if target.is_some_and(|(best_width, _)| end - candidate_start > best_width) {
            break;
        }
        if end <= candidate_end {
            let width = candidate_end - candidate_start;
            if target.is_none_or(|(best_width, best_id)| (width, id) < (best_width, best_id)) {
                target = Some((width, id));
            }
        }
    }
    target.map(|(_, id)| id).ok_or(MapError::InvalidFact(
        "semantic target does not identify an indexed declaration",
    ))
}

#[allow(clippy::too_many_arguments)]
fn validate_semantic_source(
    all: &BTreeMap<DeclarationId, Candidate<'_>>,
    source_id: DeclarationId,
    source: &Candidate<'_>,
    fact: &SemanticFact,
    revision: RevisionId,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<(), MapError> {
    if source.record.workspace_revision() != revision {
        return Err(MapError::StaleFact);
    }
    let origin = fact.provenance().origin();
    if origin.path().as_path().as_str() != source.path {
        return Err(MapError::InvalidFact(
            "semantic source declaration does not match origin URI",
        ));
    }
    let range = source.record.range();
    if fact.origin_point() < range.start_byte || fact.origin_point() >= range.end_byte {
        return Err(MapError::InvalidFact(
            "semantic source declaration does not contain request origin",
        ));
    }
    let mut smallest = None;
    let mut ambiguous = false;
    for (id, candidate) in all {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        let range = candidate.record.range();
        if candidate.path != source.path
            || fact.origin_point() < range.start_byte
            || fact.origin_point() >= range.end_byte
        {
            continue;
        }
        let width = range.end_byte - range.start_byte;
        match smallest {
            None => smallest = Some((width, *id)),
            Some((best_width, _)) if width < best_width => {
                smallest = Some((width, *id));
                ambiguous = false;
            }
            Some((best_width, _)) if width == best_width => ambiguous = true,
            Some(_) => {}
        }
    }
    if ambiguous {
        return Err(MapError::InvalidFact(
            "semantic request origin matches ambiguous declarations",
        ));
    }
    if smallest.is_none_or(|(_, id)| id != source_id) {
        return Err(MapError::InvalidFact(
            "semantic source declaration is not the smallest declaration containing request origin",
        ));
    }
    Ok(())
}

fn rank_candidates(
    all: &mut BTreeMap<DeclarationId, Candidate<'_>>,
    request: &RepositoryMapRequest,
    semantic_ids: &BTreeSet<DeclarationId>,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<(), MapError> {
    let exact_ids = request
        .personalization
        .exact_declaration_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let edits = request
        .personalization
        .current_edit_paths
        .iter()
        .collect::<BTreeSet<_>>();
    let mut frames = request
        .personalization
        .stack_frames
        .iter()
        .collect::<Vec<_>>();
    charge_sort_work(work, frames.len(), max_work, deadline)?;
    frames.sort_unstable();
    check_deadline(deadline)?;
    frames.dedup();
    let terms = request
        .personalization
        .task_terms
        .iter()
        .map(|term| term.to_lowercase())
        .collect::<BTreeSet<_>>();
    let recent = request
        .personalization
        .recently_read_paths
        .iter()
        .enumerate()
        .fold(
            BTreeMap::<&PathBuf, usize>::new(),
            |mut positions, (index, path)| {
                positions.entry(path).or_insert(index);
                positions
            },
        );

    for (id, candidate) in all {
        check_deadline(deadline)?;
        let mut score = 0_u64;
        let mut reasons = BTreeSet::new();
        if exact_ids.contains(id) {
            score += 1_000_000_000_000_000;
            reasons.insert(RankingReason::ExactDeclarationId);
        }
        for frame in &frames {
            charge_work(work, 1, max_work)?;
            if frame.path == candidate.entry.path {
                score += 100_000_000_000_000;
                reasons.insert(RankingReason::StackPath);
                if frame.symbol.as_deref().is_some_and(|symbol| {
                    symbol == candidate.record.display_name().value().as_str()
                        || symbol == candidate.record.qualified_name().value().as_str()
                }) {
                    score += 50_000_000_000_000;
                    reasons.insert(RankingReason::StackSymbol);
                }
                if frame.line.is_some_and(|line| {
                    let range = candidate.record.range();
                    range.start_line <= line && line <= range.end_line
                }) {
                    score += 25_000_000_000_000;
                    reasons.insert(RankingReason::StackLine);
                }
            }
        }
        if edits.contains(&candidate.entry.path) {
            score += 1_000_000_000_000;
            reasons.insert(RankingReason::CurrentEdit);
        }
        if let Some(position) = recent.get(&candidate.entry.path) {
            score += 100_000_000_000_u64.saturating_sub(*position as u64);
            reasons.insert(RankingReason::RecentlyRead);
        }
        if semantic_ids.contains(id) {
            score += 10_000_000_000;
            reasons.insert(RankingReason::ExplicitSemanticEvidence);
        }
        let qualified = candidate.record.qualified_name().value().to_lowercase();
        let display = candidate.record.display_name().value().to_lowercase();
        let path = candidate.path.to_lowercase();
        for term in &terms {
            charge_work(work, 1, max_work)?;
            if term == &qualified || term == &display {
                score += 1_000_000_000;
                reasons.insert(RankingReason::ExactTaskTerm);
            } else if qualified.contains(term) || display.contains(term) || path.contains(term) {
                score += 100_000_000;
                reasons.insert(RankingReason::SubstringTaskTerm);
            }
        }
        candidate.rank = score.saturating_sub(candidate.degree as u64);
        candidate.reasons = reasons.into_iter().collect();
    }
    Ok(())
}

fn expand_paths(
    index: &MetadataIndex,
    request: &RepositoryMapRequest,
    work: &mut usize,
    limits: MapLimits,
    deadline: Instant,
) -> Result<PathExpansion, MapError> {
    if request.expansion.paths.is_empty() {
        return Ok(PathExpansion {
            nodes: BTreeSet::new(),
            edges: BTreeSet::new(),
            hops: BTreeMap::new(),
            degree: BTreeMap::new(),
            graph: Vec::new(),
        });
    }

    let mut by_path = BTreeMap::new();
    for (entry_index, entry) in index.entries().iter().enumerate() {
        check_deadline(deadline)?;
        charge_work(work, 1, limits.max_work)?;
        let path = entry
            .path
            .to_str()
            .ok_or(MapError::InvalidIndex("metadata path is not UTF-8"))?;
        if by_path.insert(path, entry_index).is_some() {
            return Err(MapError::InvalidIndex("duplicate metadata path"));
        }
    }

    let mut seeds = BTreeSet::new();
    for path in &request.expansion.paths {
        let entry_index = by_path
            .get(path.as_str())
            .copied()
            .ok_or(MapError::SelectorNoMatch("path"))?;
        seeds.insert(entry_index);
    }

    let relationships = request
        .expansion
        .relationships
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut graph = Vec::new();
    for (child_index, child) in index.entries().iter().enumerate() {
        check_deadline(deadline)?;
        charge_work(work, 1, limits.max_work)?;
        let Some(parent) = child
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        else {
            continue;
        };
        let Some(parent_index) = parent
            .to_str()
            .and_then(|parent| by_path.get(parent))
            .copied()
        else {
            continue;
        };
        if relationships.contains(&RelationshipKind::Contains)
            && request.expansion.purpose != ExpansionPurpose::Dependents
        {
            graph.push(PathGraphEdge {
                source: parent_index,
                target: child_index,
                relationship: RelationshipKind::Contains,
            });
        }
        if relationships.contains(&RelationshipKind::ContainedBy)
            && request.expansion.purpose != ExpansionPurpose::Dependencies
        {
            graph.push(PathGraphEdge {
                source: child_index,
                target: parent_index,
                relationship: RelationshipKind::ContainedBy,
            });
        }
    }
    charge_sort_work(work, graph.len(), limits.max_work, deadline)?;
    graph.sort_by(|left, right| {
        let entries = index.entries();
        entries[left.source]
            .path
            .cmp(&entries[right.source].path)
            .then_with(|| entries[left.target].path.cmp(&entries[right.target].path))
            .then_with(|| left.relationship.cmp(&right.relationship))
    });
    graph.dedup_by(|left, right| {
        left.source == right.source
            && left.target == right.target
            && left.relationship == right.relationship
    });

    let mut adjacency = BTreeMap::<usize, Vec<(usize, usize)>>::new();
    for (edge_index, edge) in graph.iter().enumerate() {
        charge_work(work, 1, limits.max_work)?;
        adjacency
            .entry(edge.source)
            .or_default()
            .push((edge_index, edge.target));
    }
    let mut nodes = seeds.clone();
    let mut edges = BTreeSet::new();
    let mut hops = seeds
        .iter()
        .map(|seed| (*seed, 0_usize))
        .collect::<BTreeMap<_, _>>();
    let mut queue = seeds
        .into_iter()
        .map(|seed| (seed, 0_usize))
        .collect::<VecDeque<_>>();
    while let Some((source, at_hop)) = queue.pop_front() {
        check_deadline(deadline)?;
        let outgoing = adjacency.get(&source).map_or(&[][..], Vec::as_slice);
        if outgoing.len() > request.budget.max_degree {
            return Err(MapError::BoundExceeded(MapBound::Degree));
        }
        for (edge_index, target) in outgoing {
            charge_work(work, 1, limits.max_work)?;
            let next_hop = at_hop
                .checked_add(1)
                .ok_or(MapError::BoundExceeded(MapBound::Hops))?;
            edges.insert(*edge_index);
            if !nodes.contains(target) && next_hop > request.budget.max_hops {
                return Err(MapError::BoundExceeded(MapBound::Hops));
            }
            if nodes.insert(*target) {
                hops.insert(*target, next_hop);
                queue.push_back((*target, next_hop));
            }
            if nodes.len().saturating_add(edges.len()) > request.budget.max_items {
                return Err(MapError::BoundExceeded(MapBound::Items));
            }
        }
    }
    let mut degree = BTreeMap::new();
    for node in &nodes {
        charge_work(work, 1, limits.max_work)?;
        degree.insert(*node, adjacency.get(node).map_or(0, Vec::len));
    }
    Ok(PathExpansion {
        nodes,
        edges,
        hops,
        degree,
        graph,
    })
}

fn expand(
    all: &BTreeMap<DeclarationId, Candidate<'_>>,
    selected: &BTreeSet<DeclarationId>,
    adjacency: &Adjacency,
    request: &RepositoryMapRequest,
    work: &mut usize,
    limits: MapLimits,
    deadline: Instant,
) -> Result<ExpansionOutput, MapError> {
    let mut seeds = request
        .expansion
        .seeds
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    for seed in &seeds {
        if !all.contains_key(seed) || !selected.contains(seed) {
            return Err(MapError::InvalidRequest(
                "expansion seed is unknown or filtered",
            ));
        }
    }
    for path in &request.expansion.paths {
        for (id, candidate) in all {
            check_deadline(deadline)?;
            charge_work(work, 1, limits.max_work)?;
            if selected.contains(id) && candidate.path == path.as_str() {
                seeds.insert(*id);
            }
        }
    }
    for symbol in &request.expansion.symbols {
        let mut matched = false;
        for (id, candidate) in all {
            check_deadline(deadline)?;
            charge_work(work, 1, limits.max_work)?;
            if selected.contains(id)
                && (candidate.record.qualified_name().value().as_str() == symbol
                    || candidate.record.display_name().value().as_str() == symbol)
            {
                seeds.insert(*id);
                matched = true;
            }
        }
        if !matched {
            return Err(MapError::SelectorNoMatch("symbol"));
        }
    }
    if let Some(band) = request.expansion.score_band {
        let mut matched = false;
        for (id, candidate) in all {
            check_deadline(deadline)?;
            charge_work(work, 1, limits.max_work)?;
            if selected.contains(id) && (band.min..=band.max).contains(&candidate.rank) {
                seeds.insert(*id);
                matched = true;
            }
        }
        if !matched {
            return Err(MapError::SelectorNoMatch("score band"));
        }
    }
    let mut nodes = seeds.clone();
    let mut edges = BTreeSet::new();
    let mut hops = seeds
        .iter()
        .map(|seed| (*seed, 0_usize))
        .collect::<BTreeMap<_, _>>();
    let mut queue = seeds
        .into_iter()
        .map(|seed| (seed, 0_usize))
        .collect::<VecDeque<_>>();
    while let Some((source, at_hop)) = queue.pop_front() {
        check_deadline(deadline)?;
        let outgoing = adjacency.get(&source).map_or(&[][..], Vec::as_slice);
        if outgoing.len() > request.budget.max_degree {
            return Err(MapError::BoundExceeded(MapBound::Degree));
        }
        for (edge_index, target) in outgoing {
            charge_work(work, 1, limits.max_work)?;
            let next_hop = at_hop
                .checked_add(1)
                .ok_or(MapError::BoundExceeded(MapBound::Hops))?;
            edges.insert(*edge_index);
            if !nodes.contains(target) && next_hop > request.budget.max_hops {
                return Err(MapError::BoundExceeded(MapBound::Hops));
            }
            if nodes.insert(*target) {
                hops.insert(*target, next_hop);
                queue.push_back((*target, next_hop));
            }
            if nodes.len().saturating_add(edges.len()) > request.budget.max_items {
                return Err(MapError::BoundExceeded(MapBound::Items));
            }
        }
    }
    Ok((nodes, edges, hops))
}

fn render_path_node(
    entry: &MetadataEntry,
    hops: usize,
    degree: usize,
    revision: RevisionId,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<RepositoryPathNode, MapError> {
    check_deadline(deadline)?;
    let path = entry
        .path
        .to_str()
        .ok_or(MapError::InvalidIndex("metadata path is not UTF-8"))?;
    charge_work(
        work,
        path.len()
            .saturating_add(entry.language.as_ref().map_or(0, String::len))
            .max(1),
        max_work,
    )?;
    Ok(RepositoryPathNode {
        path: path.to_owned(),
        kind: match entry.kind {
            EntryKind::File => PathKind::File,
            EntryKind::Directory => PathKind::Directory,
        },
        language: entry.language.clone(),
        content_state: match entry.content_state {
            ContentState::Directory => PathContentState::Directory,
            ContentState::Text => PathContentState::Text,
            ContentState::Binary => PathContentState::Binary,
            ContentState::InvalidUtf8 => PathContentState::InvalidUtf8,
            ContentState::TooLarge => PathContentState::TooLarge,
            ContentState::IndexLimit => PathContentState::IndexLimit,
        },
        size: entry.size,
        revision,
        provenance: path_provenance(revision),
        degree,
        hops,
    })
}

fn render_path_edge(
    edge: PathGraphEdge,
    index: &MetadataIndex,
    hops: usize,
) -> Result<RepositoryPathEdge, MapError> {
    let source_path = index.entries()[edge.source]
        .path
        .to_str()
        .ok_or(MapError::InvalidIndex("metadata path is not UTF-8"))?;
    let target_path = index.entries()[edge.target]
        .path
        .to_str()
        .ok_or(MapError::InvalidIndex("metadata path is not UTF-8"))?;
    Ok(RepositoryPathEdge {
        source_path: source_path.to_owned(),
        target_path: target_path.to_owned(),
        relationship: edge.relationship,
        hops,
        provenance: path_provenance(index.revision()),
    })
}

fn path_provenance(revision: RevisionId) -> PathProvenance {
    PathProvenance {
        classification: FactClassification::Syntactic,
        source: PathFactSource::RepositoryTree,
        revision,
    }
}

fn render_entry(
    candidate: &Candidate<'_>,
    hops: Option<usize>,
    expansion: bool,
    max_highlight_bytes: usize,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<RepositoryMapEntry, MapError> {
    check_deadline(deadline)?;
    charge_work(
        work,
        retained_entry_bytes(candidate, max_highlight_bytes),
        max_work,
    )?;
    let provenance = candidate.record.qualified_name().provenance();
    let mut reasons = candidate.reasons.clone();
    if let Some(hops) = hops {
        reasons.push(if hops == 0 {
            RankingReason::ExpansionSeed
        } else {
            RankingReason::ExpansionNeighbor
        });
    }
    charge_sort_work(work, reasons.len(), max_work, deadline)?;
    reasons.sort_unstable();
    check_deadline(deadline)?;
    reasons.dedup();
    Ok(RepositoryMapEntry {
        declaration_id: DeclarationId::from(candidate.record.declaration_id()),
        path: candidate.path.to_owned(),
        language: candidate.record.language().as_str().to_owned(),
        qualified_name: candidate.record.qualified_name().value().as_ref().clone(),
        display_name: candidate.record.display_name().value().as_ref().clone(),
        kind: map_kind(*candidate.record.kind().value()),
        signature: candidate.record.signature().value().text().to_owned(),
        source_range: candidate.record.range().into(),
        source_lines: source_lines(candidate, max_highlight_bytes, work, max_work, deadline)?,
        provenance: syntax_provenance(provenance),
        relationships: relationship_availability(candidate.record),
        rank: candidate.rank,
        degree: candidate.degree,
        hops: expansion.then_some(hops).flatten(),
        reasons,
    })
}

fn source_lines(
    candidate: &Candidate<'_>,
    max_bytes: usize,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<Vec<SourceLine>, MapError> {
    check_deadline(deadline)?;
    let declaration = candidate.record.declaration().value().text();
    let signature = candidate.record.signature().value().text();
    let (mut source, mut truncated) = bounded_first_line(declaration, max_bytes);
    if source.is_empty() {
        (source, truncated) = bounded_first_line(signature, max_bytes);
    }
    charge_work(work, source.len().max(1), max_work)?;
    check_deadline(deadline)?;
    Ok(vec![SourceLine {
        line: candidate.record.range().start_line,
        text: source.to_owned(),
        truncated,
    }])
}

fn relationship_availability(record: &SyntacticSymbolRecord) -> RelationshipAvailability {
    RelationshipAvailability {
        containment: RelationshipState::Available,
        imports: availability(record.imports()),
        exports: availability(record.exports()),
        definitions: availability(record.definitions()),
        references: availability(record.references()),
        callers: availability(record.callers()),
        callees: availability(record.callees()),
        tests: availability(record.tests()),
        documentation: availability(record.documentation()),
    }
}

fn availability<T>(facts: &SyntacticFacts<T>) -> RelationshipState {
    match facts {
        SyntacticFacts::Available(_) => RelationshipState::Available,
        SyntacticFacts::Unavailable(UnavailableReason::NotExtracted) => {
            RelationshipState::UnavailableNotExtracted
        }
    }
}

fn render_edge(
    edge: &GraphEdge<'_>,
    all: &BTreeMap<DeclarationId, Candidate<'_>>,
    hops: usize,
) -> RepositoryMapEdge {
    let source_range = all[&edge.source].record.range().into();
    let target_range = all[&edge.target].record.range().into();
    let (fact_range, semantic_source_range, semantic_target_range) = match edge.provenance {
        GraphProvenance::Syntax(provenance) => (
            byte_range(provenance.range().start_byte, provenance.range().end_byte),
            None,
            None,
        ),
        GraphProvenance::Semantic(fact) => (
            byte_range(fact.range().start(), fact.range().end()),
            Some(byte_range(
                fact.origin_range().start(),
                fact.origin_range().end(),
            )),
            Some(match fact.target_range() {
                Some(range) => byte_range(range.start(), range.end()),
                None => byte_range(fact.range().start(), fact.range().end()),
            }),
        ),
    };
    let provenance = match edge.provenance {
        GraphProvenance::Syntax(provenance) => EdgeProvenance::TreeSitter {
            fact: syntax_provenance(provenance),
        },
        GraphProvenance::Semantic(fact) => {
            let provenance = fact.provenance();
            let origin = provenance.origin();
            EdgeProvenance::Lsp {
                fact: LspEdgeProvenance {
                    classification: FactClassification::Semantic,
                    source: MapFactSource::Lsp,
                    revision: provenance.revision(),
                    confidence_millis: 1_000,
                    origin_uri: origin.uri().to_owned(),
                    document_version: origin.document_version().get(),
                    request_generation: origin.request_generation(),
                    request_id: origin.request_id().get(),
                    server_artifact: provenance.server().server_artifact.to_string(),
                    server_configuration: provenance.server().configuration.to_string(),
                    position_encoding: position_encoding(provenance.position_encoding()).to_owned(),
                },
            }
        }
    };
    RepositoryMapEdge {
        source_declaration: edge.source,
        target_declaration: edge.target,
        relationship: edge.relationship,
        hops,
        source_range,
        target_range,
        fact_range,
        semantic_source_range,
        semantic_target_range,
        provenance,
    }
}

fn byte_range(start_byte: usize, end_byte: usize) -> MapByteRange {
    MapByteRange {
        start_byte,
        end_byte,
    }
}

fn syntax_provenance(provenance: &SyntacticProvenance) -> MapFactProvenance {
    debug_assert_eq!(provenance.source(), FactSource::Syntactic);
    MapFactProvenance {
        classification: FactClassification::Syntactic,
        source: MapFactSource::TreeSitter,
        revision: provenance.revision(),
        confidence_millis: provenance.confidence_millis(),
        grammar_identity: provenance.grammar_identity(),
        query_set_digest: provenance.query_set_digest(),
    }
}

fn map_kind(kind: SyntacticSymbolKind) -> MapSymbolKind {
    match kind {
        SyntacticSymbolKind::Function => MapSymbolKind::Function,
        SyntacticSymbolKind::Struct => MapSymbolKind::Struct,
        SyntacticSymbolKind::Enum => MapSymbolKind::Enum,
        SyntacticSymbolKind::Union => MapSymbolKind::Union,
        SyntacticSymbolKind::TypeAlias => MapSymbolKind::TypeAlias,
        SyntacticSymbolKind::Trait => MapSymbolKind::Trait,
        SyntacticSymbolKind::Module => MapSymbolKind::Module,
        SyntacticSymbolKind::Constant => MapSymbolKind::Constant,
        SyntacticSymbolKind::Static => MapSymbolKind::Static,
        SyntacticSymbolKind::Macro => MapSymbolKind::Macro,
    }
}

fn compare_candidates(left: &Candidate<'_>, right: &Candidate<'_>) -> Ordering {
    right
        .rank
        .cmp(&left.rank)
        .then_with(|| left.path.cmp(right.path))
        .then_with(|| {
            left.record
                .range()
                .start_byte
                .cmp(&right.record.range().start_byte)
        })
        .then_with(|| {
            left.record
                .declaration_id()
                .cmp(&right.record.declaration_id())
        })
}

fn compare_graph_edges(left: &GraphEdge<'_>, right: &GraphEdge<'_>) -> Ordering {
    left.source
        .cmp(&right.source)
        .then_with(|| left.target.cmp(&right.target))
        .then_with(|| left.relationship.cmp(&right.relationship))
        .then_with(|| compare_graph_provenance(&left.provenance, &right.provenance))
}

fn compare_graph_provenance(left: &GraphProvenance<'_>, right: &GraphProvenance<'_>) -> Ordering {
    match (left, right) {
        (GraphProvenance::Syntax(left), GraphProvenance::Syntax(right)) => left
            .range()
            .start_byte
            .cmp(&right.range().start_byte)
            .then_with(|| left.range().end_byte.cmp(&right.range().end_byte))
            .then_with(|| left.revision().as_bytes().cmp(right.revision().as_bytes()))
            .then_with(|| left.confidence_millis().cmp(&right.confidence_millis()))
            .then_with(|| left.grammar_identity().cmp(&right.grammar_identity()))
            .then_with(|| left.query_set_digest().cmp(&right.query_set_digest())),
        (GraphProvenance::Syntax(_), GraphProvenance::Semantic(_)) => Ordering::Less,
        (GraphProvenance::Semantic(_), GraphProvenance::Syntax(_)) => Ordering::Greater,
        (GraphProvenance::Semantic(left), GraphProvenance::Semantic(right)) => {
            let left_provenance = left.provenance();
            let right_provenance = right.provenance();
            let left_origin = left_provenance.origin();
            let right_origin = right_provenance.origin();
            left.path()
                .as_path()
                .as_str()
                .cmp(right.path().as_path().as_str())
                .then_with(|| left.range().start().cmp(&right.range().start()))
                .then_with(|| left.range().end().cmp(&right.range().end()))
                .then_with(|| {
                    semantic_range_key(left.target_range())
                        .cmp(&semantic_range_key(right.target_range()))
                })
                .then_with(|| {
                    (
                        left.origin_point(),
                        semantic_range_key(Some(left.origin_range())),
                    )
                        .cmp(&(
                            right.origin_point(),
                            semantic_range_key(Some(right.origin_range())),
                        ))
                })
                .then_with(|| {
                    left_provenance
                        .revision()
                        .as_bytes()
                        .cmp(right_provenance.revision().as_bytes())
                })
                .then_with(|| left_origin.uri().cmp(right_origin.uri()))
                .then_with(|| {
                    left_origin
                        .document_version()
                        .cmp(&right_origin.document_version())
                })
                .then_with(|| {
                    left_origin
                        .request_generation()
                        .cmp(&right_origin.request_generation())
                })
                .then_with(|| {
                    left_origin
                        .request_id()
                        .get()
                        .cmp(&right_origin.request_id().get())
                })
                .then_with(|| {
                    left_provenance
                        .server()
                        .server_artifact
                        .as_str()
                        .cmp(right_provenance.server().server_artifact.as_str())
                })
                .then_with(|| {
                    left_provenance
                        .server()
                        .configuration
                        .as_str()
                        .cmp(right_provenance.server().configuration.as_str())
                })
                .then_with(|| {
                    (left_provenance.position_encoding() as u8)
                        .cmp(&(right_provenance.position_encoding() as u8))
                })
        }
    }
}

fn semantic_range_key(
    range: Option<&crate::verify::lsp::facts::SemanticByteRange>,
) -> Option<(usize, usize)> {
    range.map(|range| (range.start(), range.end()))
}

fn compare_output_edges(left: &RepositoryMapEdge, right: &RepositoryMapEdge) -> Ordering {
    left.hops
        .cmp(&right.hops)
        .then_with(|| left.source_declaration.cmp(&right.source_declaration))
        .then_with(|| left.target_declaration.cmp(&right.target_declaration))
        .then_with(|| left.relationship.cmp(&right.relationship))
}

fn settle_size(
    response: &mut RepositoryMap,
    byte_limit: usize,
    deadline: Instant,
) -> Result<bool, MapError> {
    for _ in 0..8 {
        check_deadline(deadline)?;
        let Some(bytes) = serialized_size(response, byte_limit, deadline)? else {
            return Ok(false);
        };
        let tokens = estimate_tokens_from_bytes(bytes);
        if response.result_bytes == bytes && response.estimated_tokens == tokens {
            return Ok(true);
        }
        response.result_bytes = bytes;
        response.estimated_tokens = tokens;
    }
    Err(MapError::InvalidLimits(
        "serialized accounting did not converge",
    ))
}

fn estimate_tokens_from_bytes(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

fn serialized_size(
    value: &impl Serialize,
    limit: usize,
    deadline: Instant,
) -> Result<Option<usize>, MapError> {
    let mut writer = CappedCountingWriter {
        bytes: 0,
        limit,
        deadline,
        exceeded: false,
        timed_out: false,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        return if writer.timed_out {
            Err(MapError::TimeLimit)
        } else if writer.exceeded {
            Ok(None)
        } else {
            Err(MapError::Serialization(error))
        };
    }
    check_deadline(deadline)?;
    Ok(Some(writer.bytes))
}

struct CappedCountingWriter {
    bytes: usize,
    limit: usize,
    deadline: Instant,
    exceeded: bool,
    timed_out: bool,
}

impl Write for CappedCountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            self.timed_out = true;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "repository map time limit exceeded",
            ));
        }
        let Some(total) = self.bytes.checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other(
                "repository map result byte limit exceeded",
            ));
        };
        if total > self.limit {
            self.exceeded = true;
            return Err(io::Error::other(
                "repository map result byte limit exceeded",
            ));
        }
        self.bytes = total;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn exceeds_serialized_budget(response: &RepositoryMap, budget: MapBudget) -> bool {
    response.result_bytes > budget.max_result_bytes
        || response.estimated_tokens > budget.max_estimated_tokens
}

fn enforce_serialized_budget(response: &RepositoryMap, budget: MapBudget) -> Result<(), MapError> {
    if response.estimated_tokens > budget.max_estimated_tokens {
        Err(MapError::BoundExceeded(MapBound::EstimatedTokens))
    } else if response.result_bytes > budget.max_result_bytes {
        Err(MapError::BoundExceeded(MapBound::ResultBytes))
    } else {
        Ok(())
    }
}

fn retained_entry_bytes(candidate: &Candidate<'_>, max_highlight_bytes: usize) -> usize {
    let declaration = candidate.record.declaration().value().text();
    let (source_line, _) = bounded_first_line(declaration, max_highlight_bytes);
    let source_line = if source_line.is_empty() {
        bounded_first_line(
            candidate.record.signature().value().text(),
            max_highlight_bytes,
        )
        .0
    } else {
        source_line
    };
    [
        candidate.path.len(),
        candidate.record.language().as_str().len(),
        candidate.record.qualified_name().value().len(),
        candidate.record.display_name().value().len(),
        candidate.record.signature().value().text().len(),
        source_line.len(),
    ]
    .into_iter()
    .fold(0_usize, usize::saturating_add)
}

fn escaped_string_upper_bound(value: &str) -> usize {
    value.chars().fold(2_usize, |bytes, character| {
        bytes.saturating_add(match character {
            '"' | '\\' => 2,
            character if character <= '\u{1f}' => 6,
            character => character.len_utf8(),
        })
    })
}

fn escaped_entry_bytes(candidate: &Candidate<'_>, max_highlight_bytes: usize) -> usize {
    let declaration = candidate.record.declaration().value().text();
    let (source_line, _) = bounded_first_line(declaration, max_highlight_bytes);
    let source_line = if source_line.is_empty() {
        bounded_first_line(
            candidate.record.signature().value().text(),
            max_highlight_bytes,
        )
        .0
    } else {
        source_line
    };
    [
        candidate.path,
        candidate.record.language().as_str(),
        candidate.record.qualified_name().value().as_str(),
        candidate.record.display_name().value().as_str(),
        candidate.record.signature().value().text(),
        source_line,
    ]
    .into_iter()
    .fold(0_usize, |bytes, value| {
        bytes.saturating_add(escaped_string_upper_bound(value))
    })
}

#[allow(clippy::too_many_arguments)]
fn precharge_mandatory(
    all: &BTreeMap<DeclarationId, Candidate<'_>>,
    graph: &[GraphEdge<'_>],
    ids: &BTreeSet<DeclarationId>,
    edges: &BTreeSet<usize>,
    index: &MetadataIndex,
    paths: &PathExpansion,
    budget: MapBudget,
    max_highlight_bytes: usize,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<(), MapError> {
    let mut retained = 0_usize;
    for entry_index in &paths.nodes {
        check_deadline(deadline)?;
        let entry = &index.entries()[*entry_index];
        let path = entry
            .path
            .to_str()
            .ok_or(MapError::InvalidIndex("metadata path is not UTF-8"))?;
        retained = retained
            .saturating_add(escaped_string_upper_bound(path))
            .saturating_add(
                entry
                    .language
                    .as_deref()
                    .map_or(4, escaped_string_upper_bound),
            )
            .saturating_add(std::mem::size_of::<RepositoryPathNode>());
    }
    for edge_index in &paths.edges {
        check_deadline(deadline)?;
        let edge = paths.graph[*edge_index];
        for entry_index in [edge.source, edge.target] {
            let path = index.entries()[entry_index]
                .path
                .to_str()
                .ok_or(MapError::InvalidIndex("metadata path is not UTF-8"))?;
            retained = retained.saturating_add(escaped_string_upper_bound(path));
        }
        retained = retained.saturating_add(std::mem::size_of::<RepositoryPathEdge>());
    }
    for id in ids {
        check_deadline(deadline)?;
        retained = retained.saturating_add(escaped_entry_bytes(&all[id], max_highlight_bytes));
    }
    for edge in edges {
        check_deadline(deadline)?;
        retained = retained.saturating_add(match graph[*edge].provenance {
            GraphProvenance::Syntax(_) => std::mem::size_of::<RepositoryMapEdge>(),
            GraphProvenance::Semantic(fact) => {
                let provenance = fact.provenance();
                std::mem::size_of::<RepositoryMapEdge>()
                    .saturating_add(escaped_string_upper_bound(provenance.origin().uri()))
                    .saturating_add(escaped_string_upper_bound(
                        provenance.server().server_artifact.as_str(),
                    ))
                    .saturating_add(escaped_string_upper_bound(
                        provenance.server().configuration.as_str(),
                    ))
            }
        });
    }
    charge_work(
        work,
        retained.max(
            ids.len()
                .saturating_add(edges.len())
                .saturating_add(paths.nodes.len())
                .saturating_add(paths.edges.len()),
        ),
        max_work,
    )?;
    check_deadline(deadline)?;
    if retained.div_ceil(4) > budget.max_estimated_tokens {
        Err(MapError::BoundExceeded(MapBound::EstimatedTokens))
    } else if retained > budget.max_result_bytes {
        Err(MapError::BoundExceeded(MapBound::ResultBytes))
    } else {
        Ok(())
    }
}

fn move_ranked_prefix(
    entries: &mut Vec<RepositoryMapEntry>,
    rendered: &mut VecDeque<RepositoryMapEntry>,
    mandatory: usize,
    target: usize,
) {
    while entries.len() - mandatory < target {
        entries.push(
            rendered
                .pop_front()
                .expect("ranked prefix target is within rendered entries"),
        );
    }
    while entries.len() - mandatory > target {
        rendered.push_front(
            entries
                .pop()
                .expect("ranked prefix retains mandatory entries"),
        );
    }
}

fn update_page_state(
    response: &mut RepositoryMap,
    ranked: usize,
    consumed: usize,
    index_incomplete: bool,
    syntax_incomplete: bool,
    cursor: Option<MapCursor>,
) {
    response.item_count = response.path_nodes.len()
        + response.path_edges.len()
        + response.entries.len()
        + response.edges.len();
    response.omissions.ranked_entries = ranked.saturating_sub(consumed);
    response.completeness = if index_incomplete {
        MapCompleteness::IndexIncomplete
    } else if syntax_incomplete {
        MapCompleteness::SyntaxIncomplete
    } else if response.omissions.ranked_entries != 0 {
        MapCompleteness::RankedEntriesOmitted
    } else {
        MapCompleteness::Complete
    };
    response.truncated = response.completeness != MapCompleteness::Complete;
    response.cursor = cursor;
}

#[allow(clippy::too_many_arguments)]
fn resumable_cursor(
    cursor: Option<&MapCursor>,
    index: &MetadataIndex,
    policy_digest: [u8; 32],
    options_digest: [u8; 32],
    neighborhood_digest: [u8; 32],
    evidence_digest: [u8; 32],
    mandatory_entries: usize,
    mandatory_edges: usize,
    consumed: usize,
    ranked: usize,
    max_frontier: usize,
    index_incomplete: bool,
    syntax_incomplete: bool,
) -> Option<MapCursor> {
    (consumed < ranked && consumed <= max_frontier && !index_incomplete && !syntax_incomplete).then(
        || {
            make_cursor(
                index,
                policy_digest,
                options_digest,
                neighborhood_digest,
                evidence_digest,
                cursor.map_or(1, |cursor| cursor.page.saturating_add(1)),
                mandatory_entries,
                mandatory_edges,
                consumed,
            )
        },
    )
}

fn validate_cursor(
    cursor: Option<&MapCursor>,
    index: &MetadataIndex,
    policy_digest: [u8; 32],
    options_digest: [u8; 32],
    neighborhood_digest: [u8; 32],
    evidence_digest: [u8; 32],
    max_frontier: usize,
) -> Result<usize, MapError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let context_digest =
        cursor_context_digest(options_digest, neighborhood_digest, evidence_digest);
    let expected = cursor_digest(
        cursor.revision,
        cursor.index_digest,
        cursor.policy_digest,
        cursor.context_digest,
        cursor.page,
        cursor.mandatory_entries,
        cursor.mandatory_edges,
        cursor.frontier,
    );
    if cursor.revision != index.revision()
        || cursor.index_digest != *index.index_digest()
        || cursor.policy_digest != policy_digest
        || cursor.context_digest != context_digest
        || cursor.page == 0
        || cursor.frontier > max_frontier
        || cursor.digest != expected
    {
        Err(MapError::CursorMismatch)
    } else {
        Ok(cursor.frontier)
    }
}

#[allow(clippy::too_many_arguments)]
fn make_cursor(
    index: &MetadataIndex,
    policy_digest: [u8; 32],
    options_digest: [u8; 32],
    neighborhood_digest: [u8; 32],
    evidence_digest: [u8; 32],
    page: usize,
    mandatory_entries: usize,
    mandatory_edges: usize,
    frontier: usize,
) -> MapCursor {
    let revision = index.revision();
    let index_digest = *index.index_digest();
    let context_digest =
        cursor_context_digest(options_digest, neighborhood_digest, evidence_digest);
    MapCursor {
        revision,
        index_digest,
        policy_digest,
        context_digest,
        page,
        mandatory_entries,
        mandatory_edges,
        frontier,
        digest: cursor_digest(
            revision,
            index_digest,
            policy_digest,
            context_digest,
            page,
            mandatory_entries,
            mandatory_edges,
            frontier,
        ),
    }
}

fn digest_request(
    request: &RepositoryMapRequest,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<[u8; 32], MapError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-repository-map-request-v1\0");
    let mut terms = request
        .personalization
        .task_terms
        .iter()
        .collect::<Vec<_>>();
    charge_sort_work(work, terms.len(), max_work, deadline)?;
    terms.sort_unstable();
    check_deadline(deadline)?;
    terms.dedup();
    for term in terms {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        frame(&mut hash, term.as_bytes());
    }
    let mut ids = request.personalization.exact_declaration_ids.clone();
    charge_sort_work(work, ids.len(), max_work, deadline)?;
    ids.sort_unstable();
    check_deadline(deadline)?;
    ids.dedup();
    for id in ids {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        hash.update(&id.0);
    }
    let mut frames = request
        .personalization
        .stack_frames
        .iter()
        .collect::<Vec<_>>();
    charge_sort_work(work, frames.len(), max_work, deadline)?;
    frames.sort_unstable();
    check_deadline(deadline)?;
    frames.dedup();
    for stack in frames {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        frame(&mut hash, stack.path.as_os_str().as_encoded_bytes());
        frame(
            &mut hash,
            stack.symbol.as_deref().unwrap_or_default().as_bytes(),
        );
        hash.update(&(stack.line.unwrap_or_default() as u128).to_le_bytes());
    }
    for path in &request.personalization.recently_read_paths {
        charge_work(work, 1, max_work)?;
        check_deadline(deadline)?;
        frame(&mut hash, path.as_os_str().as_encoded_bytes());
    }
    for paths in [
        &request.personalization.current_edit_paths,
        &request.path_prefixes,
    ] {
        let mut paths = paths.iter().collect::<Vec<_>>();
        charge_sort_work(work, paths.len(), max_work, deadline)?;
        paths.sort_unstable();
        check_deadline(deadline)?;
        paths.dedup();
        for path in paths {
            check_deadline(deadline)?;
            charge_work(work, 1, max_work)?;
            frame(&mut hash, path.as_os_str().as_encoded_bytes());
        }
    }
    let mut languages = request.languages.iter().collect::<Vec<_>>();
    charge_sort_work(work, languages.len(), max_work, deadline)?;
    languages.sort_unstable();
    check_deadline(deadline)?;
    languages.dedup();
    for language in languages {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        frame(&mut hash, language.as_bytes());
    }
    hash.update(b"\0expansion-declaration-ids\0");
    let mut seeds = request.expansion.seeds.clone();
    charge_sort_work(work, seeds.len(), max_work, deadline)?;
    seeds.sort_unstable();
    check_deadline(deadline)?;
    seeds.dedup();
    for seed in seeds {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        hash.update(&seed.0);
    }
    hash.update(b"\0expansion-paths\0");
    let mut paths = request.expansion.paths.iter().collect::<Vec<_>>();
    charge_sort_work(work, paths.len(), max_work, deadline)?;
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        frame(&mut hash, path.as_str().as_bytes());
    }
    hash.update(b"\0expansion-symbols\0");
    let mut symbols = request.expansion.symbols.iter().collect::<Vec<_>>();
    charge_sort_work(work, symbols.len(), max_work, deadline)?;
    symbols.sort_unstable();
    symbols.dedup();
    for symbol in symbols {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        frame(&mut hash, symbol.as_bytes());
    }
    hash.update(b"\0expansion-score-band\0");
    match request.expansion.score_band {
        Some(band) => {
            hash.update(&[1]);
            hash.update(&band.min.to_le_bytes());
            hash.update(&band.max.to_le_bytes());
            frame(&mut hash, MAP_POLICY_RANK_VERSION.as_bytes());
        }
        None => {
            hash.update(&[0]);
        }
    }
    hash.update(&[request.expansion.purpose as u8]);
    let mut relationships = request.expansion.relationships.clone();
    charge_sort_work(work, relationships.len(), max_work, deadline)?;
    relationships.sort_unstable();
    check_deadline(deadline)?;
    relationships.dedup();
    for relationship in relationships {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        hash.update(&[relationship as u8]);
    }
    check_deadline(deadline)?;
    Ok(*hash.finalize().as_bytes())
}

fn digest_evidence(
    evidence: &[SemanticRelationship<'_>],
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<[u8; 32], MapError> {
    let mut values = Vec::with_capacity(evidence.len());
    for relationship in evidence {
        check_deadline(deadline)?;
        charge_work(work, 1, max_work)?;
        let fact = relationship.fact;
        let provenance = fact.provenance();
        let origin = provenance.origin();
        let mut hash = blake3::Hasher::new();
        hash.update(&relationship.source_declaration.0);
        hash.update(&[fact.relation() as u8]);
        frame(&mut hash, fact.path().as_path().as_str().as_bytes());
        hash.update(&(fact.range().start() as u128).to_le_bytes());
        hash.update(&(fact.range().end() as u128).to_le_bytes());
        digest_semantic_range(&mut hash, fact.target_range());
        hash.update(&(fact.origin_point() as u128).to_le_bytes());
        digest_semantic_range(&mut hash, Some(fact.origin_range()));
        hash.update(&[provenance.classification() as u8]);
        hash.update(&[provenance.source() as u8]);
        hash.update(provenance.revision().as_bytes());
        hash.update(&[provenance.confidence() as u8]);
        frame(&mut hash, origin.uri().as_bytes());
        hash.update(&origin.document_version().get().to_le_bytes());
        hash.update(&origin.request_generation().to_le_bytes());
        hash.update(&origin.request_id().get().to_le_bytes());
        frame(
            &mut hash,
            provenance.server().server_artifact.to_string().as_bytes(),
        );
        frame(
            &mut hash,
            provenance.server().configuration.to_string().as_bytes(),
        );
        hash.update(&[provenance.position_encoding() as u8]);
        values.push(*hash.finalize().as_bytes());
    }
    charge_sort_work(work, values.len(), max_work, deadline)?;
    values.sort_unstable();
    check_deadline(deadline)?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-repository-map-evidence-v1\0");
    for value in values {
        hash.update(&value);
    }
    Ok(*hash.finalize().as_bytes())
}

fn digest_semantic_range(
    hash: &mut blake3::Hasher,
    range: Option<&crate::verify::lsp::facts::SemanticByteRange>,
) {
    match range {
        Some(range) => {
            hash.update(&[1]);
            hash.update(&(range.start() as u128).to_le_bytes());
            hash.update(&(range.end() as u128).to_le_bytes());
        }
        None => {
            hash.update(&[0]);
        }
    }
}

fn digest_options(
    budget: MapBudget,
    limits: MapLimits,
    deadline: Instant,
) -> Result<[u8; 32], MapError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-repository-map-options-v1\0");
    for value in [
        budget.max_items as u128,
        budget.max_estimated_tokens as u128,
        budget.max_hops as u128,
        budget.max_degree as u128,
        budget.max_result_bytes as u128,
        limits.max_items as u128,
        limits.max_estimated_tokens as u128,
        limits.max_hops as u128,
        limits.max_degree as u128,
        limits.max_result_bytes as u128,
        limits.max_task_terms as u128,
        limits.max_exact_ids as u128,
        limits.max_stack_frames as u128,
        limits.max_recent_paths as u128,
        limits.max_current_edit_paths as u128,
        limits.max_path_filters as u128,
        limits.max_language_filters as u128,
        limits.max_expansion_seeds as u128,
        limits.max_expansion_paths as u128,
        limits.max_expansion_symbols as u128,
        limits.max_relationship_kinds as u128,
        limits.max_semantic_relationships as u128,
        limits.max_input_bytes as u128,
        limits.max_work as u128,
        limits.max_candidates as u128,
        limits.max_highlight_bytes as u128,
        limits.max_cursor_frontier as u128,
        limits.max_time.as_nanos(),
    ] {
        check_deadline(deadline)?;
        hash.update(&value.to_le_bytes());
    }
    Ok(*hash.finalize().as_bytes())
}

#[allow(clippy::too_many_arguments)]
fn cursor_digest(
    revision: RevisionId,
    index: [u8; 32],
    policy: [u8; 32],
    context: [u8; 32],
    page: usize,
    mandatory_entries: usize,
    mandatory_edges: usize,
    frontier: usize,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-repository-map-cursor-v1\0");
    hash.update(revision.to_string().as_bytes());
    hash.update(&index);
    hash.update(&policy);
    hash.update(&context);
    hash.update(&(page as u128).to_le_bytes());
    hash.update(&(mandatory_entries as u128).to_le_bytes());
    hash.update(&(mandatory_edges as u128).to_le_bytes());
    hash.update(&(frontier as u128).to_le_bytes());
    *hash.finalize().as_bytes()
}

fn cursor_context_digest(
    options: [u8; 32],
    neighborhood: [u8; 32],
    evidence: [u8; 32],
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-repository-map-cursor-context-v1\0");
    hash.update(&options);
    hash.update(&neighborhood);
    hash.update(&evidence);
    *hash.finalize().as_bytes()
}

fn position_encoding(encoding: PositionEncoding) -> &'static str {
    match encoding {
        PositionEncoding::Utf8 => "utf-8",
        PositionEncoding::Utf16 => "utf-16",
        PositionEncoding::Utf32 => "utf-32",
    }
}

fn bounded_first_line(value: &str, max_bytes: usize) -> (&str, bool) {
    let boundary = floor_char_boundary(value, max_bytes);
    let prefix = &value[..boundary];
    match prefix.find('\n') {
        Some(newline) => (&prefix[..newline], false),
        None => (
            prefix,
            boundary < value.len() && value.as_bytes().get(boundary) != Some(&b'\n'),
        ),
    }
}

fn floor_char_boundary(value: &str, max_bytes: usize) -> usize {
    let mut boundary = value.len().min(max_bytes);
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn charge_work(work: &mut usize, amount: usize, maximum: usize) -> Result<(), MapError> {
    *work = work
        .checked_add(amount)
        .filter(|work| *work <= maximum)
        .ok_or(MapError::InvalidRequest("map work limit exceeded"))?;
    Ok(())
}

fn charge_sort_work(
    work: &mut usize,
    items: usize,
    maximum: usize,
    deadline: Instant,
) -> Result<(), MapError> {
    check_deadline(deadline)?;
    if items > 1 {
        let comparisons = items
            .checked_mul((usize::BITS - (items - 1).leading_zeros()) as usize)
            .ok_or(MapError::InvalidRequest("map work limit exceeded"))?;
        charge_work(work, comparisons, maximum)?;
    }
    check_deadline(deadline)
}

fn check_deadline(deadline: Instant) -> Result<(), MapError> {
    if Instant::now() >= deadline {
        Err(MapError::TimeLimit)
    } else {
        Ok(())
    }
}

fn frame(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn serialize_hex<S, const N: usize>(value: &[u8; N], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(N * 2);
    for byte in value {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    serializer.serialize_str(&output)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return None;
    }
    let mut output = [0_u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(output)
}
