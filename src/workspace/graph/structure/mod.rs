use std::{
    cell::Cell,
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    mem::size_of,
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use globset::{GlobBuilder, GlobSetBuilder};

use crate::{
    verify::lsp::facts::{
        DiagnosticCode, LiveDiagnostic, NormalizedConfidence, RepositoryFactClassification,
        RepositoryFactProvenance, SemanticFact, SemanticRelationKind,
    },
    verify::lsp::session::PositionEncoding,
    workspace::{
        edit::ir::RootRelativePath,
        index::meta::{ContentState, MetadataEntry, MetadataIndex},
        map::{
            MapError, MapLimits, SemanticRelationship, ValidatedSemanticEdge,
            validated_semantic_edges,
        },
        revision::{EntryKind, LimitKind, ManagedWorkspace, RevisionError, RevisionId},
        syntax::{
            RUST_GRAMMAR_ABI, RUST_GRAMMAR_ARTIFACT_DIGEST, RUST_GRAMMAR_VERSION,
            RUST_QUERY_SET_DIGEST, SyntacticSymbolKind, SyntacticSymbolRecord,
            TREE_SITTER_RUNTIME_VERSION,
        },
    },
};

const TOML_PARSER_ID: &str = "toml@1.1.4+spec-1.1.0";
const GLOBSET_ID: &str = "globset@0.4.19";
const MANIFEST_POLICY: &str = "cargo-manifest-v2";
const RUST_POLICY: &str = "metadata-index-tree-sitter-v2";
const MAX_TOML_PARSER_INPUT: usize = 256 * 1024;
const MAX_CFG_DEPTH: usize = 256;
const BTREE_ENTRY_WEIGHT: usize = size_of::<[usize; 8]>();
const ARC_WEIGHT: usize = 2 * size_of::<usize>();

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId([u8; 32]);

impl NodeId {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NodeKind {
    Repository,
    Revision,
    Package,
    File,
    Symbol,
    Test,
    Diagnostic,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EdgeKind {
    Contains,
    Defines,
    Imports,
    Exports,
    References,
    Calls,
    Implements,
    Inherits,
    Overrides,
    Tests,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GraphRange {
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
}

impl GraphRange {
    pub const fn start_byte(self) -> usize {
        self.start_byte
    }

    pub const fn end_byte(self) -> usize {
        self.end_byte
    }

    pub const fn start_line(self) -> usize {
        self.start_line
    }

    pub const fn end_line(self) -> usize {
        self.end_line
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RangeKind {
    WholeFile,
    Declaration,
    Manifest,
    NormalizedFact,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProvenanceSource {
    MetadataIndex,
    CargoManifest,
    CargoConvention,
    TreeSitter,
    CargoTreeSitter,
    Lsp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EdgeProvenance {
    source: ProvenanceSource,
    path: Option<RootRelativePath>,
    range: GraphRange,
    range_kind: RangeKind,
    revision: RevisionId,
    confidence_millis: u16,
    semantic: Option<SemanticEdgeProvenance>,
    evidence_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticEdgeProvenance {
    relation: SemanticRelationKind,
    origin_uri: String,
    origin_path: RootRelativePath,
    document_version: i32,
    request_generation: u64,
    request_id: u32,
    origin_position: usize,
    origin_range: GraphRange,
    server_artifact: String,
    server_configuration: String,
    position_encoding: PositionEncoding,
    target_range: GraphRange,
    fact_range: GraphRange,
}

impl EdgeProvenance {
    pub const fn source(&self) -> ProvenanceSource {
        self.source
    }

    pub const fn path(&self) -> Option<&RootRelativePath> {
        self.path.as_ref()
    }

    pub const fn range(&self) -> GraphRange {
        self.range
    }

    pub const fn range_kind(&self) -> RangeKind {
        self.range_kind
    }

    pub const fn revision(&self) -> RevisionId {
        self.revision
    }

    pub const fn confidence_millis(&self) -> u16 {
        self.confidence_millis
    }

    pub const fn semantic(&self) -> Option<&SemanticEdgeProvenance> {
        self.semantic.as_ref()
    }

    pub const fn evidence_digest(&self) -> [u8; 32] {
        self.evidence_digest
    }
}

impl SemanticEdgeProvenance {
    pub const fn relation(&self) -> SemanticRelationKind {
        self.relation
    }

    pub fn origin_uri(&self) -> &str {
        &self.origin_uri
    }

    pub const fn origin_path(&self) -> &RootRelativePath {
        &self.origin_path
    }

    pub const fn document_version(&self) -> i32 {
        self.document_version
    }

    pub const fn request_generation(&self) -> u64 {
        self.request_generation
    }

    pub const fn request_id(&self) -> u32 {
        self.request_id
    }

    pub const fn origin_position(&self) -> usize {
        self.origin_position
    }

    pub const fn origin_range(&self) -> GraphRange {
        self.origin_range
    }

    pub fn server_artifact(&self) -> &str {
        &self.server_artifact
    }

    pub fn server_configuration(&self) -> &str {
        &self.server_configuration
    }

    pub const fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub const fn target_range(&self) -> GraphRange {
        self.target_range
    }

    pub const fn fact_range(&self) -> GraphRange {
        self.fact_range
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphNode {
    id: NodeId,
    kind: NodeKind,
    name: String,
    path: Option<RootRelativePath>,
    range: Option<GraphRange>,
    revision: RevisionId,
    structural_digest: [u8; 32],
    subgraph_digest: [u8; 32],
}

impl GraphNode {
    pub const fn id(&self) -> NodeId {
        self.id
    }

    pub const fn kind(&self) -> NodeKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn path(&self) -> Option<&RootRelativePath> {
        self.path.as_ref()
    }

    pub const fn range(&self) -> Option<GraphRange> {
        self.range
    }

    pub const fn revision(&self) -> RevisionId {
        self.revision
    }

    pub const fn structural_digest(&self) -> [u8; 32] {
        self.structural_digest
    }

    pub const fn subgraph_digest(&self) -> [u8; 32] {
        self.subgraph_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphEdge {
    source: NodeId,
    target: NodeId,
    kind: EdgeKind,
    provenance: EdgeProvenance,
    revision: RevisionId,
    structural_digest: [u8; 32],
}

impl GraphEdge {
    pub const fn source(&self) -> NodeId {
        self.source
    }

    pub const fn target(&self) -> NodeId {
        self.target
    }

    pub const fn kind(&self) -> EdgeKind {
        self.kind
    }

    pub const fn provenance(&self) -> &EdgeProvenance {
        &self.provenance
    }

    pub const fn revision(&self) -> RevisionId {
        self.revision
    }

    pub const fn structural_digest(&self) -> [u8; 32] {
        self.structural_digest
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CoverageStatus {
    Extracted,
    ObservedPartial,
    Unavailable,
    NotExtracted,
    Malformed,
    Incomplete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverageRecord {
    subject: Option<NodeId>,
    relation: EdgeKind,
    status: CoverageStatus,
    detail: &'static str,
    revision: RevisionId,
}

impl CoverageRecord {
    pub const fn subject(&self) -> Option<NodeId> {
        self.subject
    }

    pub const fn relation(&self) -> EdgeKind {
        self.relation
    }

    pub const fn status(&self) -> CoverageStatus {
        self.status
    }

    pub const fn detail(&self) -> &'static str {
        self.detail
    }

    pub const fn revision(&self) -> RevisionId {
        self.revision
    }
}

impl Ord for CoverageRecord {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.subject, self.relation, self.status, self.detail)
            .cmp(&(other.subject, other.relation, other.status, other.detail))
            .then_with(|| self.revision.to_string().cmp(&other.revision.to_string()))
    }
}

impl PartialOrd for CoverageRecord {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructureGraph {
    revision: RevisionId,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    coverage: Vec<CoverageRecord>,
    content_digest: [u8; 32],
    snapshot_digest: [u8; 32],
    index_digest: [u8; 32],
    options_digest: [u8; 32],
    logical_bytes: usize,
}

impl StructureGraph {
    pub const fn revision(&self) -> RevisionId {
        self.revision
    }

    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    pub fn edges(&self) -> &[GraphEdge] {
        &self.edges
    }

    pub fn coverage(&self) -> &[CoverageRecord] {
        &self.coverage
    }

    pub const fn content_digest(&self) -> [u8; 32] {
        self.content_digest
    }

    pub const fn snapshot_digest(&self) -> [u8; 32] {
        self.snapshot_digest
    }

    pub const fn index_digest(&self) -> [u8; 32] {
        self.index_digest
    }

    pub const fn options_digest(&self) -> [u8; 32] {
        self.options_digest
    }

    pub const fn logical_bytes(&self) -> usize {
        self.logical_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphOptions {
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_manifests: usize,
    pub max_manifest_bytes: usize,
    pub max_manifest_input_bytes: usize,
    pub max_toml_nesting: usize,
    pub max_toml_items: usize,
    pub max_toml_string_bytes: usize,
    pub max_targets_per_manifest: usize,
    pub max_dependencies_per_manifest: usize,
    pub max_workspace_dependencies: usize,
    pub max_input_bytes: usize,
    pub max_evidence: usize,
    pub max_evidence_bytes: usize,
    pub max_member_patterns: usize,
    pub max_pattern_bytes: usize,
    pub max_pattern_components: usize,
    pub max_cache_entries: usize,
    pub max_cache_bytes: usize,
    pub max_staging_bytes: usize,
    pub max_work: usize,
    pub max_time: Duration,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            max_nodes: 250_000,
            max_edges: 500_000,
            max_manifests: 4_096,
            max_manifest_bytes: 16 * 1024 * 1024,
            max_manifest_input_bytes: MAX_TOML_PARSER_INPUT,
            max_toml_nesting: 64,
            max_toml_items: 100_000,
            max_toml_string_bytes: 256 * 1024,
            max_targets_per_manifest: 8_192,
            max_dependencies_per_manifest: 65_536,
            max_workspace_dependencies: 65_536,
            max_input_bytes: 512 * 1024 * 1024,
            max_evidence: 100_000,
            max_evidence_bytes: 32 * 1024 * 1024,
            max_member_patterns: 16_384,
            max_pattern_bytes: 4_096,
            max_pattern_components: 256,
            max_cache_entries: 16_384,
            max_cache_bytes: 128 * 1024 * 1024,
            max_staging_bytes: 256 * 1024 * 1024,
            max_work: 20_000_000,
            max_time: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphBound {
    Nodes,
    Edges,
    Manifests,
    ManifestBytes,
    ManifestInputBytes,
    TomlNesting,
    TomlItems,
    TomlStringBytes,
    Targets,
    Dependencies,
    WorkspaceDependencies,
    InputBytes,
    Evidence,
    EvidenceBytes,
    MemberPatterns,
    PatternBytes,
    PatternComponents,
    CacheEntries,
    CacheBytes,
    StagingBytes,
    Work,
    Time,
}

#[derive(Debug)]
pub enum GraphError {
    Revision(RevisionError),
    InvalidOptions(&'static str),
    InvalidIndex(&'static str),
    StaleEvidence,
    UnsafePath(PathBuf),
    MalformedManifest { path: PathBuf, reason: String },
    MissingWorkspaceMember { manifest: PathBuf, pattern: String },
    MissingPathDependency { manifest: PathBuf, path: PathBuf },
    InvalidEvidence(&'static str),
    ContainmentCycle,
    BoundExceeded(GraphBound),
}

impl From<RevisionError> for GraphError {
    fn from(value: RevisionError) -> Self {
        match value {
            RevisionError::LimitExceeded(LimitKind::Time) => Self::BoundExceeded(GraphBound::Time),
            value => Self::Revision(value),
        }
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::InvalidOptions(reason) => write!(formatter, "invalid graph options: {reason}"),
            Self::InvalidIndex(reason) => write!(formatter, "invalid metadata index: {reason}"),
            Self::StaleEvidence => formatter.write_str("graph evidence is stale"),
            Self::UnsafePath(path) => write!(formatter, "unsafe graph path: {}", path.display()),
            Self::MalformedManifest { path, reason } => {
                write!(
                    formatter,
                    "malformed Cargo manifest {}: {reason}",
                    path.display()
                )
            }
            Self::MissingWorkspaceMember { manifest, pattern } => write!(
                formatter,
                "workspace member pattern {pattern:?} in {} matches no indexed package",
                manifest.display()
            ),
            Self::MissingPathDependency { manifest, path } => write!(
                formatter,
                "path dependency {} in {} has no indexed package",
                path.display(),
                manifest.display()
            ),
            Self::InvalidEvidence(reason) => write!(formatter, "invalid graph evidence: {reason}"),
            Self::ContainmentCycle => formatter.write_str("graph containment cycle"),
            Self::BoundExceeded(bound) => write!(formatter, "graph {bound:?} bound exceeded"),
        }
    }
}

impl std::error::Error for GraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefreshMetrics {
    parsed_fragments: usize,
    reused_fragments: usize,
    rebuilt_subgraphs: usize,
    reused_subgraphs: usize,
    evicted_fragments: usize,
    changed_paths: Vec<RootRelativePath>,
    consumed_work: usize,
    peak_staging_bytes: usize,
}

impl RefreshMetrics {
    pub const fn parsed_fragments(&self) -> usize {
        self.parsed_fragments
    }

    pub const fn reused_fragments(&self) -> usize {
        self.reused_fragments
    }

    pub const fn rebuilt_subgraphs(&self) -> usize {
        self.rebuilt_subgraphs
    }

    pub const fn reused_subgraphs(&self) -> usize {
        self.reused_subgraphs
    }

    pub const fn evicted_fragments(&self) -> usize {
        self.evicted_fragments
    }

    pub fn changed_paths(&self) -> &[RootRelativePath] {
        &self.changed_paths
    }

    pub const fn consumed_work(&self) -> usize {
        self.consumed_work
    }

    pub const fn peak_staging_bytes(&self) -> usize {
        self.peak_staging_bytes
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheUsage {
    entries: usize,
    logical_bytes: usize,
}

impl CacheUsage {
    pub const fn entries(self) -> usize {
        self.entries
    }

    pub const fn logical_bytes(self) -> usize {
        self.logical_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FragmentKey {
    source: [u8; 32],
    extractor: [u8; 32],
}

#[derive(Clone, Debug)]
struct CachedFragment {
    fragment: CachedFragmentValue,
    logical_bytes: usize,
    last_used: u64,
}

#[derive(Clone, Debug)]
enum CachedFragmentValue {
    Manifest(Arc<ManifestModel>),
    Rust(Arc<RustModel>),
    File(Arc<LineIndex>),
}

#[derive(Clone, Debug)]
pub struct StructureGraphProvider {
    graph: Option<StructureGraph>,
    cache: BTreeMap<FragmentKey, Arc<CachedFragment>>,
    cache_bytes: usize,
    path_digests: BTreeMap<RootRelativePath, [u8; 32]>,
    metrics: RefreshMetrics,
    clock: u64,
}

impl Default for StructureGraphProvider {
    fn default() -> Self {
        Self {
            graph: None,
            cache: BTreeMap::new(),
            cache_bytes: size_of::<BTreeMap<FragmentKey, Arc<CachedFragment>>>(),
            path_digests: BTreeMap::new(),
            metrics: RefreshMetrics::default(),
            clock: 0,
        }
    }
}

impl StructureGraphProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn graph(&self) -> Option<&StructureGraph> {
        self.graph.as_ref()
    }

    pub fn validated_graph(
        &self,
        workspace: &ManagedWorkspace,
    ) -> Result<Option<&StructureGraph>, GraphError> {
        if let Some(graph) = &self.graph {
            workspace.validate_revision(graph.revision)?;
        }
        Ok(self.graph.as_ref())
    }

    pub const fn metrics(&self) -> &RefreshMetrics {
        &self.metrics
    }

    pub fn cache_usage(&self) -> CacheUsage {
        CacheUsage {
            entries: self.cache.len(),
            logical_bytes: self.cache_bytes,
        }
    }

    pub fn refresh(
        &mut self,
        workspace: &ManagedWorkspace,
        index: &MetadataIndex,
        options: &GraphOptions,
        diagnostics: &[LiveDiagnostic],
        semantic: &[SemanticRelationship<'_>],
    ) -> Result<&StructureGraph, GraphError> {
        validate_options(options)?;
        let started = Instant::now();
        let deadline = started.checked_add(options.max_time).unwrap_or(started);
        let current = workspace.validate_revision_until(index.revision(), deadline)?;
        if current.epoch() != index.epoch() {
            return Err(GraphError::InvalidIndex(
                "workspace epoch does not match index",
            ));
        }
        let mut staged = Build::new(self, index, options, diagnostics, semantic, deadline)?;
        staged.extract()?;
        let (graph, cache, cache_bytes, path_digests, metrics, clock) = staged.finish()?;
        workspace.validate_revision_until(index.revision(), deadline)?;
        check_deadline(deadline)?;
        self.graph = Some(graph);
        self.cache = cache;
        self.cache_bytes = cache_bytes;
        self.path_digests = path_digests;
        self.metrics = metrics;
        self.clock = clock;
        Ok(self.graph.as_ref().expect("published graph"))
    }
}

#[derive(Clone, Debug)]
struct ManifestModel {
    digest: [u8; 32],
    package: Option<String>,
    auto_lib: bool,
    auto_bins: bool,
    auto_examples: bool,
    auto_tests: bool,
    auto_benches: bool,
    members: Vec<String>,
    excludes: Vec<String>,
    targets: Vec<TargetSpec>,
    dependencies: Vec<DependencySpec>,
    workspace_dependencies: BTreeMap<String, WorkspaceDependency>,
    has_path_overrides: bool,
}

#[derive(Clone, Debug)]
struct TargetSpec {
    kind: TargetKind,
    name: Option<String>,
    path: Option<PathBuf>,
    test: bool,
    harness: bool,
    required_features: Vec<String>,
}

#[derive(Clone, Debug)]
struct DependencySpec {
    section: &'static str,
    key: String,
    package: Option<String>,
    path: Option<PathBuf>,
    workspace: bool,
}

#[derive(Clone, Debug)]
struct WorkspaceDependency {
    package: Option<String>,
    path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TargetKind {
    Library,
    Binary,
    Test,
    Example,
    Bench,
}

#[derive(Clone, Debug)]
struct RustModel {
    digest: [u8; 32],
    tests: BTreeMap<[u8; 32], TestState>,
    modules: Vec<ExternalModule>,
    complete: bool,
    tests_complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestState {
    Exact,
    Disabled,
    Unresolved,
}

#[derive(Clone, Debug)]
struct ExternalModule {
    inline_ancestors: Vec<String>,
    name: String,
    path: Option<PathBuf>,
    state: TestState,
}

#[derive(Clone, Debug)]
struct ResolvedTarget {
    path: PathBuf,
    name: String,
    kind: TargetKind,
    test: bool,
    harness: bool,
    required_features: bool,
    explicit: bool,
}

#[derive(Clone)]
struct Package<'a> {
    root: PathBuf,
    manifest: &'a MetadataEntry,
    model: Arc<ManifestModel>,
    id: NodeId,
    targets: Vec<ResolvedTarget>,
    compiled: BTreeMap<PathBuf, (BTreeSet<PathBuf>, bool)>,
    workspace: WorkspaceResolution,
}

#[derive(Clone, Copy, Debug)]
enum WorkspaceResolution {
    None,
    Exact(usize),
    Ambiguous,
}

struct WorkspaceModel<'a> {
    manifest: &'a MetadataEntry,
    root: PathBuf,
    model: Arc<ManifestModel>,
}

struct Build<'a> {
    index: &'a MetadataIndex,
    options: &'a GraphOptions,
    diagnostics: &'a [LiveDiagnostic],
    semantic: &'a [SemanticRelationship<'a>],
    deadline: Instant,
    work: usize,
    cache: BTreeMap<FragmentKey, Arc<CachedFragment>>,
    cache_bytes: usize,
    staging: Staging,
    cache_staging: BTreeMap<FragmentKey, usize>,
    protected: BTreeSet<FragmentKey>,
    path_digests: BTreeMap<RootRelativePath, [u8; 32]>,
    previous_path_digests: &'a BTreeMap<RootRelativePath, [u8; 32]>,
    metrics: RefreshMetrics,
    clock: u64,
    nodes: BTreeMap<NodeId, GraphNode>,
    edges: Vec<GraphEdge>,
    coverage: BTreeSet<CoverageRecord>,
    entries: BTreeMap<PathBuf, &'a MetadataEntry>,
    line_indices: BTreeMap<PathBuf, Arc<LineIndex>>,
    whole_ranges: BTreeMap<PathBuf, GraphRange>,
    rust: BTreeMap<PathBuf, Arc<RustModel>>,
    manifests: BTreeMap<PathBuf, Arc<ManifestModel>>,
    file_ids: BTreeMap<PathBuf, NodeId>,
    symbol_ids: BTreeMap<[u8; 32], NodeId>,
    validated_semantic: Vec<ValidatedSemanticEdge<'a>>,
    validated_semantic_bytes: usize,
    node_map_bytes: usize,
    coverage_map_bytes: usize,
}

type BuildOutput = (
    StructureGraph,
    BTreeMap<FragmentKey, Arc<CachedFragment>>,
    usize,
    BTreeMap<RootRelativePath, [u8; 32]>,
    RefreshMetrics,
    u64,
);
#[derive(Debug)]
struct LineIndex {
    starts: Vec<usize>,
}

#[derive(Clone)]
struct Staging {
    bytes: Rc<Cell<usize>>,
    peak: Rc<Cell<usize>>,
    max: usize,
}

struct StagingReservation {
    bytes: Rc<Cell<usize>>,
    amount: usize,
}

impl Staging {
    fn new(max: usize) -> Self {
        Self {
            bytes: Rc::new(Cell::new(0)),
            peak: Rc::new(Cell::new(0)),
            max,
        }
    }

    fn add(&self, amount: usize) -> Result<(), GraphError> {
        let bytes = self
            .bytes
            .get()
            .checked_add(amount)
            .filter(|bytes| *bytes <= self.max)
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        self.bytes.set(bytes);
        self.peak.set(self.peak.get().max(bytes));
        Ok(())
    }

    fn reserve(&self, amount: usize) -> Result<StagingReservation, GraphError> {
        self.add(amount)?;
        Ok(StagingReservation {
            bytes: Rc::clone(&self.bytes),
            amount,
        })
    }

    fn release(&self, amount: usize) {
        self.bytes.set(self.bytes.get().saturating_sub(amount));
    }

    fn check(&self) -> Result<(), GraphError> {
        if self.bytes.get() <= self.max {
            Ok(())
        } else {
            Err(GraphError::BoundExceeded(GraphBound::StagingBytes))
        }
    }

    fn peak(&self) -> usize {
        self.peak.get()
    }
}

impl Drop for StagingReservation {
    fn drop(&mut self) {
        self.bytes.set(self.bytes.get().saturating_sub(self.amount));
    }
}

impl<'a> Build<'a> {
    fn new(
        provider: &'a StructureGraphProvider,
        index: &'a MetadataIndex,
        options: &'a GraphOptions,
        diagnostics: &'a [LiveDiagnostic],
        semantic: &'a [SemanticRelationship<'a>],
        deadline: Instant,
    ) -> Result<Self, GraphError> {
        let retained_path_digest_bytes = provider.path_digests.iter().try_fold(
            size_of::<BTreeMap<RootRelativePath, [u8; 32]>>(),
            |bytes, (path, _)| {
                bytes
                    .checked_add(root_path_map_entry_weight::<[u8; 32]>(path)?)
                    .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))
            },
        )?;
        let retained_changed_path_bytes = provider
            .metrics
            .changed_paths
            .capacity()
            .checked_mul(size_of::<RootRelativePath>())
            .and_then(|bytes| bytes.checked_add(size_of::<Vec<RootRelativePath>>()))
            .and_then(|bytes| {
                provider
                    .metrics
                    .changed_paths
                    .iter()
                    .try_fold(bytes, |bytes, path| bytes.checked_add(path.as_str().len()))
            })
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        let staging_bytes = provider
            .cache_bytes
            .checked_add(
                provider
                    .graph
                    .as_ref()
                    .map_or(0, StructureGraph::logical_bytes),
            )
            .and_then(|bytes| {
                bytes.checked_add(size_of::<BTreeMap<FragmentKey, Arc<CachedFragment>>>())
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    provider
                        .cache
                        .len()
                        .checked_mul(BTREE_ENTRY_WEIGHT + ARC_WEIGHT)?,
                )
            })
            .and_then(|bytes| bytes.checked_add(retained_path_digest_bytes))
            .and_then(|bytes| bytes.checked_add(retained_changed_path_bytes))
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        let staging = Staging::new(options.max_staging_bytes);
        staging.add(staging_bytes)?;
        let cache = provider.cache.clone();
        Ok(Self {
            index,
            options,
            diagnostics,
            semantic,
            deadline,
            work: 0,
            cache,
            cache_bytes: provider.cache_bytes,
            staging,
            cache_staging: BTreeMap::new(),
            protected: BTreeSet::new(),
            path_digests: BTreeMap::new(),
            previous_path_digests: &provider.path_digests,
            metrics: RefreshMetrics::default(),
            clock: provider.clock,
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            coverage: BTreeSet::new(),
            entries: BTreeMap::new(),
            line_indices: BTreeMap::new(),
            whole_ranges: BTreeMap::new(),
            rust: BTreeMap::new(),
            manifests: BTreeMap::new(),
            file_ids: BTreeMap::new(),
            symbol_ids: BTreeMap::new(),
            validated_semantic: Vec::new(),
            validated_semantic_bytes: 0,
            node_map_bytes: 0,
            coverage_map_bytes: 0,
        })
    }

    fn extract(&mut self) -> Result<(), GraphError> {
        self.step(self.index.entries().len())?;
        let input_bytes = self
            .index
            .entries()
            .iter()
            .try_fold(0_usize, |total, entry| {
                total
                    .checked_add(
                        usize::try_from(entry.size)
                            .map_err(|_| GraphError::BoundExceeded(GraphBound::InputBytes))?,
                    )
                    .ok_or(GraphError::BoundExceeded(GraphBound::InputBytes))
            })?;
        if input_bytes > self.options.max_input_bytes {
            return Err(GraphError::BoundExceeded(GraphBound::InputBytes));
        }
        self.add_roots()?;
        let mut manifest_count = 0_usize;
        let mut manifest_bytes = 0_usize;
        for entry in self.index.entries() {
            self.step(1)?;
            validate_path(&entry.path)?;
            self.reserve_staging(path_map_entry_weight::<&MetadataEntry>(&entry.path)?)?;
            self.entries.insert(entry.path.clone(), entry);
            self.add_file(entry)?;
            if let Some(source_digest) = entry.source_digest() {
                let path = root_path(&entry.path, self.options.max_pattern_bytes.max(1))?;
                self.reserve_staging(root_path_map_entry_weight::<[u8; 32]>(&path)?)?;
                self.path_digests.insert(path.clone(), source_digest);
                if self.previous_path_digests.get(&path) != Some(&source_digest) {
                    self.reserve_staging(size_of::<RootRelativePath>() + path.as_str().len())?;
                    self.metrics.changed_paths.push(path);
                }
            }
            let Some(source) = entry.text() else {
                self.add_file_coverage(entry)?;
                continue;
            };
            if entry
                .path
                .file_name()
                .is_some_and(|name| name == "Cargo.toml")
            {
                manifest_count = manifest_count
                    .checked_add(1)
                    .ok_or(GraphError::BoundExceeded(GraphBound::Manifests))?;
                manifest_bytes = manifest_bytes
                    .checked_add(source.len())
                    .ok_or(GraphError::BoundExceeded(GraphBound::ManifestBytes))?;
                if manifest_count > self.options.max_manifests {
                    return Err(GraphError::BoundExceeded(GraphBound::Manifests));
                }
                if manifest_bytes > self.options.max_manifest_bytes {
                    return Err(GraphError::BoundExceeded(GraphBound::ManifestBytes));
                }
                let model = self.manifest_fragment(entry)?;
                self.reserve_staging(path_map_entry_weight::<Arc<ManifestModel>>(&entry.path)?)?;
                self.manifests.insert(entry.path.clone(), model);
            }
            if entry.language.as_deref() == Some("rust") {
                let model = self.rust_fragment(entry)?;
                self.reserve_staging(path_map_entry_weight::<Arc<RustModel>>(&entry.path)?)?;
                self.rust.insert(entry.path.clone(), model);
            }
        }
        self.step(self.previous_path_digests.len())?;
        for path in self.previous_path_digests.keys() {
            if !self.path_digests.contains_key(path) {
                self.reserve_staging(size_of::<RootRelativePath>() + path.as_str().len())?;
                self.metrics.changed_paths.push(path.clone());
            }
        }
        self.charge_sort(self.metrics.changed_paths.len())?;
        self.metrics.changed_paths.sort();
        self.check_deadline()?;
        self.metrics.changed_paths.dedup();
        self.build_symbols()?;
        self.validate_evidence()?;
        let packages = self.build_packages()?;
        self.add_packages(packages)?;
        self.add_diagnostics()?;
        self.add_semantic_edges()?;
        self.complete_coverage()?;
        self.canonicalize()?;
        Ok(())
    }

    fn manifest_fragment(
        &mut self,
        entry: &MetadataEntry,
    ) -> Result<Arc<ManifestModel>, GraphError> {
        let source = entry.source_digest().ok_or(GraphError::InvalidIndex(
            "retained manifest has no source digest",
        ))?;
        let key = FragmentKey {
            source,
            extractor: extractor_digest(self.options),
        };
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or(GraphError::InvalidOptions("cache clock overflow"))?;
        if !self.protected.contains(&key) {
            self.reserve_staging(BTREE_ENTRY_WEIGHT + size_of::<FragmentKey>())?;
            self.protected.insert(key);
        }
        if let Some(cached) = self.cache.get(&key) {
            let CachedFragmentValue::Manifest(model) = &cached.fragment else {
                return Err(GraphError::InvalidIndex("fragment cache type mismatch"));
            };
            if model.digest != source {
                return Err(GraphError::InvalidIndex(
                    "manifest fragment digest mismatch",
                ));
            }
            let model = Arc::clone(model);
            let logical_bytes = cached.logical_bytes;
            let replacement_bytes = size_of::<CachedFragment>() + ARC_WEIGHT;
            self.reserve_staging(replacement_bytes)?;
            if let Some(previous) = self.cache_staging.insert(key, replacement_bytes) {
                self.staging.release(previous);
            }
            let replacement = Arc::new(CachedFragment {
                fragment: CachedFragmentValue::Manifest(Arc::clone(&model)),
                logical_bytes,
                last_used: self.clock,
            });
            self.cache.insert(key, replacement);
            self.metrics.reused_fragments += 1;
            self.step(1)?;
            return Ok(model);
        }
        self.evict_for_admission(1, 0)?;
        let _parse_reservation =
            self.ensure_temporary(entry.text().expect("manifest text").len())?;
        self.check_deadline()?;
        let (model, parse_work) = parse_manifest(
            &entry.path,
            entry.text().expect("manifest text"),
            self.options,
            self.options.max_work.saturating_sub(self.work),
            self.deadline,
        )?;
        self.step(parse_work)?;
        let logical_bytes = manifest_weight(&model)?
            .checked_add(size_of::<CachedFragment>() + ARC_WEIGHT + BTREE_ENTRY_WEIGHT)
            .ok_or(GraphError::BoundExceeded(GraphBound::CacheBytes))?;
        if logical_bytes > self.options.max_cache_bytes {
            return Err(GraphError::BoundExceeded(GraphBound::CacheBytes));
        }
        self.evict_for_admission(1, logical_bytes)?;
        let model = Arc::new(model);
        self.reserve_staging(logical_bytes)?;
        self.cache_staging.insert(key, logical_bytes);
        self.cache_bytes = self
            .cache_bytes
            .checked_add(logical_bytes)
            .ok_or(GraphError::BoundExceeded(GraphBound::CacheBytes))?;
        self.cache.insert(
            key,
            Arc::new(CachedFragment {
                fragment: CachedFragmentValue::Manifest(Arc::clone(&model)),
                logical_bytes,
                last_used: self.clock,
            }),
        );
        self.metrics.parsed_fragments += 1;
        Ok(model)
    }

    fn rust_fragment(&mut self, entry: &MetadataEntry) -> Result<Arc<RustModel>, GraphError> {
        let source_digest = entry.source_digest().ok_or(GraphError::InvalidIndex(
            "retained Rust source has no source digest",
        ))?;
        let (identity, identity_work) = rust_fragment_identity(
            self.index,
            entry,
            source_digest,
            self.options.max_work.saturating_sub(self.work),
            self.deadline,
        )?;
        self.step(identity_work)?;
        let key = FragmentKey {
            source: identity,
            extractor: rust_extractor_digest(&entry.path),
        };
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or(GraphError::InvalidOptions("cache clock overflow"))?;
        if !self.protected.contains(&key) {
            self.reserve_staging(BTREE_ENTRY_WEIGHT + size_of::<FragmentKey>())?;
            self.protected.insert(key);
        }
        if let Some(cached) = self.cache.get(&key) {
            let CachedFragmentValue::Rust(model) = &cached.fragment else {
                return Err(GraphError::InvalidIndex("fragment cache type mismatch"));
            };
            if model.digest != source_digest {
                return Err(GraphError::InvalidIndex("Rust fragment digest mismatch"));
            }
            let model = Arc::clone(model);
            let logical_bytes = cached.logical_bytes;
            let replacement_bytes = size_of::<CachedFragment>() + ARC_WEIGHT;
            self.reserve_staging(replacement_bytes)?;
            if let Some(previous) = self.cache_staging.insert(key, replacement_bytes) {
                self.staging.release(previous);
            }
            self.cache.insert(
                key,
                Arc::new(CachedFragment {
                    fragment: CachedFragmentValue::Rust(Arc::clone(&model)),
                    logical_bytes,
                    last_used: self.clock,
                }),
            );
            self.metrics.reused_subgraphs += 1;
            self.step(1)?;
            return Ok(model);
        }
        self.evict_for_admission(1, 0)?;
        let source = entry.text().expect("Rust source text");
        let attribute_staging = preflight_rust_attribute_staging(entry, source, self.deadline)?;
        let _attribute_reservation = self.ensure_temporary(attribute_staging)?;
        self.step(source.len())?;
        let (model, rust_work) = rust_model(
            entry,
            source,
            self.options.max_work.saturating_sub(self.work),
            self.deadline,
        )?;
        self.step(rust_work)?;
        let logical_bytes = rust_model_heap_weight(&model)?
            .checked_add(size_of::<RustModel>() + size_of::<CachedFragment>() + ARC_WEIGHT)
            .and_then(|bytes| bytes.checked_add(BTREE_ENTRY_WEIGHT))
            .ok_or(GraphError::BoundExceeded(GraphBound::CacheBytes))?;
        if logical_bytes > self.options.max_cache_bytes {
            return Err(GraphError::BoundExceeded(GraphBound::CacheBytes));
        }
        self.evict_for_admission(1, logical_bytes)?;
        let model = Arc::new(model);
        self.reserve_staging(logical_bytes)?;
        self.cache_staging.insert(key, logical_bytes);
        self.cache_bytes = self
            .cache_bytes
            .checked_add(logical_bytes)
            .ok_or(GraphError::BoundExceeded(GraphBound::CacheBytes))?;
        self.cache.insert(
            key,
            Arc::new(CachedFragment {
                fragment: CachedFragmentValue::Rust(Arc::clone(&model)),
                logical_bytes,
                last_used: self.clock,
            }),
        );
        self.metrics.rebuilt_subgraphs += 1;
        Ok(model)
    }

    fn line_fragment(&mut self, entry: &MetadataEntry) -> Result<Arc<LineIndex>, GraphError> {
        let source = entry.source_digest().ok_or(GraphError::InvalidIndex(
            "retained text source has no source digest",
        ))?;
        let key = FragmentKey {
            source,
            extractor: line_extractor_digest(&entry.path),
        };
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or(GraphError::InvalidOptions("cache clock overflow"))?;
        if !self.protected.contains(&key) {
            self.reserve_staging(BTREE_ENTRY_WEIGHT + size_of::<FragmentKey>())?;
            self.protected.insert(key);
        }
        if let Some(cached) = self.cache.get(&key) {
            let CachedFragmentValue::File(index) = &cached.fragment else {
                return Err(GraphError::InvalidIndex("fragment cache type mismatch"));
            };
            let index = Arc::clone(index);
            let logical_bytes = cached.logical_bytes;
            let replacement_bytes = size_of::<CachedFragment>() + ARC_WEIGHT;
            self.reserve_staging(replacement_bytes)?;
            if let Some(previous) = self.cache_staging.insert(key, replacement_bytes) {
                self.staging.release(previous);
            }
            self.cache.insert(
                key,
                Arc::new(CachedFragment {
                    fragment: CachedFragmentValue::File(Arc::clone(&index)),
                    logical_bytes,
                    last_used: self.clock,
                }),
            );
            self.step(1)?;
            return Ok(index);
        }
        self.evict_for_admission(1, 0)?;
        self.step(entry.text().expect("text source").len())?;
        let (_, line_count) = LineIndex::preflight(entry, self.deadline)?;
        let index = Arc::new(LineIndex::new(entry, line_count, self.deadline)?);
        let logical_bytes = index
            .starts
            .capacity()
            .checked_mul(size_of::<usize>())
            .and_then(|bytes| {
                bytes.checked_add(
                    size_of::<LineIndex>()
                        + size_of::<CachedFragment>()
                        + ARC_WEIGHT
                        + BTREE_ENTRY_WEIGHT,
                )
            })
            .ok_or(GraphError::BoundExceeded(GraphBound::CacheBytes))?;
        self.evict_for_admission(1, logical_bytes)?;
        self.reserve_staging(logical_bytes)?;
        self.cache_staging.insert(key, logical_bytes);
        self.cache_bytes = self
            .cache_bytes
            .checked_add(logical_bytes)
            .ok_or(GraphError::BoundExceeded(GraphBound::CacheBytes))?;
        self.cache.insert(
            key,
            Arc::new(CachedFragment {
                fragment: CachedFragmentValue::File(Arc::clone(&index)),
                logical_bytes,
                last_used: self.clock,
            }),
        );
        Ok(index)
    }

    fn evict_for_admission(&mut self, entries: usize, bytes: usize) -> Result<(), GraphError> {
        if self
            .cache
            .len()
            .checked_add(entries)
            .is_some_and(|count| count <= self.options.max_cache_entries)
            && self
                .cache_bytes
                .checked_add(bytes)
                .is_some_and(|count| count <= self.options.max_cache_bytes)
        {
            return Ok(());
        }
        self.step(self.cache.len())?;
        let mut candidates = self
            .cache
            .iter()
            .filter(|(key, _)| !self.protected.contains(key))
            .map(|(key, item)| (item.last_used, *key))
            .collect::<Vec<_>>();
        self.charge_sort(candidates.len())?;
        candidates.sort_unstable();
        self.check_deadline()?;
        let mut candidates = candidates.into_iter();
        while self
            .cache
            .len()
            .checked_add(entries)
            .is_none_or(|count| count > self.options.max_cache_entries)
            || self
                .cache_bytes
                .checked_add(bytes)
                .is_none_or(|count| count > self.options.max_cache_bytes)
        {
            let (_, candidate) = candidates.next().ok_or_else(|| {
                if self.cache.len().saturating_add(entries) > self.options.max_cache_entries {
                    GraphError::BoundExceeded(GraphBound::CacheEntries)
                } else {
                    GraphError::BoundExceeded(GraphBound::CacheBytes)
                }
            })?;
            self.step(1)?;
            let removed = self
                .cache
                .remove(&candidate)
                .expect("cache admission candidate");
            self.cache_bytes = self.cache_bytes.saturating_sub(removed.logical_bytes);
            if let Some(staged) = self.cache_staging.remove(&candidate) {
                self.staging.release(staged);
            }
            self.metrics.evicted_fragments += 1;
        }
        Ok(())
    }

    fn add_roots(&mut self) -> Result<(), GraphError> {
        let repository = node_id(b"repository", b"current");
        let revision = node_id(b"revision", b"current");
        self.insert_node(NodeKind::Repository, repository, "repository", None, None)?;
        self.insert_node(
            NodeKind::Revision,
            revision,
            self.index.revision().to_string(),
            None,
            None,
        )?;
        self.push_edge(
            repository,
            revision,
            EdgeKind::Contains,
            EdgeProvenance {
                source: ProvenanceSource::MetadataIndex,
                path: None,
                range: GraphRange {
                    start_byte: 0,
                    end_byte: 0,
                    start_line: 1,
                    end_line: 1,
                },
                range_kind: RangeKind::WholeFile,
                revision: self.index.revision(),
                confidence_millis: 1_000,
                semantic: None,
                evidence_digest: *blake3::hash(b"metadata-index-repository-revision").as_bytes(),
            },
        )
    }

    fn add_file(&mut self, entry: &MetadataEntry) -> Result<(), GraphError> {
        let id = node_id(b"file", entry.path.as_os_str().as_encoded_bytes());
        let path = root_path(&entry.path, self.options.max_input_bytes)?;
        let range = if entry.kind == EntryKind::File && entry.text().is_some() {
            let index = self.line_fragment(entry)?;
            self.reserve_staging(path_map_entry_weight::<Arc<LineIndex>>(&entry.path)?)?;
            let range = index.range(entry, 0, entry.text().expect("checked text").len())?;
            self.line_indices.insert(entry.path.clone(), index);
            self.reserve_staging(path_map_entry_weight::<GraphRange>(&entry.path)?)?;
            self.whole_ranges.insert(entry.path.clone(), range);
            Some(range)
        } else if entry.kind == EntryKind::File && entry.size == 0 {
            let range = GraphRange {
                start_byte: 0,
                end_byte: 0,
                start_line: 1,
                end_line: 1,
            };
            self.reserve_staging(path_map_entry_weight::<GraphRange>(&entry.path)?)?;
            self.whole_ranges.insert(entry.path.clone(), range);
            Some(range)
        } else {
            None
        };
        let name = path.as_str().to_owned();
        self.insert_node(NodeKind::File, id, name, Some(path), range)?;
        let node = self.nodes.get_mut(&id).expect("inserted file node");
        let mut hash = blake3::Hasher::new();
        hash.update(b"kit-structure-file-v2\0");
        hash.update(&node.structural_digest);
        hash.update(&[entry.kind as u8, entry.content_state as u8]);
        hash.update(&entry.size.to_le_bytes());
        if let Some(source) = entry.source_digest() {
            hash.update(&source);
        }
        node.structural_digest = *hash.finalize().as_bytes();
        self.reserve_staging(path_map_entry_weight::<NodeId>(&entry.path)?)?;
        self.file_ids.insert(entry.path.clone(), id);
        Ok(())
    }

    fn add_file_coverage(&mut self, entry: &MetadataEntry) -> Result<(), GraphError> {
        let status = match entry.content_state {
            ContentState::IndexLimit | ContentState::TooLarge => CoverageStatus::Incomplete,
            _ => CoverageStatus::Unavailable,
        };
        self.insert_coverage(CoverageRecord {
            subject: self.file_ids.get(&entry.path).copied(),
            relation: EdgeKind::Defines,
            status,
            detail: "source text unavailable",
            revision: self.index.revision(),
        })
    }

    fn build_symbols(&mut self) -> Result<(), GraphError> {
        let count = self
            .index
            .entries()
            .iter()
            .try_fold(0_usize, |total, entry| {
                total
                    .checked_add(entry.syntax_records.len() + 1)
                    .ok_or(GraphError::BoundExceeded(GraphBound::Work))
            })?;
        self.step(count)?;
        for entry in self.index.entries() {
            let file = self.file_ids[&entry.path];
            let (status, detail) = if entry.syntax_has_parse_errors {
                (CoverageStatus::Malformed, "syntax contains parse errors")
            } else if entry.syntax_truncated {
                (CoverageStatus::Incomplete, "syntax extraction truncated")
            } else if entry.language.as_deref() == Some("rust") && entry.text().is_some() {
                (
                    CoverageStatus::Extracted,
                    "tree-sitter declarations extracted",
                )
            } else {
                (
                    CoverageStatus::NotExtracted,
                    "structural declarations not extracted for language",
                )
            };
            self.insert_coverage(CoverageRecord {
                subject: Some(file),
                relation: EdgeKind::Defines,
                status,
                detail,
                revision: self.index.revision(),
            })?;
            for record in entry.syntax_records.iter() {
                self.validate_record(entry, record)?;
                let id = NodeId(record.declaration_id());
                let range =
                    self.graph_range(entry, record.range().start_byte, record.range().end_byte)?;
                self.insert_node(
                    NodeKind::Symbol,
                    id,
                    record.qualified_name().value().as_ref(),
                    Some(root_path(&entry.path, self.options.max_input_bytes)?),
                    Some(range),
                )?;
                if !self.symbol_ids.contains_key(&record.declaration_id()) {
                    self.reserve_staging(
                        BTREE_ENTRY_WEIGHT + size_of::<[u8; 32]>() + size_of::<NodeId>(),
                    )?;
                    self.symbol_ids.insert(record.declaration_id(), id);
                }
                let provenance = syntax_provenance(self.index.revision(), entry, record, range)?;
                self.push_edge(file, id, EdgeKind::Defines, provenance.clone())?;
                if let Some(parent) = record.enclosing_symbol() {
                    self.push_edge(NodeId(*parent.value()), id, EdgeKind::Contains, provenance)?;
                }
            }
        }
        Ok(())
    }

    fn validate_record(
        &self,
        entry: &MetadataEntry,
        record: &SyntacticSymbolRecord,
    ) -> Result<(), GraphError> {
        if record.workspace_revision() != self.index.revision()
            || record.canonical_path() != entry.path
        {
            return Err(GraphError::InvalidIndex("stale syntax record"));
        }
        self.graph_range(entry, record.range().start_byte, record.range().end_byte)?;
        Ok(())
    }

    fn build_packages(&mut self) -> Result<Vec<Package<'a>>, GraphError> {
        let mut packages = Vec::new();
        let manifests = self
            .manifests
            .iter()
            .map(|(path, model)| (path.clone(), Arc::clone(model)))
            .collect::<Vec<_>>();
        for (path, model) in manifests {
            self.step(1)?;
            let Some(name) = &model.package else { continue };
            if name.is_empty() {
                return Err(manifest_error(&path, "package.name must not be empty"));
            }
            let root = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            packages.push(Package {
                root: root.clone(),
                manifest: self.entries[&path],
                model,
                id: node_id(b"package", root.as_os_str().as_encoded_bytes()),
                targets: Vec::new(),
                compiled: BTreeMap::new(),
                workspace: WorkspaceResolution::None,
            });
        }
        self.charge_sort(packages.len())?;
        packages.sort_by(|left, right| left.root.cmp(&right.root));
        self.check_deadline()?;
        let roots = packages
            .iter()
            .enumerate()
            .map(|(index, item)| (item.root.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let workspaces = self.workspace_models()?;
        self.resolve_workspace_membership(&mut packages, &workspaces)?;

        let mut candidates = vec![Vec::<ResolvedTarget>::new(); packages.len()];
        self.step(self.entries.len())?;
        for (path, entry) in &self.entries {
            if entry.kind != EntryKind::File {
                continue;
            }
            let Some(owner) = deepest_owner(path, &roots) else {
                continue;
            };
            if let Some(target) = conventional_target(&packages[owner].root, path) {
                candidates[owner].push(target);
            }
        }
        for (index, package) in packages.iter_mut().enumerate() {
            package.targets = discover_targets(
                package,
                &self.entries,
                std::mem::take(&mut candidates[index]),
            )?;
            self.step(package.targets.len())?;
            for target in &package.targets {
                let compiled = self.discover_compiled(&target.path)?;
                package.compiled.insert(target.path.clone(), compiled);
            }
        }
        Ok(packages)
    }

    fn workspace_models(&mut self) -> Result<Vec<WorkspaceModel<'a>>, GraphError> {
        let mut output = Vec::new();
        for (path, model) in &self.manifests {
            if model.members.is_empty()
                && model.excludes.is_empty()
                && model.workspace_dependencies.is_empty()
                && !model.has_path_overrides
            {
                continue;
            }
            output.push(WorkspaceModel {
                manifest: self.entries[path],
                root: path.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
                model: Arc::clone(model),
            });
        }
        Ok(output)
    }

    fn resolve_workspace_membership(
        &mut self,
        packages: &mut [Package<'a>],
        workspaces: &[WorkspaceModel<'a>],
    ) -> Result<(), GraphError> {
        let total_patterns = workspaces.iter().try_fold(0_usize, |total, workspace| {
            total
                .checked_add(workspace.model.members.len() + workspace.model.excludes.len())
                .ok_or(GraphError::BoundExceeded(GraphBound::MemberPatterns))
        })?;
        if total_patterns > self.options.max_member_patterns {
            return Err(GraphError::BoundExceeded(GraphBound::MemberPatterns));
        }
        let match_work = total_patterns
            .checked_mul(packages.len())
            .ok_or(GraphError::BoundExceeded(GraphBound::Work))?;
        self.step(total_patterns)?;
        self.step(match_work)?;
        self.check_deadline()?;
        let mut builder = GlobSetBuilder::new();
        let mut owners = Vec::<(usize, bool, usize)>::new();
        for (workspace_index, workspace) in workspaces.iter().enumerate() {
            for (excluded, patterns) in [
                (false, &workspace.model.members),
                (true, &workspace.model.excludes),
            ] {
                for (pattern_index, pattern) in patterns.iter().enumerate() {
                    validate_pattern(pattern, self.options)?;
                    let rooted = rooted_pattern(&workspace.root, pattern);
                    let glob = GlobBuilder::new(&rooted)
                        .literal_separator(true)
                        .backslash_escape(true)
                        .build()
                        .map_err(|error| {
                            manifest_error(&workspace.manifest.path, &error.to_string())
                        })?;
                    builder.add(glob);
                    owners.push((workspace_index, excluded, pattern_index));
                }
            }
        }
        let matcher = builder.build().map_err(|error| {
            GraphError::InvalidOptions(Box::leak(error.to_string().into_boxed_str()))
        })?;
        self.check_deadline()?;
        let mut matched = vec![false; owners.len()];
        let mut memberships = vec![BTreeSet::<usize>::new(); workspaces.len()];
        let mut excluded_packages = vec![BTreeSet::<usize>::new(); workspaces.len()];
        let workspace_roots = workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| (workspace.root.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let package_roots = packages
            .iter()
            .enumerate()
            .map(|(index, package)| (package.root.clone(), index))
            .collect::<BTreeMap<_, _>>();
        for (package_index, package) in packages.iter().enumerate() {
            self.check_deadline()?;
            let mut included = BTreeSet::new();
            let mut excluded = BTreeSet::new();
            for match_index in matcher.matches(&package.root) {
                matched[match_index] = true;
                let (workspace, is_exclude, _) = owners[match_index];
                if is_exclude {
                    excluded.insert(workspace);
                } else {
                    included.insert(workspace);
                }
            }
            self.check_deadline()?;
            if let Some(index) = workspace_roots.get(&package.root) {
                included.insert(*index);
            }
            included.retain(|index| !excluded.contains(index));
            for workspace in excluded {
                excluded_packages[workspace].insert(package_index);
            }
            for workspace in included {
                memberships[workspace].insert(package_index);
            }
        }
        for (match_index, was_matched) in matched.into_iter().enumerate() {
            let (workspace, excluded, pattern) = owners[match_index];
            if !excluded && !was_matched {
                return Err(GraphError::MissingWorkspaceMember {
                    manifest: workspaces[workspace].manifest.path.clone(),
                    pattern: workspaces[workspace].model.members[pattern].clone(),
                });
            }
        }
        for (workspace_index, workspace) in workspaces.iter().enumerate() {
            for dependency in workspace.model.workspace_dependencies.values() {
                self.step(1)?;
                let Some(path) = &dependency.path else {
                    continue;
                };
                let root = normalize_join(&workspace.root, path)?;
                if let Some(package) = package_roots.get(&root)
                    && !excluded_packages[workspace_index].contains(package)
                    && root.starts_with(&workspace.root)
                {
                    memberships[workspace_index].insert(*package);
                }
            }
        }
        for _ in 0..=packages.len() {
            let mut changed = false;
            for (workspace_index, workspace) in workspaces.iter().enumerate() {
                self.step(1)?;
                let members = memberships[workspace_index]
                    .iter()
                    .copied()
                    .collect::<Vec<_>>();
                for package_index in members {
                    self.step(1)?;
                    let package = &packages[package_index];
                    for dependency in &package.model.dependencies {
                        self.step(1)?;
                        let resolved = if dependency.workspace {
                            workspace
                                .model
                                .workspace_dependencies
                                .get(&dependency.key)
                                .and_then(|dependency| dependency.path.as_ref())
                                .map(|path| (workspace.root.as_path(), path))
                        } else {
                            dependency
                                .path
                                .as_ref()
                                .map(|path| (package.root.as_path(), path))
                        };
                        let Some((base, path)) = resolved else {
                            continue;
                        };
                        let root = normalize_join(base, path)?;
                        let Some(target) = package_roots.get(&root) else {
                            continue;
                        };
                        if root.starts_with(&workspace.root)
                            && !excluded_packages[workspace_index].contains(target)
                        {
                            changed |= memberships[workspace_index].insert(*target);
                        }
                    }
                }
            }
            if !changed {
                let mut owners_by_package = vec![Vec::new(); packages.len()];
                for (workspace, members) in memberships.iter().enumerate() {
                    for package in members {
                        self.step(1)?;
                        owners_by_package[*package].push(workspace);
                    }
                }
                for (package_index, package) in packages.iter_mut().enumerate() {
                    package.workspace = match owners_by_package[package_index].as_slice() {
                        [] => WorkspaceResolution::None,
                        [workspace] => WorkspaceResolution::Exact(*workspace),
                        _ => WorkspaceResolution::Ambiguous,
                    };
                }
                return Ok(());
            }
        }
        Err(GraphError::InvalidIndex(
            "workspace path dependency closure did not converge",
        ))
    }

    fn discover_compiled(
        &mut self,
        target: &Path,
    ) -> Result<(BTreeSet<PathBuf>, bool), GraphError> {
        let mut compiled = BTreeSet::from([target.to_path_buf()]);
        let mut queue = VecDeque::from([target.to_path_buf()]);
        let mut complete = true;
        while let Some(path) = queue.pop_front() {
            self.step(1)?;
            let Some(model) = self.rust.get(&path).cloned() else {
                continue;
            };
            complete &= model.complete;
            let root = path == target;
            let base = module_base(&path, root);
            for module in &model.modules {
                self.step(1)?;
                match module.state {
                    TestState::Disabled => continue,
                    TestState::Unresolved => {
                        complete = false;
                        continue;
                    }
                    TestState::Exact => {}
                }
                let mut base = base.clone();
                for ancestor in &module.inline_ancestors {
                    base.push(ancestor);
                }
                let candidates = if let Some(explicit) = &module.path {
                    vec![normalize_join(
                        path.parent().unwrap_or_else(|| Path::new("")),
                        explicit,
                    )?]
                } else {
                    vec![
                        base.join(format!("{}.rs", module.name)),
                        base.join(&module.name).join("mod.rs"),
                    ]
                };
                let mut found = candidates.into_iter().filter(|candidate| {
                    self.entries
                        .get(candidate)
                        .is_some_and(|entry| entry.kind == EntryKind::File)
                });
                let first = found.next();
                if found.next().is_some() {
                    complete = false;
                    continue;
                }
                let Some(found) = first else {
                    complete = false;
                    continue;
                };
                if compiled.insert(found.clone()) {
                    queue.push_back(found);
                }
            }
        }
        Ok((compiled, complete))
    }

    fn add_packages(&mut self, packages: Vec<Package<'a>>) -> Result<(), GraphError> {
        let revision_node = node_id(b"revision", b"current");
        let roots = packages
            .iter()
            .enumerate()
            .map(|(index, package)| (package.root.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let workspaces = self.workspace_models()?;
        for package in &packages {
            let name = package.model.package.as_deref().expect("package model");
            self.insert_node(
                NodeKind::Package,
                package.id,
                name,
                Some(root_path(
                    &package.manifest.path,
                    self.options.max_input_bytes,
                )?),
                None,
            )?;
            let provenance = manifest_provenance(
                self.index.revision(),
                package.manifest,
                self.whole_range(package.manifest)?,
            )?;
            self.push_edge(revision_node, package.id, EdgeKind::Contains, provenance)?;
        }
        self.step(self.entries.len())?;
        for path in self.entries.keys().cloned().collect::<Vec<_>>() {
            let Some(owner) = deepest_owner(&path, &roots) else {
                continue;
            };
            let package = &packages[owner];
            self.push_edge(
                package.id,
                self.file_ids[&path],
                EdgeKind::Contains,
                manifest_provenance(
                    self.index.revision(),
                    package.manifest,
                    self.whole_range(package.manifest)?,
                )?,
            )?;
        }
        for package in &packages {
            self.add_dependency_edges(package, &packages, &roots, &workspaces)?;
            for target in &package.targets {
                self.add_target_tests(package, target)?;
            }
        }
        Ok(())
    }

    fn add_dependency_edges(
        &mut self,
        package: &Package<'a>,
        packages: &[Package<'a>],
        roots: &BTreeMap<PathBuf, usize>,
        workspaces: &[WorkspaceModel<'a>],
    ) -> Result<(), GraphError> {
        let inherited_overrides = match package.workspace {
            WorkspaceResolution::Exact(index) => workspaces[index].model.has_path_overrides,
            WorkspaceResolution::Ambiguous => true,
            WorkspaceResolution::None => false,
        };
        if package.model.has_path_overrides || inherited_overrides {
            self.insert_coverage(CoverageRecord {
                subject: Some(package.id),
                relation: EdgeKind::Imports,
                status: CoverageStatus::Unavailable,
                detail: "Cargo patch or replace resolution is unavailable",
                revision: self.index.revision(),
            })?;
        }
        self.step(package.model.dependencies.len())?;
        for dependency in &package.model.dependencies {
            let _resolved_name = dependency.package.as_deref().unwrap_or(&dependency.key);
            let resolved = if dependency.workspace {
                match package.workspace {
                    WorkspaceResolution::Exact(index) => {
                        let workspace = &workspaces[index];
                        match workspace.model.workspace_dependencies.get(&dependency.key) {
                            Some(inherited) => {
                                if let Some(path) = inherited.path.as_ref() {
                                    let _name =
                                        inherited.package.as_deref().unwrap_or(&dependency.key);
                                    Some((workspace.root.as_path(), path, workspace.manifest))
                                } else {
                                    self.insert_coverage(CoverageRecord {
                                        subject: Some(package.id),
                                        relation: EdgeKind::Imports,
                                        status: CoverageStatus::Unavailable,
                                        detail: "workspace dependency is not a local path",
                                        revision: self.index.revision(),
                                    })?;
                                    None
                                }
                            }
                            None => {
                                self.insert_coverage(CoverageRecord {
                                    subject: Some(package.id),
                                    relation: EdgeKind::Imports,
                                    status: CoverageStatus::Unavailable,
                                    detail: "workspace dependency inheritance is missing",
                                    revision: self.index.revision(),
                                })?;
                                None
                            }
                        }
                    }
                    WorkspaceResolution::None => {
                        self.insert_coverage(CoverageRecord {
                            subject: Some(package.id),
                            relation: EdgeKind::Imports,
                            status: CoverageStatus::Unavailable,
                            detail: "workspace dependency owner is unavailable",
                            revision: self.index.revision(),
                        })?;
                        None
                    }
                    WorkspaceResolution::Ambiguous => {
                        self.insert_coverage(CoverageRecord {
                            subject: Some(package.id),
                            relation: EdgeKind::Imports,
                            status: CoverageStatus::Unavailable,
                            detail: "workspace dependency owner is ambiguous",
                            revision: self.index.revision(),
                        })?;
                        None
                    }
                }
            } else {
                dependency
                    .path
                    .as_ref()
                    .map(|path| (package.root.as_path(), path, package.manifest))
            };
            let Some((base, relative, provenance_manifest)) = resolved else {
                continue;
            };
            let target_root = normalize_join(base, relative)?;
            let Some(target_index) = roots.get(&target_root).copied() else {
                return Err(GraphError::MissingPathDependency {
                    manifest: package.manifest.path.clone(),
                    path: target_root,
                });
            };
            self.push_edge(
                package.id,
                packages[target_index].id,
                EdgeKind::Imports,
                dependency_provenance(
                    self.index.revision(),
                    provenance_manifest,
                    self.whole_range(provenance_manifest)?,
                    dependency,
                )?,
            )?;
        }
        Ok(())
    }

    fn add_target_tests(
        &mut self,
        package: &Package<'a>,
        target: &ResolvedTarget,
    ) -> Result<(), GraphError> {
        if !target.test || !target.harness || target.required_features {
            self.insert_coverage(CoverageRecord {
                subject: Some(package.id),
                relation: EdgeKind::Tests,
                status: CoverageStatus::Unavailable,
                detail: if target.required_features {
                    "target required features are unresolved"
                } else {
                    "target test harness is disabled"
                },
                revision: self.index.revision(),
            })?;
            return Ok(());
        }
        let target_file = self.file_ids[&target.path];
        if target.kind == TargetKind::Test {
            let mut identity = blake3::Hasher::new();
            frame(&mut identity, package.root.as_os_str().as_encoded_bytes());
            frame(&mut identity, target.path.as_os_str().as_encoded_bytes());
            frame(&mut identity, target.name.as_bytes());
            let id = node_id(b"cargo-test-target", identity.finalize().as_bytes());
            if !self.nodes.contains_key(&id) {
                let entry = self.entries[&target.path];
                self.insert_node(
                    NodeKind::Test,
                    id,
                    &target.name,
                    Some(root_path(&target.path, self.options.max_input_bytes)?),
                    Some(self.whole_range(entry)?),
                )?;
                let provenance = if target.explicit {
                    manifest_provenance(
                        self.index.revision(),
                        package.manifest,
                        self.whole_range(package.manifest)?,
                    )?
                } else {
                    convention_provenance(self.index.revision(), entry, self.whole_range(entry)?)?
                };
                self.push_edge(target_file, id, EdgeKind::Tests, provenance)?;
            }
        }
        let (compiled, complete) = &package.compiled[&target.path];
        if !complete {
            self.insert_coverage(CoverageRecord {
                subject: Some(target_file),
                relation: EdgeKind::Tests,
                status: CoverageStatus::Unavailable,
                detail: "cfg or module reachability is unresolved",
                revision: self.index.revision(),
            })?;
            return Ok(());
        }
        let pending_capacity = compiled.iter().try_fold(0_usize, |total, path| {
            total
                .checked_add(self.entries[path].syntax_records.len())
                .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))
        })?;
        let _pending_reservation = self.ensure_temporary(
            pending_capacity
                .checked_mul(size_of::<(&MetadataEntry, &SyntacticSymbolRecord)>())
                .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?,
        )?;
        let mut pending = Vec::with_capacity(pending_capacity);
        let mut tests_complete = true;
        for path in compiled {
            self.step(1)?;
            let entry = self.entries[path];
            let Some(model) = self.rust.get(path) else {
                continue;
            };
            tests_complete &= model.tests_complete;
            for record in entry.syntax_records.iter() {
                let exact = self
                    .rust
                    .get(path)
                    .and_then(|model| model.tests.get(&record.declaration_id()).copied())
                    == Some(TestState::Exact);
                self.step(1)?;
                if !exact {
                    continue;
                }
                if self.edges.len().saturating_add(pending.len()) >= self.options.max_edges {
                    return Err(GraphError::BoundExceeded(GraphBound::Edges));
                }
                pending.push((entry, record));
            }
        }
        self.step(pending.len())?;
        for (entry, record) in pending {
            let test = node_id(b"test", &record.declaration_id());
            if !self.nodes.contains_key(&test) {
                let range =
                    self.graph_range(entry, record.range().start_byte, record.range().end_byte)?;
                self.insert_node(
                    NodeKind::Test,
                    test,
                    record.qualified_name().value().as_ref(),
                    Some(root_path(&entry.path, self.options.max_input_bytes)?),
                    Some(range),
                )?;
                self.push_edge(
                    NodeId(record.declaration_id()),
                    test,
                    EdgeKind::Defines,
                    syntax_provenance(self.index.revision(), entry, record, range)?,
                )?;
            }
            self.push_edge(
                target_file,
                test,
                EdgeKind::Tests,
                cargo_syntax_provenance(
                    self.index.revision(),
                    package.manifest,
                    entry,
                    record,
                    target.explicit,
                    self.graph_range(entry, record.range().start_byte, record.range().end_byte)?,
                )?,
            )?;
        }
        if !tests_complete {
            self.insert_coverage(CoverageRecord {
                subject: Some(target_file),
                relation: EdgeKind::Tests,
                status: CoverageStatus::Unavailable,
                detail: "test cfg reachability is unresolved",
                revision: self.index.revision(),
            })?;
        }
        Ok(())
    }

    fn validate_evidence(&mut self) -> Result<(), GraphError> {
        let count = self
            .diagnostics
            .len()
            .checked_add(self.semantic.len())
            .ok_or(GraphError::BoundExceeded(GraphBound::Evidence))?;
        if count > self.options.max_evidence {
            return Err(GraphError::BoundExceeded(GraphBound::Evidence));
        }
        self.step(count)?;
        let mut bytes = 0_usize;
        for diagnostic in self.diagnostics {
            bytes = bytes
                .checked_add(diagnostic_weight(diagnostic)?)
                .ok_or(GraphError::BoundExceeded(GraphBound::EvidenceBytes))?;
            self.validate_diagnostic(diagnostic)?;
        }
        for relationship in self.semantic {
            bytes = bytes
                .checked_add(semantic_weight(relationship.fact)?)
                .ok_or(GraphError::BoundExceeded(GraphBound::EvidenceBytes))?;
        }
        if bytes > self.options.max_evidence_bytes {
            return Err(GraphError::BoundExceeded(GraphBound::EvidenceBytes));
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        let limits = MapLimits {
            max_semantic_relationships: self.options.max_evidence,
            max_input_bytes: self.options.max_evidence_bytes,
            max_work: self.options.max_work.saturating_sub(self.work).max(1),
            max_candidates: self.options.max_nodes,
            max_time: remaining,
            ..MapLimits::default()
        };
        let semantic_temporary = self.ensure_temporary(
            self.index
                .entries()
                .iter()
                .try_fold(0_usize, |total, entry| {
                    total
                        .checked_add(entry.syntax_records.len())
                        .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))
                })?
                .checked_mul(5 * BTREE_ENTRY_WEIGHT)
                .and_then(|bytes| {
                    bytes.checked_add(
                        self.semantic
                            .len()
                            .checked_mul(size_of::<ValidatedSemanticEdge<'_>>())?,
                    )
                })
                .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?,
        )?;
        let (validated, semantic_work) =
            validated_semantic_edges(self.index, self.semantic, limits, self.deadline)
                .map_err(graph_semantic_error)?;
        self.step(semantic_work)?;
        let validated_bytes = validated
            .capacity()
            .checked_mul(size_of::<ValidatedSemanticEdge<'_>>())
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        self.reserve_staging(validated_bytes)?;
        self.validated_semantic_bytes = validated_bytes;
        self.validated_semantic = validated;
        drop(semantic_temporary);
        Ok(())
    }

    fn validate_diagnostic(&self, diagnostic: &LiveDiagnostic) -> Result<(), GraphError> {
        let provenance = diagnostic.provenance();
        if provenance.revision() != self.index.revision() {
            return Err(GraphError::StaleEvidence);
        }
        if provenance.classification() != RepositoryFactClassification::Semantic
            || provenance.source() != RepositoryFactProvenance::Lsp
            || provenance.confidence() != NormalizedConfidence::ExactSource
        {
            return Err(GraphError::InvalidEvidence(
                "diagnostic is not normalized exact LSP evidence",
            ));
        }
        let path = Path::new(diagnostic.path().as_path().as_str());
        let entry = self.entries.get(path).ok_or(GraphError::InvalidEvidence(
            "diagnostic path is not indexed",
        ))?;
        self.graph_range(entry, diagnostic.range().start(), diagnostic.range().end())?;
        Ok(())
    }

    fn add_diagnostics(&mut self) -> Result<(), GraphError> {
        for diagnostic in self.diagnostics {
            self.step(1)?;
            let path = PathBuf::from(diagnostic.path().as_path().as_str());
            let entry = self.entries[&path];
            let range =
                self.graph_range(entry, diagnostic.range().start(), diagnostic.range().end())?;
            let id = diagnostic_id(diagnostic);
            if !self.nodes.contains_key(&id) {
                self.insert_node(
                    NodeKind::Diagnostic,
                    id,
                    diagnostic.message(),
                    Some(diagnostic.path().as_path().clone()),
                    Some(range),
                )?;
            }
            let provenance = diagnostic_provenance(self.index.revision(), diagnostic, range);
            self.push_edge(self.file_ids[&path], id, EdgeKind::Contains, provenance)?;
        }
        Ok(())
    }

    fn add_semantic_edges(&mut self) -> Result<(), GraphError> {
        let validated = std::mem::take(&mut self.validated_semantic);
        for relationship in validated.iter().cloned() {
            self.step(1)?;
            let fact = relationship.fact;
            let path = fact.path().as_path().as_str();
            let kind = match fact.relation() {
                SemanticRelationKind::Implementation => EdgeKind::Implements,
                SemanticRelationKind::Reference => EdgeKind::References,
                SemanticRelationKind::Declaration
                | SemanticRelationKind::Definition
                | SemanticRelationKind::TypeDefinition => EdgeKind::References,
            };
            let origin = NodeId(relationship.source_declaration.as_bytes());
            let target = NodeId(relationship.target_declaration.as_bytes());
            let (source, target) = match fact.relation() {
                SemanticRelationKind::Reference | SemanticRelationKind::Implementation => {
                    (target, origin)
                }
                SemanticRelationKind::Declaration
                | SemanticRelationKind::Definition
                | SemanticRelationKind::TypeDefinition => (origin, target),
            };
            let target_entry = self.entries[Path::new(path)];
            let origin_path = Path::new(fact.provenance().origin().path().as_path().as_str());
            let origin_entry = self.entries[origin_path];
            let fact_range =
                self.graph_range(target_entry, fact.range().start(), fact.range().end())?;
            let target_range = fact
                .target_range()
                .map(|range| self.graph_range(target_entry, range.start(), range.end()))
                .transpose()?
                .unwrap_or(fact_range);
            let provenance = semantic_provenance(
                self.index.revision(),
                fact,
                fact_range,
                target_range,
                self.graph_range(
                    origin_entry,
                    fact.origin_range().start(),
                    fact.origin_range().end(),
                )?,
            );
            self.push_edge(source, target, kind, provenance)?;
        }
        drop(validated);
        self.staging.release(self.validated_semantic_bytes);
        self.validated_semantic_bytes = 0;
        Ok(())
    }

    fn complete_coverage(&mut self) -> Result<(), GraphError> {
        let observed = self
            .edges
            .iter()
            .map(|edge| edge.kind)
            .collect::<BTreeSet<_>>();
        for relation in [
            EdgeKind::Contains,
            EdgeKind::Defines,
            EdgeKind::Imports,
            EdgeKind::Exports,
            EdgeKind::References,
            EdgeKind::Calls,
            EdgeKind::Implements,
            EdgeKind::Inherits,
            EdgeKind::Overrides,
            EdgeKind::Tests,
        ] {
            let unavailable = self
                .coverage
                .iter()
                .filter(|record| record.relation == relation)
                .map(|record| record.status)
                .find(|status| {
                    matches!(
                        status,
                        CoverageStatus::Malformed
                            | CoverageStatus::Incomplete
                            | CoverageStatus::Unavailable
                    )
                });
            let (status, detail) = if let Some(status) = unavailable {
                (status, "exact extraction is not complete for all subjects")
            } else if observed.contains(&relation)
                && matches!(relation, EdgeKind::References | EdgeKind::Implements)
            {
                (
                    CoverageStatus::ObservedPartial,
                    "normalized semantic facts observed without completeness proof",
                )
            } else if observed.contains(&relation) {
                (CoverageStatus::Extracted, "exact facts emitted")
            } else if matches!(relation, EdgeKind::References | EdgeKind::Implements) {
                (
                    CoverageStatus::Unavailable,
                    "normalized semantic facts unavailable",
                )
            } else {
                (CoverageStatus::NotExtracted, "no exact facts extracted")
            };
            self.insert_coverage(CoverageRecord {
                subject: None,
                relation,
                status,
                detail,
                revision: self.index.revision(),
            })?;
        }
        if self.index.source_truncated() {
            self.insert_coverage(CoverageRecord {
                subject: None,
                relation: EdgeKind::Contains,
                status: CoverageStatus::Incomplete,
                detail: "metadata index source scan truncated",
                revision: self.index.revision(),
            })?;
        }
        Ok(())
    }

    fn canonicalize(&mut self) -> Result<(), GraphError> {
        self.charge_sort(self.edges.len())?;
        self.edges.sort_by(|left, right| {
            (
                left.source,
                left.target,
                left.kind,
                provenance_key(&left.provenance),
            )
                .cmp(&(
                    right.source,
                    right.target,
                    right.kind,
                    provenance_key(&right.provenance),
                ))
        });
        self.check_deadline()?;
        self.edges.dedup_by(|left, right| {
            left.source == right.source
                && left.target == right.target
                && left.kind == right.kind
                && left.provenance == right.provenance
        });
        self.detect_containment_cycle()
    }

    fn detect_containment_cycle(&mut self) -> Result<(), GraphError> {
        let containment = self
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Contains)
            .count();
        self.step(containment)?;
        let adjacency_weight = self
            .nodes
            .len()
            .checked_mul(2 * BTREE_ENTRY_WEIGHT)
            .and_then(|value| value.checked_add(containment.checked_mul(2 * size_of::<NodeId>())?))
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        let _adjacency_reservation = self.ensure_temporary(adjacency_weight)?;
        let mut incoming = BTreeMap::<NodeId, usize>::new();
        let mut outgoing = BTreeMap::<NodeId, Vec<NodeId>>::new();
        for edge in self
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Contains)
        {
            *incoming.entry(edge.target).or_default() += 1;
            incoming.entry(edge.source).or_default();
            outgoing.entry(edge.source).or_default().push(edge.target);
        }
        let mut ready = incoming
            .iter()
            .filter_map(|(node, degree)| (*degree == 0).then_some(*node))
            .collect::<BTreeSet<_>>();
        let mut visited = 0_usize;
        while let Some(node) = ready.pop_first() {
            self.step(1)?;
            visited += 1;
            if let Some(targets) = outgoing.get(&node) {
                for target in targets {
                    let degree = incoming.get_mut(target).expect("target degree");
                    *degree -= 1;
                    if *degree == 0 {
                        ready.insert(*target);
                    }
                }
            }
        }
        if visited == incoming.len() {
            Ok(())
        } else {
            Err(GraphError::ContainmentCycle)
        }
    }

    fn finish(mut self) -> Result<BuildOutput, GraphError> {
        self.evict()?;
        let finish_work = self
            .nodes
            .len()
            .checked_mul(2)
            .and_then(|value| value.checked_add(self.edges.len().checked_mul(3)?))
            .and_then(|value| value.checked_add(self.coverage.len()))
            .ok_or(GraphError::BoundExceeded(GraphBound::Work))?;
        self.step(finish_work)?;
        let structural_weight = self
            .nodes
            .len()
            .checked_mul(size_of::<(NodeId, [u8; 32])>() + BTREE_ENTRY_WEIGHT)
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        let _structural_reservation = self.ensure_temporary(structural_weight)?;
        let structural = self
            .nodes
            .iter()
            .map(|(id, node)| (*id, node.structural_digest))
            .collect::<BTreeMap<_, _>>();
        let adjacency_weight = self
            .edges
            .len()
            .checked_mul(2 * (size_of::<([u8; 32], NodeId)>() + BTREE_ENTRY_WEIGHT))
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        let _adjacency_reservation = self.ensure_temporary(adjacency_weight)?;
        let mut adjacency = BTreeMap::<NodeId, Vec<([u8; 32], NodeId)>>::new();
        for edge in &self.edges {
            self.check_deadline()?;
            if !structural.contains_key(&edge.source) || !structural.contains_key(&edge.target) {
                return Err(GraphError::InvalidIndex("graph edge endpoint is unknown"));
            }
            adjacency
                .entry(edge.source)
                .or_default()
                .push((edge.structural_digest, edge.target));
            adjacency
                .entry(edge.target)
                .or_default()
                .push((edge.structural_digest, edge.source));
        }
        let evidence_count = self
            .diagnostics
            .len()
            .checked_add(self.semantic.len())
            .ok_or(GraphError::BoundExceeded(GraphBound::Work))?;
        self.charge_sort(evidence_count)?;
        self.step(evidence_count)?;
        let evidence_digest = digest_evidence(self.diagnostics, self.semantic, self.deadline)?;
        let node_output_bytes = self
            .nodes
            .len()
            .checked_mul(size_of::<GraphNode>())
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        self.reserve_staging(node_output_bytes)?;
        let nodes_map = std::mem::take(&mut self.nodes);
        let mut nodes = Vec::with_capacity(nodes_map.len());
        nodes.extend(nodes_map.into_values());
        self.staging.release(self.node_map_bytes);
        self.node_map_bytes = 0;
        for node in &mut nodes {
            check_deadline(self.deadline)?;
            let mut hash = blake3::Hasher::new();
            hash.update(b"kit-structure-subgraph-v2\0");
            hash.update(&node.structural_digest);
            for (edge, neighbor) in adjacency.get(&node.id).into_iter().flatten() {
                hash.update(edge);
                hash.update(&structural[neighbor]);
            }
            node.subgraph_digest = *hash.finalize().as_bytes();
        }
        let coverage_output_bytes = self
            .coverage
            .len()
            .checked_mul(size_of::<CoverageRecord>())
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        self.reserve_staging(coverage_output_bytes)?;
        let coverage_set = std::mem::take(&mut self.coverage);
        let mut coverage = Vec::with_capacity(coverage_set.len());
        coverage.extend(coverage_set);
        self.staging.release(self.coverage_map_bytes);
        self.coverage_map_bytes = 0;
        self.reserve_staging(size_of::<StructureGraph>())?;
        let edges = self.edges;
        let options_digest = digest_options(self.options);
        let mut content = blake3::Hasher::new();
        content.update(b"kit-structure-content-v2\0");
        for node in &nodes {
            check_deadline(self.deadline)?;
            content.update(&node.structural_digest);
        }
        for edge in &edges {
            check_deadline(self.deadline)?;
            content.update(&edge.structural_digest);
        }
        for item in &coverage {
            check_deadline(self.deadline)?;
            digest_coverage(&mut content, item);
        }
        let content_digest = *content.finalize().as_bytes();
        let mut snapshot = blake3::Hasher::new();
        snapshot.update(b"kit-structure-snapshot-v2\0");
        snapshot.update(self.index.revision().as_bytes());
        snapshot.update(self.index.index_digest());
        snapshot.update(&options_digest);
        snapshot.update(&evidence_digest);
        snapshot.update(&content_digest);
        let snapshot_digest = *snapshot.finalize().as_bytes();
        let logical_bytes = structure_graph_logical_weight(&nodes, &edges, &coverage)?;
        check_deadline(self.deadline)?;
        self.staging.check()?;
        self.metrics.consumed_work = self.work;
        self.metrics.peak_staging_bytes = self.staging.peak();
        let graph = StructureGraph {
            revision: self.index.revision(),
            nodes,
            edges,
            coverage,
            content_digest,
            snapshot_digest,
            index_digest: *self.index.index_digest(),
            options_digest,
            logical_bytes,
        };
        Ok((
            graph,
            self.cache,
            self.cache_bytes,
            self.path_digests,
            self.metrics,
            self.clock,
        ))
    }

    fn evict(&mut self) -> Result<(), GraphError> {
        if self.cache.len() <= self.options.max_cache_entries
            && self.cache_bytes <= self.options.max_cache_bytes
        {
            return Ok(());
        }
        self.step(self.cache.len())?;
        let mut candidates = self
            .cache
            .iter()
            .filter(|(key, _)| !self.protected.contains(key))
            .map(|(key, item)| (item.last_used, *key))
            .collect::<Vec<_>>();
        self.charge_sort(candidates.len())?;
        candidates.sort();
        self.check_deadline()?;
        for (_, key) in candidates {
            if self.cache.len() <= self.options.max_cache_entries
                && self.cache_bytes <= self.options.max_cache_bytes
            {
                break;
            }
            self.step(1)?;
            let removed = self.cache.remove(&key).expect("eviction candidate");
            self.cache_bytes = self.cache_bytes.saturating_sub(removed.logical_bytes);
            if let Some(staged) = self.cache_staging.remove(&key) {
                self.staging.release(staged);
            }
            self.metrics.evicted_fragments += 1;
        }
        if self.cache.len() > self.options.max_cache_entries {
            Err(GraphError::BoundExceeded(GraphBound::CacheEntries))
        } else if self.cache_bytes > self.options.max_cache_bytes {
            Err(GraphError::BoundExceeded(GraphBound::CacheBytes))
        } else {
            Ok(())
        }
    }

    fn insert_node(
        &mut self,
        kind: NodeKind,
        id: NodeId,
        name: impl Into<String>,
        path: Option<RootRelativePath>,
        range: Option<GraphRange>,
    ) -> Result<(), GraphError> {
        if self.nodes.contains_key(&id) {
            return Err(GraphError::InvalidIndex("duplicate graph node id"));
        }
        if self.nodes.len() == self.options.max_nodes {
            return Err(GraphError::BoundExceeded(GraphBound::Nodes));
        }
        let name = name.into();
        let map_weight = size_of::<NodeId>()
            .checked_add(size_of::<GraphNode>() + BTREE_ENTRY_WEIGHT)
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        let node_weight = map_weight
            .checked_add(name.capacity())
            .and_then(|weight| {
                weight.checked_add(path.as_ref().map_or(0, |path| path.as_str().len()))
            })
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        self.reserve_staging(node_weight)?;
        self.node_map_bytes = self
            .node_map_bytes
            .checked_add(map_weight)
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        let structural_digest = digest_node(kind, id, &name, path.as_ref(), range);
        self.nodes.insert(
            id,
            GraphNode {
                id,
                kind,
                name,
                path,
                range,
                revision: self.index.revision(),
                structural_digest,
                subgraph_digest: [0; 32],
            },
        );
        Ok(())
    }

    fn insert_coverage(&mut self, record: CoverageRecord) -> Result<(), GraphError> {
        if self.coverage.contains(&record) {
            return Ok(());
        }
        let weight = BTREE_ENTRY_WEIGHT + size_of::<CoverageRecord>();
        self.reserve_staging(weight)?;
        self.coverage_map_bytes = self
            .coverage_map_bytes
            .checked_add(weight)
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        self.coverage.insert(record);
        Ok(())
    }

    fn push_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        kind: EdgeKind,
        provenance: EdgeProvenance,
    ) -> Result<(), GraphError> {
        if provenance.revision != self.index.revision() {
            return Err(GraphError::StaleEvidence);
        }
        if self.edges.len() == self.options.max_edges {
            return Err(GraphError::BoundExceeded(GraphBound::Edges));
        }
        if !(1..=1_000).contains(&provenance.confidence_millis) {
            return Err(GraphError::InvalidEvidence(
                "edge confidence must be between 1 and 1000",
            ));
        }
        self.reserve_staging(
            size_of::<GraphEdge>()
                .checked_add(provenance_heap_weight(&provenance)?)
                .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?,
        )?;
        let structural_digest = digest_edge(source, target, kind, &provenance);
        self.edges.push(GraphEdge {
            source,
            target,
            kind,
            provenance,
            revision: self.index.revision(),
            structural_digest,
        });
        Ok(())
    }

    fn charge_sort(&mut self, count: usize) -> Result<(), GraphError> {
        let factor = usize::BITS as usize - count.max(1).leading_zeros() as usize;
        self.step(
            count
                .checked_mul(factor)
                .ok_or(GraphError::BoundExceeded(GraphBound::Work))?,
        )
    }

    fn graph_range(
        &self,
        entry: &MetadataEntry,
        start: usize,
        end: usize,
    ) -> Result<GraphRange, GraphError> {
        self.line_indices
            .get(&entry.path)
            .ok_or(GraphError::InvalidIndex("range source text is unavailable"))?
            .range(entry, start, end)
    }

    fn whole_range(&self, entry: &MetadataEntry) -> Result<GraphRange, GraphError> {
        self.whole_ranges
            .get(&entry.path)
            .copied()
            .ok_or(GraphError::InvalidIndex("whole-file range is unavailable"))
    }

    fn ensure_temporary(&self, amount: usize) -> Result<StagingReservation, GraphError> {
        self.staging.reserve(amount)
    }

    fn reserve_staging(&mut self, amount: usize) -> Result<(), GraphError> {
        self.staging.add(amount)
    }

    fn step(&mut self, amount: usize) -> Result<(), GraphError> {
        self.work = self
            .work
            .checked_add(amount)
            .filter(|work| *work <= self.options.max_work)
            .ok_or(GraphError::BoundExceeded(GraphBound::Work))?;
        self.check_deadline()
    }

    fn check_deadline(&self) -> Result<(), GraphError> {
        check_deadline(self.deadline)
    }
}

fn parse_manifest(
    path: &Path,
    source: &str,
    options: &GraphOptions,
    max_work: usize,
    deadline: Instant,
) -> Result<(ManifestModel, usize), GraphError> {
    let mut work = preflight_toml(source, options, max_work, deadline)?;
    check_deadline(deadline)?;
    let value: toml::Value =
        toml::from_str(source).map_err(|error| GraphError::MalformedManifest {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    check_deadline(deadline)?;
    validate_toml_value(&value, 0, &mut work, options, deadline)?;
    let table = value
        .as_table()
        .ok_or_else(|| manifest_error(path, "manifest root must be a table"))?;
    let package_table = table
        .get("package")
        .map(|value| {
            value
                .as_table()
                .ok_or_else(|| manifest_error(path, "package must be a table"))
        })
        .transpose()?;
    let package = package_table
        .and_then(|table| table.get("name"))
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| manifest_error(path, "package.name must be a string"))
        })
        .transpose()?;
    let edition = package_table
        .and_then(|table| table.get("edition"))
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| manifest_error(path, "package.edition must be a string"))?
                .parse::<u16>()
                .map_err(|_| manifest_error(path, "package.edition is invalid"))
        })
        .transpose()?
        .unwrap_or(2015);
    if !matches!(edition, 2015 | 2018 | 2021 | 2024) {
        return Err(manifest_error(path, "package.edition is unsupported"));
    }
    let has_explicit_target = ["lib", "bin", "test", "example", "bench"]
        .iter()
        .any(|key| table.contains_key(*key));
    let auto_default = edition != 2015 || !has_explicit_target;
    let auto_lib = optional_bool(path, package_table, "autolib")?.unwrap_or(auto_default);
    let auto_bins = optional_bool(path, package_table, "autobins")?.unwrap_or(auto_default);
    let auto_examples = optional_bool(path, package_table, "autoexamples")?.unwrap_or(auto_default);
    let auto_tests = optional_bool(path, package_table, "autotests")?.unwrap_or(auto_default);
    let auto_benches = optional_bool(path, package_table, "autobenches")?.unwrap_or(auto_default);
    let workspace = table
        .get("workspace")
        .map(|value| {
            value
                .as_table()
                .ok_or_else(|| manifest_error(path, "workspace must be a table"))
        })
        .transpose()?;
    let members = string_array(
        path,
        workspace,
        "members",
        options.max_member_patterns,
        GraphBound::MemberPatterns,
    )?;
    let excludes = string_array(
        path,
        workspace,
        "exclude",
        options.max_member_patterns,
        GraphBound::MemberPatterns,
    )?;
    if members.len().saturating_add(excludes.len()) > options.max_member_patterns {
        return Err(GraphError::BoundExceeded(GraphBound::MemberPatterns));
    }
    let mut targets = Vec::new();
    if let Some(value) = table.get("lib") {
        targets.push(parse_target(path, value, TargetKind::Library)?);
    }
    for (key, kind) in [
        ("bin", TargetKind::Binary),
        ("test", TargetKind::Test),
        ("example", TargetKind::Example),
        ("bench", TargetKind::Bench),
    ] {
        let Some(value) = table.get(key) else {
            continue;
        };
        let array = value
            .as_array()
            .ok_or_else(|| manifest_error(path, "target section must be an array of tables"))?;
        if targets.len().saturating_add(array.len()) > options.max_targets_per_manifest {
            return Err(GraphError::BoundExceeded(GraphBound::Targets));
        }
        for item in array {
            targets.push(parse_target(path, item, kind)?);
        }
    }
    let mut dependencies = Vec::new();
    collect_dependencies(
        path,
        table,
        &mut dependencies,
        options.max_dependencies_per_manifest,
    )?;
    if let Some(targets_table) = table.get("target").and_then(toml::Value::as_table) {
        for target in targets_table.values() {
            let target = target
                .as_table()
                .ok_or_else(|| manifest_error(path, "target configuration must be a table"))?;
            collect_dependencies(
                path,
                target,
                &mut dependencies,
                options.max_dependencies_per_manifest,
            )?;
        }
    }
    let workspace_dependencies =
        parse_workspace_dependencies(path, workspace, options.max_workspace_dependencies)?;
    let has_path_overrides = table.contains_key("patch") || table.contains_key("replace");
    check_deadline(deadline)?;
    Ok((
        ManifestModel {
            digest: *blake3::hash(source.as_bytes()).as_bytes(),
            package,
            auto_lib,
            auto_bins,
            auto_examples,
            auto_tests,
            auto_benches,
            members,
            excludes,
            targets,
            dependencies,
            workspace_dependencies,
            has_path_overrides,
        },
        work,
    ))
}

fn preflight_toml(
    source: &str,
    options: &GraphOptions,
    max_work: usize,
    deadline: Instant,
) -> Result<usize, GraphError> {
    let parser_limit = options.max_manifest_input_bytes.min(MAX_TOML_PARSER_INPUT);
    if source.len() > parser_limit {
        return Err(GraphError::BoundExceeded(GraphBound::ManifestInputBytes));
    }
    if source.len() > max_work {
        return Err(GraphError::BoundExceeded(GraphBound::Work));
    }
    preflight_toml_structure(source, options, deadline)?;
    let bytes = source.as_bytes();
    let mut depth = 0_usize;
    let mut items = 0_usize;
    let mut string_bytes = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    let mut comment = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if index.is_multiple_of(4096) {
            check_deadline(deadline)?;
        }
        if comment {
            if byte == b'\n' {
                comment = false;
            }
            continue;
        }
        if let Some(delimiter) = quote {
            if delimiter == b'"' && !escaped && byte == b'\\' {
                escaped = true;
                continue;
            }
            if !escaped && byte == delimiter {
                quote = None;
            } else {
                string_bytes = string_bytes
                    .checked_add(1)
                    .ok_or(GraphError::BoundExceeded(GraphBound::TomlStringBytes))?;
                if string_bytes > options.max_toml_string_bytes {
                    return Err(GraphError::BoundExceeded(GraphBound::TomlStringBytes));
                }
            }
            escaped = false;
            continue;
        }
        match byte {
            b'#' => comment = true,
            b'\'' | b'"' => {
                preflight_toml_item(&mut items, options)?;
                quote = Some(byte);
                string_bytes = 0;
            }
            b'[' | b'{' => {
                preflight_toml_item(&mut items, options)?;
                depth += 1;
                if depth > options.max_toml_nesting {
                    return Err(GraphError::BoundExceeded(GraphBound::TomlNesting));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            b'=' | b',' | b'.' => preflight_toml_item(&mut items, options)?,
            _ => {}
        }
    }
    check_deadline(deadline)?;
    Ok(source.len())
}

fn preflight_toml_structure(
    source: &str,
    options: &GraphOptions,
    deadline: Instant,
) -> Result<(), GraphError> {
    let mut table_depth = 0_usize;
    let mut targets = 0_usize;
    let mut dependencies = 0_usize;
    let mut workspace_dependencies = 0_usize;
    let mut current_table = Vec::<String>::new();
    let mut multiline = None;
    for (line_index, raw_line) in source.lines().enumerate() {
        if line_index.is_multiple_of(256) {
            check_deadline(deadline)?;
        }
        if let Some(delimiter) = multiline {
            if raw_line.contains(delimiter) {
                multiline = None;
            }
            continue;
        }
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        multiline = if line.matches("\"\"\"").count() % 2 == 1 {
            Some("\"\"\"")
        } else if line.matches("'''").count() % 2 == 1 {
            Some("'''")
        } else {
            None
        };
        let array_header = line.starts_with("[[");
        let header = if array_header {
            line.strip_prefix("[[")
                .and_then(|line| line.strip_suffix("]]"))
        } else {
            line.strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
        };
        if let Some(header) = header {
            let components = toml_path_components(header);
            table_depth = components.len();
            current_table = components
                .iter()
                .map(|component| (*component).to_owned())
                .collect();
            check_toml_depth(table_depth, options)?;
            if array_header {
                match components.as_slice() {
                    ["bin" | "test" | "example" | "bench"] => targets += 1,
                    [
                        "dependencies" | "dev-dependencies" | "build-dependencies",
                        ..,
                    ] if components.len() >= 2 => {
                        dependencies += 1;
                    }
                    [
                        "target",
                        ..,
                        "dependencies" | "dev-dependencies" | "build-dependencies",
                        _,
                    ] if components.len() >= 4 => {
                        dependencies += 1;
                    }
                    ["workspace", "dependencies", ..] if components.len() >= 3 => {
                        workspace_dependencies += 1;
                    }
                    _ => {}
                }
            }
            if targets > options.max_targets_per_manifest {
                return Err(GraphError::BoundExceeded(GraphBound::Targets));
            }
            if dependencies > options.max_dependencies_per_manifest {
                return Err(GraphError::BoundExceeded(GraphBound::Dependencies));
            }
            if workspace_dependencies > options.max_workspace_dependencies {
                return Err(GraphError::BoundExceeded(GraphBound::WorkspaceDependencies));
            }
            continue;
        }
        if let Some((key, _)) = split_toml_assignment(line) {
            let key_components = toml_path_components(key);
            check_toml_depth(table_depth.saturating_add(key_components.len()), options)?;
            let mut assignment = current_table.clone();
            assignment.extend(key_components.into_iter().map(str::to_owned));
            if let Some(bound) = assignment
                .get(..assignment.len().saturating_sub(1))
                .and_then(dependency_table_bound)
            {
                match bound {
                    GraphBound::Dependencies => {
                        increment_preflight_count(
                            &mut dependencies,
                            options.max_dependencies_per_manifest,
                            bound,
                        )?;
                    }
                    GraphBound::WorkspaceDependencies => {
                        increment_preflight_count(
                            &mut workspace_dependencies,
                            options.max_workspace_dependencies,
                            bound,
                        )?;
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
    check_deadline(deadline)
}

fn dependency_table_bound(components: &[String]) -> Option<GraphBound> {
    let dependency = |value: &str| {
        matches!(
            value,
            "dependencies" | "dev-dependencies" | "build-dependencies"
        )
    };
    if components.len() == 1 && dependency(&components[0])
        || components.len() == 3 && components[0] == "target" && dependency(&components[2])
    {
        Some(GraphBound::Dependencies)
    } else if components.len() == 2
        && components[0] == "workspace"
        && components[1] == "dependencies"
    {
        Some(GraphBound::WorkspaceDependencies)
    } else {
        None
    }
}

fn increment_preflight_count(
    count: &mut usize,
    limit: usize,
    bound: GraphBound,
) -> Result<(), GraphError> {
    *count = count
        .checked_add(1)
        .ok_or(GraphError::BoundExceeded(bound))?;
    if *count > limit {
        Err(GraphError::BoundExceeded(bound))
    } else {
        Ok(())
    }
}

fn strip_toml_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in line.bytes().enumerate() {
        if let Some(delimiter) = quote {
            if delimiter == b'"' && !escaped && byte == b'\\' {
                escaped = true;
                continue;
            }
            if !escaped && byte == delimiter {
                quote = None;
            }
            escaped = false;
        } else if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
        } else if byte == b'#' {
            return &line[..index];
        }
    }
    line
}

fn split_toml_assignment(line: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    for (index, byte) in line.bytes().enumerate() {
        if quote == Some(byte) {
            quote = None;
        } else if quote.is_none() && (byte == b'\'' || byte == b'"') {
            quote = Some(byte);
        } else if quote.is_none() && byte == b'=' {
            return Some((&line[..index], &line[index + 1..]));
        }
    }
    None
}

fn toml_path_components(path: &str) -> Vec<&str> {
    let mut output = Vec::new();
    let mut quote = None;
    let mut start = 0;
    for (index, byte) in path.bytes().enumerate() {
        if quote == Some(byte) {
            quote = None;
        } else if quote.is_none() && (byte == b'\'' || byte == b'"') {
            quote = Some(byte);
        } else if quote.is_none() && byte == b'.' {
            let component = path[start..index].trim().trim_matches(['\'', '"']);
            if !component.is_empty() {
                output.push(component);
            }
            start = index + 1;
        }
    }
    let component = path[start..].trim().trim_matches(['\'', '"']);
    if !component.is_empty() {
        output.push(component);
    }
    output
}

fn check_toml_depth(depth: usize, options: &GraphOptions) -> Result<(), GraphError> {
    if depth > options.max_toml_nesting {
        Err(GraphError::BoundExceeded(GraphBound::TomlNesting))
    } else {
        Ok(())
    }
}

fn preflight_toml_item(items: &mut usize, options: &GraphOptions) -> Result<(), GraphError> {
    *items = items
        .checked_add(1)
        .ok_or(GraphError::BoundExceeded(GraphBound::TomlItems))?;
    if *items > options.max_toml_items {
        Err(GraphError::BoundExceeded(GraphBound::TomlItems))
    } else {
        Ok(())
    }
}

fn validate_toml_value(
    value: &toml::Value,
    depth: usize,
    work: &mut usize,
    options: &GraphOptions,
    deadline: Instant,
) -> Result<(), GraphError> {
    check_deadline(deadline)?;
    if depth > options.max_toml_nesting {
        return Err(GraphError::BoundExceeded(GraphBound::TomlNesting));
    }
    *work = work
        .checked_add(1)
        .ok_or(GraphError::BoundExceeded(GraphBound::TomlItems))?;
    if *work > options.max_toml_items {
        return Err(GraphError::BoundExceeded(GraphBound::TomlItems));
    }
    match value {
        toml::Value::String(value) if value.len() > options.max_toml_string_bytes => {
            Err(GraphError::BoundExceeded(GraphBound::TomlStringBytes))
        }
        toml::Value::Array(values) => {
            for value in values {
                validate_toml_value(value, depth + 1, work, options, deadline)?;
            }
            Ok(())
        }
        toml::Value::Table(values) => {
            for (key, value) in values {
                if key.len() > options.max_toml_string_bytes {
                    return Err(GraphError::BoundExceeded(GraphBound::TomlStringBytes));
                }
                validate_toml_value(value, depth + 1, work, options, deadline)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn optional_bool(
    path: &Path,
    table: Option<&toml::Table>,
    key: &str,
) -> Result<Option<bool>, GraphError> {
    table
        .and_then(|table| table.get(key))
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| manifest_error(path, "automatic target setting must be a boolean"))
        })
        .transpose()
}

fn string_array(
    path: &Path,
    table: Option<&toml::Table>,
    key: &str,
    limit: usize,
    bound: GraphBound,
) -> Result<Vec<String>, GraphError> {
    let Some(value) = table.and_then(|table| table.get(key)) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| manifest_error(path, "workspace pattern setting must be an array"))?;
    if values.len() > limit {
        return Err(GraphError::BoundExceeded(bound));
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| manifest_error(path, "workspace patterns must be strings"))
        })
        .collect()
}

fn parse_target(
    path: &Path,
    value: &toml::Value,
    kind: TargetKind,
) -> Result<TargetSpec, GraphError> {
    let table = value
        .as_table()
        .ok_or_else(|| manifest_error(path, "target must be a table"))?;
    let name = optional_string(path, table, "name")?;
    if kind != TargetKind::Library && name.as_deref().is_none_or(str::is_empty) {
        return Err(manifest_error(
            path,
            "explicit non-library target requires name",
        ));
    }
    let target_path = optional_string(path, table, "path")?.map(PathBuf::from);
    if let Some(value) = &target_path {
        validate_relative(value)?;
    }
    let default_test = matches!(
        kind,
        TargetKind::Library | TargetKind::Binary | TargetKind::Test
    );
    let test = table
        .get("test")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| manifest_error(path, "target test must be a boolean"))
        })
        .transpose()?
        .unwrap_or(default_test);
    let harness = table
        .get("harness")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| manifest_error(path, "target harness must be a boolean"))
        })
        .transpose()?
        .unwrap_or(true);
    let required_features = match table.get("required-features") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| manifest_error(path, "required-features must be an array"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| manifest_error(path, "required-features must contain strings"))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(TargetSpec {
        kind,
        name,
        path: target_path,
        test,
        harness,
        required_features,
    })
}

fn optional_string(
    path: &Path,
    table: &toml::Table,
    key: &str,
) -> Result<Option<String>, GraphError> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| manifest_error(path, "manifest value must be a string"))
        })
        .transpose()
}

fn collect_dependencies(
    path: &Path,
    table: &toml::Table,
    output: &mut Vec<DependencySpec>,
    limit: usize,
) -> Result<(), GraphError> {
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(dependencies) = table.get(section) else {
            continue;
        };
        let dependencies = dependencies
            .as_table()
            .ok_or_else(|| manifest_error(path, "dependency section must be a table"))?;
        if output.len().saturating_add(dependencies.len()) > limit {
            return Err(GraphError::BoundExceeded(GraphBound::Dependencies));
        }
        for (name, value) in dependencies {
            let Some(table) = value.as_table() else {
                continue;
            };
            let dependency_path = optional_string(path, table, "path")?.map(PathBuf::from);
            if let Some(value) = &dependency_path {
                validate_relative_dependency(value)?;
            }
            let workspace = table
                .get("workspace")
                .map(|value| {
                    value.as_bool().ok_or_else(|| {
                        manifest_error(path, "dependency workspace must be a boolean")
                    })
                })
                .transpose()?
                .unwrap_or(false);
            if workspace && dependency_path.is_some() {
                return Err(manifest_error(
                    path,
                    "workspace dependency may not also declare path",
                ));
            }
            output.push(DependencySpec {
                section,
                key: name.clone(),
                package: optional_string(path, table, "package")?,
                path: dependency_path,
                workspace,
            });
        }
    }
    Ok(())
}

fn parse_workspace_dependencies(
    path: &Path,
    workspace: Option<&toml::Table>,
    limit: usize,
) -> Result<BTreeMap<String, WorkspaceDependency>, GraphError> {
    let Some(value) = workspace.and_then(|table| table.get("dependencies")) else {
        return Ok(BTreeMap::new());
    };
    let dependencies = value
        .as_table()
        .ok_or_else(|| manifest_error(path, "workspace.dependencies must be a table"))?;
    if dependencies.len() > limit {
        return Err(GraphError::BoundExceeded(GraphBound::WorkspaceDependencies));
    }
    let mut output = BTreeMap::new();
    for (name, value) in dependencies {
        let Some(table) = value.as_table() else {
            output.insert(
                name.clone(),
                WorkspaceDependency {
                    package: None,
                    path: None,
                },
            );
            continue;
        };
        let dependency_path = optional_string(path, table, "path")?.map(PathBuf::from);
        if let Some(value) = &dependency_path {
            validate_relative_dependency(value)?;
        }
        output.insert(
            name.clone(),
            WorkspaceDependency {
                package: optional_string(path, table, "package")?,
                path: dependency_path,
            },
        );
    }
    Ok(output)
}

fn rust_model(
    entry: &MetadataEntry,
    source: &str,
    max_work: usize,
    deadline: Instant,
) -> Result<(RustModel, usize), GraphError> {
    let mut tests = BTreeMap::new();
    let mut modules = Vec::new();
    let (attribute_spans, attributes_complete) = scan_rust_attributes(source, deadline)?;
    let mut work = 0_usize;
    let mut complete =
        !entry.syntax_has_parse_errors && !entry.syntax_truncated && attributes_complete;
    let mut tests_complete = complete;
    let first_item = entry
        .syntax_records
        .iter()
        .map(|record| record.range().start_byte)
        .min()
        .unwrap_or(source.len());
    let crate_attributes = attribute_spans
        .iter()
        .filter(|attribute| attribute.inner && attribute.start < first_item)
        .map(|attribute| &source[attribute.content_start..attribute.content_end])
        .collect::<Vec<_>>();
    let crate_state = cfg_state(&crate_attributes, &mut work, max_work, deadline)?;
    let mut attributes = BTreeMap::<[u8; 32], Vec<&str>>::new();
    let mut local_states = BTreeMap::new();
    let mut parents = BTreeMap::new();
    for record in entry.syntax_records.iter() {
        check_deadline(deadline)?;
        let (attrs, inspected) = item_attributes(
            source,
            &attribute_spans,
            record.range().start_byte,
            record.range().end_byte,
        );
        charge_local_work(&mut work, inspected + 1, max_work, deadline)?;
        local_states.insert(
            record.declaration_id(),
            cfg_state(&attrs, &mut work, max_work, deadline)?,
        );
        parents.insert(
            record.declaration_id(),
            record.enclosing_symbol().map(|parent| *parent.value()),
        );
        attributes.insert(record.declaration_id(), attrs);
    }
    let mut states = BTreeMap::new();
    {
        let mut budget = LocalWork {
            consumed: &mut work,
            max: max_work,
            deadline,
        };
        for record in entry.syntax_records.iter() {
            resolve_cfg_state(
                record.declaration_id(),
                crate_state,
                &local_states,
                &parents,
                &mut states,
                &mut budget,
            )?;
        }
    }
    for record in entry.syntax_records.iter() {
        check_deadline(deadline)?;
        let attrs = &attributes[&record.declaration_id()];
        let mut state = states[&record.declaration_id()];
        let mut exact_test = false;
        let mut unresolved_test = false;
        for attribute in attrs {
            let parsed = parse_rust_attribute(attribute, &mut work, max_work, deadline)?;
            exact_test |= parsed.test == TestAttribute::Exact;
            unresolved_test |= parsed.test == TestAttribute::Unresolved || !parsed.exact;
        }
        if unresolved_test {
            state = combine_test_state(state, TestState::Unresolved);
        }
        if *record.kind().value() == SyntacticSymbolKind::Function
            && (exact_test || unresolved_test)
        {
            tests.insert(record.declaration_id(), state);
            tests_complete &= state != TestState::Unresolved;
        }
        if *record.kind().value() != SyntacticSymbolKind::Module {
            continue;
        }
        let declaration = source
            .get(record.range().start_byte..record.range().end_byte)
            .ok_or(GraphError::InvalidIndex(
                "module declaration range is invalid",
            ))?
            .trim();
        if !declaration.ends_with(';') {
            continue;
        }
        let explicit = attrs
            .iter()
            .find_map(|attribute| parse_path_attribute(attribute));
        if attrs.iter().any(|attribute| {
            compact_attribute(attribute).is_some_and(|attribute| attribute.starts_with("path="))
        }) && explicit.is_none()
        {
            complete = false;
            continue;
        }
        let mut qualified = record
            .qualified_name()
            .value()
            .split("::")
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let name = qualified
            .pop()
            .unwrap_or_else(|| record.display_name().value().to_string());
        complete &= state != TestState::Unresolved;
        modules.push(ExternalModule {
            inline_ancestors: qualified,
            name,
            path: explicit,
            state,
        });
    }
    Ok((
        RustModel {
            digest: entry.source_digest().ok_or(GraphError::InvalidIndex(
                "retained Rust source has no source digest",
            ))?,
            tests,
            modules,
            complete,
            tests_complete,
        },
        work,
    ))
}

#[derive(Clone, Copy)]
struct RustAttributeSpan {
    start: usize,
    content_start: usize,
    content_end: usize,
    end: usize,
    inner: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TestAttribute {
    None,
    Exact,
    Unresolved,
}

struct ParsedRustAttribute {
    state: TestState,
    test: TestAttribute,
    exact: bool,
}

fn preflight_rust_attribute_staging(
    entry: &MetadataEntry,
    source: &str,
    deadline: Instant,
) -> Result<usize, GraphError> {
    let mut attributes = 0_usize;
    let mut depth = 0_usize;
    let mut max_depth = 0_usize;
    for (index, byte) in source.bytes().enumerate() {
        if index.is_multiple_of(4096) {
            check_deadline(deadline)?;
        }
        if byte == b'#' {
            attributes = attributes
                .checked_add(1)
                .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        }
        match byte {
            b'[' | b'(' | b'{' => {
                depth = depth
                    .checked_add(1)
                    .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
                max_depth = max_depth.max(depth);
            }
            b']' | b')' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    check_deadline(deadline)?;
    let record_map_weight = BTREE_ENTRY_WEIGHT
        + size_of::<[u8; 32]>()
        + size_of::<Vec<&str>>()
        + (BTREE_ENTRY_WEIGHT + size_of::<[u8; 32]>() + size_of::<TestState>()) * 3
        + BTREE_ENTRY_WEIGHT
        + size_of::<[u8; 32]>()
        + size_of::<Option<[u8; 32]>>()
        + 2 * size_of::<ExternalModule>();
    let qualified_name_bytes = entry
        .syntax_records
        .iter()
        .try_fold(0_usize, |total, record| {
            total.checked_add(record.qualified_name().value().len())
        });
    attributes
        .checked_mul(2 * (size_of::<RustAttributeSpan>() + size_of::<&str>()))
        .and_then(|bytes| bytes.checked_add(2 * max_depth))
        .and_then(|bytes| bytes.checked_add(2 * size_of::<Vec<RustAttributeSpan>>()))
        .and_then(|bytes| {
            bytes.checked_add(entry.syntax_records.len().checked_mul(record_map_weight)?)
        })
        .and_then(|bytes| bytes.checked_add(qualified_name_bytes?))
        .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))
}

fn scan_rust_attributes(
    source: &str,
    deadline: Instant,
) -> Result<(Vec<RustAttributeSpan>, bool), GraphError> {
    let bytes = source.as_bytes();
    let mut attributes = Vec::new();
    let mut index = 0;
    let mut complete = true;
    while index < bytes.len() {
        if index.is_multiple_of(4096) {
            check_deadline(deadline)?;
        }
        if bytes[index..].starts_with(b"//") {
            index = bytes[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| index + offset + 1);
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            let Some(end) = skip_block_comment(bytes, index, deadline)? else {
                complete = false;
                break;
            };
            index = end;
            continue;
        }
        if let Some(end) = skip_rust_literal(bytes, index, deadline)? {
            index = end;
            continue;
        }
        if bytes[index] != b'#' {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        let inner = bytes.get(index) == Some(&b'!');
        if inner {
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
        }
        if bytes.get(index) != Some(&b'[') {
            index = start + 1;
            continue;
        }
        let content_start = index + 1;
        let Some(end) = balanced_attribute_end(bytes, index, deadline)? else {
            complete = false;
            break;
        };
        attributes.push(RustAttributeSpan {
            start,
            content_start,
            content_end: end - 1,
            end,
            inner,
        });
        index = end;
    }
    check_deadline(deadline)?;
    Ok((attributes, complete))
}

fn balanced_attribute_end(
    bytes: &[u8],
    start: usize,
    deadline: Instant,
) -> Result<Option<usize>, GraphError> {
    let mut stack = vec![b']'];
    let mut index = start + 1;
    while index < bytes.len() {
        if index.is_multiple_of(4096) {
            check_deadline(deadline)?;
        }
        if bytes[index..].starts_with(b"//") {
            index = bytes[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| index + offset + 1);
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            let Some(end) = skip_block_comment(bytes, index, deadline)? else {
                return Ok(None);
            };
            index = end;
            continue;
        }
        if let Some(end) = skip_rust_literal(bytes, index, deadline)? {
            index = end;
            continue;
        }
        match bytes[index] {
            b'[' => stack.push(b']'),
            b'(' => stack.push(b')'),
            b'{' => stack.push(b'}'),
            byte if stack.last() == Some(&byte) => {
                stack.pop();
                if stack.is_empty() {
                    return Ok(Some(index + 1));
                }
            }
            _ => {}
        }
        index += 1;
    }
    Ok(None)
}

fn skip_block_comment(
    bytes: &[u8],
    start: usize,
    deadline: Instant,
) -> Result<Option<usize>, GraphError> {
    let mut depth = 1_usize;
    let mut index = start + 2;
    while index < bytes.len() {
        if index.is_multiple_of(4096) {
            check_deadline(deadline)?;
        }
        if bytes[index..].starts_with(b"/*") {
            depth += 1;
            index += 2;
        } else if bytes[index..].starts_with(b"*/") {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return Ok(Some(index));
            }
        } else {
            index += 1;
        }
    }
    Ok(None)
}

fn skip_rust_literal(
    bytes: &[u8],
    start: usize,
    deadline: Instant,
) -> Result<Option<usize>, GraphError> {
    let mut prefix = start;
    if bytes.get(prefix) == Some(&b'b') {
        prefix += 1;
    }
    if bytes.get(prefix) == Some(&b'r') {
        prefix += 1;
        let hashes = bytes[prefix..]
            .iter()
            .take_while(|byte| **byte == b'#')
            .count();
        prefix += hashes;
        if bytes.get(prefix) == Some(&b'"') {
            let suffix = &bytes[prefix + 1..];
            for (offset, byte) in suffix.iter().enumerate() {
                if offset.is_multiple_of(4096) {
                    check_deadline(deadline)?;
                }
                if *byte == b'"'
                    && suffix
                        .get(offset + 1..offset + 1 + hashes)
                        .is_some_and(|value| value.iter().all(|byte| *byte == b'#'))
                {
                    return Ok(Some(prefix + offset + hashes + 2));
                }
            }
            return Ok(Some(bytes.len()));
        }
    }
    let Some(&quote) = bytes.get(prefix) else {
        return Ok(None);
    };
    if quote != b'"' && quote != b'\'' {
        return Ok(None);
    }
    let mut escaped = false;
    for (index, &byte) in bytes.iter().enumerate().skip(prefix + 1) {
        if index.is_multiple_of(4096) {
            check_deadline(deadline)?;
        }
        if quote == b'\'' && byte == b'\n' {
            return Ok(None);
        }
        if !escaped && byte == quote {
            return Ok(Some(index + 1));
        }
        escaped = !escaped && byte == b'\\';
        if byte != b'\\' {
            escaped = false;
        }
    }
    Ok((quote == b'"').then_some(bytes.len()))
}

fn resolve_cfg_state(
    id: [u8; 32],
    crate_state: TestState,
    local: &BTreeMap<[u8; 32], TestState>,
    parents: &BTreeMap<[u8; 32], Option<[u8; 32]>>,
    resolved: &mut BTreeMap<[u8; 32], TestState>,
    budget: &mut LocalWork<'_>,
) -> Result<TestState, GraphError> {
    if let Some(state) = resolved.get(&id) {
        return Ok(*state);
    }
    let mut current = id;
    let mut path = Vec::new();
    let mut visiting = BTreeSet::new();
    let parent_state = loop {
        budget.charge(1)?;
        if let Some(state) = resolved.get(&current) {
            break *state;
        }
        if path.len() >= local.len() || !visiting.insert(current) {
            return Err(GraphError::InvalidIndex("cyclic enclosing symbol chain"));
        }
        let state = *local
            .get(&current)
            .ok_or(GraphError::InvalidIndex("enclosing symbol is missing"))?;
        path.push((current, state));
        match parents.get(&current) {
            Some(Some(parent)) => current = *parent,
            Some(None) => break crate_state,
            None => return Err(GraphError::InvalidIndex("enclosing symbol is missing")),
        }
    };
    let mut state = parent_state;
    while let Some((current, local)) = path.pop() {
        budget.charge(1)?;
        state = combine_test_state(state, local);
        resolved.insert(current, state);
    }
    Ok(*resolved.get(&id).unwrap_or(&state))
}

struct LocalWork<'a> {
    consumed: &'a mut usize,
    max: usize,
    deadline: Instant,
}

impl LocalWork<'_> {
    fn charge(&mut self, amount: usize) -> Result<(), GraphError> {
        charge_local_work(self.consumed, amount, self.max, self.deadline)
    }
}

fn combine_test_state(left: TestState, right: TestState) -> TestState {
    if left == TestState::Disabled || right == TestState::Disabled {
        TestState::Disabled
    } else if left == TestState::Unresolved || right == TestState::Unresolved {
        TestState::Unresolved
    } else {
        TestState::Exact
    }
}

fn item_attributes<'a>(
    source: &'a str,
    attributes: &[RustAttributeSpan],
    start: usize,
    end: usize,
) -> (Vec<&'a str>, usize) {
    let boundary = attributes.partition_point(|attribute| attribute.end <= start);
    let mut output = Vec::new();
    let mut cursor = start;
    let mut inspected = 0;
    for attribute in attributes[..boundary].iter().rev() {
        inspected += 1;
        if attribute.inner || !rust_trivia(&source[attribute.end..cursor]) {
            break;
        }
        output.push(&source[attribute.content_start..attribute.content_end]);
        cursor = attribute.start;
    }
    output.reverse();
    cursor = start;
    for attribute in attributes[boundary..]
        .iter()
        .take_while(|attribute| attribute.start < end)
    {
        inspected += 1;
        if attribute.inner || !rust_trivia(&source[cursor..attribute.start]) {
            break;
        }
        output.push(&source[attribute.content_start..attribute.content_end]);
        cursor = attribute.end;
    }
    (output, inspected)
}

fn rust_trivia(mut source: &str) -> bool {
    loop {
        source = source.trim_start_matches(char::is_whitespace);
        if let Some(comment) = source.strip_prefix("//") {
            source = comment.find('\n').map_or("", |end| &comment[end + 1..]);
        } else if let Some(comment) = source.strip_prefix("/*") {
            let Some(end) = comment.find("*/") else {
                return false;
            };
            source = &comment[end + 2..];
        } else {
            return source.is_empty();
        }
    }
}

fn cfg_state(
    attributes: &[&str],
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<TestState, GraphError> {
    let mut state = TestState::Exact;
    for attribute in attributes {
        state = combine_test_state(
            state,
            parse_rust_attribute(attribute, work, max_work, deadline)?.state,
        );
    }
    Ok(state)
}

fn parse_path_attribute(attribute: &str) -> Option<PathBuf> {
    let compact = compact_attribute(attribute)?;
    let value = compact.strip_prefix("path=\"")?.strip_suffix('"')?;
    if value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let path = PathBuf::from(value);
    validate_relative(&path).ok()?;
    Some(path)
}

fn parse_rust_attribute(
    attribute: &str,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<ParsedRustAttribute, GraphError> {
    let Some(attribute) = compact_attribute(attribute) else {
        return Ok(ParsedRustAttribute {
            state: TestState::Unresolved,
            test: TestAttribute::Unresolved,
            exact: false,
        });
    };
    if attribute == "test" {
        return Ok(ParsedRustAttribute {
            state: TestState::Exact,
            test: TestAttribute::Exact,
            exact: true,
        });
    }
    if inert_libtest_attribute(&attribute) {
        return Ok(ParsedRustAttribute {
            state: TestState::Exact,
            test: TestAttribute::None,
            exact: true,
        });
    }
    if let Some(expression) = attribute
        .strip_prefix("cfg(")
        .and_then(|value| value.strip_suffix(')'))
    {
        let state = match exact_cfg(expression, work, max_work, deadline)? {
            Some(true) => TestState::Exact,
            Some(false) => TestState::Disabled,
            None => TestState::Unresolved,
        };
        return Ok(ParsedRustAttribute {
            state,
            test: TestAttribute::None,
            exact: state != TestState::Unresolved,
        });
    }
    if let Some(arguments) = attribute
        .strip_prefix("cfg_attr(")
        .and_then(|value| value.strip_suffix(')'))
    {
        let Some(parts) = split_cfg_arguments(arguments, work, max_work, deadline)? else {
            return Ok(ParsedRustAttribute {
                state: TestState::Unresolved,
                test: TestAttribute::Unresolved,
                exact: false,
            });
        };
        if let [condition, "test"] = parts.as_slice() {
            return Ok(match exact_cfg(condition, work, max_work, deadline)? {
                Some(true) => ParsedRustAttribute {
                    state: TestState::Exact,
                    test: TestAttribute::Exact,
                    exact: true,
                },
                Some(false) => ParsedRustAttribute {
                    state: TestState::Exact,
                    test: TestAttribute::None,
                    exact: true,
                },
                None => ParsedRustAttribute {
                    state: TestState::Unresolved,
                    test: TestAttribute::Unresolved,
                    exact: false,
                },
            });
        }
    }
    if parse_path_attribute_compact(&attribute).is_some() {
        Ok(ParsedRustAttribute {
            state: TestState::Exact,
            test: TestAttribute::None,
            exact: true,
        })
    } else {
        Ok(ParsedRustAttribute {
            state: TestState::Unresolved,
            test: TestAttribute::Unresolved,
            exact: false,
        })
    }
}

fn compact_attribute(attribute: &str) -> Option<String> {
    if attribute.contains("//") || attribute.contains("/*") {
        return None;
    }
    Some(
        attribute
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .map(char::from)
            .collect(),
    )
}

fn parse_path_attribute_compact(attribute: &str) -> Option<&str> {
    attribute.strip_prefix("path=\"")?.strip_suffix('"')
}

fn inert_libtest_attribute(attribute: &str) -> bool {
    attribute == "ignore"
        || attribute == "should_panic"
        || attribute
            .strip_prefix("ignore=")
            .is_some_and(compact_string_literal)
        || attribute
            .strip_prefix("should_panic(expected=")
            .and_then(|value| value.strip_suffix(')'))
            .is_some_and(compact_string_literal)
}

fn compact_string_literal(value: &str) -> bool {
    if value.starts_with('"') {
        let mut escaped = false;
        for (index, byte) in value.bytes().enumerate().skip(1) {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                return index + 1 == value.len();
            }
        }
        return false;
    }
    let Some(raw) = value.strip_prefix('r') else {
        return false;
    };
    let hashes = raw.bytes().take_while(|byte| *byte == b'#').count();
    if raw.as_bytes().get(hashes) != Some(&b'"') {
        return false;
    }
    let delimiter = format!("\"{}", "#".repeat(hashes));
    raw[hashes + 1..]
        .find(&delimiter)
        .is_some_and(|index| hashes + 1 + index + delimiter.len() == raw.len())
}

#[derive(Clone, Copy)]
enum CfgOperator {
    Root,
    All,
    Any,
    Not,
}

struct CfgFrame {
    operator: CfgOperator,
    values: usize,
    result: Option<bool>,
}

fn exact_cfg(
    expression: &str,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<Option<bool>, GraphError> {
    let mut frames = vec![CfgFrame {
        operator: CfgOperator::Root,
        values: 0,
        result: None,
    }];
    let mut index = 0;
    let mut expect_value = true;
    while index < expression.len() {
        charge_local_work(work, 1, max_work, deadline)?;
        if expect_value {
            if expression.as_bytes()[index] == b')' {
                let Some(frame) = frames.pop() else {
                    return Ok(None);
                };
                if matches!(frame.operator, CfgOperator::Root | CfgOperator::Not)
                    && frame.values == 0
                {
                    return Ok(None);
                }
                let Some(parent) = frames.last_mut() else {
                    return Ok(None);
                };
                cfg_add_value(parent, cfg_frame_value(&frame))?;
                index += 1;
                expect_value = false;
                continue;
            }
            let operator = if expression[index..].starts_with("all(") {
                Some((CfgOperator::All, 4))
            } else if expression[index..].starts_with("any(") {
                Some((CfgOperator::Any, 4))
            } else if expression[index..].starts_with("not(") {
                Some((CfgOperator::Not, 4))
            } else {
                None
            };
            if let Some((operator, consumed)) = operator {
                if frames.len() >= MAX_CFG_DEPTH {
                    return Ok(None);
                }
                charge_local_work(work, consumed - 1, max_work, deadline)?;
                frames.push(CfgFrame {
                    operator,
                    values: 0,
                    result: None,
                });
                index += consumed;
                continue;
            }
            let start = index;
            let mut nested = 0_usize;
            while index < expression.len() {
                let byte = expression.as_bytes()[index];
                if nested == 0 && (byte == b',' || byte == b')') {
                    break;
                }
                match byte {
                    b'(' => {
                        nested += 1;
                        if nested >= MAX_CFG_DEPTH {
                            return Ok(None);
                        }
                    }
                    b')' => nested = nested.saturating_sub(1),
                    _ => {}
                }
                index += 1;
                charge_local_work(work, 1, max_work, deadline)?;
            }
            if start == index || nested != 0 {
                return Ok(None);
            }
            let value = (&expression[start..index] == "test").then_some(true);
            cfg_add_value(frames.last_mut().expect("root cfg frame"), value)?;
            expect_value = false;
            continue;
        }
        match expression.as_bytes()[index] {
            b',' => {
                if matches!(
                    frames.last().map(|frame| frame.operator),
                    Some(CfgOperator::Root)
                ) {
                    return Ok(None);
                }
                index += 1;
                expect_value = true;
            }
            b')' => {
                let Some(frame) = frames.pop() else {
                    return Ok(None);
                };
                if matches!(frame.operator, CfgOperator::Root) {
                    return Ok(None);
                }
                let Some(parent) = frames.last_mut() else {
                    return Ok(None);
                };
                cfg_add_value(parent, cfg_frame_value(&frame))?;
                index += 1;
            }
            _ => return Ok(None),
        }
    }
    if frames.len() != 1 || expect_value || frames[0].values != 1 {
        Ok(None)
    } else {
        Ok(frames[0].result)
    }
}

fn cfg_add_value(frame: &mut CfgFrame, value: Option<bool>) -> Result<(), GraphError> {
    frame.values = frame
        .values
        .checked_add(1)
        .ok_or(GraphError::BoundExceeded(GraphBound::Work))?;
    if matches!(frame.operator, CfgOperator::Root | CfgOperator::Not) && frame.values > 1 {
        frame.result = None;
        return Ok(());
    }
    frame.result = match frame.operator {
        CfgOperator::Root | CfgOperator::Not if frame.values == 1 => value,
        CfgOperator::All if frame.values == 1 => value,
        CfgOperator::Any if frame.values == 1 => value,
        CfgOperator::All => frame.result.zip(value).map(|(left, right)| left && right),
        CfgOperator::Any => frame.result.zip(value).map(|(left, right)| left || right),
        CfgOperator::Root | CfgOperator::Not => None,
    };
    Ok(())
}

fn cfg_frame_value(frame: &CfgFrame) -> Option<bool> {
    match frame.operator {
        CfgOperator::All if frame.values == 0 => Some(true),
        CfgOperator::Any if frame.values == 0 => Some(false),
        CfgOperator::Not if frame.values == 1 => frame.result.map(|value| !value),
        CfgOperator::All | CfgOperator::Any | CfgOperator::Root => frame.result,
        CfgOperator::Not => None,
    }
}

fn split_cfg_arguments<'a>(
    arguments: &'a str,
    work: &mut usize,
    max_work: usize,
    deadline: Instant,
) -> Result<Option<Vec<&'a str>>, GraphError> {
    let mut depth = 0_usize;
    let mut start = 0;
    let mut output = Vec::new();
    for (index, byte) in arguments.bytes().enumerate() {
        charge_local_work(work, 1, max_work, deadline)?;
        match byte {
            b'(' => {
                depth += 1;
                if depth >= MAX_CFG_DEPTH {
                    return Ok(None);
                }
            }
            b')' if depth == 0 => return Ok(None),
            b')' => depth -= 1,
            b',' if depth == 0 => {
                output.push(&arguments[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < arguments.len() {
        output.push(&arguments[start..]);
    }
    Ok((depth == 0).then_some(output))
}

fn charge_local_work(
    work: &mut usize,
    amount: usize,
    max_work: usize,
    deadline: Instant,
) -> Result<(), GraphError> {
    *work = work
        .checked_add(amount)
        .filter(|work| *work <= max_work)
        .ok_or(GraphError::BoundExceeded(GraphBound::Work))?;
    check_deadline(deadline)
}

fn discover_targets(
    package: &Package<'_>,
    entries: &BTreeMap<PathBuf, &MetadataEntry>,
    candidates: Vec<ResolvedTarget>,
) -> Result<Vec<ResolvedTarget>, GraphError> {
    let mut targets = BTreeMap::<PathBuf, ResolvedTarget>::new();
    for target in &package.model.targets {
        let relative = match &target.path {
            Some(path) => path.clone(),
            None => resolve_default_target_path(package, target, entries)?,
        };
        validate_relative(&relative)?;
        let candidate = package.root.join(&relative);
        if entries
            .get(&candidate)
            .is_none_or(|entry| entry.kind != EntryKind::File)
        {
            return Err(manifest_error(
                &package.manifest.path,
                &format!(
                    "explicit target {} is not an indexed file",
                    relative.display()
                ),
            ));
        }
        targets.insert(
            candidate.clone(),
            ResolvedTarget {
                path: candidate,
                name: target
                    .name
                    .clone()
                    .unwrap_or_else(|| default_target_name(package, target, &relative)),
                kind: target.kind,
                test: target.test,
                harness: target.harness,
                required_features: !target.required_features.is_empty(),
                explicit: true,
            },
        );
    }
    for (kind, enabled, relative) in [
        (TargetKind::Library, package.model.auto_lib, "src/lib.rs"),
        (TargetKind::Binary, package.model.auto_bins, "src/main.rs"),
    ] {
        if !enabled {
            continue;
        }
        let path = package.root.join(relative);
        if entries
            .get(&path)
            .is_some_and(|entry| entry.kind == EntryKind::File)
            && !targets.contains_key(&path)
        {
            targets.insert(
                path.clone(),
                ResolvedTarget {
                    path,
                    name: package.model.package.clone().unwrap_or_default(),
                    kind,
                    test: true,
                    harness: true,
                    required_features: false,
                    explicit: false,
                },
            );
        }
    }
    let mut conventional = BTreeMap::<(TargetKind, String), PathBuf>::new();
    for target in &candidates {
        let enabled = match target.kind {
            TargetKind::Binary => package.model.auto_bins,
            TargetKind::Example => package.model.auto_examples,
            TargetKind::Test => package.model.auto_tests,
            TargetKind::Bench => package.model.auto_benches,
            TargetKind::Library => package.model.auto_lib,
        };
        if enabled {
            let key = (target.kind, target.name.clone());
            if let Some(previous) = conventional.insert(key, target.path.clone())
                && previous != target.path
            {
                return Err(manifest_error(
                    &package.manifest.path,
                    &format!(
                        "automatic target {} is ambiguous between {} and {}",
                        target.name,
                        previous.display(),
                        target.path.display()
                    ),
                ));
            }
        }
    }
    for target in candidates {
        let enabled = match target.kind {
            TargetKind::Binary => package.model.auto_bins,
            TargetKind::Example => package.model.auto_examples,
            TargetKind::Test => package.model.auto_tests,
            TargetKind::Bench => package.model.auto_benches,
            TargetKind::Library => package.model.auto_lib,
        };
        if enabled
            && !targets
                .values()
                .any(|explicit| explicit.kind == target.kind && explicit.name == target.name)
        {
            targets.insert(target.path.clone(), target);
        }
    }
    Ok(targets.into_values().collect())
}

fn resolve_default_target_path(
    package: &Package<'_>,
    target: &TargetSpec,
    entries: &BTreeMap<PathBuf, &MetadataEntry>,
) -> Result<PathBuf, GraphError> {
    if target.kind == TargetKind::Library {
        let path = PathBuf::from("src/lib.rs");
        return entries
            .get(&package.root.join(&path))
            .is_some_and(|entry| entry.kind == EntryKind::File)
            .then_some(path)
            .ok_or_else(|| {
                manifest_error(
                    &package.manifest.path,
                    "explicit library target has no default source file",
                )
            });
    }
    let name = target.name.as_deref().expect("validated target name");
    let directory = match target.kind {
        TargetKind::Binary => "src/bin",
        TargetKind::Test => "tests",
        TargetKind::Example => "examples",
        TargetKind::Bench => "benches",
        TargetKind::Library => unreachable!(),
    };
    let file = PathBuf::from(directory).join(format!("{name}.rs"));
    let main = PathBuf::from(directory).join(name).join("main.rs");
    let mut candidates = Vec::with_capacity(3);
    if target.kind == TargetKind::Binary && Some(name) == package.model.package.as_deref() {
        candidates.push(PathBuf::from("src/main.rs"));
    }
    candidates.extend([file, main]);
    let mut present = candidates.into_iter().filter(|candidate| {
        entries
            .get(&package.root.join(candidate))
            .is_some_and(|entry| entry.kind == EntryKind::File)
    });
    let first = present.next();
    if present.next().is_some() {
        Err(manifest_error(
            &package.manifest.path,
            &format!("explicit target {name} has ambiguous default paths"),
        ))
    } else if let Some(path) = first {
        Ok(path)
    } else {
        Err(manifest_error(
            &package.manifest.path,
            &format!("explicit target {name} has no default source file"),
        ))
    }
}

fn default_target_name(package: &Package<'_>, target: &TargetSpec, relative: &Path) -> String {
    if matches!(target.kind, TargetKind::Library | TargetKind::Binary)
        && relative == Path::new("src/main.rs")
        || relative == Path::new("src/lib.rs")
    {
        package.model.package.clone().unwrap_or_default()
    } else {
        relative
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

fn conventional_target(root: &Path, path: &Path) -> Option<ResolvedTarget> {
    let relative = path.strip_prefix(root).ok()?;
    let components = relative
        .components()
        .map(|item| item.as_os_str())
        .collect::<Vec<_>>();
    let (kind, name, test) = if components.len() == 3
        && components[0] == "src"
        && components[1] == "bin"
        && path.extension().is_some_and(|ext| ext == "rs")
    {
        (
            TargetKind::Binary,
            path.file_stem()?.to_string_lossy().into_owned(),
            true,
        )
    } else if components.len() == 4
        && components[0] == "src"
        && components[1] == "bin"
        && components[3] == "main.rs"
    {
        (
            TargetKind::Binary,
            components[2].to_string_lossy().into_owned(),
            true,
        )
    } else if components.len() == 2
        && components[0] == "tests"
        && path.extension().is_some_and(|ext| ext == "rs")
    {
        (
            TargetKind::Test,
            path.file_stem()?.to_string_lossy().into_owned(),
            true,
        )
    } else if components.len() == 3 && components[0] == "tests" && components[2] == "main.rs" {
        (
            TargetKind::Test,
            components[1].to_string_lossy().into_owned(),
            true,
        )
    } else if components.len() == 2
        && components[0] == "examples"
        && path.extension().is_some_and(|ext| ext == "rs")
    {
        (
            TargetKind::Example,
            path.file_stem()?.to_string_lossy().into_owned(),
            false,
        )
    } else if components.len() == 3 && components[0] == "examples" && components[2] == "main.rs" {
        (
            TargetKind::Example,
            components[1].to_string_lossy().into_owned(),
            false,
        )
    } else if components.len() == 2
        && components[0] == "benches"
        && path.extension().is_some_and(|ext| ext == "rs")
    {
        (
            TargetKind::Bench,
            path.file_stem()?.to_string_lossy().into_owned(),
            false,
        )
    } else if components.len() == 3 && components[0] == "benches" && components[2] == "main.rs" {
        (
            TargetKind::Bench,
            components[1].to_string_lossy().into_owned(),
            false,
        )
    } else {
        return None;
    };
    Some(ResolvedTarget {
        path: path.to_path_buf(),
        name,
        kind,
        test,
        harness: true,
        required_features: false,
        explicit: false,
    })
}

fn deepest_owner(path: &Path, roots: &BTreeMap<PathBuf, usize>) -> Option<usize> {
    let mut candidate = if path.extension().is_some() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    loop {
        if let Some(owner) = roots.get(candidate) {
            return Some(*owner);
        }
        let Some(parent) = candidate.parent() else {
            break;
        };
        if parent == candidate {
            break;
        }
        candidate = parent;
    }
    roots.get(Path::new("")).copied()
}

fn module_base(path: &Path, root: bool) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    if root || path.file_name().is_some_and(|name| name == "mod.rs") {
        parent.to_path_buf()
    } else {
        parent.join(path.file_stem().unwrap_or_default())
    }
}

impl LineIndex {
    fn preflight(entry: &MetadataEntry, deadline: Instant) -> Result<(usize, usize), GraphError> {
        let text = entry
            .text()
            .ok_or(GraphError::InvalidIndex("range source text is unavailable"))?;
        if entry.size != text.len() as u64 {
            return Err(GraphError::InvalidIndex(
                "retained source size does not match metadata",
            ));
        }
        let mut lines = 1_usize;
        for (index, byte) in text.bytes().enumerate() {
            if index.is_multiple_of(4096) {
                check_deadline(deadline)?;
            }
            lines = lines
                .checked_add(usize::from(byte == b'\n'))
                .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        }
        let bytes = lines
            .checked_mul(size_of::<usize>())
            .and_then(|bytes| bytes.checked_add(size_of::<LineIndex>() + BTREE_ENTRY_WEIGHT))
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        Ok((bytes, lines))
    }

    fn new(entry: &MetadataEntry, capacity: usize, deadline: Instant) -> Result<Self, GraphError> {
        let text = entry
            .text()
            .ok_or(GraphError::InvalidIndex("range source text is unavailable"))?;
        check_deadline(deadline)?;
        let mut starts = Vec::with_capacity(capacity);
        starts.push(0);
        for (index, byte) in text.bytes().enumerate() {
            if index.is_multiple_of(4096) {
                check_deadline(deadline)?;
            }
            if byte == b'\n' {
                starts.push(index + 1);
            }
        }
        Ok(Self { starts })
    }

    fn range(
        &self,
        entry: &MetadataEntry,
        start: usize,
        end: usize,
    ) -> Result<GraphRange, GraphError> {
        let text = entry
            .text()
            .ok_or(GraphError::InvalidIndex("range source text is unavailable"))?;
        if entry.size != text.len() as u64
            || start > end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return Err(GraphError::InvalidIndex(
                "range is not on exact UTF-8 source boundaries",
            ));
        }
        Ok(GraphRange {
            start_byte: start,
            end_byte: end,
            start_line: self.starts.partition_point(|line| *line <= start),
            end_line: self.starts.partition_point(|line| *line <= end),
        })
    }
}

fn syntax_provenance(
    revision: RevisionId,
    entry: &MetadataEntry,
    record: &SyntacticSymbolRecord,
    range: GraphRange,
) -> Result<EdgeProvenance, GraphError> {
    Ok(EdgeProvenance {
        source: ProvenanceSource::TreeSitter,
        path: Some(root_path(&entry.path, usize::MAX)?),
        range,
        range_kind: RangeKind::Declaration,
        revision,
        confidence_millis: 1_000,
        semantic: None,
        evidence_digest: record.declaration_id(),
    })
}

fn manifest_provenance(
    revision: RevisionId,
    entry: &MetadataEntry,
    range: GraphRange,
) -> Result<EdgeProvenance, GraphError> {
    Ok(EdgeProvenance {
        source: ProvenanceSource::CargoManifest,
        path: Some(root_path(&entry.path, usize::MAX)?),
        range,
        range_kind: RangeKind::Manifest,
        revision,
        confidence_millis: 1_000,
        semantic: None,
        evidence_digest: entry
            .source_digest()
            .ok_or(GraphError::InvalidIndex("manifest digest is unavailable"))?,
    })
}

fn dependency_provenance(
    revision: RevisionId,
    entry: &MetadataEntry,
    range: GraphRange,
    dependency: &DependencySpec,
) -> Result<EdgeProvenance, GraphError> {
    let mut provenance = manifest_provenance(revision, entry, range)?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"cargo-dependency-v1\0");
    hash.update(&provenance.evidence_digest);
    frame(&mut hash, dependency.section.as_bytes());
    frame(&mut hash, dependency.key.as_bytes());
    frame(
        &mut hash,
        dependency.package.as_deref().unwrap_or("").as_bytes(),
    );
    frame(
        &mut hash,
        dependency
            .path
            .as_ref()
            .map_or(&[][..], |path| path.as_os_str().as_encoded_bytes()),
    );
    hash.update(&[u8::from(dependency.workspace)]);
    provenance.evidence_digest = *hash.finalize().as_bytes();
    Ok(provenance)
}

fn convention_provenance(
    revision: RevisionId,
    entry: &MetadataEntry,
    range: GraphRange,
) -> Result<EdgeProvenance, GraphError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"cargo-target-convention-v1\0");
    frame(&mut hash, entry.path.as_os_str().as_encoded_bytes());
    Ok(EdgeProvenance {
        source: ProvenanceSource::CargoConvention,
        path: Some(root_path(&entry.path, usize::MAX)?),
        range,
        range_kind: RangeKind::WholeFile,
        revision,
        confidence_millis: 1_000,
        semantic: None,
        evidence_digest: *hash.finalize().as_bytes(),
    })
}

fn cargo_syntax_provenance(
    revision: RevisionId,
    manifest: &MetadataEntry,
    entry: &MetadataEntry,
    record: &SyntacticSymbolRecord,
    explicit: bool,
    range: GraphRange,
) -> Result<EdgeProvenance, GraphError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"cargo-tree-sitter-test-v1\0");
    hash.update(&record.declaration_id());
    hash.update(
        &manifest
            .source_digest()
            .ok_or(GraphError::InvalidIndex("manifest digest is unavailable"))?,
    );
    hash.update(&[u8::from(explicit)]);
    Ok(EdgeProvenance {
        source: ProvenanceSource::CargoTreeSitter,
        path: Some(root_path(&entry.path, usize::MAX)?),
        range,
        range_kind: RangeKind::Declaration,
        revision,
        confidence_millis: 1_000,
        semantic: None,
        evidence_digest: *hash.finalize().as_bytes(),
    })
}

fn diagnostic_provenance(
    revision: RevisionId,
    diagnostic: &LiveDiagnostic,
    range: GraphRange,
) -> EdgeProvenance {
    EdgeProvenance {
        source: ProvenanceSource::Lsp,
        path: Some(diagnostic.path().as_path().clone()),
        range,
        range_kind: RangeKind::NormalizedFact,
        revision,
        confidence_millis: 1_000,
        semantic: None,
        evidence_digest: diagnostic_id(diagnostic).0,
    }
}

fn semantic_provenance(
    revision: RevisionId,
    fact: &SemanticFact,
    fact_range: GraphRange,
    target_range: GraphRange,
    origin_range: GraphRange,
) -> EdgeProvenance {
    let normalized = fact.provenance();
    let origin = normalized.origin();
    let semantic = SemanticEdgeProvenance {
        relation: fact.relation(),
        origin_uri: origin.uri().to_owned(),
        origin_path: origin.path().as_path().clone(),
        document_version: origin.document_version().get(),
        request_generation: origin.request_generation(),
        request_id: origin.request_id().get(),
        origin_position: fact.origin_point(),
        origin_range,
        server_artifact: normalized.server().server_artifact.to_string(),
        server_configuration: normalized.server().configuration.to_string(),
        position_encoding: normalized.position_encoding(),
        target_range,
        fact_range,
    };
    let mut provenance = EdgeProvenance {
        source: ProvenanceSource::Lsp,
        path: Some(fact.path().as_path().clone()),
        range: fact_range,
        range_kind: RangeKind::NormalizedFact,
        revision,
        confidence_millis: 1_000,
        semantic: Some(semantic),
        evidence_digest: [0; 32],
    };
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-semantic-evidence-v2\0");
    hash.update(&[fact.relation() as u8]);
    digest_provenance_fields(&mut hash, &provenance, false, true);
    provenance.evidence_digest = *hash.finalize().as_bytes();
    provenance
}

fn diagnostic_id(diagnostic: &LiveDiagnostic) -> NodeId {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-diagnostic-id-v2\0");
    frame(&mut hash, diagnostic.path().as_path().as_str().as_bytes());
    frame(&mut hash, &diagnostic.range().start().to_le_bytes());
    frame(&mut hash, &diagnostic.range().end().to_le_bytes());
    match diagnostic.severity() {
        Some(severity) => {
            frame(&mut hash, b"some");
            frame(&mut hash, &[severity]);
        }
        None => frame(&mut hash, b"none"),
    }
    match diagnostic.code() {
        Some(DiagnosticCode::Integer(value)) => {
            frame(&mut hash, b"integer");
            frame(&mut hash, &value.to_le_bytes());
        }
        Some(DiagnosticCode::String(value)) => {
            frame(&mut hash, b"string");
            frame(&mut hash, value.as_bytes());
        }
        None => frame(&mut hash, b"none"),
    }
    match diagnostic.source() {
        Some(source) => {
            frame(&mut hash, b"some");
            frame(&mut hash, source.as_bytes());
        }
        None => frame(&mut hash, b"none"),
    }
    frame(&mut hash, diagnostic.message().as_bytes());
    frame(&mut hash, diagnostic.provenance().uri().as_bytes());
    frame(
        &mut hash,
        diagnostic
            .provenance()
            .server()
            .server_artifact
            .as_str()
            .as_bytes(),
    );
    frame(
        &mut hash,
        &diagnostic
            .provenance()
            .document_version()
            .get()
            .to_le_bytes(),
    );
    frame(
        &mut hash,
        &[diagnostic.provenance().position_encoding() as u8],
    );
    frame(
        &mut hash,
        diagnostic
            .provenance()
            .server()
            .configuration
            .as_str()
            .as_bytes(),
    );
    NodeId(*hash.finalize().as_bytes())
}

fn validate_options(options: &GraphOptions) -> Result<(), GraphError> {
    let nonzero = [
        options.max_nodes,
        options.max_edges,
        options.max_manifests,
        options.max_manifest_bytes,
        options.max_manifest_input_bytes,
        options.max_toml_nesting,
        options.max_toml_items,
        options.max_toml_string_bytes,
        options.max_targets_per_manifest,
        options.max_dependencies_per_manifest,
        options.max_workspace_dependencies,
        options.max_input_bytes,
        options.max_evidence,
        options.max_evidence_bytes,
        options.max_member_patterns,
        options.max_pattern_bytes,
        options.max_pattern_components,
        options.max_cache_entries,
        options.max_cache_bytes,
        options.max_staging_bytes,
        options.max_work,
    ];
    if nonzero.contains(&0) || options.max_time.is_zero() {
        Err(GraphError::InvalidOptions("all bounds must be nonzero"))
    } else if options.max_manifest_input_bytes > MAX_TOML_PARSER_INPUT {
        Err(GraphError::InvalidOptions(
            "manifest parser input exceeds the hard cap",
        ))
    } else {
        Ok(())
    }
}

fn check_deadline(deadline: Instant) -> Result<(), GraphError> {
    if Instant::now() >= deadline {
        Err(GraphError::BoundExceeded(GraphBound::Time))
    } else {
        Ok(())
    }
}

fn graph_semantic_error(error: MapError) -> GraphError {
    match error {
        MapError::StaleFact => GraphError::StaleEvidence,
        MapError::InvalidIndex(reason) => GraphError::InvalidIndex(reason),
        MapError::InvalidFact(reason) => GraphError::InvalidEvidence(reason),
        MapError::TimeLimit => GraphError::BoundExceeded(GraphBound::Time),
        MapError::InvalidRequest("too many semantic relationships") => {
            GraphError::BoundExceeded(GraphBound::Evidence)
        }
        MapError::InvalidRequest("input byte limit exceeded") => {
            GraphError::BoundExceeded(GraphBound::EvidenceBytes)
        }
        MapError::InvalidRequest("map work limit exceeded") => {
            GraphError::BoundExceeded(GraphBound::Work)
        }
        _ => GraphError::InvalidEvidence("semantic evidence validation failed"),
    }
}

fn validate_path(path: &Path) -> Result<(), GraphError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        Err(GraphError::UnsafePath(path.to_path_buf()))
    } else {
        Ok(())
    }
}

fn root_path(path: &Path, max_bytes: usize) -> Result<RootRelativePath, GraphError> {
    validate_path(path)?;
    let value = path
        .to_str()
        .ok_or_else(|| GraphError::UnsafePath(path.to_path_buf()))?;
    RootRelativePath::parse(value, max_bytes)
        .map_err(|_| GraphError::UnsafePath(path.to_path_buf()))
}

fn validate_relative(path: &Path) -> Result<(), GraphError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        Err(GraphError::UnsafePath(path.to_path_buf()))
    } else {
        Ok(())
    }
}

fn validate_relative_dependency(path: &Path) -> Result<(), GraphError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::RootDir | Component::Prefix(_)))
    {
        Err(GraphError::UnsafePath(path.to_path_buf()))
    } else {
        Ok(())
    }
}

