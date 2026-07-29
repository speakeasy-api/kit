use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fmt,
    mem::size_of,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use tree_sitter::{
    InputEdit, Language, Node, ParseOptions, Parser, Point, Query, QueryCursor, QueryCursorOptions,
    StreamingIterator, Tree,
};

use crate::workspace::revision::RevisionId;

pub const TREE_SITTER_RUNTIME_VERSION: &str = "0.25.10";
pub const RUST_GRAMMAR_VERSION: &str = "tree-sitter-rust@0.24.0";
pub const RUST_GRAMMAR_ABI: usize = 15;
pub const RUST_GRAMMAR_ARTIFACT_DIGEST: &str =
    "sha256:4b9b18034c684a2420722be8b2a91c9c44f2546b631c039edf575ccba8c61be1";
pub const RUST_QUERY_SET_DIGEST: &str =
    "blake3:c8c12ba2ce020cbd6b3c30eb8852dae0b7f67e08d72a60036415824f995d20d2";
pub const RUST_QUERY: &[u8] = br#"
[
  (function_item name: (identifier) @name) @declaration
  (function_signature_item name: (identifier) @name) @declaration
  (struct_item name: (type_identifier) @name) @declaration
  (enum_item name: (type_identifier) @name) @declaration
  (union_item name: (type_identifier) @name) @declaration
  (type_item name: (type_identifier) @name) @declaration
  (trait_item name: (type_identifier) @name) @declaration
  (mod_item name: (identifier) @name) @declaration
  (const_item name: (identifier) @name) @declaration
  (static_item name: (identifier) @name) @declaration
  (macro_definition name: (identifier) @name) @declaration
]
"#;
const ARC_ALLOCATION_OVERHEAD: usize = 2 * size_of::<usize>();
const BTREE_ENTRY_LOGICAL_OVERHEAD: usize = 3 * size_of::<usize>();
const TREE_NODE_LOGICAL_WEIGHT: usize = 64;
const PARSER_OPAQUE_LOGICAL_WEIGHT: usize = 64 * 1024;
const QUERY_OPAQUE_LOGICAL_WEIGHT: usize = 32 * 1024;
// These are deterministic logical charges. They do not estimate allocator bytes or hard RSS.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SyntaxLanguage {
    Rust,
}

impl SyntaxLanguage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanguageDescriptor {
    declaration_query: &'static [u8],
}

impl LanguageDescriptor {
    pub const fn rust() -> Self {
        Self {
            declaration_query: RUST_QUERY,
        }
    }

    #[cfg(test)]
    const fn rust_with_declaration_query(query: &'static [u8]) -> Self {
        Self {
            declaration_query: query,
        }
    }

    pub const fn language(&self) -> SyntaxLanguage {
        SyntaxLanguage::Rust
    }

    pub const fn grammar_abi(&self) -> usize {
        RUST_GRAMMAR_ABI
    }