fn normalize_join(base: &Path, relative: &Path) -> Result<PathBuf, GraphError> {
    validate_relative_dependency(relative)?;
    let mut output = base.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => output.push(value),
            Component::ParentDir if output.pop() => {}
            _ => return Err(GraphError::UnsafePath(relative.to_path_buf())),
        }
    }
    if output
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        Err(GraphError::UnsafePath(relative.to_path_buf()))
    } else {
        Ok(output)
    }
}

fn validate_pattern(pattern: &str, options: &GraphOptions) -> Result<(), GraphError> {
    if pattern.is_empty() || pattern.len() > options.max_pattern_bytes {
        return Err(GraphError::BoundExceeded(GraphBound::PatternBytes));
    }
    if pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(GraphError::UnsafePath(PathBuf::from(pattern)));
    }
    if pattern.split('/').count() > options.max_pattern_components {
        return Err(GraphError::BoundExceeded(GraphBound::PatternComponents));
    }
    Ok(())
}

fn rooted_pattern(root: &Path, pattern: &str) -> String {
    if root.as_os_str().is_empty() {
        pattern.to_owned()
    } else {
        format!("{}/{}", root.to_string_lossy(), pattern)
    }
}

fn manifest_error(path: &Path, reason: &str) -> GraphError {
    GraphError::MalformedManifest {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    }
}

fn path_map_entry_weight<T>(path: &Path) -> Result<usize, GraphError> {
    BTREE_ENTRY_WEIGHT
        .checked_add(size_of::<PathBuf>())
        .and_then(|weight| weight.checked_add(size_of::<T>()))
        .and_then(|weight| weight.checked_add(path.as_os_str().len()))
        .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))
}

fn root_path_map_entry_weight<T>(path: &RootRelativePath) -> Result<usize, GraphError> {
    BTREE_ENTRY_WEIGHT
        .checked_add(size_of::<RootRelativePath>())
        .and_then(|weight| weight.checked_add(size_of::<T>()))
        .and_then(|weight| weight.checked_add(path.as_str().len()))
        .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))
}

fn rust_model_heap_weight(model: &RustModel) -> Result<usize, GraphError> {
    let mut total = model
        .tests
        .len()
        .checked_mul(BTREE_ENTRY_WEIGHT + size_of::<[u8; 32]>() + size_of::<TestState>())
        .and_then(|weight| {
            weight.checked_add(
                model
                    .modules
                    .capacity()
                    .checked_mul(size_of::<ExternalModule>())?,
            )
        })
        .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
    for module in &model.modules {
        total = total
            .checked_add(
                module
                    .inline_ancestors
                    .capacity()
                    .checked_mul(size_of::<String>())
                    .and_then(|weight| {
                        module
                            .inline_ancestors
                            .iter()
                            .try_fold(weight, |weight, value| weight.checked_add(value.capacity()))
                    })
                    .and_then(|weight| weight.checked_add(module.name.capacity()))
                    .and_then(|weight| {
                        weight.checked_add(
                            module
                                .path
                                .as_ref()
                                .map_or(0, |path| path.as_os_str().len()),
                        )
                    })
                    .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?,
            )
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
    }
    Ok(total)
}