    pub const fn grammar_version(&self) -> &'static str {
        RUST_GRAMMAR_VERSION
    }

    pub const fn grammar_artifact_digest(&self) -> &'static str {
        RUST_GRAMMAR_ARTIFACT_DIGEST
    }

    pub const fn declaration_query(&self) -> &'static [u8] {
        self.declaration_query
    }

    pub fn query_set_digest(&self) -> [u8; 32] {
        query_set_digest_unchecked(self.declaration_query)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxOptions {
    pub max_path_bytes: usize,
    pub max_source_bytes: usize,
    pub max_query_bytes: usize,
    pub max_captures: usize,
    /// Conservative deterministic extraction working weight.
    pub max_scope_weight: usize,
    pub max_symbols: usize,
    pub max_symbol_bytes: usize,
}

impl Default for SyntaxOptions {
    fn default() -> Self {
        Self {
            max_path_bytes: 4 * 1024,
            max_source_bytes: 2 * 1024 * 1024,
            max_query_bytes: 64 * 1024,
            max_captures: 4_096,
            max_scope_weight: 2 * 1024 * 1024,
            max_symbols: 256,
            max_symbol_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxCacheLimits {
    /// Maximum files in the published resident cache.
    pub max_resident_files: usize,
    /// Resident cache logical weight, including the runtime; not allocator bytes or RSS.
    pub max_resident_logical_weight: usize,
    /// Maximum transient candidates retained during an active staged snapshot.
    pub max_staging_files: usize,
    /// Candidate-only staging weight; total peak may reach resident plus staging limits.
    pub max_staging_logical_weight: usize,
    pub max_queries: usize,
    pub max_query_bytes: usize,
}

impl Default for SyntaxCacheLimits {
    fn default() -> Self {
        Self {
            max_resident_files: 4_096,
            max_resident_logical_weight: 256 * 1024 * 1024,
            max_staging_files: 4_096,
            max_staging_logical_weight: 256 * 1024 * 1024,
            max_queries: 16,
            max_query_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParseAction {
    Reused,
    Full,
    Incremental,
    ExtractionOnly,
    RevisionRefresh,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyntaxMetrics {
    pub reused: u64,
    pub full_parses: u64,
    pub incremental_parses: u64,
    pub extraction_refreshes: u64,
    pub revision_refreshes: u64,
    pub evicted_files: u64,
    pub pruned_files: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyntaxCacheUsage {
    pub resident_files: usize,
    /// Includes resident files and the shared runtime; not allocator bytes or RSS.
    pub resident_logical_weight: usize,
    pub staging_files: usize,
    /// Candidate-only transient staging weight.
    pub staging_logical_weight: usize,
    pub total_files: usize,
    /// Resident plus staging logical weight.
    pub total_logical_weight: usize,
    pub compiled_queries: usize,
    /// Exact retained query-source bytes.
    pub query_source_bytes: usize,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SyntaxTestState {
    usage: SyntaxCacheUsage,
    metrics: SyntaxMetrics,
    clock: u64,
    snapshot_active: bool,
    files: Vec<SyntaxTestFileState>,
    candidate_weight: usize,
    candidates: Vec<(PathBuf, [u8; 32])>,
    queries: Vec<([u8; 32], u64)>,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct SyntaxTestFileState {
    path: PathBuf,
    last_used: u64,
    protected: bool,
    identity: [u8; 32],
    records: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyntaxCacheIdentity {
    language: SyntaxLanguage,
    grammar: GrammarIdentity,
    query_set_digest: [u8; 32],
    extraction_digest: [u8; 32],
    path: Arc<Path>,
    source_digest: [u8; 32],
    revision: RevisionId,
}

impl SyntaxCacheIdentity {
    fn same_except_revision(&self, other: &Self) -> bool {
        self.language == other.language
            && self.grammar == other.grammar
            && self.query_set_digest == other.query_set_digest
            && self.extraction_digest == other.extraction_digest
            && self.path == other.path
            && self.source_digest == other.source_digest
    }

    pub(crate) const fn source_digest(&self) -> [u8; 32] {
        self.source_digest
    }

    #[cfg(test)]
    pub(crate) fn canonical_digest(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        hash.update(b"kit-syntax-cache-v2\0");
        frame(&mut hash, self.language.as_str().as_bytes());
        hash.update(&(self.grammar.abi as u128).to_le_bytes());
        frame(&mut hash, self.grammar.version.as_bytes());
        frame(&mut hash, self.grammar.artifact_digest.as_bytes());
        hash.update(&self.query_set_digest);
        hash.update(&self.extraction_digest);
        frame(&mut hash, self.path.as_os_str().as_encoded_bytes());
        hash.update(&self.source_digest);
        hash.update(self.revision.as_bytes());
        *hash.finalize().as_bytes()
    }

    fn canonical_digest_before(&self, deadline: Instant) -> Result<[u8; 32], SyntaxError> {
        let mut hash = blake3::Hasher::new();
        hash.update(b"kit-syntax-cache-v2\0");
        frame_before(&mut hash, self.language.as_str().as_bytes(), deadline)?;
        hash.update(&(self.grammar.abi as u128).to_le_bytes());
        frame_before(&mut hash, self.grammar.version.as_bytes(), deadline)?;
        frame_before(&mut hash, self.grammar.artifact_digest.as_bytes(), deadline)?;
        hash.update(&self.query_set_digest);
        hash.update(&self.extraction_digest);
        frame_before(
            &mut hash,
            self.path.as_os_str().as_encoded_bytes(),
            deadline,
        )?;
        hash.update(&self.source_digest);
        hash.update(self.revision.as_bytes());
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        Ok(*hash.finalize().as_bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GrammarIdentity {
    abi: usize,
    version: Arc<str>,
    artifact_digest: Arc<str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FactSource {
    Syntactic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntacticProvenance {
    source: FactSource,
    confidence_millis: u16,
    revision: RevisionId,
    range: SourceRange,
    grammar_identity: [u8; 32],
    query_set_digest: [u8; 32],
}

impl SyntacticProvenance {
    pub const fn source(&self) -> FactSource {
        self.source
    }

    pub const fn confidence_millis(&self) -> u16 {
        self.confidence_millis
    }

    pub const fn revision(&self) -> RevisionId {
        self.revision
    }

    pub const fn range(&self) -> SourceRange {
        self.range
    }

    pub const fn grammar_identity(&self) -> [u8; 32] {
        self.grammar_identity
    }

    pub const fn query_set_digest(&self) -> [u8; 32] {
        self.query_set_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntacticFact<T> {
    value: T,
    provenance: SyntacticProvenance,
}

impl<T> SyntacticFact<T> {
    pub const fn value(&self) -> &T {
        &self.value
    }

    pub const fn provenance(&self) -> &SyntacticProvenance {
        &self.provenance
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    NotExtracted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyntacticFacts<T> {
    Available(Arc<[SyntacticFact<T>]>),
    Unavailable(UnavailableReason),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SyntacticSymbolKind {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntacticText {
    text: Arc<String>,
    truncated: bool,
}

impl SyntacticText {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntacticSymbolRecord {
    declaration_id: [u8; 32],
    workspace_revision: RevisionId,
    canonical_path: Arc<Path>,
    language: SyntaxLanguage,
    qualified_name: SyntacticFact<Arc<String>>,
    display_name: SyntacticFact<Arc<String>>,
    kind: SyntacticFact<SyntacticSymbolKind>,
    signature: SyntacticFact<SyntacticText>,
    declaration: SyntacticFact<SyntacticText>,
    range: SourceRange,
    enclosing_symbol: Option<SyntacticFact<[u8; 32]>>,
    imports: SyntacticFacts<Arc<String>>,
    exports: SyntacticFacts<Arc<String>>,
    definitions: SyntacticFacts<SourceRange>,
    references: SyntacticFacts<Arc<String>>,
    callers: SyntacticFacts<[u8; 32]>,
    callees: SyntacticFacts<Arc<String>>,
    tests: SyntacticFacts<Arc<String>>,
    documentation: SyntacticFacts<Arc<String>>,
}

impl SyntacticSymbolRecord {
    pub const fn declaration_id(&self) -> [u8; 32] {
        self.declaration_id
    }

    pub const fn workspace_revision(&self) -> RevisionId {
        self.workspace_revision
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub const fn language(&self) -> SyntaxLanguage {
        self.language
    }

    pub const fn qualified_name(&self) -> &SyntacticFact<Arc<String>> {
        &self.qualified_name
    }

    pub const fn display_name(&self) -> &SyntacticFact<Arc<String>> {
        &self.display_name
    }

    pub const fn kind(&self) -> &SyntacticFact<SyntacticSymbolKind> {
        &self.kind
    }

    pub const fn signature(&self) -> &SyntacticFact<SyntacticText> {
        &self.signature
    }

    pub const fn declaration(&self) -> &SyntacticFact<SyntacticText> {
        &self.declaration
    }

    pub const fn range(&self) -> SourceRange {
        self.range
    }

    pub const fn enclosing_symbol(&self) -> Option<&SyntacticFact<[u8; 32]>> {
        self.enclosing_symbol.as_ref()
    }

    pub const fn imports(&self) -> &SyntacticFacts<Arc<String>> {
        &self.imports
    }

    pub const fn exports(&self) -> &SyntacticFacts<Arc<String>> {
        &self.exports
    }

    pub const fn definitions(&self) -> &SyntacticFacts<SourceRange> {
        &self.definitions
    }

    pub const fn references(&self) -> &SyntacticFacts<Arc<String>> {
        &self.references
    }

    pub const fn callers(&self) -> &SyntacticFacts<[u8; 32]> {
        &self.callers
    }

    pub const fn callees(&self) -> &SyntacticFacts<Arc<String>> {
        &self.callees
    }

    pub const fn tests(&self) -> &SyntacticFacts<Arc<String>> {
        &self.tests
    }

    pub const fn documentation(&self) -> &SyntacticFacts<Arc<String>> {
        &self.documentation
    }

    pub(crate) fn canonical_digest_before(
        &self,
        deadline: Instant,
    ) -> Result<[u8; 32], SyntaxError> {
        let mut hash = blake3::Hasher::new();
        hash.update(b"kit-syntactic-symbol-v2\0");
        hash.update(&self.declaration_id);
        hash.update(self.workspace_revision.as_bytes());
        frame_before(
            &mut hash,
            self.canonical_path.as_os_str().as_encoded_bytes(),
            deadline,
        )?;
        hash.update(&[self.language as u8]);
        digest_string_fact(&mut hash, &self.qualified_name, deadline)?;
        digest_string_fact(&mut hash, &self.display_name, deadline)?;
        digest_kind_fact(&mut hash, &self.kind);
        digest_text_fact(&mut hash, &self.signature, deadline)?;
        digest_text_fact(&mut hash, &self.declaration, deadline)?;
        digest_range(&mut hash, self.range);
        digest_optional_id_fact(&mut hash, self.enclosing_symbol.as_ref());
        digest_string_facts(&mut hash, &self.imports, deadline)?;
        digest_string_facts(&mut hash, &self.exports, deadline)?;
        digest_range_facts(&mut hash, &self.definitions, deadline)?;
        digest_string_facts(&mut hash, &self.references, deadline)?;
        digest_id_facts(&mut hash, &self.callers, deadline)?;
        digest_string_facts(&mut hash, &self.callees, deadline)?;
        digest_string_facts(&mut hash, &self.tests, deadline)?;
        digest_string_facts(&mut hash, &self.documentation, deadline)?;
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        Ok(*hash.finalize().as_bytes())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SyntaxResult {
    pub(crate) identity: SyntaxCacheIdentity,
    pub(crate) records: Arc<[SyntacticSymbolRecord]>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) action: ParseAction,
    pub(crate) has_parse_errors: bool,
    pub(crate) rejected_malformed: usize,
    pub(crate) truncated: bool,
    pub(crate) omitted: usize,
    pub(crate) canonical_digest: [u8; 32],
    source: Arc<String>,
    tree: Tree,
}

impl SyntaxResult {
    pub(crate) fn source(&self) -> Arc<String> {
        Arc::clone(&self.source)
    }
}

pub(crate) struct CachedSyntaxTree {
    pub(crate) source: Arc<String>,
    pub(crate) tree: Tree,
}

#[derive(Debug)]
pub enum SyntaxError {
    UnsupportedLanguage(String),
    UnsafePath(PathBuf),
    InvalidUtf8,
    SourceTooLarge { bytes: usize, max: usize },
    QueryTooLarge { bytes: usize, max: usize },
    InvalidQuery(String),
    InvalidOptions(&'static str),
    IncompatibleGrammar { expected: usize, actual: usize },
    ParseFailed,
    ParseTimeout,
    QueryTimeout,
    CacheUnavailable(PathBuf),
    AllocationFailed,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedLanguage(language) => {
                write!(formatter, "unsupported syntax language: {language}")
            }
            Self::UnsafePath(path) => write!(formatter, "unsafe syntax path: {}", path.display()),
            Self::InvalidUtf8 => formatter.write_str("syntax source is not UTF-8"),
            Self::SourceTooLarge { bytes, max } => {
                write!(formatter, "syntax source has {bytes} bytes; limit is {max}")
            }
            Self::QueryTooLarge { bytes, max } => {
                write!(formatter, "syntax query has {bytes} bytes; limit is {max}")
            }
            Self::InvalidQuery(reason) => write!(formatter, "invalid syntax query: {reason}"),
            Self::InvalidOptions(reason) => write!(formatter, "invalid syntax options: {reason}"),
            Self::IncompatibleGrammar { expected, actual } => write!(
                formatter,
                "incompatible Rust grammar ABI {actual}; pinned ABI is {expected}"
            ),
            Self::ParseFailed => formatter.write_str("Tree-sitter parse failed"),
            Self::ParseTimeout => formatter.write_str("Tree-sitter parse timed out"),
            Self::QueryTimeout => formatter.write_str("Tree-sitter query timed out"),
            Self::CacheUnavailable(path) => {
                write!(
                    formatter,
                    "syntax tree is not cached for {}",
                    path.display()
                )
            }
            Self::AllocationFailed => formatter.write_str("syntax allocation failed"),
        }
    }
}

impl std::error::Error for SyntaxError {}

pub struct SyntaxIndex {
    limits: SyntaxCacheLimits,
    cache: BTreeMap<Arc<Path>, CachedFile>,
    logical_weight: usize,
    candidates: BTreeMap<Arc<Path>, CachedFile>,
    candidate_weight: usize,
    clock: u64,
    metrics: SyntaxMetrics,
    runtime: RustRuntime,
    snapshot_active: bool,
    #[cfg(test)]
    cancel_next_parse: bool,
    #[cfg(test)]
    expire_next_query_after_compile: bool,
    #[cfg(test)]
    fail_path: Option<PathBuf>,
}

#[derive(Clone)]
struct CachedFile {
    identity: SyntaxCacheIdentity,
    source: Arc<String>,
    tree: Tree,
    records: Arc<[SyntacticSymbolRecord]>,
    has_parse_errors: bool,
    rejected_malformed: usize,
    truncated: bool,
    omitted: usize,
    canonical_digest: [u8; 32],
    node_count: usize,
    logical_weight: usize,
    last_used: u64,
    protected: bool,
}

struct RustRuntime {
    parser: Parser,
    grammar: GrammarIdentity,
    scope_query: Arc<Query>,
    queries: BTreeMap<[u8; 32], CachedQuery>,
    query_source_bytes: usize,
    logical_weight: usize,
}

#[derive(Clone)]
struct CachedQuery {
    query: Arc<Query>,
    source_bytes: usize,
    logical_weight: usize,
    last_used: u64,
}

struct QueryUse {
    query: Arc<Query>,
    cached_digest: Option<[u8; 32]>,
    admission: Option<QueryAdmission>,
    projected_runtime_weight: usize,
}

struct QueryAdmission {
    digest: [u8; 32],
    cached: CachedQuery,
    evictions: Vec<[u8; 32]>,
}

struct CacheAdmission {
    evictions: Vec<Arc<Path>>,
}

struct CandidateAdmission {
    removals: Vec<Arc<Path>>,
}

enum CacheTarget {
    Resident(CacheAdmission),
    Candidate(CandidateAdmission),
    None,
}

impl RustRuntime {
    fn pinned(limits: &SyntaxCacheLimits) -> Result<Self, SyntaxError> {
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let actual = language.abi_version();
        if actual != RUST_GRAMMAR_ABI {
            return Err(SyntaxError::IncompatibleGrammar {
                expected: RUST_GRAMMAR_ABI,
                actual,
            });
        }
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|_| SyntaxError::IncompatibleGrammar {
                expected: RUST_GRAMMAR_ABI,
                actual,
            })?;
        let scope_query = Arc::new(
            Query::new(
                &language,
                std::str::from_utf8(RUST_QUERY)
                    .map_err(|_| SyntaxError::InvalidQuery("query is not UTF-8".to_owned()))?,
            )
            .map_err(|error| SyntaxError::InvalidQuery(error.to_string()))?,
        );
        let query_source_bytes = RUST_QUERY.len();
        let logical_weight = runtime_base_weight(query_source_bytes);
        if query_source_bytes > limits.max_query_bytes
            || logical_weight > limits.max_resident_logical_weight
        {
            return Err(SyntaxError::InvalidOptions(
                "syntax cache cannot retain the pinned runtime",
            ));
        }
        Ok(Self {
            parser,
            grammar: GrammarIdentity {
                abi: actual,
                version: Arc::from(RUST_GRAMMAR_VERSION),
                artifact_digest: Arc::from(RUST_GRAMMAR_ARTIFACT_DIGEST),
            },
            scope_query,
            queries: BTreeMap::new(),
            query_source_bytes,
            logical_weight,
        })
    }

    fn fork(&self) -> Result<Self, SyntaxError> {
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|_| SyntaxError::IncompatibleGrammar {
                expected: RUST_GRAMMAR_ABI,
                actual: language.abi_version(),
            })?;
        Ok(Self {
            parser,
            grammar: self.grammar.clone(),
            scope_query: Arc::clone(&self.scope_query),
            queries: self.queries.clone(),
            query_source_bytes: self.query_source_bytes,
            logical_weight: self.logical_weight,
        })
    }
}

impl Default for SyntaxIndex {
    fn default() -> Self {
        Self::with_cache_limits(SyntaxCacheLimits::default())
            .expect("default syntax cache limits are valid")
    }
}

impl SyntaxIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cache_limits(limits: SyntaxCacheLimits) -> Result<Self, SyntaxError> {
        if limits.max_resident_files == 0
            || limits.max_resident_logical_weight == 0
            || limits.max_staging_files == 0
            || limits.max_staging_logical_weight == 0
            || limits.max_queries == 0
            || limits.max_query_bytes == 0
        {
            return Err(SyntaxError::InvalidOptions(
                "all syntax cache bounds must be nonzero",
            ));
        }
        let runtime = RustRuntime::pinned(&limits)?;
        Ok(Self {
            limits,
            cache: BTreeMap::new(),
            logical_weight: 0,
            candidates: BTreeMap::new(),
            candidate_weight: 0,
            clock: 0,
            metrics: SyntaxMetrics::default(),
            runtime,
            snapshot_active: false,
            #[cfg(test)]
            cancel_next_parse: false,
            #[cfg(test)]
            expire_next_query_after_compile: false,
            #[cfg(test)]
            fail_path: None,
        })
    }

    pub fn metrics(&self) -> SyntaxMetrics {
        self.metrics
    }

    pub fn cache_usage(&self) -> SyntaxCacheUsage {
        let resident_logical_weight = self
            .logical_weight
            .saturating_add(self.runtime.logical_weight);
        SyntaxCacheUsage {
            resident_files: self.cache.len(),
            resident_logical_weight,
            staging_files: self.candidates.len(),
            staging_logical_weight: self.candidate_weight,
            total_files: self.cache.len().saturating_add(self.candidates.len()),
            total_logical_weight: resident_logical_weight.saturating_add(self.candidate_weight),
            compiled_queries: self.runtime.queries.len() + 1,
            query_source_bytes: self.runtime.query_source_bytes,
        }
    }

    pub(crate) fn cached_rust_tree_before(
        &self,
        revision: RevisionId,
        path: &Path,
        source: &[u8],
        deadline: Instant,
    ) -> Result<CachedSyntaxTree, SyntaxError> {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let source_digest = checked_digest(source, deadline, SyntaxError::QueryTimeout)?;
        let cached = self
            .cache
            .get(path)
            .filter(|cached| {
                cached.identity.revision == revision
                    && cached.identity.language == SyntaxLanguage::Rust
                    && cached.identity.source_digest == source_digest
            })
            .ok_or_else(|| SyntaxError::CacheUnavailable(path.to_owned()))?;
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        Ok(CachedSyntaxTree {
            source: Arc::clone(&cached.source),
            tree: cached.tree.clone(),
        })
    }

    pub(crate) fn ensure_cached_rust_tree_before(
        &mut self,
        revision: RevisionId,
        path: &Path,
        source: &[u8],
        max_source_bytes: usize,
        deadline: Instant,
    ) -> Result<CachedSyntaxTree, SyntaxError> {
        if let Ok(cached) = self.cached_rust_tree_before(revision, path, source, deadline) {
            return Ok(cached);
        }
        let result = self.index_source(
            revision,
            &LanguageDescriptor::rust(),
            path,
            "rust",
            source,
            &SyntaxOptions {
                max_source_bytes,
                ..SyntaxOptions::default()
            },
            deadline,
        )?;
        Ok(CachedSyntaxTree {
            source: result.source,
            tree: result.tree,
        })
    }

    pub(crate) fn fork(&self) -> Result<Self, SyntaxError> {
        Ok(Self {
            limits: self.limits.clone(),
            cache: self.cache.clone(),
            logical_weight: self.logical_weight,
            candidates: self.candidates.clone(),
            candidate_weight: self.candidate_weight,
            clock: self.clock,
            metrics: self.metrics,
            runtime: self.runtime.fork()?,
            snapshot_active: self.snapshot_active,
            #[cfg(test)]
            cancel_next_parse: self.cancel_next_parse,
            #[cfg(test)]
            expire_next_query_after_compile: self.expire_next_query_after_compile,
            #[cfg(test)]
            fail_path: self.fail_path.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_path_for_test(&mut self, path: PathBuf) {
        self.fail_path = Some(path);
    }

    #[cfg(test)]
    pub(crate) fn clear_fail_path_for_test(&mut self) {
        self.fail_path = None;
    }

    #[cfg(test)]
    pub(crate) fn test_state(&self) -> SyntaxTestState {
        SyntaxTestState {
            usage: self.cache_usage(),
            metrics: self.metrics,
            clock: self.clock,
            snapshot_active: self.snapshot_active,
            files: self
                .cache
                .iter()
                .map(|(path, cached)| SyntaxTestFileState {
                    path: path.to_path_buf(),
                    last_used: cached.last_used,
                    protected: cached.protected,
                    identity: cached.identity.canonical_digest(),
                    records: cached.canonical_digest,
                })
                .collect(),
            candidate_weight: self.candidate_weight,
            candidates: self
                .candidates
                .iter()
                .map(|(path, cached)| (path.to_path_buf(), cached.canonical_digest))
                .collect(),
            queries: self
                .runtime
                .queries
                .iter()
                .map(|(digest, cached)| (*digest, cached.last_used))
                .collect(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn index_snapshot_source_before(
        &mut self,
        revision: RevisionId,
        path: &Path,
        language: &str,
        source: &[u8],
        descriptor: &LanguageDescriptor,
        options: &SyntaxOptions,
        deadline: Instant,
    ) -> Result<SyntaxResult, SyntaxError> {
        self.index_source(
            revision, descriptor, path, language, source, options, deadline,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn index_source(
        &mut self,
        revision: RevisionId,
        descriptor: &LanguageDescriptor,
        path: &Path,
        language: &str,
        source: &[u8],
        options: &SyntaxOptions,
        deadline: Instant,
    ) -> Result<SyntaxResult, SyntaxError> {
        check_deadline(deadline, SyntaxError::ParseTimeout)?;
        validate(path, language, source, descriptor, options, deadline)?;
        #[cfg(test)]
        if self.fail_path.as_deref() == Some(path) {
            return Err(SyntaxError::ParseTimeout);
        }
        let declaration_query_digest = checked_digest(
            descriptor.declaration_query(),
            deadline,
            SyntaxError::QueryTimeout,
        )?;
        let query_set_digest = query_set_digest_before(descriptor.declaration_query(), deadline)?;
        let extraction_digest = extraction_digest(query_set_digest, options);
        let source_digest = checked_digest(source, deadline, SyntaxError::ParseTimeout)?;
        let grammar = self.runtime.grammar.clone();
        let canonical_path: Arc<Path> = Arc::from(path);
        let identity = SyntaxCacheIdentity {
            language: SyntaxLanguage::Rust,
            grammar,
            query_set_digest,
            extraction_digest,
            path: Arc::clone(&canonical_path),
            source_digest,
            revision,
        };

        if self
            .cache
            .get(path)
            .is_some_and(|cached| cached.identity == identity)
        {
            let cached = self.cache.get(path).expect("cache entry was checked");
            let result = SyntaxResult {
                identity,
                records: Arc::clone(&cached.records),
                action: ParseAction::Reused,
                has_parse_errors: cached.has_parse_errors,
                rejected_malformed: cached.rejected_malformed,
                truncated: cached.truncated,
                omitted: cached.omitted,
                canonical_digest: cached.canonical_digest,
                source: Arc::clone(&cached.source),
                tree: cached.tree.clone(),
            };
            check_deadline(deadline, SyntaxError::ParseTimeout)?;
            let clock = self.next_clock();
            self.cache
                .get_mut(path)
                .expect("cache entry was checked")
                .last_used = clock;
            self.metrics.reused += 1;
            return Ok(result);
        }

        if self
            .cache
            .get(path)
            .is_some_and(|cached| cached.identity.same_except_revision(&identity))
        {
            let cached = self.cache.get(path).expect("cache entry was checked");
            let records = Arc::from(rebind_records(&cached.records, revision, deadline)?);
            let canonical_digest = digest_result(
                &identity,
                &records,
                cached.has_parse_errors,
                cached.rejected_malformed,
                cached.truncated,
                cached.omitted,
                deadline,
            )?;
            let source = Arc::clone(&cached.source);
            let has_parse_errors = cached.has_parse_errors;
            let rejected_malformed = cached.rejected_malformed;
            let truncated = cached.truncated;
            let omitted = cached.omitted;
            let node_count = cached.node_count;
            let replacement = CachedFile {
                identity: identity.clone(),
                source: Arc::clone(&source),
                tree: cached.tree.clone(),
                records: Arc::clone(&records),
                has_parse_errors,
                rejected_malformed,
                truncated,
                omitted,
                canonical_digest,
                node_count,
                logical_weight: cached_file_weight(path, &source, &records, node_count),
                last_used: 0,
                protected: cached.protected,
            };
            let admission = self.plan_cache_admission(
                path,
                replacement.logical_weight,
                self.runtime.logical_weight,
                deadline,
            )?;
            let target = if let Some(admission) = admission {
                CacheTarget::Resident(admission)
            } else if let Some(admission) =
                self.plan_candidate_admission(&identity.path, replacement.logical_weight, deadline)?
            {
                CacheTarget::Candidate(admission)
            } else {
                CacheTarget::None
            };
            let result_tree = replacement.tree.clone();
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            match target {
                CacheTarget::Resident(admission) => {
                    self.commit_cache(Arc::clone(&identity.path), replacement, admission);
                }
                CacheTarget::Candidate(admission) => {
                    self.commit_candidate(Arc::clone(&identity.path), replacement, admission);
                }
                CacheTarget::None => {}
            }
            self.metrics.revision_refreshes += 1;
            return Ok(SyntaxResult {
                identity,
                records,
                action: ParseAction::RevisionRefresh,
                has_parse_errors,
                rejected_malformed,
                truncated,
                omitted,
                canonical_digest,
                source,
                tree: result_tree,
            });
        }

        let query_use = self.query_for(descriptor, declaration_query_digest, deadline)?;
        let prior = self.cache.get(path);
        let same_grammar = prior.is_some_and(|cached| cached.identity.grammar == identity.grammar);
        let same_source =
            prior.is_some_and(|cached| cached.identity.source_digest == source_digest);
        let prior_tree = prior.map(|cached| cached.tree.clone());
        let prior_source = prior.map(|cached| Arc::clone(&cached.source));
        let prior_protected = prior.is_some_and(|cached| cached.protected);
        let prior_node_count = prior.map(|cached| cached.node_count);
        let (tree, action, parsed_now) = if same_grammar && same_source {
            (
                prior_tree.as_ref().expect("checked prior cache").clone(),
                ParseAction::ExtractionOnly,
                false,
            )
        } else {
            let mut edited =
                same_grammar.then(|| prior_tree.as_ref().expect("checked prior").clone());
            if let (Some(tree), Some(cached_source)) = (edited.as_mut(), prior_source.as_ref()) {
                tree.edit(&single_edit_before(
                    cached_source.as_bytes(),
                    source,
                    deadline,
                )?);
            }
            let timed_out = Cell::new(false);
            #[cfg(test)]
            let force_timeout = std::mem::take(&mut self.cancel_next_parse);
            #[cfg(not(test))]
            let force_timeout = false;
            let forced = Cell::new(force_timeout);
            let mut progress = |_: &tree_sitter::ParseState| {
                let expired = forced.replace(false) || Instant::now() >= deadline;
                timed_out.set(expired);
                expired
            };
            let parse_options = ParseOptions::new().progress_callback(&mut progress);
            let mut input = |offset: usize, _| source.get(offset..).unwrap_or_default();
            let parsed = self.runtime.parser.parse_with_options(
                &mut input,
                edited.as_ref(),
                Some(parse_options),
            );
            let Some(tree) = parsed else {
                self.runtime.parser.reset();
                return Err(if timed_out.get() {
                    SyntaxError::ParseTimeout
                } else {
                    SyntaxError::ParseFailed
                });
            };
            (
                tree,
                if edited.is_some() {
                    ParseAction::Incremental
                } else {
                    ParseAction::Full
                },
                true,
            )
        };
        let scope_query = Arc::clone(&self.runtime.scope_query);
        let built = (|| {
            check_deadline(deadline, SyntaxError::ParseTimeout)?;
            let node_count = if parsed_now {
                count_tree_nodes(&tree, deadline)?
            } else {
                prior_node_count.expect("extraction-only parse has cached node count")
            };
            let extracted = extract(
                &tree,
                source,
                canonical_path,
                revision,
                &identity.grammar,
                query_set_digest,
                descriptor.declaration_query() == RUST_QUERY,
                &query_use.query,
                &scope_query,
                options,
                deadline,
            )?;
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            let Extraction {
                records: extracted_records,
                rejected_malformed,
                truncated,
                omitted,
            } = extracted;
            let source = copy_text_before(
                std::str::from_utf8(source).map_err(|_| SyntaxError::InvalidUtf8)?,
                deadline,
            )?;
            let records: Arc<[SyntacticSymbolRecord]> = Arc::from(extracted_records);
            let has_parse_errors = tree.root_node().has_error();
            let canonical_digest = digest_result(
                &identity,
                &records,
                has_parse_errors,
                rejected_malformed,
                truncated,
                omitted,
                deadline,
            )?;
            let logical_weight = cached_file_weight(path, &source, &records, node_count);
            let admission = self.plan_cache_admission(
                path,
                logical_weight,
                query_use.projected_runtime_weight,
                deadline,
            )?;
            let target = if let Some(admission) = admission {
                CacheTarget::Resident(admission)
            } else if let Some(admission) =
                self.plan_candidate_admission(&identity.path, logical_weight, deadline)?
            {
                CacheTarget::Candidate(admission)
            } else {
                CacheTarget::None
            };
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            Ok::<_, SyntaxError>((
                rejected_malformed,
                truncated,
                omitted,
                source,
                records,
                has_parse_errors,
                canonical_digest,
                node_count,
                logical_weight,
                target,
            ))
        })();
        let (
            rejected_malformed,
            truncated,
            omitted,
            source,
            records,
            has_parse_errors,
            canonical_digest,
            node_count,
            logical_weight,
            target,
        ) = match built {
            Ok(built) => built,
            Err(error) => {
                if parsed_now {
                    self.runtime.parser.reset();
                }
                return Err(error);
            }
        };
        self.commit_query_use(query_use);
        let result_tree = tree.clone();
        let cached = CachedFile {
            identity: identity.clone(),
            source: Arc::clone(&source),
            tree,
            records: Arc::clone(&records),
            has_parse_errors,
            rejected_malformed,
            truncated,
            omitted,
            canonical_digest,
            node_count,
            logical_weight,
            last_used: 0,
            protected: prior_protected || self.snapshot_active,
        };
        match target {
            CacheTarget::Resident(admission) => {
                self.commit_cache(Arc::clone(&identity.path), cached, admission);
            }
            CacheTarget::Candidate(admission) => {
                self.commit_candidate(Arc::clone(&identity.path), cached, admission);
            }
            CacheTarget::None => {}
        }
        match action {
            ParseAction::Full => self.metrics.full_parses += 1,
            ParseAction::Incremental => self.metrics.incremental_parses += 1,
            ParseAction::ExtractionOnly => self.metrics.extraction_refreshes += 1,
            ParseAction::Reused | ParseAction::RevisionRefresh => unreachable!(),
        }
        Ok(SyntaxResult {
            identity,
            records,
            action,
            has_parse_errors,
            rejected_malformed,
            truncated,
            omitted,
            canonical_digest,
            source,
            tree: result_tree,
        })
    }

    pub(crate) fn begin_snapshot_before(&mut self, deadline: Instant) -> Result<(), SyntaxError> {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let mut expired = false;
        for cached in self.cache.values_mut() {
            if Instant::now() >= deadline {
                expired = true;
            }
            cached.protected = true;
        }
        if expired {
            for cached in self.cache.values_mut() {
                cached.protected = false;
            }
            self.snapshot_active = false;
            Err(SyntaxError::QueryTimeout)
        } else {
            self.snapshot_active = true;
            Ok(())
        }
    }

    pub(crate) fn finish_snapshot_before(
        &mut self,
        deadline: Instant,
        retained: Option<&BTreeSet<PathBuf>>,
    ) -> Result<(), SyntaxError> {
        let mut ranked = BTreeMap::new();
        for (path, cached) in &self.cache {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if retained.is_none_or(|retained| retained.contains(path.as_ref())) {
                ranked.insert(Arc::clone(path), (cached.clone(), false));
            }
        }
        for (path, cached) in &self.candidates {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if retained.is_none_or(|retained| retained.contains(path.as_ref())) {
                ranked.insert(Arc::clone(path), (cached.clone(), true));
            }
        }

        let mut selected = BTreeMap::new();
        let mut selected_weight = 0_usize;
        let mut planned_clock = self.clock;
        if retained.is_none() {
            for (path, cached) in &self.cache {
                check_deadline(deadline, SyntaxError::QueryTimeout)?;
                let mut cached = cached.clone();
                cached.protected = false;
                selected_weight += cached.logical_weight;
                selected.insert(Arc::clone(path), cached);
            }
        }
        for (path, (mut cached, candidate)) in ranked {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if selected.contains_key(path.as_ref()) {
                continue;
            }
            if selected.len() == self.limits.max_resident_files
                || self
                    .runtime
                    .logical_weight
                    .saturating_add(selected_weight)
                    .saturating_add(cached.logical_weight)
                    > self.limits.max_resident_logical_weight
            {
                continue;
            }
            if candidate {
                planned_clock = planned_clock.wrapping_add(1);
                cached.last_used = planned_clock;
            }
            cached.protected = false;
            selected_weight += cached.logical_weight;
            selected.insert(path, cached);
        }

        let mut pruned = 0_u64;
        let mut evicted = 0_u64;
        for (path, cached) in &self.cache {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if retained.is_some_and(|retained| !retained.contains(path.as_ref())) {
                pruned += 1;
            } else if selected
                .get(path.as_ref())
                .is_none_or(|selected| selected.canonical_digest != cached.canonical_digest)
            {
                evicted += 1;
            }
        }
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        self.cache = selected;
        self.logical_weight = selected_weight;
        self.candidates.clear();
        self.candidate_weight = 0;
        self.clock = planned_clock;
        self.metrics.pruned_files += pruned;
        self.metrics.evicted_files += evicted;
        self.snapshot_active = false;
        Ok(())
    }

    fn query_for(
        &mut self,
        descriptor: &LanguageDescriptor,
        digest: [u8; 32],
        deadline: Instant,
    ) -> Result<QueryUse, SyntaxError> {
        if descriptor.declaration_query() == RUST_QUERY {
            return Ok(QueryUse {
                query: Arc::clone(&self.runtime.scope_query),
                cached_digest: None,
                admission: None,
                projected_runtime_weight: self.runtime.logical_weight,
            });
        }
        if let Some(cached) = self.runtime.queries.get(&digest) {
            return Ok(QueryUse {
                query: Arc::clone(&cached.query),
                cached_digest: Some(digest),
                admission: None,
                projected_runtime_weight: self.runtime.logical_weight,
            });
        }
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let source_bytes = descriptor.declaration_query().len();
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let text = std::str::from_utf8(descriptor.declaration_query())
            .map_err(|_| SyntaxError::InvalidQuery("query is not UTF-8".to_owned()))?;
        let query = Arc::new(
            Query::new(&language, text)
                .map_err(|error| SyntaxError::InvalidQuery(error.to_string()))?,
        );
        if query.capture_index_for_name("declaration").is_none()
            || query.capture_index_for_name("name").is_none()
        {
            return Err(SyntaxError::InvalidQuery(
                "query must capture @declaration and @name".to_owned(),
            ));
        }
        #[cfg(test)]
        if std::mem::take(&mut self.expire_next_query_after_compile) {
            return Err(SyntaxError::QueryTimeout);
        }
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let logical_weight = query_logical_weight(source_bytes);
        let mandatory_source_bytes = RUST_QUERY.len();
        let custom_source_bytes = self
            .runtime
            .queries
            .values()
            .fold(0_usize, |total, cached| {
                total.saturating_add(cached.source_bytes)
            });
        let custom_weight = self
            .runtime
            .queries
            .values()
            .fold(0_usize, |total, cached| {
                total.saturating_add(cached.logical_weight)
            });
        let mandatory_weight = self.runtime.logical_weight.saturating_sub(custom_weight);
        let can_ever_retain = self.limits.max_queries > 1
            && mandatory_source_bytes.saturating_add(source_bytes) <= self.limits.max_query_bytes
            && mandatory_weight
                .saturating_add(self.logical_weight)
                .saturating_add(logical_weight)
                <= self.limits.max_resident_logical_weight;
        if !can_ever_retain {
            return Ok(QueryUse {
                query,
                cached_digest: None,
                admission: None,
                projected_runtime_weight: self.runtime.logical_weight,
            });
        }
        let mut candidates = Vec::new();
        candidates
            .try_reserve_exact(self.runtime.queries.len())
            .map_err(|_| SyntaxError::AllocationFailed)?;
        for (candidate_digest, cached) in &self.runtime.queries {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            candidates.push((cached.last_used, *candidate_digest));
        }
        candidates.sort_unstable();
        let mut count = self.runtime.queries.len() + 1;
        let mut query_bytes = mandatory_source_bytes.saturating_add(custom_source_bytes);
        let mut runtime_weight = self.runtime.logical_weight;
        let mut evictions = Vec::new();
        evictions
            .try_reserve_exact(candidates.len())
            .map_err(|_| SyntaxError::AllocationFailed)?;
        for (_, candidate_digest) in candidates {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if count < self.limits.max_queries
                && query_bytes.saturating_add(source_bytes) <= self.limits.max_query_bytes
                && self
                    .logical_weight
                    .saturating_add(runtime_weight)
                    .saturating_add(logical_weight)
                    <= self.limits.max_resident_logical_weight
            {
                break;
            }
            let cached = self
                .runtime
                .queries
                .get(&candidate_digest)
                .expect("query candidate came from the cache");
            count -= 1;
            query_bytes -= cached.source_bytes;
            runtime_weight -= cached.logical_weight;
            evictions.push(candidate_digest);
        }
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let admission = (count < self.limits.max_queries
            && query_bytes.saturating_add(source_bytes) <= self.limits.max_query_bytes
            && self
                .logical_weight
                .saturating_add(runtime_weight)
                .saturating_add(logical_weight)
                <= self.limits.max_resident_logical_weight)
            .then(|| QueryAdmission {
                digest,
                cached: CachedQuery {
                    query: Arc::clone(&query),
                    source_bytes,
                    logical_weight,
                    last_used: 0,
                },
                evictions,
            });
        let projected_runtime_weight =
            admission.as_ref().map_or(self.runtime.logical_weight, |_| {
                runtime_weight + logical_weight
            });
        Ok(QueryUse {
            query,
            cached_digest: None,
            admission,
            projected_runtime_weight,
        })
    }

    fn plan_cache_admission(
        &self,
        path: &Path,
        candidate_weight: usize,
        runtime_weight: usize,
        deadline: Instant,
    ) -> Result<Option<CacheAdmission>, SyntaxError> {
        let previous = self.cache.get(path);
        let previous_weight = previous.map_or(0, |cached| cached.logical_weight);
        let mut file_weight = self
            .logical_weight
            .saturating_sub(previous_weight)
            .saturating_add(candidate_weight);
        let mut file_count = self.cache.len() - usize::from(previous.is_some()) + 1;
        if runtime_weight.saturating_add(candidate_weight) > self.limits.max_resident_logical_weight
        {
            return Ok(None);
        }
        if file_count <= self.limits.max_resident_files
            && runtime_weight.saturating_add(file_weight) <= self.limits.max_resident_logical_weight
        {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            return Ok(Some(CacheAdmission {
                evictions: Vec::new(),
            }));
        }
        let mut candidates = Vec::new();
        candidates
            .try_reserve_exact(self.cache.len())
            .map_err(|_| SyntaxError::AllocationFailed)?;
        for (candidate_path, cached) in &self.cache {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if candidate_path.as_ref() != path && !cached.protected {
                candidates.push((
                    cached.last_used,
                    candidate_path.clone(),
                    cached.logical_weight,
                ));
            }
        }
        candidates.sort_unstable_by(|left, right| {
            left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1))
        });
        let mut evictions = Vec::new();
        evictions
            .try_reserve_exact(candidates.len())
            .map_err(|_| SyntaxError::AllocationFailed)?;
        for (_, candidate_path, weight) in candidates {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if file_count <= self.limits.max_resident_files
                && runtime_weight.saturating_add(file_weight)
                    <= self.limits.max_resident_logical_weight
            {
                break;
            }
            file_count -= 1;
            file_weight -= weight;
            evictions.push(candidate_path);
        }
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        Ok((file_count <= self.limits.max_resident_files
            && runtime_weight.saturating_add(file_weight)
                <= self.limits.max_resident_logical_weight)
            .then_some(CacheAdmission { evictions }))
    }

    fn plan_candidate_admission(
        &self,
        path: &Arc<Path>,
        candidate_weight: usize,
        deadline: Instant,
    ) -> Result<Option<CandidateAdmission>, SyntaxError> {
        if !self.snapshot_active {
            return Ok(None);
        }
        let previous = self.candidates.get(path.as_ref());
        let staged_count = self.candidates.len() - usize::from(previous.is_some()) + 1;
        let staged_weight = self
            .candidate_weight
            .saturating_sub(previous.map_or(0, |cached| cached.logical_weight))
            .saturating_add(candidate_weight);
        if staged_count <= self.limits.max_staging_files
            && staged_weight <= self.limits.max_staging_logical_weight
        {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            return Ok(Some(CandidateAdmission {
                removals: Vec::new(),
            }));
        }
        let mut ranked = Vec::new();
        ranked
            .try_reserve_exact(self.candidates.len().saturating_add(1))
            .map_err(|_| SyntaxError::AllocationFailed)?;
        for (candidate_path, cached) in &self.candidates {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if candidate_path != path {
                ranked.push((Arc::clone(candidate_path), cached.logical_weight, false));
            }
        }
        ranked.push((Arc::clone(path), candidate_weight, true));
        ranked.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let mut selected = BTreeSet::new();
        let mut selected_weight = 0_usize;
        let mut selected_count = 0_usize;
        let mut candidate_selected = false;
        for (candidate_path, weight, is_candidate) in ranked {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if selected_count < self.limits.max_staging_files
                && selected_weight.saturating_add(weight) <= self.limits.max_staging_logical_weight
            {
                selected_count += 1;
                selected_weight += weight;
                candidate_selected |= is_candidate;
                selected.insert(candidate_path);
            }
        }
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        if !candidate_selected {
            return Ok(None);
        }
        let mut removals = Vec::new();
        removals
            .try_reserve_exact(self.candidates.len())
            .map_err(|_| SyntaxError::AllocationFailed)?;
        for candidate_path in self.candidates.keys() {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if !selected.contains(candidate_path) {
                removals.push(Arc::clone(candidate_path));
            }
        }
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        Ok(Some(CandidateAdmission { removals }))
    }

    fn commit_query_use(&mut self, query_use: QueryUse) {
        if let Some(digest) = query_use.cached_digest {
            let clock = self.next_clock();
            self.runtime
                .queries
                .get_mut(&digest)
                .expect("cached query was planned")
                .last_used = clock;
        }
        if let Some(mut admission) = query_use.admission {
            for digest in admission.evictions {
                let cached = self
                    .runtime
                    .queries
                    .remove(&digest)
                    .expect("query eviction was planned");
                self.runtime.query_source_bytes -= cached.source_bytes;
                self.runtime.logical_weight -= cached.logical_weight;
            }
            admission.cached.last_used = self.next_clock();
            self.runtime.query_source_bytes += admission.cached.source_bytes;
            self.runtime.logical_weight += admission.cached.logical_weight;
            self.runtime
                .queries
                .insert(admission.digest, admission.cached);
        }
    }

    fn commit_cache(&mut self, path: Arc<Path>, mut cached: CachedFile, admission: CacheAdmission) {
        for evicted in admission.evictions {
            self.remove_cache(&evicted)
                .expect("cache eviction was planned");
            self.metrics.evicted_files += 1;
        }
        self.remove_candidate(path.as_ref());
        self.remove_cache(&path);
        cached.last_used = self.next_clock();
        self.logical_weight += cached.logical_weight;
        self.cache.insert(path, cached);
    }

    fn commit_candidate(
        &mut self,
        path: Arc<Path>,
        mut cached: CachedFile,
        admission: CandidateAdmission,
    ) {
        for removed in admission.removals {
            self.remove_candidate(removed.as_ref());
        }
        self.remove_candidate(path.as_ref());
        cached.protected = false;
        self.candidate_weight += cached.logical_weight;
        self.candidates.insert(path, cached);
    }

    fn remove_cache(&mut self, path: &Path) -> Option<CachedFile> {
        let cached = self.cache.remove(path)?;
        self.logical_weight -= cached.logical_weight;
        Some(cached)
    }

    fn remove_candidate(&mut self, path: &Path) -> Option<CachedFile> {
        let cached = self.candidates.remove(path)?;
        self.candidate_weight -= cached.logical_weight;
        Some(cached)
    }

    fn next_clock(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }
}

struct Extraction {
    records: Vec<SyntacticSymbolRecord>,
    rejected_malformed: usize,
    truncated: bool,
    omitted: usize,
}

struct Selection {
    ranges: BTreeSet<(usize, usize)>,
    rejected_malformed: usize,
    truncated: bool,
}

struct RawScope<'tree> {
    node: Node<'tree>,
    name: Arc<String>,
    kind: SyntacticSymbolKind,
    range: SourceRange,
    parent: Option<usize>,
    qualified: Arc<String>,
    id: [u8; 32],
}

struct ScopeCollection<'tree> {
    scopes: Vec<RawScope<'tree>>,
    ranges: BTreeSet<(usize, usize)>,
    rejected_malformed: usize,
    truncated: bool,
    logical_weight: usize,
}

#[allow(clippy::too_many_arguments)]
fn extract(
    tree: &Tree,
    source: &[u8],
    canonical_path: Arc<Path>,
    revision: RevisionId,
    grammar: &GrammarIdentity,
    query_set_digest: [u8; 32],
    pinned_selection: bool,
    declaration_query: &Query,
    scope_query: &Query,
    options: &SyntaxOptions,
    deadline: Instant,
) -> Result<Extraction, SyntaxError> {
    let recovery_fence = malformed_recovery_fence(tree, source, deadline)?;
    let ScopeCollection {
        mut scopes,
        ranges,
        rejected_malformed: scope_rejected,
        truncated: scope_truncated,
        logical_weight: mut working_weight,
    } = collect_scopes(tree, source, scope_query, recovery_fence, options, deadline)?;
    let selection = if pinned_selection {
        Selection {
            ranges,
            rejected_malformed: scope_rejected,
            truncated: scope_truncated,
        }
    } else {
        select_declarations(
            tree,
            source,
            declaration_query,
            recovery_fence,
            options,
            deadline,
        )?
    };
    scopes.sort_by(|left, right| {
        left.range
            .start_byte
            .cmp(&right.range.start_byte)
            .then_with(|| right.range.end_byte.cmp(&left.range.end_byte))
            .then_with(|| left.name.cmp(&right.name))
    });
    let mut stack = Vec::new();
    stack
        .try_reserve_exact(scopes.len())
        .map_err(|_| SyntaxError::AllocationFailed)?;
    let mut duplicate_ids: BTreeMap<(SyntacticSymbolKind, Arc<String>), usize> = BTreeMap::new();
    let mut impl_owners: BTreeMap<(usize, usize), Arc<String>> = BTreeMap::new();
    let mut qualification_truncated = false;
    let mut processed = 0_usize;
    for index in 0..scopes.len() {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        while stack.last().is_some_and(|parent: &usize| {
            scopes[*parent].range.end_byte < scopes[index].range.end_byte
        }) {
            stack.pop();
        }
        scopes[index].parent = stack.last().copied();
        let owner = match enclosing_impl_owner(
            scopes[index].node,
            source,
            &mut impl_owners,
            &mut working_weight,
            options.max_scope_weight,
            deadline,
        )? {
            OwnerLookup::None => None,
            OwnerLookup::Found { owner, start } => {
                if scopes[index]
                    .parent
                    .is_some_and(|parent| scopes[parent].range.start_byte >= start)
                {
                    None
                } else {
                    Some(owner)
                }
            }
            OwnerLookup::Limit => {
                qualification_truncated = true;
                break;
            }
        };
        let qualified_len = match (scopes[index].parent, owner.as_ref()) {
            (Some(parent), Some(owner)) => scopes[parent]
                .qualified
                .len()
                .saturating_add(owner.len())
                .saturating_add(scopes[index].name.len())
                .saturating_add(4),
            (Some(parent), None) => scopes[parent]
                .qualified
                .len()
                .saturating_add(scopes[index].name.len())
                .saturating_add(2),
            (None, Some(owner)) => owner
                .len()
                .saturating_add(scopes[index].name.len())
                .saturating_add(2),
            (None, None) => scopes[index].name.len(),
        };
        let qualified_weight = if scopes[index].parent.is_none() && owner.is_none() {
            0
        } else {
            qualified_len.saturating_add(ARC_ALLOCATION_OVERHEAD)
        };
        let new_key_weight = size_of::<((SyntacticSymbolKind, Arc<String>), usize)>()
            .saturating_add(BTREE_ENTRY_LOGICAL_OVERHEAD);
        if working_weight
            .saturating_add(qualified_weight)
            .saturating_add(new_key_weight)
            > options.max_scope_weight
        {
            qualification_truncated = true;
            break;
        }
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let qualified: Arc<String> = match (scopes[index].parent, owner) {
            (Some(parent), Some(owner)) => copy_text_parts_before(
                &[
                    &scopes[parent].qualified,
                    "::",
                    &owner,
                    "::",
                    &scopes[index].name,
                ],
                qualified_len,
                deadline,
            )?,
            (Some(parent), None) => copy_text_parts_before(
                &[&scopes[parent].qualified, "::", &scopes[index].name],
                qualified_len,
                deadline,
            )?,
            (None, Some(owner)) => copy_text_parts_before(
                &[&owner, "::", &scopes[index].name],
                qualified_len,
                deadline,
            )?,
            (None, None) => Arc::clone(&scopes[index].name),
        };
        let key = (scopes[index].kind, Arc::clone(&qualified));
        working_weight = working_weight
            .saturating_add(qualified_weight)
            .saturating_add(new_key_weight);
        scopes[index].qualified = qualified;
        let duplicate = duplicate_ids.entry(key).or_insert(0_usize);
        scopes[index].id = declaration_id(
            &canonical_path,
            scopes[index].kind,
            &scopes[index].qualified,
            *duplicate,
            deadline,
        )?;
        *duplicate += 1;
        stack.push(index);
        processed += 1;
    }
    scopes.truncate(processed);

    let grammar_identity = grammar_identity(grammar);
    let mut records = Vec::new();
    records
        .try_reserve_exact(options.max_symbols.min(selection.ranges.len()))
        .map_err(|_| SyntaxError::AllocationFailed)?;
    let mut symbol_bytes = 0_usize;
    let mut selected_clean = 0_usize;
    for scope in &scopes {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        if !selection
            .ranges
            .contains(&(scope.range.start_byte, scope.range.end_byte))
        {
            continue;
        }
        selected_clean += 1;
        if records.len() == options.max_symbols {
            continue;
        }
        let fixed = scope.name.len().saturating_add(scope.qualified.len());
        if symbol_bytes.saturating_add(fixed) > options.max_symbol_bytes {
            continue;
        }
        let remaining = options.max_symbol_bytes - symbol_bytes - fixed;
        let signature_end = scope
            .node
            .child_by_field_name("body")
            .map_or(scope.range.end_byte, |body| body.start_byte());
        let signature = &source[scope.range.start_byte..signature_end];
        let declaration = &source[scope.range.start_byte..scope.range.end_byte];
        let signature_limit = remaining / 2;
        let signature = bounded_text(signature.trim_ascii_end(), signature_limit, deadline)?;
        let declaration_limit = remaining - signature.text.len();
        let declaration = bounded_text(declaration, declaration_limit, deadline)?;
        symbol_bytes += fixed + signature.text.len() + declaration.text.len();
        let provenance = SyntacticProvenance {
            source: FactSource::Syntactic,
            confidence_millis: 1_000,
            revision,
            range: scope.range,
            grammar_identity,
            query_set_digest,
        };
        records.push(SyntacticSymbolRecord {
            declaration_id: scope.id,
            workspace_revision: revision,
            canonical_path: Arc::clone(&canonical_path),
            language: SyntaxLanguage::Rust,
            qualified_name: fact(Arc::clone(&scope.qualified), &provenance),
            display_name: fact(Arc::clone(&scope.name), &provenance),
            kind: fact(scope.kind, &provenance),
            signature: fact(signature, &provenance),
            declaration: fact(declaration, &provenance),
            range: scope.range,
            enclosing_symbol: scope
                .parent
                .map(|parent| fact(scopes[parent].id, &provenance)),
            imports: unavailable(),
            exports: unavailable(),
            definitions: SyntacticFacts::Available(Arc::from([fact(scope.range, &provenance)])),
            references: unavailable(),
            callers: unavailable(),
            callees: unavailable(),
            tests: unavailable(),
            documentation: unavailable(),
        });
    }
    let truncated = selection.truncated || scope_truncated || qualification_truncated;
    let omitted = selection
        .ranges
        .len()
        .saturating_sub(records.len())
        .saturating_add(usize::from(selection.truncated || scope_truncated));
    Ok(Extraction {
        records,
        rejected_malformed: selection.rejected_malformed,
        truncated: truncated || omitted != 0 || selected_clean < selection.ranges.len(),
        omitted,
    })
}

fn select_declarations(
    tree: &Tree,
    source: &[u8],
    query: &Query,
    recovery_fence: Option<usize>,
    options: &SyntaxOptions,
    deadline: Instant,
) -> Result<Selection, SyntaxError> {
    let declaration_capture = query
        .capture_index_for_name("declaration")
        .ok_or_else(|| SyntaxError::InvalidQuery("missing @declaration capture".to_owned()))?;
    let name_capture = query
        .capture_index_for_name("name")
        .ok_or_else(|| SyntaxError::InvalidQuery("missing @name capture".to_owned()))?;
    let timed_out = Cell::new(false);
    let mut progress = |_: &tree_sitter::QueryCursorState| {
        let expired = Instant::now() >= deadline;
        timed_out.set(expired);
        expired
    };
    let query_options = QueryCursorOptions::new().progress_callback(&mut progress);
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches_with_options(query, tree.root_node(), source, query_options);
    let mut ranges = BTreeSet::new();
    let mut rejected_malformed = 0;
    let mut truncated = false;
    let mut captures = 0_usize;
    while let Some(found) = matches.next() {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        if captures == options.max_captures {
            truncated = true;
            break;
        }
        captures += 1;
        let declaration = found.nodes_for_capture_index(declaration_capture).next();
        let name = found.nodes_for_capture_index(name_capture).next();
        let Some((declaration, name)) = declaration.zip(name) else {
            rejected_malformed += 1;
            continue;
        };
        if invalid_capture(declaration, name)
            || recovery_fence.is_some_and(|start| declaration.start_byte() >= start)
            || malformed_owner_ancestor(declaration, deadline)?
        {
            rejected_malformed += 1;
            continue;
        }
        let range = (declaration.start_byte(), declaration.end_byte());
        ranges.insert(range);
    }
    drop(matches);
    if timed_out.get() {
        return Err(SyntaxError::QueryTimeout);
    }
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    Ok(Selection {
        ranges,
        rejected_malformed,
        truncated,
    })
}

fn collect_scopes<'tree>(
    tree: &'tree Tree,
    source: &[u8],
    query: &Query,
    recovery_fence: Option<usize>,
    options: &SyntaxOptions,
    deadline: Instant,
) -> Result<ScopeCollection<'tree>, SyntaxError> {
    let declaration_capture = query
        .capture_index_for_name("declaration")
        .expect("pinned scope query has declaration capture");
    let name_capture = query
        .capture_index_for_name("name")
        .expect("pinned scope query has name capture");
    let timed_out = Cell::new(false);
    let mut progress = |_: &tree_sitter::QueryCursorState| {
        let expired = Instant::now() >= deadline;
        timed_out.set(expired);
        expired
    };
    let query_options = QueryCursorOptions::new().progress_callback(&mut progress);
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches_with_options(query, tree.root_node(), source, query_options);
    let mut by_range: BTreeMap<(usize, usize), RawScope<'tree>> = BTreeMap::new();
    let mut logical_weight = size_of::<BTreeMap<(usize, usize), RawScope<'_>>>()
        .saturating_add(size_of::<BTreeSet<(usize, usize)>>());
    let mut truncated = false;
    let mut rejected_malformed = 0_usize;
    let mut captures = 0_usize;
    while let Some(found) = matches.next() {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        if captures == options.max_captures {
            truncated = true;
            break;
        }
        captures += 1;
        let declaration = found.nodes_for_capture_index(declaration_capture).next();
        let name = found.nodes_for_capture_index(name_capture).next();
        let Some((declaration, name)) = declaration.zip(name) else {
            rejected_malformed += 1;
            continue;
        };
        if invalid_capture(declaration, name)
            || recovery_fence.is_some_and(|start| declaration.start_byte() >= start)
            || malformed_owner_ancestor(declaration, deadline)?
        {
            rejected_malformed += 1;
            continue;
        }
        let range = (declaration.start_byte(), declaration.end_byte());
        if by_range.contains_key(&range) {
            continue;
        }
        let name = name
            .utf8_text(source)
            .map_err(|_| SyntaxError::InvalidUtf8)?;
        let candidate_weight = name
            .len()
            .saturating_add(ARC_ALLOCATION_OVERHEAD.saturating_mul(2))
            .saturating_add(size_of::<RawScope<'_>>())
            .saturating_add(size_of::<((usize, usize), RawScope<'_>)>())
            .saturating_add(size_of::<(usize, usize)>())
            .saturating_add(size_of::<usize>())
            .saturating_add(BTREE_ENTRY_LOGICAL_OVERHEAD.saturating_mul(2));
        if logical_weight.saturating_add(candidate_weight) > options.max_scope_weight {
            truncated = true;
            break;
        }
        logical_weight = logical_weight.saturating_add(candidate_weight);
        let name = copy_text_before(name, deadline)?;
        let Some(kind) = symbol_kind(declaration.kind()) else {
            continue;
        };
        by_range.insert(
            range,
            RawScope {
                node: declaration,
                name,
                kind,
                range: node_range(declaration),
                parent: None,
                qualified: Arc::new(String::new()),
                id: [0; 32],
            },
        );
    }
    drop(matches);
    if timed_out.get() {
        return Err(SyntaxError::QueryTimeout);
    }
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    let mut ranges = BTreeSet::new();
    let mut scopes = Vec::new();
    scopes
        .try_reserve_exact(by_range.len())
        .map_err(|_| SyntaxError::AllocationFailed)?;
    for (range, scope) in by_range {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        ranges.insert(range);
        scopes.push(scope);
    }
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    Ok(ScopeCollection {
        scopes,
        ranges,
        rejected_malformed,
        truncated,
        logical_weight,
    })
}

fn invalid_capture(declaration: Node<'_>, name: Node<'_>) -> bool {
    invalid_node(declaration)
        || invalid_node(name)
        || name.start_byte() < declaration.start_byte()
        || name.end_byte() > declaration.end_byte()
}

fn invalid_node(node: Node<'_>) -> bool {
    node.has_error() || node.is_error() || node.is_missing()
}

fn malformed_recovery_fence(
    tree: &Tree,
    source: &[u8],
    deadline: Instant,
) -> Result<Option<usize>, SyntaxError> {
    let mut cursor = tree.walk();
    let mut fence: Option<usize> = None;
    loop {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let node = cursor.node();
        if node.is_error() {
            let mut braces = 0_i64;
            for chunk in source[node.byte_range()].chunks(64 * 1024) {
                check_deadline(deadline, SyntaxError::QueryTimeout)?;
                for byte in chunk {
                    braces += i64::from(*byte == b'{');
                    braces -= i64::from(*byte == b'}');
                }
            }
            if braces > 0 {
                fence = Some(fence.map_or(node.start_byte(), |start| start.min(node.start_byte())));
            }
        }
        if cursor.goto_first_child() || cursor.goto_next_sibling() {
            continue;
        }
        loop {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if !cursor.goto_parent() {
                return Ok(fence);
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn malformed_owner_ancestor(mut node: Node<'_>, deadline: Instant) -> Result<bool, SyntaxError> {
    while let Some(parent) = node.parent() {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        if (parent.kind() == "impl_item" || symbol_kind(parent.kind()).is_some())
            && invalid_node(parent)
        {
            return Ok(true);
        }
        node = parent;
    }
    Ok(false)
}

enum OwnerLookup {
    None,
    Found { owner: Arc<String>, start: usize },
    Limit,
}

fn enclosing_impl_owner(
    mut node: Node<'_>,
    source: &[u8],
    owners: &mut BTreeMap<(usize, usize), Arc<String>>,
    logical_weight: &mut usize,
    max_weight: usize,
    deadline: Instant,
) -> Result<OwnerLookup, SyntaxError> {
    let implementation = loop {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let Some(parent) = node.parent() else {
            return Ok(OwnerLookup::None);
        };
        if parent.kind() == "impl_item" {
            break parent;
        }
        node = parent;
    };
    let range = implementation.byte_range();
    let key = (range.start, range.end);
    if let Some(owner) = owners.get(&key) {
        return Ok(OwnerLookup::Found {
            owner: Arc::clone(owner),
            start: range.start,
        });
    }
    let Some(type_node) = implementation.child_by_field_name("type") else {
        return Ok(OwnerLookup::None);
    };
    let ty = type_node
        .utf8_text(source)
        .map_err(|_| SyntaxError::InvalidUtf8)?;
    let trait_name = implementation
        .child_by_field_name("trait")
        .map(|trait_node| {
            trait_node
                .utf8_text(source)
                .map_err(|_| SyntaxError::InvalidUtf8)
        })
        .transpose()?;
    let owner_len = trait_name.map_or(ty.len(), |trait_name| {
        ty.len().saturating_add(trait_name.len()).saturating_add(6)
    });
    let added = owner_len
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
        .saturating_add(size_of::<((usize, usize), Arc<String>)>())
        .saturating_add(BTREE_ENTRY_LOGICAL_OVERHEAD);
    if logical_weight.saturating_add(added) > max_weight {
        return Ok(OwnerLookup::Limit);
    }
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    let owner = if let Some(trait_name) = trait_name {
        copy_text_parts_before(&["<", ty, " as ", trait_name, ">"], owner_len, deadline)?
    } else {
        copy_text_before(ty, deadline)?
    };
    *logical_weight = logical_weight.saturating_add(added);
    owners.insert(key, Arc::clone(&owner));
    Ok(OwnerLookup::Found {
        owner,
        start: range.start,
    })
}

fn bounded_text(
    source: &[u8],
    max: usize,
    deadline: Instant,
) -> Result<SyntacticText, SyntaxError> {
    let mut end = source.len().min(max);
    while end > 0 && !char_boundary(source, end) {
        end -= 1;
    }
    let text = copy_text_before(
        std::str::from_utf8(&source[..end]).map_err(|_| SyntaxError::InvalidUtf8)?,
        deadline,
    )?;
    Ok(SyntacticText {
        text,
        truncated: end < source.len(),
    })
}

fn copy_text_before(source: &str, deadline: Instant) -> Result<Arc<String>, SyntaxError> {
    copy_text_parts_before(&[source], source.len(), deadline)
}

fn copy_text_parts_before(
    parts: &[&str],
    length: usize,
    deadline: Instant,
) -> Result<Arc<String>, SyntaxError> {
    let mut text = String::new();
    text.try_reserve_exact(length)
        .map_err(|_| SyntaxError::AllocationFailed)?;
    for source in parts {
        let mut start = 0_usize;
        while start < source.len() {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            let mut end = start.saturating_add(64 * 1024).min(source.len());
            while end < source.len() && !source.is_char_boundary(end) {
                end -= 1;
            }
            text.push_str(&source[start..end]);
            start = end;
        }
    }
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    let text = Arc::new(text);
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    Ok(text)
}

fn fact<T>(value: T, provenance: &SyntacticProvenance) -> SyntacticFact<T> {
    SyntacticFact {
        value,
        provenance: provenance.clone(),
    }
}

fn unavailable<T>() -> SyntacticFacts<T> {
    SyntacticFacts::Unavailable(UnavailableReason::NotExtracted)
}

fn rebind_records(
    records: &[SyntacticSymbolRecord],
    revision: RevisionId,
    deadline: Instant,
) -> Result<Vec<SyntacticSymbolRecord>, SyntaxError> {
    let mut rebound = Vec::new();
    rebound
        .try_reserve_exact(records.len())
        .map_err(|_| SyntaxError::AllocationFailed)?;
    for record in records {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let mut record = record.clone();
        record.workspace_revision = revision;
        rebind_fact(&mut record.qualified_name, revision);
        rebind_fact(&mut record.display_name, revision);
        rebind_fact(&mut record.kind, revision);
        rebind_fact(&mut record.signature, revision);
        rebind_fact(&mut record.declaration, revision);
        if let Some(enclosing) = &mut record.enclosing_symbol {
            rebind_fact(enclosing, revision);
        }
        rebind_facts(&mut record.imports, revision, deadline)?;
        rebind_facts(&mut record.exports, revision, deadline)?;
        rebind_facts(&mut record.definitions, revision, deadline)?;
        rebind_facts(&mut record.references, revision, deadline)?;
        rebind_facts(&mut record.callers, revision, deadline)?;
        rebind_facts(&mut record.callees, revision, deadline)?;
        rebind_facts(&mut record.tests, revision, deadline)?;
        rebind_facts(&mut record.documentation, revision, deadline)?;
        rebound.push(record);
    }
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    Ok(rebound)
}

fn rebind_fact<T>(fact: &mut SyntacticFact<T>, revision: RevisionId) {
    fact.provenance.revision = revision;
}

fn rebind_facts<T: Clone>(
    facts: &mut SyntacticFacts<T>,
    revision: RevisionId,
    deadline: Instant,
) -> Result<(), SyntaxError> {
    if let SyntacticFacts::Available(facts) = facts {
        for fact in Arc::make_mut(facts) {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            rebind_fact(fact, revision);
        }
    }
    Ok(())
}

fn validate(
    path: &Path,
    language: &str,
    source: &[u8],
    descriptor: &LanguageDescriptor,
    options: &SyntaxOptions,
    deadline: Instant,
) -> Result<(), SyntaxError> {
    check_deadline(deadline, SyntaxError::ParseTimeout)?;
    if options.max_path_bytes == 0
        || options.max_source_bytes == 0
        || options.max_query_bytes == 0
        || options.max_captures == 0
        || options.max_scope_weight == 0
        || options.max_symbols == 0
        || options.max_symbol_bytes == 0
    {
        return Err(SyntaxError::InvalidOptions("all bounds must be nonzero"));
    }
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(SyntaxError::UnsafePath(path.to_owned()));
    }
    for component in path.components() {
        check_deadline(deadline, SyntaxError::ParseTimeout)?;
        if !matches!(component, Component::Normal(_)) {
            return Err(SyntaxError::UnsafePath(path.to_owned()));
        }
    }
    if path.as_os_str().as_encoded_bytes().len() > options.max_path_bytes {
        return Err(SyntaxError::UnsafePath(path.to_owned()));
    }
    if language != descriptor.language().as_str() || language != "rust" {
        return Err(SyntaxError::UnsupportedLanguage(language.to_owned()));
    }
    if source.len() > options.max_source_bytes {
        return Err(SyntaxError::SourceTooLarge {
            bytes: source.len(),
            max: options.max_source_bytes,
        });
    }
    if descriptor.declaration_query().len() > options.max_query_bytes {
        return Err(SyntaxError::QueryTooLarge {
            bytes: descriptor.declaration_query().len(),
            max: options.max_query_bytes,
        });
    }
    validate_utf8_before(source, deadline, SyntaxError::ParseTimeout)?;
    validate_utf8_before(
        descriptor.declaration_query(),
        deadline,
        SyntaxError::QueryTimeout,
    )
    .map_err(|error| match error {
        SyntaxError::InvalidUtf8 => SyntaxError::InvalidQuery("query is not UTF-8".to_owned()),
        error => error,
    })?;
    check_deadline(deadline, SyntaxError::ParseTimeout)?;
    Ok(())
}

fn validate_utf8_before(
    bytes: &[u8],
    deadline: Instant,
    timeout: SyntaxError,
) -> Result<(), SyntaxError> {
    let query_timeout = matches!(timeout, SyntaxError::QueryTimeout);
    let mut start = 0;
    while start < bytes.len() {
        check_deadline(
            deadline,
            if query_timeout {
                SyntaxError::QueryTimeout
            } else {
                SyntaxError::ParseTimeout
            },
        )?;
        let mut end = start.saturating_add(64 * 1024).min(bytes.len());
        while end < bytes.len() && !char_boundary(bytes, end) {
            end -= 1;
        }
        if end == start {
            return Err(SyntaxError::InvalidUtf8);
        }
        std::str::from_utf8(&bytes[start..end]).map_err(|_| SyntaxError::InvalidUtf8)?;
        start = end;
    }
    Ok(())
}

fn checked_digest(
    bytes: &[u8],
    deadline: Instant,
    timeout: SyntaxError,
) -> Result<[u8; 32], SyntaxError> {
    let query_timeout = matches!(timeout, SyntaxError::QueryTimeout);
    let mut hash = blake3::Hasher::new();
    for chunk in bytes.chunks(64 * 1024) {
        check_deadline(
            deadline,
            if query_timeout {
                SyntaxError::QueryTimeout
            } else {
                SyntaxError::ParseTimeout
            },
        )?;
        hash.update(chunk);
    }
    check_deadline(
        deadline,
        if query_timeout {
            SyntaxError::QueryTimeout
        } else {
            SyntaxError::ParseTimeout
        },
    )?;
    Ok(*hash.finalize().as_bytes())
}

fn query_set_digest_before(declaration: &[u8], deadline: Instant) -> Result<[u8; 32], SyntaxError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-rust-query-set-v1\0");
    for query in [declaration, RUST_QUERY] {
        hash.update(&(query.len() as u64).to_le_bytes());
        for chunk in query.chunks(64 * 1024) {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            hash.update(chunk);
        }
    }
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    Ok(*hash.finalize().as_bytes())
}

fn query_set_digest_unchecked(declaration: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-rust-query-set-v1\0");
    for query in [declaration, RUST_QUERY] {
        frame(&mut hash, query);
    }
    *hash.finalize().as_bytes()
}

fn extraction_digest(query_set_digest: [u8; 32], options: &SyntaxOptions) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-syntax-extraction-v1\0");
    hash.update(&query_set_digest);
    for value in [
        options.max_captures,
        options.max_scope_weight,
        options.max_symbols,
        options.max_symbol_bytes,
    ] {
        hash.update(&(value as u128).to_le_bytes());
    }
    *hash.finalize().as_bytes()
}

fn grammar_identity(grammar: &GrammarIdentity) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-syntax-grammar-v2\0");
    hash.update(&(grammar.abi as u128).to_le_bytes());
    frame(&mut hash, grammar.version.as_bytes());
    frame(&mut hash, grammar.artifact_digest.as_bytes());
    *hash.finalize().as_bytes()
}

fn symbol_kind(kind: &str) -> Option<SyntacticSymbolKind> {
    Some(match kind {
        "function_item" => SyntacticSymbolKind::Function,
        "function_signature_item" => SyntacticSymbolKind::Function,
        "struct_item" => SyntacticSymbolKind::Struct,
        "enum_item" => SyntacticSymbolKind::Enum,
        "union_item" => SyntacticSymbolKind::Union,
        "type_item" => SyntacticSymbolKind::TypeAlias,
        "trait_item" => SyntacticSymbolKind::Trait,
        "mod_item" => SyntacticSymbolKind::Module,
        "const_item" => SyntacticSymbolKind::Constant,
        "static_item" => SyntacticSymbolKind::Static,
        "macro_definition" => SyntacticSymbolKind::Macro,
        _ => return None,
    })
}

fn count_tree_nodes(tree: &Tree, deadline: Instant) -> Result<usize, SyntaxError> {
    let mut cursor = tree.walk();
    let mut count = 1_usize;
    loop {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        if cursor.goto_first_child() || cursor.goto_next_sibling() {
            count = count.saturating_add(1);
            continue;
        }
        loop {
            check_deadline(deadline, SyntaxError::QueryTimeout)?;
            if !cursor.goto_parent() {
                return Ok(count);
            }
            if cursor.goto_next_sibling() {
                count = count.saturating_add(1);
                break;
            }
        }
    }
}

fn node_range(node: Node<'_>) -> SourceRange {
    SourceRange {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

fn single_edit_before(old: &[u8], new: &[u8], deadline: Instant) -> Result<InputEdit, SyntaxError> {
    let common = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < common {
        check_deadline(deadline, SyntaxError::ParseTimeout)?;
        let end = prefix.saturating_add(64 * 1024).min(common);
        if let Some(difference) = old[prefix..end]
            .iter()
            .zip(&new[prefix..end])
            .position(|(left, right)| left != right)
        {
            prefix += difference;
            break;
        }
        prefix = end;
    }
    while !char_boundary(old, prefix) || !char_boundary(new, prefix) {
        prefix -= 1;
    }
    let suffix_limit = (old.len() - prefix).min(new.len() - prefix);
    let mut suffix = 0;
    while suffix < suffix_limit {
        check_deadline(deadline, SyntaxError::ParseTimeout)?;
        let take = (64 * 1024).min(suffix_limit - suffix);
        let old_start = old.len() - suffix - take;
        let new_start = new.len() - suffix - take;
        if let Some(difference) = old[old_start..old_start + take]
            .iter()
            .rev()
            .zip(new[new_start..new_start + take].iter().rev())
            .position(|(left, right)| left != right)
        {
            suffix += difference;
            break;
        }
        suffix += take;
    }
    while !char_boundary(old, old.len() - suffix) || !char_boundary(new, new.len() - suffix) {
        suffix -= 1;
    }
    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;
    Ok(InputEdit {
        start_byte: prefix,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_before(old, prefix, deadline)?,
        old_end_position: point_before(old, old_end, deadline)?,
        new_end_position: point_before(new, new_end, deadline)?,
    })
}

fn char_boundary(source: &[u8], index: usize) -> bool {
    index == source.len() || source[index] & 0b1100_0000 != 0b1000_0000
}

fn point_before(source: &[u8], offset: usize, deadline: Instant) -> Result<Point, SyntaxError> {
    let mut row = 0;
    let mut line_start = 0;
    for (chunk_index, chunk) in source[..offset].chunks(64 * 1024).enumerate() {
        check_deadline(deadline, SyntaxError::ParseTimeout)?;
        let chunk_start = chunk_index * 64 * 1024;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'\n' {
                row += 1;
                line_start = chunk_start + index + 1;
            }
        }
    }
    check_deadline(deadline, SyntaxError::ParseTimeout)?;
    Ok(Point {
        row,
        column: offset - line_start,
    })
}

fn declaration_id(
    path: &Path,
    kind: SyntacticSymbolKind,
    qualified: &str,
    duplicate: usize,
    deadline: Instant,
) -> Result<[u8; 32], SyntaxError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-syntax-declaration-v2\0");
    frame_before(&mut hash, path.as_os_str().as_encoded_bytes(), deadline)?;
    hash.update(&[kind as u8]);
    frame_before(&mut hash, qualified.as_bytes(), deadline)?;
    hash.update(&(duplicate as u128).to_le_bytes());
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    Ok(*hash.finalize().as_bytes())
}

fn digest_result(
    identity: &SyntaxCacheIdentity,
    records: &[SyntacticSymbolRecord],
    has_errors: bool,
    rejected_malformed: usize,
    truncated: bool,
    omitted: usize,
    deadline: Instant,
) -> Result<[u8; 32], SyntaxError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-syntax-result-v2\0");
    hash.update(&identity.canonical_digest_before(deadline)?);
    hash.update(&[u8::from(has_errors), u8::from(truncated)]);
    hash.update(&(rejected_malformed as u128).to_le_bytes());
    hash.update(&(omitted as u128).to_le_bytes());
    hash.update(&(records.len() as u128).to_le_bytes());
    for record in records {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        let digest = record.canonical_digest_before(deadline)?;
        hash.update(&digest);
    }
    check_deadline(deadline, SyntaxError::QueryTimeout)?;
    Ok(*hash.finalize().as_bytes())
}

fn cached_file_weight(
    path: &Path,
    source: &str,
    records: &[SyntacticSymbolRecord],
    node_count: usize,
) -> usize {
    path.as_os_str()
        .as_encoded_bytes()
        .len()
        .saturating_mul(2)
        .saturating_add(size_of::<CachedFile>())
        .saturating_add(size_of::<(Arc<Path>, CachedFile)>())
        .saturating_add(BTREE_ENTRY_LOGICAL_OVERHEAD)
        .saturating_add(source.len())
        .saturating_add(node_count.saturating_mul(TREE_NODE_LOGICAL_WEIGHT))
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
        .saturating_add(records.iter().fold(0_usize, |total, record| {
            total.saturating_add(syntactic_record_logical_weight(record))
        }))
}

fn runtime_base_weight(query_source_bytes: usize) -> usize {
    PARSER_OPAQUE_LOGICAL_WEIGHT
        .saturating_add(QUERY_OPAQUE_LOGICAL_WEIGHT)
        .saturating_add(query_source_bytes)
        .saturating_add(size_of::<RustRuntime>())
        .saturating_add(size_of::<Parser>())
        .saturating_add(size_of::<BTreeMap<[u8; 32], CachedQuery>>())
        .saturating_add(RUST_GRAMMAR_VERSION.len())
        .saturating_add(RUST_GRAMMAR_ARTIFACT_DIGEST.len())
        .saturating_add(ARC_ALLOCATION_OVERHEAD.saturating_mul(2))
}

fn query_logical_weight(source_bytes: usize) -> usize {
    size_of::<CachedQuery>()
        .saturating_add(size_of::<([u8; 32], CachedQuery)>())
        .saturating_add(BTREE_ENTRY_LOGICAL_OVERHEAD)
        .saturating_add(QUERY_OPAQUE_LOGICAL_WEIGHT)
        .saturating_add(source_bytes)
}

fn syntactic_record_logical_weight(record: &SyntacticSymbolRecord) -> usize {
    size_of::<SyntacticSymbolRecord>()
        .saturating_add(record.qualified_name.value.len())
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
        .saturating_add(record.display_name.value.len())
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
        .saturating_add(record.signature.value.text.len())
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
        .saturating_add(record.declaration.value.text.len())
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
        .saturating_add(match &record.definitions {
            SyntacticFacts::Available(facts) => ARC_ALLOCATION_OVERHEAD.saturating_add(
                facts
                    .len()
                    .saturating_mul(size_of::<SyntacticFact<SourceRange>>()),
            ),
            SyntacticFacts::Unavailable(_) => 0,
        })
}

pub(crate) fn metadata_syntactic_record_logical_weight(record: &SyntacticSymbolRecord) -> usize {
    syntactic_record_logical_weight(record)
        .saturating_add(record.canonical_path.as_os_str().as_encoded_bytes().len())
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
}

fn digest_provenance(hash: &mut blake3::Hasher, provenance: &SyntacticProvenance) {
    hash.update(&[provenance.source as u8]);
    hash.update(&provenance.confidence_millis.to_le_bytes());
    hash.update(provenance.revision.as_bytes());
    digest_range(hash, provenance.range);
    hash.update(&provenance.grammar_identity);
    hash.update(&provenance.query_set_digest);
}

fn digest_string_fact(
    hash: &mut blake3::Hasher,
    fact: &SyntacticFact<Arc<String>>,
    deadline: Instant,
) -> Result<(), SyntaxError> {
    frame_before(hash, fact.value.as_bytes(), deadline)?;
    digest_provenance(hash, &fact.provenance);
    Ok(())
}

fn digest_kind_fact(hash: &mut blake3::Hasher, fact: &SyntacticFact<SyntacticSymbolKind>) {
    hash.update(&[fact.value as u8]);
    digest_provenance(hash, &fact.provenance);
}

fn digest_text_fact(
    hash: &mut blake3::Hasher,
    fact: &SyntacticFact<SyntacticText>,
    deadline: Instant,
) -> Result<(), SyntaxError> {
    frame_before(hash, fact.value.text.as_bytes(), deadline)?;
    hash.update(&[u8::from(fact.value.truncated)]);
    digest_provenance(hash, &fact.provenance);
    Ok(())
}

fn digest_optional_id_fact(hash: &mut blake3::Hasher, fact: Option<&SyntacticFact<[u8; 32]>>) {
    if let Some(fact) = fact {
        hash.update(&[1]);
        hash.update(&fact.value);
        digest_provenance(hash, &fact.provenance);
    } else {
        hash.update(&[0]);
    }
}

fn digest_string_facts(
    hash: &mut blake3::Hasher,
    facts: &SyntacticFacts<Arc<String>>,
    deadline: Instant,
) -> Result<(), SyntaxError> {
    match facts {
        SyntacticFacts::Available(facts) => {
            hash.update(&[1]);
            hash.update(&(facts.len() as u128).to_le_bytes());
            for fact in facts.iter() {
                check_deadline(deadline, SyntaxError::QueryTimeout)?;
                digest_string_fact(hash, fact, deadline)?;
            }
        }
        SyntacticFacts::Unavailable(reason) => {
            hash.update(&[0, *reason as u8]);
        }
    };
    Ok(())
}

fn digest_range_facts(
    hash: &mut blake3::Hasher,
    facts: &SyntacticFacts<SourceRange>,
    deadline: Instant,
) -> Result<(), SyntaxError> {
    match facts {
        SyntacticFacts::Available(facts) => {
            hash.update(&[1]);
            hash.update(&(facts.len() as u128).to_le_bytes());
            for fact in facts.iter() {
                check_deadline(deadline, SyntaxError::QueryTimeout)?;
                digest_range(hash, fact.value);
                digest_provenance(hash, &fact.provenance);
            }
        }
        SyntacticFacts::Unavailable(reason) => {
            hash.update(&[0, *reason as u8]);
        }
    };
    Ok(())
}

fn digest_id_facts(
    hash: &mut blake3::Hasher,
    facts: &SyntacticFacts<[u8; 32]>,
    deadline: Instant,
) -> Result<(), SyntaxError> {
    match facts {
        SyntacticFacts::Available(facts) => {
            hash.update(&[1]);
            hash.update(&(facts.len() as u128).to_le_bytes());
            for fact in facts.iter() {
                check_deadline(deadline, SyntaxError::QueryTimeout)?;
                hash.update(&fact.value);
                digest_provenance(hash, &fact.provenance);
            }
        }
        SyntacticFacts::Unavailable(reason) => {
            hash.update(&[0, *reason as u8]);
        }
    };
    Ok(())
}

fn digest_range(hash: &mut blake3::Hasher, range: SourceRange) {
    for value in [
        range.start_byte,
        range.end_byte,
        range.start_line,
        range.end_line,
    ] {
        hash.update(&(value as u128).to_le_bytes());
    }
}

fn check_deadline(deadline: Instant, error: SyntaxError) -> Result<(), SyntaxError> {
    if Instant::now() >= deadline {
        Err(error)
    } else {
        Ok(())
    }
}

fn frame(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn frame_before(
    hash: &mut blake3::Hasher,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), SyntaxError> {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    for chunk in bytes.chunks(64 * 1024) {
        check_deadline(deadline, SyntaxError::QueryTimeout)?;
        hash.update(chunk);
    }
    check_deadline(deadline, SyntaxError::QueryTimeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FUNCTIONS_ONLY_QUERY: &[u8] = br#"
(function_item name: (identifier) @name) @declaration
"#;

    #[derive(Debug, Eq, PartialEq)]
    struct CacheState {
        usage: SyntaxCacheUsage,
        metrics: SyntaxMetrics,
        clock: u64,
        files: Vec<(PathBuf, u64, [u8; 32])>,
        queries: Vec<([u8; 32], u64)>,
    }

    fn revision(byte: u8) -> RevisionId {
        RevisionId::parse(&format!("r:{}", format!("{byte:02x}").repeat(32))).unwrap()
    }

    fn index(
        syntax: &mut SyntaxIndex,
        revision: RevisionId,
        path: &str,
        source: &[u8],
        descriptor: &LanguageDescriptor,
        options: &SyntaxOptions,
    ) -> Result<SyntaxResult, SyntaxError> {
        syntax.index_source(
            revision,
            descriptor,
            Path::new(path),
            "rust",
            source,
            options,
            Instant::now() + std::time::Duration::from_secs(10),
        )
    }

    fn cache_state(syntax: &SyntaxIndex) -> CacheState {
        CacheState {
            usage: syntax.cache_usage(),
            metrics: syntax.metrics(),
            clock: syntax.clock,
            files: syntax
                .cache
                .iter()
                .map(|(path, cached)| {
                    (
                        path.to_path_buf(),
                        cached.last_used,
                        cached.canonical_digest,
                    )
                })
                .collect(),
            queries: syntax
                .runtime
                .queries
                .iter()
                .map(|(digest, cached)| (*digest, cached.last_used))
                .collect(),
        }
    }

    #[test]
    fn opaque_grammar_identity_invalidates_cache_identity() {
        let revision = RevisionId::parse(&format!("r:{}", "01".repeat(32))).unwrap();
        let identity = SyntaxCacheIdentity {
            language: SyntaxLanguage::Rust,
            grammar: GrammarIdentity {
                abi: RUST_GRAMMAR_ABI,
                version: Arc::from(RUST_GRAMMAR_VERSION),
                artifact_digest: Arc::from(RUST_GRAMMAR_ARTIFACT_DIGEST),
            },
            query_set_digest: [2; 32],
            extraction_digest: [3; 32],
            path: Arc::from(Path::new("lib.rs")),
            source_digest: [4; 32],
            revision,
        };
        let mut changed = identity.clone();
        changed.grammar.artifact_digest = Arc::from("sha256:internal-test-replacement");
        assert!(!identity.same_except_revision(&changed));
        assert_ne!(identity.canonical_digest(), changed.canonical_digest());
    }

    #[test]
    fn abi_change_forces_full_parse_and_matches_private_provenance() {
        let mut syntax = SyntaxIndex::new();
        let revision = revision(1);
        let first = index(
            &mut syntax,
            revision,
            "lib.rs",
            b"fn item() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        assert_eq!(first.action, ParseAction::Full);
        syntax.runtime.grammar.abi += 1;
        let expected = grammar_identity(&syntax.runtime.grammar);
        let second = index(
            &mut syntax,
            revision,
            "lib.rs",
            b"fn item() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        assert_eq!(second.action, ParseAction::Full);
        assert_eq!(
            second.records[0].display_name.provenance.grammar_identity,
            expected
        );
    }

    #[test]
    fn private_query_hash_change_invalidates_facts_without_reparsing() {
        let mut syntax = SyntaxIndex::new();
        let revision = revision(8);
        let pinned = index(
            &mut syntax,
            revision,
            "query-change.rs",
            b"struct Item; fn run() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        let changed = index(
            &mut syntax,
            revision,
            "query-change.rs",
            b"struct Item; fn run() {}\n",
            &LanguageDescriptor::rust_with_declaration_query(FUNCTIONS_ONLY_QUERY),
            &SyntaxOptions::default(),
        )
        .unwrap();
        assert_eq!(changed.action, ParseAction::ExtractionOnly);
        assert_ne!(
            pinned.identity.query_set_digest,
            changed.identity.query_set_digest
        );
        assert_ne!(pinned.canonical_digest, changed.canonical_digest);
        assert_eq!(changed.records.len(), 1);
        assert_eq!(syntax.metrics.full_parses, 1);
        assert_eq!(syntax.metrics.extraction_refreshes, 1);
    }

    #[test]
    fn failed_operations_do_not_publish_cache_query_lru_or_metrics() {
        let revision = revision(2);
        let mut syntax = SyntaxIndex::new();
        index(
            &mut syntax,
            revision,
            "transaction.rs",
            b"fn original() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();

        let before = cache_state(&syntax);
        assert!(matches!(
            syntax.index_source(
                revision,
                &LanguageDescriptor::rust(),
                Path::new("transaction.rs"),
                "rust",
                b"fn original() {}\n",
                &SyntaxOptions::default(),
                Instant::now(),
            ),
            Err(SyntaxError::ParseTimeout)
        ));
        assert_eq!(cache_state(&syntax), before);

        syntax.expire_next_query_after_compile = true;
        assert!(matches!(
            index(
                &mut syntax,
                revision,
                "transaction.rs",
                b"fn original() {}\n",
                &LanguageDescriptor::rust_with_declaration_query(FUNCTIONS_ONLY_QUERY),
                &SyntaxOptions::default(),
            ),
            Err(SyntaxError::QueryTimeout)
        ));
        assert_eq!(cache_state(&syntax), before);

        let changed = format!(
            "fn original() {{\n{}\n}}\n",
            "let changed = true;\n".repeat(20_000)
        );
        syntax.cancel_next_parse = true;
        assert!(matches!(
            index(
                &mut syntax,
                revision,
                "transaction.rs",
                changed.as_bytes(),
                &LanguageDescriptor::rust(),
                &SyntaxOptions::default(),
            ),
            Err(SyntaxError::ParseTimeout)
        ));
        assert_eq!(cache_state(&syntax), before);
        assert_eq!(
            index(
                &mut syntax,
                revision,
                "transaction.rs",
                changed.as_bytes(),
                &LanguageDescriptor::rust(),
                &SyntaxOptions::default(),
            )
            .unwrap()
            .action,
            ParseAction::Incremental
        );
    }

    #[test]
    fn capture_and_scope_limits_keep_a_localized_useful_prefix() {
        let source = b"mod outer { fn one() {} fn two() {} fn three() {} }\n";
        let mut syntax = SyntaxIndex::new();
        let result = index(
            &mut syntax,
            revision(3),
            "prefix.rs",
            source,
            &LanguageDescriptor::rust(),
            &SyntaxOptions {
                max_captures: 2,
                ..SyntaxOptions::default()
            },
        )
        .unwrap();
        assert!(result.truncated);
        assert!(result.omitted > 0);
        assert_eq!(result.records.len(), 2);
        assert_eq!(result.records[0].qualified_name.value.as_ref(), "outer");
        assert_eq!(
            result.records[1].qualified_name.value.as_ref(),
            "outer::one"
        );

        let result = index(
            &mut SyntaxIndex::new(),
            revision(3),
            "scope-prefix.rs",
            source,
            &LanguageDescriptor::rust(),
            &SyntaxOptions {
                max_scope_weight: 800,
                ..SyntaxOptions::default()
            },
        )
        .unwrap();
        assert!(result.truncated);
        assert!(!result.records.is_empty());
        assert!(result.records.len() < 4);
    }

    #[test]
    fn qualification_truncation_reports_exact_known_omissions_once() {
        let revision = revision(10);
        let source = b"mod outer { fn one() {} fn two() {} }\n";
        let mut syntax = SyntaxIndex::new();
        index(
            &mut syntax,
            revision,
            "omitted.rs",
            source,
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        let tree = syntax
            .cache
            .get(Path::new("omitted.rs"))
            .unwrap()
            .tree
            .clone();
        let collected = collect_scopes(
            &tree,
            source,
            &syntax.runtime.scope_query,
            None,
            &SyntaxOptions::default(),
            Instant::now() + std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert!(!collected.truncated);
        let first_key_weight = size_of::<((SyntacticSymbolKind, Arc<String>), usize)>()
            .saturating_add(BTREE_ENTRY_LOGICAL_OVERHEAD);
        let result = index(
            &mut syntax,
            revision,
            "omitted.rs",
            source,
            &LanguageDescriptor::rust(),
            &SyntaxOptions {
                max_scope_weight: collected.logical_weight + first_key_weight,
                ..SyntaxOptions::default()
            },
        )
        .unwrap();
        assert!(result.truncated);
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.omitted, 2);
    }

    #[test]
    fn malformed_declaration_ancestors_fence_nested_items() {
        for source in [
            "mod broken { fn child() {} let = ; }\n",
            "trait Broken { fn child() {} const BAD: = 1; }\n",
            "fn broken() { fn child() {} let = ; }\n",
            "const BAD: () = { fn child() {} let = ; };\n",
            "static BAD: () = { fn child() {} let = ; };\n",
            "struct A; impl A { fn child() {} let = ; }\n",
            "fn broken() { fn child() {}\n",
        ] {
            let result = index(
                &mut SyntaxIndex::new(),
                revision(4),
                "owner.rs",
                source.as_bytes(),
                &LanguageDescriptor::rust(),
                &SyntaxOptions::default(),
            )
            .unwrap();
            assert!(result.has_parse_errors, "{source}");
            assert!(
                result
                    .records
                    .iter()
                    .all(|record| record.display_name.value.as_ref() != "child")
            );
        }
    }

    #[test]
    fn logical_weight_and_exact_query_source_bounds_are_truthful() {
        let mut syntax = SyntaxIndex::with_cache_limits(SyntaxCacheLimits {
            max_resident_files: 2,
            max_resident_logical_weight: 256 * 1024,
            max_staging_files: 2,
            max_staging_logical_weight: 256 * 1024,
            max_queries: 1,
            max_query_bytes: 64 * 1024,
        })
        .unwrap();
        index(
            &mut syntax,
            revision(5),
            "weight.rs",
            b"fn weighted() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        let usage = syntax.cache_usage();
        assert!(usage.resident_logical_weight <= 256 * 1024);
        assert_eq!(usage.query_source_bytes, RUST_QUERY.len());
        assert_eq!(usage.compiled_queries, 1);
    }

    #[test]
    fn fitting_cache_admissions_plan_no_evictions_or_state_changes() {
        let revision = revision(6);
        let mut syntax = SyntaxIndex::with_cache_limits(SyntaxCacheLimits {
            max_resident_files: 2,
            max_resident_logical_weight: 1024 * 1024,
            max_staging_files: 2,
            max_staging_logical_weight: 1024 * 1024,
            max_queries: 1,
            max_query_bytes: 64 * 1024,
        })
        .unwrap();
        index(
            &mut syntax,
            revision,
            "resident.rs",
            b"fn resident() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        let candidate_weight = syntax
            .cache
            .get(Path::new("resident.rs"))
            .unwrap()
            .logical_weight;
        let before = syntax.test_state();
        let admission = syntax
            .plan_cache_admission(
                Path::new("fitting.rs"),
                candidate_weight,
                syntax.runtime.logical_weight,
                Instant::now() + std::time::Duration::from_secs(1),
            )
            .unwrap()
            .unwrap();
        assert!(admission.evictions.is_empty());
        assert_eq!(syntax.test_state(), before);

        syntax.limits.max_resident_files = 1;
        syntax
            .begin_snapshot_before(Instant::now() + std::time::Duration::from_secs(1))
            .unwrap();
        index(
            &mut syntax,
            revision,
            "staged.rs",
            b"fn staged() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        let candidate_weight = syntax
            .candidates
            .get(Path::new("staged.rs"))
            .unwrap()
            .logical_weight;
        let before = syntax.test_state();
        let admission = syntax
            .plan_candidate_admission(
                &Arc::from(Path::new("fitting-staged.rs")),
                candidate_weight,
                Instant::now() + std::time::Duration::from_secs(1),
            )
            .unwrap()
            .unwrap();
        assert!(admission.removals.is_empty());
        assert_eq!(syntax.test_state(), before);
    }

    #[test]
    fn uncacheable_test_query_does_not_evict_a_resident_query() {
        let revision = revision(6);
        let mut syntax = SyntaxIndex::new();
        index(
            &mut syntax,
            revision,
            "query.rs",
            b"fn item() {}\n",
            &LanguageDescriptor::rust_with_declaration_query(FUNCTIONS_ONLY_QUERY),
            &SyntaxOptions::default(),
        )
        .unwrap();
        let resident = syntax.runtime.queries.keys().copied().collect::<Vec<_>>();
        assert_eq!(resident.len(), 1);
        syntax.limits.max_resident_logical_weight =
            syntax.cache_usage().resident_logical_weight + 128;
        let large_query: &'static [u8] = Box::leak(
            format!(
                "(function_item name: (identifier) @name) @declaration\n{}",
                " ".repeat(16 * 1024)
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        index(
            &mut syntax,
            revision,
            "query.rs",
            b"fn item() {}\n",
            &LanguageDescriptor::rust_with_declaration_query(large_query),
            &SyntaxOptions::default(),
        )
        .unwrap();
        assert_eq!(
            syntax.runtime.queries.keys().copied().collect::<Vec<_>>(),
            resident
        );
    }

    #[test]
    fn expired_snapshot_finish_does_not_clear_or_prune_before_commit() {
        let mut syntax = SyntaxIndex::with_cache_limits(SyntaxCacheLimits {
            max_resident_files: 1,
            max_resident_logical_weight: 1024 * 1024,
            max_staging_files: 1,
            max_staging_logical_weight: 1024 * 1024,
            max_queries: 1,
            max_query_bytes: 64 * 1024,
        })
        .unwrap();
        index(
            &mut syntax,
            revision(9),
            "finish.rs",
            b"fn finish() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        syntax
            .begin_snapshot_before(Instant::now() + std::time::Duration::from_secs(1))
            .unwrap();
        index(
            &mut syntax,
            revision(9),
            "candidate.rs",
            b"fn candidate() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        assert_eq!(syntax.candidates.len(), 1);
        let before = syntax.test_state();
        assert!(matches!(
            syntax.finish_snapshot_before(Instant::now(), Some(&BTreeSet::new())),
            Err(SyntaxError::QueryTimeout)
        ));
        assert_eq!(syntax.test_state(), before);
    }

    #[test]
    fn over_capacity_snapshots_converge_on_stable_ranked_residents() {
        let revision = revision(11);
        let mut syntax = SyntaxIndex::with_cache_limits(SyntaxCacheLimits {
            max_resident_files: 2,
            max_resident_logical_weight: 1024 * 1024,
            max_staging_files: 2,
            max_staging_logical_weight: 1024 * 1024,
            max_queries: 1,
            max_query_bytes: 64 * 1024,
        })
        .unwrap();
        for (path, source) in [("y.rs", b"fn y() {}\n"), ("z.rs", b"fn z() {}\n")] {
            index(
                &mut syntax,
                revision,
                path,
                source,
                &LanguageDescriptor::rust(),
                &SyntaxOptions::default(),
            )
            .unwrap();
        }
        assert!(syntax.cache.contains_key(Path::new("z.rs")));

        let eligible = ["a.rs", "y.rs", "z.rs"]
            .into_iter()
            .map(PathBuf::from)
            .collect::<BTreeSet<_>>();
        syntax
            .begin_snapshot_before(Instant::now() + std::time::Duration::from_secs(1))
            .unwrap();
        index(
            &mut syntax,
            revision,
            "a.rs",
            b"fn a() {}\n",
            &LanguageDescriptor::rust(),
            &SyntaxOptions::default(),
        )
        .unwrap();
        assert!(syntax.candidates.contains_key(Path::new("a.rs")));
        let active_usage = syntax.cache_usage();
        assert!(active_usage.staging_files > 0);
        assert!(active_usage.staging_files <= syntax.limits.max_staging_files);
        assert!(active_usage.staging_logical_weight <= syntax.limits.max_staging_logical_weight);
        assert_eq!(
            active_usage.total_files,
            active_usage.resident_files + active_usage.staging_files
        );
        assert!(
            active_usage.total_files
                <= syntax.limits.max_resident_files + syntax.limits.max_staging_files
        );
        assert_eq!(
            active_usage.total_logical_weight,
            active_usage.resident_logical_weight + active_usage.staging_logical_weight
        );
        assert!(
            active_usage.total_logical_weight
                <= syntax.limits.max_resident_logical_weight
                    + syntax.limits.max_staging_logical_weight
        );
        syntax
            .finish_snapshot_before(
                Instant::now() + std::time::Duration::from_secs(1),
                Some(&eligible),
            )
            .unwrap();
        assert!(syntax.cache.contains_key(Path::new("a.rs")));
        assert!(syntax.cache.contains_key(Path::new("y.rs")));
        assert!(!syntax.cache.contains_key(Path::new("z.rs")));

        for _ in 0..2 {
            let reused = syntax.metrics.reused;
            syntax
                .begin_snapshot_before(Instant::now() + std::time::Duration::from_secs(1))
                .unwrap();
            for (path, source) in [
                ("a.rs", b"fn a() {}\n"),
                ("y.rs", b"fn y() {}\n"),
                ("z.rs", b"fn z() {}\n"),
            ] {
                index(
                    &mut syntax,
                    revision,
                    path,
                    source,
                    &LanguageDescriptor::rust(),
                    &SyntaxOptions::default(),
                )
                .unwrap();
            }
            syntax
                .finish_snapshot_before(
                    Instant::now() + std::time::Duration::from_secs(1),
                    Some(&eligible),
                )
                .unwrap();
            assert_eq!(syntax.metrics.reused - reused, 2);
            assert_eq!(
                syntax
                    .cache
                    .keys()
                    .map(|path| path.as_ref())
                    .collect::<Vec<_>>(),
                [Path::new("a.rs"), Path::new("y.rs")]
            );
        }
    }

    #[test]
    fn deep_nesting_is_localized_by_scope_weight() {
        let mut source = String::new();
        for depth in 0..300 {
            source.push_str(&format!("mod level_{depth} {{ "));
        }
        source.push_str("fn leaf() {} ");
        for _ in 0..300 {
            source.push('}');
        }
        let result = index(
            &mut SyntaxIndex::new(),
            revision(7),
            "deep.rs",
            source.as_bytes(),
            &LanguageDescriptor::rust(),
            &SyntaxOptions {
                max_scope_weight: 16 * 1024,
                ..SyntaxOptions::default()
            },
        )
        .unwrap();
        assert!(result.truncated);
        assert!(!result.records.is_empty());
        assert!(result.records.len() < 301);
    }
}