fn structure_graph_logical_weight(
    nodes: &Vec<GraphNode>,
    edges: &Vec<GraphEdge>,
    coverage: &Vec<CoverageRecord>,
) -> Result<usize, GraphError> {
    let mut total = size_of::<StructureGraph>()
        .checked_add(
            nodes
                .capacity()
                .checked_mul(size_of::<GraphNode>())
                .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?,
        )
        .and_then(|bytes| bytes.checked_add(edges.capacity().checked_mul(size_of::<GraphEdge>())?))
        .and_then(|bytes| {
            bytes.checked_add(
                coverage
                    .capacity()
                    .checked_mul(size_of::<CoverageRecord>())?,
            )
        })
        .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
    for node in nodes {
        total = total
            .checked_add(node.name.capacity())
            .and_then(|bytes| {
                bytes.checked_add(node.path.as_ref().map_or(0, |path| path.as_str().len()))
            })
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
    }
    for edge in edges {
        total = total
            .checked_add(provenance_heap_weight(&edge.provenance)?)
            .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
    }
    Ok(total)
}

fn provenance_heap_weight(provenance: &EdgeProvenance) -> Result<usize, GraphError> {
    let mut total = provenance
        .path
        .as_ref()
        .map_or(0, |path| path.as_str().len());
    if let Some(semantic) = &provenance.semantic {
        for amount in [
            semantic.origin_uri.capacity(),
            semantic.origin_path.as_str().len(),
            semantic.server_artifact.capacity(),
            semantic.server_configuration.capacity(),
        ] {
            total = total
                .checked_add(amount)
                .ok_or(GraphError::BoundExceeded(GraphBound::StagingBytes))?;
        }
    }
    Ok(total)
}

fn manifest_weight(value: &ManifestModel) -> Result<usize, GraphError> {
    let mut total = size_of::<ManifestModel>();
    let mut add = |amount: usize| -> Result<(), GraphError> {
        total = total
            .checked_add(amount)
            .ok_or(GraphError::BoundExceeded(GraphBound::CacheBytes))?;
        Ok(())
    };
    add(value.package.as_ref().map_or(0, |value| value.capacity()))?;
    for value in value.members.iter().chain(&value.excludes) {
        add(size_of::<String>() + value.capacity())?;
    }
    for target in &value.targets {
        add(size_of::<TargetSpec>()
            + target.name.as_ref().map_or(0, |value| value.capacity())
            + target
                .path
                .as_ref()
                .map_or(0, |value| value.as_os_str().len()))?;
        for feature in &target.required_features {
            add(size_of::<String>() + feature.capacity())?;
        }
    }
    for dependency in &value.dependencies {
        add(size_of::<DependencySpec>()
            + dependency.section.len()
            + dependency.key.capacity()
            + dependency
                .package
                .as_ref()
                .map_or(0, |value| value.capacity())
            + dependency
                .path
                .as_ref()
                .map_or(0, |value| value.as_os_str().len()))?;
    }
    for (name, dependency) in &value.workspace_dependencies {
        add(BTREE_ENTRY_WEIGHT
            + name.capacity()
            + size_of::<WorkspaceDependency>()
            + dependency
                .package
                .as_ref()
                .map_or(0, |value| value.capacity())
            + dependency
                .path
                .as_ref()
                .map_or(0, |value| value.as_os_str().len()))?;
    }
    Ok(total)
}

fn diagnostic_weight(value: &LiveDiagnostic) -> Result<usize, GraphError> {
    value
        .path()
        .as_path()
        .as_str()
        .len()
        .checked_add(value.message().len())
        .and_then(|total| total.checked_add(value.source().map_or(0, str::len)))
        .and_then(|total| total.checked_add(size_of::<LiveDiagnostic>()))
        .ok_or(GraphError::BoundExceeded(GraphBound::EvidenceBytes))
}

fn semantic_weight(value: &SemanticFact) -> Result<usize, GraphError> {
    value
        .path()
        .as_path()
        .as_str()
        .len()
        .checked_add(value.provenance().origin().uri().len())
        .and_then(|total| total.checked_add(size_of::<SemanticFact>()))
        .ok_or(GraphError::BoundExceeded(GraphBound::EvidenceBytes))
}

fn extractor_digest(options: &GraphOptions) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-manifest-extractor-v2\0");
    for value in [TOML_PARSER_ID, GLOBSET_ID, MANIFEST_POLICY] {
        frame(&mut hash, value.as_bytes());
    }
    for value in [
        options.max_manifest_input_bytes,
        options.max_toml_nesting,
        options.max_toml_items,
        options.max_toml_string_bytes,
        options.max_targets_per_manifest,
        options.max_dependencies_per_manifest,
        options.max_workspace_dependencies,
        options.max_member_patterns,
        options.max_pattern_bytes,
        options.max_pattern_components,
    ] {
        hash.update(&(value as u128).to_le_bytes());
    }
    *hash.finalize().as_bytes()
}

fn rust_fragment_identity(
    index: &MetadataIndex,
    entry: &MetadataEntry,
    source: [u8; 32],
    max_work: usize,
    deadline: Instant,
) -> Result<([u8; 32], usize), GraphError> {
    let mut hash = blake3::Hasher::new();
    let mut work = 0_usize;
    let mut update = |bytes: &[u8]| -> Result<(), GraphError> {
        work = work
            .checked_add(bytes.len())
            .filter(|work| *work <= max_work)
            .ok_or(GraphError::BoundExceeded(GraphBound::Work))?;
        hash.update(bytes);
        Ok(())
    };
    update(b"kit-structure-rust-fragment-identity-v3\0")?;
    update(&source)?;
    update(&index.options_digest())?;
    update(&[
        u8::from(entry.syntax_has_parse_errors),
        u8::from(entry.syntax_truncated),
    ])?;
    update(&(entry.syntax_rejected_malformed as u128).to_le_bytes())?;
    update(&(entry.syntax_omitted as u128).to_le_bytes())?;
    update(&(entry.syntax_records.len() as u128).to_le_bytes())?;
    for record in entry.syntax_records.iter() {
        check_deadline(deadline)?;
        update(&record.declaration_id())?;
        update(&(record.range().start_byte as u128).to_le_bytes())?;
        update(&(record.range().end_byte as u128).to_le_bytes())?;
        match record.enclosing_symbol() {
            Some(parent) => {
                update(&[1])?;
                update(parent.value())?;
            }
            None => {
                update(&[0])?;
            }
        }
    }
    check_deadline(deadline)?;
    Ok((*hash.finalize().as_bytes(), work))
}

fn rust_extractor_digest(path: &Path) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-rust-extractor-v2\0");
    for value in [
        RUST_POLICY,
        RUST_GRAMMAR_VERSION,
        TREE_SITTER_RUNTIME_VERSION,
    ] {
        frame(&mut hash, value.as_bytes());
    }
    hash.update(&RUST_GRAMMAR_ABI.to_le_bytes());
    hash.update(RUST_GRAMMAR_ARTIFACT_DIGEST.as_bytes());
    hash.update(RUST_QUERY_SET_DIGEST.as_bytes());
    frame(&mut hash, path.as_os_str().as_encoded_bytes());
    *hash.finalize().as_bytes()
}

fn line_extractor_digest(path: &Path) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-line-index-v2\0");
    frame(&mut hash, path.as_os_str().as_encoded_bytes());
    *hash.finalize().as_bytes()
}

fn node_id(namespace: &[u8], identity: &[u8]) -> NodeId {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-node-id-v2\0");
    frame(&mut hash, namespace);
    frame(&mut hash, identity);
    NodeId(*hash.finalize().as_bytes())
}

fn digest_node(
    kind: NodeKind,
    id: NodeId,
    name: &str,
    path: Option<&RootRelativePath>,
    range: Option<GraphRange>,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-node-v2\0");
    hash.update(&[kind as u8]);
    hash.update(&id.0);
    if kind != NodeKind::Revision {
        frame(&mut hash, name.as_bytes());
    }
    if let Some(path) = path {
        frame(&mut hash, path.as_str().as_bytes());
    }
    if let Some(range) = range {
        digest_range(&mut hash, range);
    }
    *hash.finalize().as_bytes()
}

fn digest_edge(
    source: NodeId,
    target: NodeId,
    kind: EdgeKind,
    provenance: &EdgeProvenance,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-edge-v2\0");
    hash.update(&source.0);
    hash.update(&target.0);
    frame(&mut hash, &[kind as u8]);
    digest_provenance_fields(&mut hash, provenance, true, provenance.semantic.is_some());
    *hash.finalize().as_bytes()
}

fn digest_provenance_fields(
    hash: &mut blake3::Hasher,
    provenance: &EdgeProvenance,
    include_evidence_digest: bool,
    include_revision: bool,
) {
    frame(hash, &[provenance.source as u8]);
    match &provenance.path {
        Some(path) => {
            frame(hash, &[1]);
            frame(hash, path.as_str().as_bytes());
        }
        None => frame(hash, &[0]),
    }
    digest_range(hash, provenance.range);
    frame(hash, &[provenance.range_kind as u8]);
    if include_revision {
        frame(hash, provenance.revision.as_bytes());
    }
    frame(hash, &provenance.confidence_millis.to_le_bytes());
    match &provenance.semantic {
        Some(semantic) => {
            frame(hash, &[1]);
            frame(hash, &[semantic.relation as u8]);
            frame(hash, semantic.origin_uri.as_bytes());
            frame(hash, semantic.origin_path.as_str().as_bytes());
            frame(hash, &semantic.document_version.to_le_bytes());
            frame(hash, &semantic.request_generation.to_le_bytes());
            frame(hash, &semantic.request_id.to_le_bytes());
            frame(hash, &(semantic.origin_position as u128).to_le_bytes());
            digest_range(hash, semantic.origin_range);
            frame(hash, semantic.server_artifact.as_bytes());
            frame(hash, semantic.server_configuration.as_bytes());
            frame(hash, &[semantic.position_encoding as u8]);
            digest_range(hash, semantic.target_range);
            digest_range(hash, semantic.fact_range);
        }
        None => frame(hash, &[0]),
    }
    if include_evidence_digest {
        frame(hash, &provenance.evidence_digest);
    }
}

fn digest_options(options: &GraphOptions) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-options-v2\0");
    for identity in [
        TOML_PARSER_ID,
        GLOBSET_ID,
        MANIFEST_POLICY,
        RUST_POLICY,
        TREE_SITTER_RUNTIME_VERSION,
        RUST_GRAMMAR_VERSION,
        RUST_GRAMMAR_ARTIFACT_DIGEST,
        RUST_QUERY_SET_DIGEST,
    ] {
        frame(&mut hash, identity.as_bytes());
    }
    hash.update(&(RUST_GRAMMAR_ABI as u128).to_le_bytes());
    for value in [
        options.max_nodes,
        options.max_edges,
        options.max_manifests,
        options.max_manifest_bytes,
        options.max_manifest_input_bytes,
        options.max_toml_nesting,
        options.max_toml_items,
        options.max_toml_string_bytes,
        options.max_targets_per_manifest,
        options.max_dependencies_per_manifest,
        options.max_workspace_dependencies,
        options.max_input_bytes,
        options.max_evidence,
        options.max_evidence_bytes,
        options.max_member_patterns,
        options.max_pattern_bytes,
        options.max_pattern_components,
        options.max_cache_entries,
        options.max_cache_bytes,
        options.max_staging_bytes,
        options.max_work,
    ] {
        hash.update(&(value as u128).to_le_bytes());
    }
    hash.update(&options.max_time.as_nanos().to_le_bytes());
    *hash.finalize().as_bytes()
}

fn digest_evidence(
    diagnostics: &[LiveDiagnostic],
    semantic: &[SemanticRelationship<'_>],
    deadline: Instant,
) -> Result<[u8; 32], GraphError> {
    let mut records = Vec::with_capacity(diagnostics.len() + semantic.len());
    for diagnostic in diagnostics {
        check_deadline(deadline)?;
        records.push(diagnostic_id(diagnostic).0);
    }
    for relationship in semantic {
        check_deadline(deadline)?;
        let fact = relationship.fact;
        let mut hash = blake3::Hasher::new();
        hash.update(b"kit-structure-semantic-record-v2\0");
        frame(&mut hash, &relationship.source_declaration.as_bytes());
        digest_semantic_fact(&mut hash, fact);
        records.push(*hash.finalize().as_bytes());
    }
    records.sort();
    check_deadline(deadline)?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-structure-evidence-v2\0");
    for record in records {
        check_deadline(deadline)?;
        hash.update(&record);
    }
    check_deadline(deadline)?;
    Ok(*hash.finalize().as_bytes())
}

fn digest_semantic_fact(hash: &mut blake3::Hasher, fact: &SemanticFact) {
    let provenance = fact.provenance();
    let origin = provenance.origin();
    frame(hash, &[fact.relation() as u8]);
    frame(hash, fact.path().as_path().as_str().as_bytes());
    frame(hash, &(fact.range().start() as u128).to_le_bytes());
    frame(hash, &(fact.range().end() as u128).to_le_bytes());
    match fact.target_range() {
        Some(range) => {
            frame(hash, &[1]);
            frame(hash, &(range.start() as u128).to_le_bytes());
            frame(hash, &(range.end() as u128).to_le_bytes());
        }
        None => frame(hash, &[0]),
    }
    frame(hash, &(fact.origin_point() as u128).to_le_bytes());
    frame(hash, &(fact.origin_range().start() as u128).to_le_bytes());
    frame(hash, &(fact.origin_range().end() as u128).to_le_bytes());
    frame(hash, &[provenance.classification() as u8]);
    frame(hash, &[provenance.source() as u8]);
    frame(hash, provenance.revision().as_bytes());
    frame(hash, &[provenance.confidence() as u8]);
    frame(hash, origin.uri().as_bytes());
    frame(hash, origin.path().as_path().as_str().as_bytes());
    frame(hash, &origin.document_version().get().to_le_bytes());
    frame(hash, &origin.request_generation().to_le_bytes());
    frame(hash, &origin.request_id().get().to_le_bytes());
    frame(
        hash,
        provenance.server().server_artifact.as_str().as_bytes(),
    );
    frame(hash, provenance.server().configuration.as_str().as_bytes());
    frame(hash, &[provenance.position_encoding() as u8]);
}

fn digest_coverage(hash: &mut blake3::Hasher, value: &CoverageRecord) {
    if let Some(subject) = value.subject {
        hash.update(&subject.0);
    } else {
        hash.update(&[0; 32]);
    }
    hash.update(&[value.relation as u8, value.status as u8]);
    frame(hash, value.detail.as_bytes());
}

fn digest_range(hash: &mut blake3::Hasher, range: GraphRange) {
    for value in [
        range.start_byte,
        range.end_byte,
        range.start_line,
        range.end_line,
    ] {
        frame(hash, &(value as u128).to_le_bytes());
    }
}

fn frame(hash: &mut blake3::Hasher, value: &[u8]) {
    hash.update(&(value.len() as u64).to_le_bytes());
    hash.update(value);
}

fn provenance_key(
    value: &EdgeProvenance,
) -> (
    ProvenanceSource,
    Option<&RootRelativePath>,
    GraphRange,
    RangeKind,
    [u8; 32],
) {
    (
        value.source,
        value.path.as_ref(),
        value.range,
        value.range_kind,
        value.evidence_digest,
    )
}
