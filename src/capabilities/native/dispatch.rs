use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agent::adapters::grammar_edit::{
        EditOrchestrator, EditPathTrace, GrammarEditContext, NativeEditOutcome, NativeEditServices,
    },
    api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
    capabilities::kernel::invoke::{AuthorizedInvocation, CanonicalOutput, DispatchOutcome},
    domain::config::{Executor as ConfigExecutor, Grant, RunConfigSnapshot},
    executor::{
        backends::local_os::{LocalCommand, LocalOsBackend, SandboxPaths},
        cancel::{SqliteCancellationCoordinator, WorkspaceIdentity},
        check::CheckRunner,
        process::own::ProcessRegistryRegistration,
        profile::{
            Architecture, CompatibilityOptIn, ExecutorProfile, Platform, ProfileSpec,
            ResourceLimits, TrustTier,
        },
    },
    store::artifacts::{ArtifactRetention, ArtifactStore},
    telemetry::redact::CaptureBoundary,
    verify::lsp::facts::SemanticFact,
    verify::profiles::{ProfileSelection, VerificationRegistry},
    workspace::{
        acquire::AcquisitionResult,
        edit::ir::RootRelativePath,
        graph::{
            history::{
                HistoryError, HistoryGraphProvider, HistoryOptions, HistoryRequest,
                ValidatedHistoryFence,
            },
            structure::{
                GraphError, GraphOptions, HistoryEnrichmentLimits, NodeId, StructureGraph,
                StructureGraphProvider,
            },
        },
        index::meta::{IndexOptions, MetadataIndex},
        map::{
            DeclarationId, ExpansionPurpose, ExpansionRequest, MapBound, MapBudget, MapCursor,
            MapError, MapLimits, Personalization, RelationshipKind, RepositoryMapRequest,
            ScoreBand, SemanticRelationship, StackFrame, build_repository_map_with_history,
            build_repository_map_with_structure, validate_semantic_evidence,
        },
        read::{ArtifactContext, ReadOptions, ReadRange, ReadRequest, read_projected_with_state},
        revision::{ManagedWorkspace, RevisionId, RevisionOptions},
        search::{
            discover::{DiscoverCursor, DiscoverOptions, DiscoverQuery, discover},
            lexical::{
                SearchCursor, SearchMode, SearchOptions, SearchQuery, search_projected_with_state,
            },
            structural::{StructuralOptions, StructuralQuery, search as structural_search},
        },
        syntax::SyntaxIndex,
    },
};

pub(crate) struct NativeFormatterRuntime {
    pub descriptor: crate::workspace::edit::format::FormatterDescriptor,
    pub executor: crate::executor::formatter::FormatterExecutor,
}

#[derive(Clone)]
pub(crate) struct NativeFeedbackRuntime {
    pub database: PathBuf,
    pub adapters: BTreeMap<String, crate::verify::feedback::DiagnosticAdapter>,
    pub limits: crate::verify::feedback::FeedbackLimits,
}

use super::catalog::{
    NATIVE_MAP_MAX_DEGREE, NATIVE_MAP_MAX_ESTIMATED_TOKENS, NATIVE_MAP_MAX_EXPANSION_SELECTORS,
    NATIVE_MAP_MAX_HOPS, NATIVE_MAP_MAX_ITEMS, NATIVE_MAP_MAX_RELATIONSHIPS,
    NATIVE_MAP_MAX_RESULT_BYTES, NATIVE_MAP_MAX_SEMANTIC_EVIDENCE_BYTES,
    NATIVE_MAP_MAX_SEMANTIC_RELATIONSHIPS,
};
use super::{MAX_NATIVE_OUTPUT_BYTES, NativeCatalog, NativeTool};

const MAX_RUN_CPU_MILLIS: u64 = 60_000;
const MAX_RUN_MEMORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_RUN_PIDS: u32 = 512;
const MAX_RUN_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_RUN_DISK_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAX_RUN_IO_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAX_RUN_OUTPUT_BYTES: u64 = 1024 * 1024;
const MAX_RUN_WALL_TIME_MILLIS: u64 = 10 * 60 * 1000;
const MAX_NATIVE_WORKSPACE_SCAN_TIME: std::time::Duration = std::time::Duration::from_secs(20);
const NATIVE_MAP_TOTAL_WORK: usize = 21_000_000;
const NATIVE_MAP_TOTAL_MEMORY_BYTES: usize = 320 * 1024 * 1024;
const NATIVE_MAP_TOTAL_TIME: Duration = Duration::from_secs(30);
const NATIVE_HISTORY_TOTAL_WORK: usize = 121_000_000;
const NATIVE_HISTORY_TOTAL_MEMORY_BYTES: usize = 832 * 1024 * 1024;
const NATIVE_HISTORY_TOTAL_TIME: Duration = Duration::from_secs(65);
const STRUCTURAL_PREVIEW_MAX_ENTRIES: usize = 128;
const STRUCTURAL_PREVIEW_MAX_BYTES: usize = 8 * 1024 * 1024;
const STRUCTURAL_PREVIEW_TTL: Duration = Duration::from_secs(15 * 60);
pub(crate) const MAX_EDIT_VALIDATION_TIME: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

#[derive(Clone)]
struct StructuralPreviewRecord {
    principal: String,
    project: String,
    workspace: String,
    revision: String,
    index_digest: [u8; 32],
    workspace_digest: String,
    canonical_ir: Vec<u8>,
    ir_digest: String,
    change_diff_digest: String,
    created: Instant,
    expires: Instant,
    retained_bytes: usize,
}

#[derive(Default)]
struct StructuralPreviewRegistry {
    entries: BTreeMap<[u8; 32], StructuralPreviewRecord>,
    retained_bytes: usize,
}

impl StructuralPreviewRegistry {
    fn prune(&mut self, now: Instant, revision: &str) {
        let stale = self
            .entries
            .iter()
            .filter_map(|(digest, record)| {
                (record.expires <= now || record.revision != revision).then_some(*digest)
            })
            .collect::<Vec<_>>();
        for digest in stale {
            self.remove(&digest);
        }
    }

    fn insert(&mut self, mut record: StructuralPreviewRecord) -> Result<String, String> {
        if record.retained_bytes > STRUCTURAL_PREVIEW_MAX_BYTES {
            return Err("structural_preview_unavailable".to_owned());
        }
        while self.entries.len() >= STRUCTURAL_PREVIEW_MAX_ENTRIES
            || self.retained_bytes.saturating_add(record.retained_bytes)
                > STRUCTURAL_PREVIEW_MAX_BYTES
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.created)
                .map(|(digest, _)| *digest)
                .ok_or_else(|| "structural_preview_unavailable".to_owned())?;
            self.remove(&oldest);
        }
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).map_err(|_| "structural_preview_unavailable".to_owned())?;
        let token = format!("kitsp1_{}", hex(&random));
        let digest = structural_preview_token_digest(&token);
        if self.entries.contains_key(&digest) {
            return Err("structural_preview_unavailable".to_owned());
        }
        record.expires = record
            .created
            .checked_add(STRUCTURAL_PREVIEW_TTL)
            .ok_or_else(|| "structural_preview_unavailable".to_owned())?;
        self.retained_bytes += record.retained_bytes;
        self.entries.insert(digest, record);
        Ok(token)
    }

    fn remove(&mut self, digest: &[u8; 32]) -> Option<StructuralPreviewRecord> {
        let record = self.entries.remove(digest)?;
        self.retained_bytes = self.retained_bytes.saturating_sub(record.retained_bytes);
        Some(record)
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.retained_bytes = 0;
    }
}

pub(crate) struct NativeRuntime {
    pub extension_guard: crate::capabilities::extensions::NativeExtensionGuard,
    pub workspace_id: crate::domain::ids::WorkspaceId,
    pub process_registration: Option<ProcessRegistryRegistration>,
    pub cancellation: SqliteCancellationCoordinator,
    pub live_cancellation: Arc<AtomicBool>,
    pub container_image: Option<String>,
    pub verification_registry: VerificationRegistry,
    pub check_runner: Option<CheckRunner>,
    pub custody: crate::domain::secret::SecretCustody,
    pub secrets: Vec<crate::domain::secret::SecretLease>,
    pub syntax_executors: Vec<crate::executor::syntax::SyntaxExecutor>,
    pub formatter_required: bool,
    pub formatter: Option<NativeFormatterRuntime>,
    pub feedback: Option<NativeFeedbackRuntime>,
    pub semantic_evidence: NativeSemanticEvidenceStore,
    pub edit_validation_time: std::time::Duration,
    pub cursor_key: [u8; 32],
    #[cfg(test)]
    pub run_runner: Option<CheckRunner>,
}

#[derive(Clone, Debug)]
pub(crate) struct NativeSemanticRelationship {
    pub source_declaration: DeclarationId,
    pub fact: SemanticFact,
}

#[derive(Clone, Default)]
pub(crate) struct NativeSemanticEvidenceStore {
    inner: Arc<Mutex<NativeSemanticEvidenceState>>,
}

#[derive(Default)]
struct NativeSemanticEvidenceState {
    revision: Option<RevisionId>,
    relationships: Vec<NativeSemanticRelationship>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeStructureGraphKey {
    revision: RevisionId,
    index_digest: [u8; 32],
    semantic_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeHistoryGraphKey {
    revision: RevisionId,
    index_digest: [u8; 32],
    request_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeUnifiedGraphKey {
    structure_snapshot_digest: [u8; 32],
    history_snapshot_digest: [u8; 32],
    request_digest: [u8; 32],
}

impl NativeSemanticEvidenceStore {
    #[allow(dead_code)]
    pub(crate) fn publish(
        &self,
        index: &MetadataIndex,
        relationship: NativeSemanticRelationship,
    ) -> Result<(), String> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "native semantic evidence store unavailable".to_owned())?;
        if state
            .revision
            .is_some_and(|revision| revision != index.revision())
        {
            return Err("native semantic evidence revision mismatch".to_owned());
        }
        let mut relationships = state.relationships.clone();
        relationships.push(relationship);
        validate_native_semantic_replacement(index, &relationships)?;
        relationships = relationships.into_boxed_slice().into_vec();
        state.revision = Some(index.revision());
        state.relationships = relationships;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn replace(
        &self,
        index: &MetadataIndex,
        mut relationships: Vec<NativeSemanticRelationship>,
    ) -> Result<(), String> {
        if relationships.is_empty() {
            return self.clear();
        }
        validate_native_semantic_replacement(index, &relationships)?;
        relationships = relationships.into_boxed_slice().into_vec();
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "native semantic evidence store unavailable".to_owned())?;
        state.revision = Some(index.revision());
        state.relationships = relationships;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn clear(&self) -> Result<(), String> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "native semantic evidence store unavailable".to_owned())?;
        state.revision = None;
        state.relationships.clear();
        Ok(())
    }

    fn snapshot(
        &self,
        revision: RevisionId,
    ) -> Result<Vec<NativeSemanticRelationship>, NativeMapError> {
        let state = self.inner.lock().map_err(|_| NativeMapError::Unavailable)?;
        if state.relationships.is_empty() {
            return Err(NativeMapError::SemanticEvidenceUnavailable);
        }
        if state.revision != Some(revision) {
            return Err(NativeMapError::EvidenceStale);
        }
        Ok(state.relationships.clone())
    }

    fn clear_if_stale(&self, revision: RevisionId) {
        if let Ok(mut state) = self.inner.lock()
            && state.revision.is_some_and(|stored| stored != revision)
        {
            state.revision = None;
            state.relationships.clear();
        }
    }

    fn available(&self, revision: RevisionId) -> bool {
        self.inner
            .lock()
            .is_ok_and(|state| state.revision == Some(revision) && !state.relationships.is_empty())
    }

    fn snapshot_if_current(
        &self,
        revision: RevisionId,
    ) -> Result<Vec<NativeSemanticRelationship>, NativeMapError> {
        let state = self.inner.lock().map_err(|_| NativeMapError::Unavailable)?;
        Ok(if state.revision == Some(revision) {
            state.relationships.clone()
        } else {
            Vec::new()
        })
    }

    #[cfg(test)]
    pub(crate) fn shares_state_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

#[allow(dead_code)]
fn validate_native_semantic_replacement(
    index: &MetadataIndex,
    relationships: &[NativeSemanticRelationship],
) -> Result<(), String> {
    validate_native_semantic_store(relationships)?;
    if relationships
        .iter()
        .any(|relationship| relationship.fact.provenance().revision() != index.revision())
    {
        return Err("native semantic evidence revision mismatch".to_owned());
    }
    let evidence = relationships
        .iter()
        .map(|relationship| {
            SemanticRelationship::new(relationship.source_declaration, &relationship.fact)
        })
        .collect::<Vec<_>>();
    validate_semantic_evidence(index, &evidence, bounded_map_limits())
        .map_err(|error| format!("native semantic evidence rejected: {error}"))
}

pub(crate) struct NativeDispatcher {
    extension_guard: crate::capabilities::extensions::NativeExtensionGuard,
    root: PathBuf,
    workspace: Option<ManagedWorkspace>,
    index: Option<MetadataIndex>,
    syntax_index: SyntaxIndex,
    structure_graph: StructureGraphProvider,
    structure_graph_key: Option<NativeStructureGraphKey>,
    history_graph: HistoryGraphProvider,
    history_graph_key: Option<NativeHistoryGraphKey>,
    unified_graph: Option<(NativeUnifiedGraphKey, Arc<StructureGraph>)>,
    unified_peak_bytes: usize,
    structural_previews: StructuralPreviewRegistry,
    build: PathBuf,
    temp: PathBuf,
    artifacts: Arc<ArtifactStore>,
    authenticated: AuthenticatedPrincipal,
    grants: GrantSnapshot,
    config: RunConfigSnapshot,
    acquisition: Option<AcquisitionResult>,
    workspace_id: crate::domain::ids::WorkspaceId,
    process_registration: Option<ProcessRegistryRegistration>,
    cancellation: SqliteCancellationCoordinator,
    live_cancellation: Arc<AtomicBool>,
    container_image: Option<String>,
    verification_registry: VerificationRegistry,
    check_runner: Option<CheckRunner>,
    custody: crate::domain::secret::SecretCustody,
    secrets: Vec<crate::domain::secret::SecretLease>,
    syntax_executors: Vec<crate::executor::syntax::SyntaxExecutor>,
    formatter_required: bool,
    formatter: Option<NativeFormatterRuntime>,
    feedback: Option<NativeFeedbackRuntime>,
    semantic_evidence: NativeSemanticEvidenceStore,
    edit_validation_time: std::time::Duration,
    cursor_key: [u8; 32],
    projection_state: crate::domain::secret::JsonProjectionState,
    read_replay: Option<ReadReplay>,
    #[cfg(test)]
    run_runner: Option<CheckRunner>,
}

impl NativeDispatcher {
    pub(crate) fn open(
        root: PathBuf,
        scratch: &Path,
        artifacts: Arc<ArtifactStore>,
        authenticated: AuthenticatedPrincipal,
        config: RunConfigSnapshot,
        acquisition: Option<AcquisitionResult>,
        runtime: NativeRuntime,
    ) -> Result<Self, String> {
        if runtime.edit_validation_time.is_zero()
            || runtime.edit_validation_time > MAX_EDIT_VALIDATION_TIME
        {
            return Err("native edit validation policy is outside the trusted bound".to_owned());
        }
        let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
        if !root.is_dir() {
            return Err("trusted project root is not a directory".to_owned());
        }
        let build = scratch.join("build");
        let temp = scratch.join("tmp");
        std::fs::create_dir_all(&build).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&temp).map_err(|error| error.to_string())?;
        let grants = authenticated.grant_snapshot().clone();
        let projection_state = runtime.custody.projection_state();
        Ok(Self {
            extension_guard: runtime.extension_guard,
            root,
            workspace: None,
            index: None,
            syntax_index: SyntaxIndex::new(),
            structure_graph: StructureGraphProvider::new(),
            structure_graph_key: None,
            history_graph: HistoryGraphProvider::new(),
            history_graph_key: None,
            unified_graph: None,
            unified_peak_bytes: 0,
            structural_previews: StructuralPreviewRegistry::default(),
            build,
            temp,
            artifacts,
            authenticated,
            grants,
            config,
            acquisition,
            workspace_id: runtime.workspace_id,
            process_registration: runtime.process_registration,
            cancellation: runtime.cancellation,
            live_cancellation: runtime.live_cancellation,
            container_image: runtime.container_image,
            verification_registry: runtime.verification_registry,
            check_runner: runtime.check_runner,
            custody: runtime.custody,
            secrets: runtime.secrets,
            syntax_executors: runtime.syntax_executors,
            formatter_required: runtime.formatter_required,
            formatter: runtime.formatter,
            feedback: runtime.feedback,
            semantic_evidence: runtime.semantic_evidence,
            edit_validation_time: runtime.edit_validation_time,
            cursor_key: runtime.cursor_key,
            projection_state,
            read_replay: None,
            #[cfg(test)]
            run_runner: runtime.run_runner,
        })
    }

    pub(crate) fn revision(&mut self) -> Result<String, String> {
        self.revision_state().map(|(revision, _)| revision)
    }

    pub(crate) fn revision_state(&mut self) -> Result<(String, String), String> {
        let workspace = self.ensure_workspace()?;
        workspace.mark_dirty();
        workspace
            .current_revision()
            .map(|revision| (revision.id().to_string(), revision.digest().to_string()))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn bind_authority(
        &mut self,
        authenticated: AuthenticatedPrincipal,
        config: RunConfigSnapshot,
        attempt: crate::domain::lifecycle::AttemptOwnership,
        cancellation: Arc<AtomicBool>,
    ) {
        self.grants = authenticated.grant_snapshot().clone();
        self.authenticated = authenticated;
        self.config = config;
        self.live_cancellation = cancellation;
        if let Some(runner) = &mut self.check_runner {
            runner.bind_attempt(attempt);
        }
        if let Some(formatter) = &mut self.formatter {
            formatter.executor.bind_attempt(attempt);
        }
    }

    pub(crate) fn dispatch(&mut self, invocation: &AuthorizedInvocation) -> DispatchOutcome {
        if self.extension_guard.ensure_current().is_err() {
            return failed("native_extension_contract_inactive");
        }
        if self.cancelled() {
            return failed("cancelled");
        }
        let Some(descriptor) = NativeCatalog::all()
            .iter()
            .find(|descriptor| descriptor.identity() == invocation.capability())
        else {
            return failed("native_tool_binding_unknown");
        };
        if descriptor.schema().normalized_digest() != invocation.schema_digest()
            || descriptor.effect() != invocation.effect()
        {
            return failed("native_tool_binding_mismatch");
        }
        if descriptor.tool() == NativeTool::Edit {
            return match self.edit(invocation.arguments(), invocation.attempt()) {
                Ok((data, artifacts, true)) => committed_output(
                    data,
                    artifacts,
                    &self.artifacts,
                    &self.custody,
                    &mut self.projection_state,
                ),
                Ok((data, artifacts, false)) => streaming_output(
                    data,
                    artifacts,
                    &self.artifacts,
                    &self.custody,
                    &mut self.projection_state,
                ),
                Err(code) => failed(&code),
            };
        }
        let result = match descriptor.tool() {
            NativeTool::Discover => self.discover(invocation.arguments()),
            NativeTool::Search => self.search(invocation.arguments()),
            NativeTool::Read => self.read(invocation.arguments()),
            NativeTool::Edit => unreachable!(),
            NativeTool::Run => self.run(invocation.arguments(), invocation.attempt()),
            NativeTool::Check => self.check(invocation.arguments(), invocation.attempt()),
        };
        match result {
            Ok((_data, _artifacts)) if self.cancelled() => failed("cancelled_after_dispatch"),
            Ok((data, artifacts))
                if matches!(descriptor.tool(), NativeTool::Search | NativeTool::Read) =>
            {
                projected_output(data, artifacts, &self.artifacts)
            }
            Ok((data, artifacts)) => streaming_output(
                data,
                artifacts,
                &self.artifacts,
                &self.custody,
                &mut self.projection_state,
            ),
            Err(code) => failed(&code),
        }
    }

    fn cancelled(&self) -> bool {
        self.live_cancellation.load(Ordering::Acquire)
    }

    fn ensure_workspace(&mut self) -> Result<&ManagedWorkspace, String> {
        if self.workspace.is_none() {
            let defaults = RevisionOptions::default();
            let options = RevisionOptions {
                max_scan_time: defaults.max_scan_time.max(
                    self.edit_validation_time
                        .min(MAX_NATIVE_WORKSPACE_SCAN_TIME),
                ),
                ..defaults
            };
            self.workspace = Some(
                ManagedWorkspace::open_with_options(&self.root, options)
                    .map_err(|error| error.to_string())?,
            );
        }
        Ok(self.workspace.as_ref().expect("workspace was initialized"))
    }

    fn workspace_index(
        &mut self,
        expected: &str,
    ) -> Result<(ManagedWorkspace, MetadataIndex), String> {
        let expected = revision(expected)?;
        let workspace = self.ensure_workspace()?.clone();
        workspace.mark_dirty();
        let current = workspace
            .current_revision()
            .map_err(code("workspace_revision_failed"))?;
        if current.id() != expected {
            return Err("stale_revision".to_owned());
        }
        self.structural_previews
            .prune(Instant::now(), &current.id().to_string());
        if let Some(index) = &self.index
            && index.revision() == expected
        {
            return Ok((workspace, index.clone()));
        }
        let index_options = IndexOptions {
            max_build_time: std::time::Duration::from_secs(60),
            ..IndexOptions::default()
        };
        let index = MetadataIndex::build_with_syntax(
            &workspace,
            expected,
            &index_options,
            &mut self.syntax_index,
        )
        .map_err(code("workspace_index_failed"))?;
        self.structure_graph_key = None;
        self.history_graph_key = None;
        self.unified_graph = None;
        self.index = Some(index.clone());
        Ok((workspace, index))
    }

    fn refresh_structure_graph(
        &mut self,
        workspace: &ManagedWorkspace,
        index: &MetadataIndex,
        stored: &[NativeSemanticRelationship],
        evidence: &[SemanticRelationship<'_>],
        options: &GraphOptions,
    ) -> Result<bool, NativeMapError> {
        let key = NativeStructureGraphKey {
            revision: index.revision(),
            index_digest: *index.index_digest(),
            semantic_digest: native_semantic_digest(stored),
        };
        if self.structure_graph_key == Some(key)
            && self
                .structure_graph
                .validated_graph(workspace)
                .map_err(NativeMapError::from)?
                .is_some_and(|graph| graph.index_digest() == *index.index_digest())
        {
            return Ok(false);
        }
        self.unified_graph = None;
        self.structure_graph
            .refresh(workspace, index, options, &[], evidence)
            .map_err(NativeMapError::from)?;
        self.structure_graph_key = Some(key);
        self.unified_graph = None;
        Ok(true)
    }

    fn refresh_history_graph(
        &mut self,
        workspace: &ManagedWorkspace,
        index: &MetadataIndex,
        request: &HistoryRequest,
        options: &HistoryOptions,
    ) -> Result<ValidatedHistoryFence, NativeMapError> {
        let key = NativeHistoryGraphKey {
            revision: index.revision(),
            index_digest: *index.index_digest(),
            request_digest: native_history_request_digest(request),
        };
        if self.history_graph_key == Some(key) {
            let fence = self
                .history_graph
                .validated_fence(workspace)
                .map_err(NativeMapError::from)?
                .ok_or(NativeMapError::HistoryEvidenceUnavailable)?;
            if self
                .history_graph
                .graph()
                .is_some_and(|graph| graph.index_digest() == *index.index_digest())
            {
                return Ok(fence);
            }
        }
        self.unified_graph = None;
        let (_, fence) = self
            .history_graph
            .refresh_fenced(workspace, index, request, options)
            .map_err(NativeMapError::from)?;
        self.history_graph_key = Some(key);
        self.unified_graph = None;
        Ok(fence)
    }

    fn discover(&mut self, bytes: &[u8]) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: DiscoverInput = decode(bytes)?;
        if let DiscoverInput::Map(input) = input {
            let (request, cursor) = native_map_request(input.map).map_err(NativeMapError::code)?;
            let (workspace, index) = self.workspace_index(&input.expected_revision)?;
            let semantic_requested = request
                .expansion
                .relationships
                .iter()
                .any(|relationship| relationship.is_semantic());
            let graph_requested = native_structure_requested(&request);
            let history_requested = native_history_requested(&request);
            let stored = if semantic_requested {
                self.semantic_evidence
                    .snapshot(index.revision())
                    .map_err(NativeMapError::code)?
            } else if graph_requested {
                self.semantic_evidence
                    .snapshot_if_current(index.revision())
                    .map_err(NativeMapError::code)?
            } else {
                self.semantic_evidence.clear_if_stale(index.revision());
                Vec::new()
            };
            let evidence = native_semantic_evidence(&stored, index.revision())
                .map_err(NativeMapError::code)?;
            let semantic_evidence_available = self.semantic_evidence.available(index.revision());
            let (graph_options, history_options, map_limits) =
                bounded_unified_map_limits(graph_requested, history_requested)
                    .map_err(NativeMapError::code)?;
            if graph_requested {
                self.refresh_structure_graph(
                    &workspace,
                    &index,
                    &stored,
                    &evidence,
                    &graph_options,
                )
                .map_err(NativeMapError::code)?;
            }
            let history_fence = if history_requested {
                let include_changed_with = request
                    .expansion
                    .relationships
                    .contains(&RelationshipKind::ChangedWith);
                let history_request = HistoryRequest::new(
                    request.history_paths.clone(),
                    request.blame_paths.clone(),
                    include_changed_with,
                );
                Some(
                    self.refresh_history_graph(
                        &workspace,
                        &index,
                        &history_request,
                        &history_options,
                    )
                    .map_err(NativeMapError::code)?,
                )
            } else {
                None
            };
            let unified = if history_requested {
                let history = self
                    .history_graph
                    .graph()
                    .ok_or(NativeMapError::HistoryEvidenceUnavailable)
                    .map_err(NativeMapError::code)?;
                let base = self
                    .structure_graph
                    .graph()
                    .ok_or(NativeMapError::GraphEvidenceUnavailable)
                    .map_err(NativeMapError::code)?;
                let key = NativeUnifiedGraphKey {
                    structure_snapshot_digest: base.snapshot_digest(),
                    history_snapshot_digest: history.snapshot_digest(),
                    request_digest: history.request_digest(),
                };
                if self
                    .unified_graph
                    .as_ref()
                    .is_none_or(|(cached, _)| *cached != key)
                {
                    let retained_cache = self
                        .structure_graph
                        .cache_usage()
                        .logical_bytes()
                        .checked_add(self.history_graph.cache_usage().logical_bytes())
                        .and_then(|bytes| {
                            bytes.checked_add(
                                history_fence
                                    .as_ref()
                                    .map_or(0, |fence| fence.metrics().peak_memory_bytes()),
                            )
                        })
                        .and_then(|bytes| {
                            bytes.checked_add(
                                self.unified_graph
                                    .as_ref()
                                    .map_or(0, |(_, graph)| graph.logical_bytes()),
                            )
                        })
                        .and_then(|bytes| bytes.checked_add(map_limits.max_memory_bytes))
                        .ok_or(NativeMapError::Unavailable)
                        .map_err(NativeMapError::code)?;
                    let enrichment_memory = NATIVE_HISTORY_TOTAL_MEMORY_BYTES
                        .checked_sub(retained_cache)
                        .filter(|bytes| *bytes != 0)
                        .ok_or(NativeMapError::Unavailable)
                        .map_err(NativeMapError::code)?;
                    let graph = history
                        .enrich_structure(
                            base,
                            HistoryEnrichmentLimits {
                                max_work: NATIVE_HISTORY_TOTAL_WORK
                                    .saturating_sub(graph_options.max_work)
                                    .saturating_sub(history_options.max_work)
                                    .saturating_sub(map_limits.max_work)
                                    .saturating_sub(
                                        ValidatedHistoryFence::conservative_metrics(4).work(),
                                    )
                                    .max(1),
                                max_memory_bytes: enrichment_memory,
                                max_time: Duration::from_secs(5),
                            },
                        )
                        .map_err(NativeMapError::from)
                        .map_err(NativeMapError::code)?;
                    self.unified_peak_bytes = retained_cache
                        .saturating_sub(map_limits.max_memory_bytes)
                        .saturating_add(graph.enrichment_peak_bytes())
                        .saturating_add(map_limits.max_memory_bytes);
                    if self.unified_peak_bytes > NATIVE_HISTORY_TOTAL_MEMORY_BYTES {
                        return Err(NativeMapError::Unavailable.code());
                    }
                    self.unified_graph = Some((key, Arc::new(graph)));
                }
                self.unified_graph
                    .as_ref()
                    .map(|(_, graph)| Arc::clone(graph))
            } else {
                None
            };
            let structure = unified.as_deref().or_else(|| {
                graph_requested
                    .then(|| self.structure_graph.graph())
                    .flatten()
            });
            let response =
                if let (Some(structure), Some(fence)) = (structure, history_fence.as_ref()) {
                    build_repository_map_with_history(
                        &workspace,
                        &index,
                        &request,
                        &evidence,
                        map_limits,
                        cursor.as_ref(),
                        structure,
                        fence,
                    )
                } else {
                    build_repository_map_with_structure(
                        &workspace,
                        &index,
                        &request,
                        &evidence,
                        map_limits,
                        cursor.as_ref(),
                        structure,
                    )
                }
                .map_err(NativeMapError::from)
                .map_err(NativeMapError::code)?;
            let bytes = response
                .to_canonical_json()
                .map_err(|_| "map_serialization_failed".to_owned())?;
            if bytes.len() > NATIVE_MAP_MAX_RESULT_BYTES {
                return Err("map_result_bytes_bound_exceeded".to_owned());
            }
            let map = serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| "map_serialization_failed".to_owned())?;
            if let Some(fence) = history_fence.as_ref() {
                fence
                    .validate_repository(
                        &workspace,
                        Instant::now()
                            .checked_add(map_limits.max_time)
                            .unwrap_or_else(Instant::now),
                    )
                    .map_err(NativeMapError::from)
                    .map_err(NativeMapError::code)?;
                let resources = fence.metrics();
                let observed_work = self
                    .history_graph
                    .metrics()
                    .consumed_work()
                    .checked_add(resources.work())
                    .ok_or_else(|| NativeMapError::Unavailable.code())?;
                let observed_commands = self
                    .history_graph
                    .metrics()
                    .commands()
                    .checked_add(resources.commands())
                    .ok_or_else(|| NativeMapError::Unavailable.code())?;
                let observed_output = self
                    .history_graph
                    .metrics()
                    .output_bytes()
                    .checked_add(resources.output_bytes())
                    .ok_or_else(|| NativeMapError::Unavailable.code())?;
                let observed_memory = self
                    .history_graph
                    .metrics()
                    .peak_staging_bytes()
                    .checked_add(resources.peak_memory_bytes())
                    .and_then(|bytes| {
                        bytes.checked_add(self.history_graph.cache_usage().logical_bytes())
                    })
                    .and_then(|bytes| {
                        bytes.checked_add(self.structure_graph.cache_usage().logical_bytes())
                    })
                    .and_then(|bytes| {
                        bytes.checked_add(
                            self.unified_graph
                                .as_ref()
                                .map_or(0, |(_, graph)| graph.logical_bytes()),
                        )
                    })
                    .and_then(|memory| memory.checked_add(bytes.capacity()))
                    .map(|bytes| bytes.max(self.unified_peak_bytes))
                    .ok_or_else(|| NativeMapError::Unavailable.code())?;
                if observed_work > NATIVE_HISTORY_TOTAL_WORK
                    || observed_memory > NATIVE_HISTORY_TOTAL_MEMORY_BYTES
                    || observed_commands > HistoryOptions::default().max_commands
                    || observed_output > HistoryOptions::default().max_output_bytes
                {
                    return Err(NativeMapError::Unavailable.code());
                }
            }
            return Ok((
                json!({
                    "mode": "map",
                    "map": map,
                    "semanticEvidenceAvailable": semantic_evidence_available,
                }),
                Vec::new(),
            ));
        }
        let DiscoverInput::Legacy(input) = input else {
            unreachable!()
        };
        let (workspace, index) = self.workspace_index(&input.expected_revision)?;
        let response = discover(
            &workspace,
            &index,
            &DiscoverQuery {
                terms: input.terms,
                roots: input.roots.into_iter().map(PathBuf::from).collect(),
                languages: input.languages,
            },
            &bounded_discover_options(),
            input.cursor.as_ref(),
        )
        .map_err(code("discover_failed"))?;
        serde_json::to_value(response)
            .map(|value| (value, Vec::new()))
            .map_err(code("serialization_failed"))
    }

    fn search(&mut self, bytes: &[u8]) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: SearchInput = decode(bytes)?;
        let (workspace, index) = self.workspace_index(&input.expected_revision)?;
        if matches!(input.mode, SearchModeInput::Structural) {
            if input.cursor.is_some() && input.rewrite.is_some() {
                return Err("structural_rewrite_cursor_rejected".to_owned());
            }
            if input.cursor.is_some() {
                return Err("structural_cursor_rejected".to_owned());
            }
            let response = structural_search(
                &workspace,
                &index,
                &mut self.syntax_index,
                &StructuralQuery {
                    pattern: input.text,
                    rewrite: input.rewrite,
                },
                &StructuralOptions {
                    path_prefixes: input.path_prefixes.into_iter().map(PathBuf::from).collect(),
                    languages: input.languages,
                    max_change_diff_bytes: MAX_NATIVE_OUTPUT_BYTES / 4,
                    max_output_bytes: MAX_NATIVE_OUTPUT_BYTES / 2,
                    max_time: std::time::Duration::from_secs(30),
                    ..StructuralOptions::default()
                },
            )
            .map_err(code("structural_search_failed"))?;
            let value = serde_json::to_value(&response).map_err(code("serialization_failed"))?;
            let token = if let Some(rewrite) =
                response.rewrite.as_ref().filter(|rewrite| rewrite.changed)
            {
                let canonical_ir = rewrite.ir.canonical_bytes();
                let retained_bytes = canonical_ir
                    .len()
                    .checked_add(rewrite.ir_digest.len())
                    .and_then(|bytes| bytes.checked_add(rewrite.change_diff_digest.len()))
                    .and_then(|bytes| bytes.checked_add(512))
                    .ok_or_else(|| "structural_preview_unavailable".to_owned())?;
                Some(self.structural_previews.insert(StructuralPreviewRecord {
                    principal: self.authenticated.principal_id().to_string(),
                    project: self.config.project_id().to_string(),
                    workspace: self.workspace_id.to_string(),
                    revision: index.revision().to_string(),
                    index_digest: *index.index_digest(),
                    workspace_digest: index.digest().to_string(),
                    canonical_ir,
                    ir_digest: rewrite.ir_digest.clone(),
                    change_diff_digest: rewrite.change_diff_digest.clone(),
                    created: Instant::now(),
                    expires: Instant::now(),
                    retained_bytes,
                })?)
            } else {
                None
            };
            let mut value = self.custody.project_json_stream(
                CaptureBoundary::WorkspaceMetadata,
                &value,
                &mut self.projection_state,
            );
            if let Some(token) = token {
                value["rewrite"]["apply"] = json!({"preview_token": token});
            }
            settle_result_bytes(&mut value)?;
            return Ok((value, Vec::new()));
        }
        if input.rewrite.is_some() {
            return Err("lexical_rewrite_rejected".to_owned());
        }
        search_projected_with_state(
            &workspace,
            &index,
            &SearchQuery {
                text: input.text,
                mode: input.mode.into(),
            },
            &SearchOptions {
                path_prefixes: input.path_prefixes.into_iter().map(PathBuf::from).collect(),
                languages: input.languages,
                max_result_bytes: MAX_NATIVE_OUTPUT_BYTES / 2,
                max_time: std::time::Duration::from_secs(30),
                ..SearchOptions::default()
            },
            input.cursor.as_ref(),
            &self.custody,
            &mut self.projection_state,
            &self.cursor_key,
            &self.authenticated.principal_id().to_string(),
            &self.config.project_id().to_string(),
        )
        .map(|value| (value, Vec::new()))
        .map_err(code("search_failed"))
    }

    fn read(&mut self, bytes: &[u8]) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: ReadInput = decode(bytes)?;
        let cursor_binding = read_cursor_binding(
            &input,
            &self.authenticated.principal_id().to_string(),
            &self.config.project_id().to_string(),
        )?;
        if let Some(cursor) = &input.cursor {
            let cursor_state =
                open_read_cursor(cursor, &cursor_binding, &self.cursor_key, &self.custody)?;
            self.workspace_index(&input.expected_revision)?;
            if let Some(replay) = &self.read_replay
                && replay.cursor == *cursor
                && replay.binding == cursor_binding
                && replay.projection_state == self.projection_state
            {
                return Ok((replay.data.clone(), replay.artifacts.clone()));
            }
            if !self.projection_state.merge_forward(cursor_state) {
                return Err("read_cursor_invalid".to_owned());
            }
        } else if self.projection_state.custody_revision() != self.custody.revision() {
            self.projection_state = self.custody.projection_state();
        }
        let cursor_state = self.projection_state.clone();
        let (workspace, index) = self.workspace_index(&input.expected_revision)?;
        let response = read_projected_with_state(
            &workspace,
            &index,
            &self.artifacts,
            &ArtifactContext {
                principal: self.authenticated.principal_id().to_string(),
                project: self.config.project_id().to_string(),
                retention: ArtifactRetention::Forever,
            },
            &ReadRequest {
                expected_revision: revision(&input.expected_revision)?,
                path: PathBuf::from(input.path),
                range: input.range.into(),
            },
            &ReadOptions {
                max_inline_bytes: 32 * 1024,
                max_result_bytes: MAX_NATIVE_OUTPUT_BYTES / 2,
                max_time: std::time::Duration::from_secs(30),
                ..ReadOptions::default()
            },
            &self.custody,
            &mut self.projection_state,
        )
        .map_err(code("read_failed"))?;
        let artifacts = response
            .artifact
            .as_ref()
            .map(|artifact| vec![artifact.id.clone()])
            .unwrap_or_default();
        let mut value = serde_json::to_value(response).map_err(code("serialization_failed"))?;
        let cursor = seal_read_cursor(&cursor_state, &cursor_binding, &self.cursor_key)?;
        value["cursor"] = serde_json::to_value(&cursor).map_err(code("serialization_failed"))?;
        self.read_replay = Some(ReadReplay {
            cursor,
            binding: cursor_binding,
            projection_state: self.projection_state.clone(),
            data: value.clone(),
            artifacts: artifacts.clone(),
        });
        Ok((value, artifacts))
    }

    fn edit(
        &mut self,
        bytes: &[u8],
        attempt: crate::domain::lifecycle::AttemptOwnership,
    ) -> Result<(Value, Vec<String>, bool), String> {
        self.ensure_not_cancelled()?;
        if self.verification_registry.is_empty() {
            return Err("trusted_edit_registry_unavailable".to_owned());
        }
        if self.syntax_executors.is_empty() {
            return Err("trusted_edit_syntax_unavailable".to_owned());
        }
        if self.formatter_required && self.formatter.is_none() {
            return Err("trusted_edit_formatter_unavailable".to_owned());
        }
        if self
            .feedback
            .as_ref()
            .is_none_or(|feedback| feedback.adapters.is_empty())
        {
            return Err("trusted_edit_feedback_unavailable".to_owned());
        }
        let limits = crate::workspace::edit::ir::EditLimits {
            max_authorization_time: std::time::Duration::from_secs(30),
            max_validation_time: self.edit_validation_time,
            ..crate::workspace::edit::ir::EditLimits::default()
        };
        let input: Value = serde_json::from_slice(bytes).map_err(code("invalid_arguments"))?;
        let preview_token = input
            .get("preview_token")
            .is_some()
            .then(|| {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct PreviewInput {
                    preview_token: String,
                }
                serde_json::from_value::<PreviewInput>(input.clone())
                    .map(|input| input.preview_token)
                    .map_err(|_| "invalid_arguments".to_owned())
            })
            .transpose()?;
        let ir = preview_token
            .as_deref()
            .map(|token| self.resolve_structural_preview(token, limits))
            .transpose()?;
        let expected = ir.as_ref().map_or_else(
            || {
                input
                    .get("expected_revision")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| "invalid_arguments".to_owned())
            },
            |ir| Ok(ir.expected_revision().to_string()),
        )?;
        let workspace = self.ensure_workspace()?.clone();
        let context = GrammarEditContext::from_workspace(workspace, self.root.clone(), limits)
            .map_err(code("edit_context_failed"))?;
        if context.expected_revision().as_str() != expected {
            return Err("stale_revision".to_owned());
        }
        let mut trace = EditPathTrace::default();
        let runner = self
            .check_runner
            .as_mut()
            .ok_or_else(|| "trusted_edit_runner_unavailable".to_owned())?;
        let mut syntax_executors = self.syntax_executors.iter_mut().collect::<Vec<_>>();
        let feedback = self
            .feedback
            .as_ref()
            .expect("trusted feedback was checked above");
        let formatter = self
            .formatter
            .as_mut()
            .map(|formatter| (&formatter.descriptor, &mut formatter.executor));
        let services = NativeEditServices {
            workspace_id: self.workspace_id.to_string(),
            attempt,
            feedback_database: &feedback.database,
            build: &self.build,
            temp: &self.temp,
            diagnostic_adapters: &feedback.adapters,
            feedback_limits: feedback.limits.clone(),
            formatter,
        };
        let outcome = if let Some(ir) = ir {
            EditOrchestrator::execute_native_ir(
                ir,
                &context,
                &self.authenticated,
                &self.grants,
                &self.config,
                &self.artifacts,
                &self.live_cancellation,
                &self.verification_registry,
                runner,
                &self.secrets,
                &mut syntax_executors,
                services,
                &mut trace,
            )
        } else {
            EditOrchestrator::execute_native(
                bytes,
                &context,
                &self.authenticated,
                &self.grants,
                &self.config,
                &self.artifacts,
                &self.live_cancellation,
                &self.verification_registry,
                runner,
                &self.secrets,
                &mut syntax_executors,
                services,
                &mut trace,
            )
        }
        .map_err(native_edit_error)?;
        match outcome {
            NativeEditOutcome::Aborted { receipt, feedback } => {
                let artifacts = vec![
                    receipt.result_artifact.reference.clone(),
                    feedback.payload_artifact.reference.clone(),
                    feedback.report_artifact.reference.clone(),
                ];
                Ok((
                    json!({
                        "outcome": "aborted",
                        "feedback": feedback.payload,
                        "feedback_artifacts": {
                            "payload_artifact": feedback.payload_artifact.reference,
                            "report_artifact": feedback.report_artifact.reference,
                        },
                        "events": feedback.events,
                        "trace": trace.ids(),
                        "verification": receipt,
                    }),
                    artifacts,
                    false,
                ))
            }
            NativeEditOutcome::Committed { edit, feedback } => {
                self.index = None;
                self.structure_graph_key = None;
                self.history_graph_key = None;
                self.unified_graph = None;
                self.structural_previews.clear();
                let receipt = edit.verification_receipt();
                let change_diff = std::str::from_utf8(edit.change_diff())
                    .expect("materialized textual change diff is UTF-8");
                let diff_artifact = json!({
                    "reference": edit.diff_artifact_reference().to_string(),
                    "digest": edit.diff_artifact_digest().to_string(),
                    "media_type": "text/x-diff; charset=utf-8",
                    "class": "diff",
                    "provenance": {
                        "principal_id": self.grants.principal_id(),
                        "project_id": self.grants.project_id(),
                        "transaction_id": edit.transaction_id(),
                        "revision_id": edit.revision().id(),
                    },
                });
                let artifacts = vec![
                    edit.diff_artifact_reference().to_string(),
                    receipt.result_artifact.reference.clone(),
                    feedback.payload_artifact.reference.clone(),
                    feedback.report_artifact.reference.clone(),
                ];
                Ok((
                    json!({
                        "outcome": if edit.committed_with_cancel_race() {
                            "committed_with_cancel_race"
                        } else {
                            "committed"
                        },
                        "diff_artifact": diff_artifact,
                        "diff_preview": edit.diff_preview(),
                        "change_diff": change_diff,
                        "change_diff_complete": edit.change_diff_complete(),
                        "feedback": feedback.payload,
                        "feedback_artifacts": {
                            "payload_artifact": feedback.payload_artifact.reference,
                            "report_artifact": feedback.report_artifact.reference,
                        },
                        "events": feedback.events,
                        "revision": {
                            "digest": edit.revision().digest().to_string(),
                            "epoch": edit.revision().epoch().to_string(),
                            "id": edit.revision().id().to_string(),
                        },
                        "trace": trace.ids(),
                        "transaction_id": edit.transaction_id(),
                        "verification": receipt,
                    }),
                    artifacts,
                    true,
                ))
            }
        }
    }

    fn resolve_structural_preview(
        &mut self,
        token: &str,
        limits: crate::workspace::edit::ir::EditLimits,
    ) -> Result<crate::workspace::edit::ir::EditIr, String> {
        let valid = token.strip_prefix("kitsp1_").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if !valid {
            return Err("structural_preview_invalid".to_owned());
        }
        let digest = structural_preview_token_digest(token);
        let Some(record) = self.structural_previews.entries.get(&digest).cloned() else {
            return Err("structural_preview_invalid".to_owned());
        };
        if record.principal != self.authenticated.principal_id().to_string()
            || record.project != self.config.project_id().to_string()
            || record.workspace != self.workspace_id.to_string()
        {
            return Err("structural_preview_invalid".to_owned());
        }
        if record.expires <= Instant::now() {
            self.structural_previews.remove(&digest);
            return Err("structural_preview_invalid".to_owned());
        }
        let state = self.revision_state();
        let Ok((revision, workspace_digest)) = state else {
            self.structural_previews.remove(&digest);
            return Err("structural_preview_invalid".to_owned());
        };
        if revision != record.revision || workspace_digest != record.workspace_digest {
            self.structural_previews.remove(&digest);
            return Err("structural_preview_invalid".to_owned());
        }
        let Ok((_, index)) = self.workspace_index(&record.revision) else {
            self.structural_previews.remove(&digest);
            return Err("structural_preview_invalid".to_owned());
        };
        if *index.index_digest() != record.index_digest {
            self.structural_previews.remove(&digest);
            return Err("structural_preview_invalid".to_owned());
        }
        if record.expires <= Instant::now() {
            self.structural_previews.remove(&digest);
            return Err("structural_preview_invalid".to_owned());
        }
        let record = self
            .structural_previews
            .remove(&digest)
            .ok_or_else(|| "structural_preview_invalid".to_owned())?;
        let ir =
            crate::workspace::edit::ir::EditIr::from_canonical_bytes(&record.canonical_ir, limits)
                .map_err(|_| "structural_preview_invalid".to_owned())?;
        if ir.digest() != record.ir_digest
            || ir.expected_revision().as_str() != record.revision
            || ir.expected_change_diff_digest() != Some(record.change_diff_digest.as_str())
        {
            return Err("structural_preview_invalid".to_owned());
        }
        Ok(ir)
    }

    fn run(
        &mut self,
        bytes: &[u8],
        attempt: crate::domain::lifecycle::AttemptOwnership,
    ) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: RunInput = decode(bytes)?;
        if input.working_directory != "."
            || input.argv.is_empty()
            || input.mounts != RunMounts::required()
            || !trusted_run_limits(input.limits)
        {
            return Err("run_request_rejected".to_owned());
        }
        if input.network == NetworkPolicy::ProfileGrants
            && !self
                .config
                .effective_authority()
                .contains(&Grant::NetworkEgress)
        {
            return Err("network_grant_required".to_owned());
        }
        if !input.host_compatibility
            && self.config.effective().executor == ConfigExecutor::IsolatedVm
        {
            return Err("configured_vm_executor_unavailable".to_owned());
        }
        let spec = if input.host_compatibility {
            if !self
                .authenticated
                .grant_snapshot()
                .grants()
                .contains(&Grant::HostProcessCompatibility)
                || !self
                    .config
                    .effective_authority()
                    .contains(&Grant::HostProcessCompatibility)
            {
                return Err("host_compatibility_grant_required".to_owned());
            }
            ProfileSpec::host_compatibility(
                host_platform()?,
                host_architecture()?,
                input.limits,
                CompatibilityOptIn::trusted_local("native run policy grant")
                    .map_err(code("executor_profile_rejected"))?,
            )
        } else {
            ProfileSpec::isolated(
                if self.config.effective().executor == ConfigExecutor::RestrictedContainer {
                    TrustTier::Restricted
                } else {
                    TrustTier::TrustedLocal
                },
                host_platform()?,
                host_architecture()?,
                input.limits,
            )
        };
        let profile = ExecutorProfile::new(spec).map_err(code("executor_profile_rejected"))?;
        #[cfg(test)]
        if self.run_runner.is_some() {
            return self.run_conformance(input, attempt);
        }
        if self.config.effective().executor == ConfigExecutor::RestrictedContainer {
            let acquisition = self
                .acquisition
                .as_ref()
                .ok_or_else(|| "workspace_acquisition_unavailable".to_owned())?;
            let registration = self
                .process_registration
                .as_ref()
                .ok_or_else(|| "attempt_executor_unavailable".to_owned())?;
            let image = self
                .container_image
                .as_deref()
                .ok_or_else(|| "trusted_run_image_unavailable".to_owned())?;
            let argv = input.argv;
            let command_digest = digest(&serde_json::to_vec(&argv).expect("argv serializes"));
            let config_digest = format!("sha256:{}", hex(&self.config.digest()));
            let plan = crate::executor::backends::container::prepare_captured(
                &profile,
                acquisition,
                &self.build,
                &self.temp,
                "native-run",
                image,
                argv.clone(),
                &input.environment,
                crate::executor::backends::container::CheckExecutionRequest {
                    program: &argv[0],
                    arguments: &argv[1..],
                    binary_digest: &command_digest,
                    config_digest: &config_digest,
                },
            )
            .map_err(code("executor_isolation_unavailable"))?;
            let report = plan
                .run_registered(
                    crate::domain::lifecycle::ProcessOwnership::Attempt(attempt),
                    &self.cancellation,
                    WorkspaceIdentity::from_acquisition(self.workspace_id, acquisition),
                    registration.clone(),
                    false,
                )
                .map_err(code("attempt_executor_unavailable"))?;
            let child = report
                .child_output
                .ok_or_else(|| "executor_output_unavailable".to_owned())?;
            let stdout = self.persist_log(&child.stdout.bytes)?;
            let stderr = self.persist_log(&child.stderr.bytes)?;
            let evidence = json!({
                "boundary_id": report.evidence.boundary_id,
                "boundary_absent": report.evidence.boundary_absent,
                "helper_identity": report.evidence.helper_identity,
                "image_digest": report.evidence.resolved_image_digest,
                "inspected": report.evidence.inspected,
                "invocation_digest": report.evidence.invocation_digest,
                "kill_attempted": report.evidence.kill_attempted,
                "plan_digest": report.evidence.plan_digest,
                "process_id": report.evidence.process_id,
                "quiescent": report.evidence.quiescent,
                "reaped": report.evidence.reaped,
                "runtime_identity": report.evidence.runtime_identity,
                "survivors": report.evidence.survivors,
            });
            let process =
                self.persist_report(&serde_json::to_vec(&evidence).expect("evidence serializes"))?;
            return Ok((
                json!({
                    "outcome": match report.outcome {
                        crate::executor::backends::container::ExecutionOutcome::Success => json!({"status": "success", "exit_code": 0}),
                        crate::executor::backends::container::ExecutionOutcome::Exit(code) => json!({"status": "exit", "exit_code": code}),
                        crate::executor::backends::container::ExecutionOutcome::Signal(signal) => json!({"status": "signal", "signal": signal}),
                    },
                    "process_artifact": process.clone(),
                    "stderr_artifact": stderr.clone(),
                    "stdout_artifact": stdout.clone(),
                }),
                vec![stdout, stderr, process],
            ));
        }
        let paths = SandboxPaths::new(&self.root, &self.build, &self.temp)
            .map_err(code("executor_paths_rejected"))?;
        let backend = LocalOsBackend::select(&profile, &paths)
            .map_err(code("executor_isolation_unavailable"))?;
        let mut command = LocalCommand::new(&input.argv[0], &self.root);
        for argument in input.argv.into_iter().skip(1) {
            command = command.arg(argument);
        }
        for (key, value) in input.environment {
            command = command.env(key, value);
        }
        let _prepared = backend
            .prepare(&profile, &paths, command)
            .map_err(code("executor_prepare_failed"))?;
        // M003 currently exposes no attempt-owned local launch authority. Never
        // fall back to an unowned host process from a model tool.
        Err("attempt_executor_unavailable".to_owned())
    }

    #[cfg(test)]
    fn run_conformance(
        &mut self,
        input: RunInput,
        _attempt: crate::domain::lifecycle::AttemptOwnership,
    ) -> Result<(Value, Vec<String>), String> {
        let command = crate::executor::check::CheckCommand::new(
            "native-run",
            input.argv[0].clone(),
            input.argv[1..].to_vec(),
            format!("example.invalid/native-run@sha256:{}", "a".repeat(64)),
            format!("sha256:{}", "b".repeat(64)),
            format!("sha256:{}", "c".repeat(64)),
            input.limits,
        )
        .map_err(code("run_request_rejected"))?;
        let source_digest = crate::executor::check::immutable_tree_digest(&self.root)
            .map_err(code("run_tree_failed"))?;
        let completion = self
            .run_runner
            .as_mut()
            .expect("test run runner was checked")
            .execute(crate::executor::check::CheckExecutionRequest {
                command: &command,
                immutable_source: &self.root,
                source_digest: &source_digest,
                build: &self.build,
                temp: &self.temp,
                max_preview_bytes: 16 * 1024,
                artifacts: &self.artifacts,
                principal: &self.authenticated.principal_id().to_string(),
                project: &self.config.project_id().to_string(),
                retention: ArtifactRetention::Forever,
                stored_at_unix_micros: crate::store::artifacts::now_unix_micros()
                    .map_err(code("artifact_clock_unavailable"))?,
                secrets: &self.secrets,
                more_boundaries: false,
            })
            .map_err(|_| {
                if self.cancelled() {
                    "cancelled".to_owned()
                } else {
                    "attempt_executor_unavailable".to_owned()
                }
            })?;
        let stdout = completion.stdout_artifact.reference().to_owned();
        let stderr = completion.stderr_artifact.reference().to_owned();
        let process = completion.process_artifact.reference().to_owned();
        Ok((
            json!({
                "outcome": match completion.status {
                    crate::executor::check::CheckStatus::Pass => json!({"status": "success", "exit_code": 0}),
                    crate::executor::check::CheckStatus::Exit(code) => json!({"status": "exit", "exit_code": code}),
                },
                "process": completion.process,
                "process_artifact": process,
                "stderr_artifact": stderr,
                "stdout_artifact": stdout,
            }),
            vec![stdout, stderr, process],
        ))
    }

    fn check(
        &mut self,
        bytes: &[u8],
        attempt: crate::domain::lifecycle::AttemptOwnership,
    ) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: CheckInput = decode(bytes)?;
        if input.profile == CheckProfile::Targeted && input.targets.is_empty() {
            return Err("check_targets_required".to_owned());
        }
        if input.profile == CheckProfile::Full
            && !self
                .config
                .effective_authority()
                .contains(&Grant::VerificationFull)
        {
            return Err("verification_full_grant_required".to_owned());
        }
        if self.verification_registry.is_empty() {
            return Err("trusted_check_registry_unavailable".to_owned());
        }
        if self.check_runner.is_none() {
            return Err("trusted_check_runner_unavailable".to_owned());
        }
        let feedback = self
            .feedback
            .as_ref()
            .ok_or_else(|| "trusted_check_feedback_unavailable".to_owned())?
            .clone();
        if feedback.adapters.is_empty() {
            return Err("trusted_check_feedback_unavailable".to_owned());
        }
        let selection = match input.profile {
            CheckProfile::Syntax => ProfileSelection::Syntax,
            CheckProfile::Fast => ProfileSelection::Fast,
            CheckProfile::Targeted => ProfileSelection::Targeted {
                exact_targets: input.targets.into_iter().collect(),
            },
            CheckProfile::Full => ProfileSelection::Full,
        };
        if self
            .verification_registry
            .select_native(&selection, &self.grants, &self.config)
            .map_err(code("check_profile_rejected"))?
            .is_empty()
        {
            return Err("check_profile_empty".to_owned());
        }
        let revision = self
            .ensure_workspace()?
            .current_revision()
            .map_err(code("check_tree_failed"))?;
        let plan_digest = format!("blake3:{}", blake3::hash(bytes).to_hex());
        let context = crate::workspace::edit::validate::EditOperationContext::current(
            revision.id().to_string(),
            revision.epoch().to_string(),
            revision.digest().to_string(),
            plan_digest,
        );
        let authority = crate::verify::feedback::FeedbackAuthority::issue(
            &self.authenticated,
            self.workspace_id.to_string(),
            self.config.run_id().to_string(),
            context.selected_plan_digest(),
            attempt.fencing_token.get(),
        )
        .map_err(code("check_feedback_authority_unavailable"))?;
        let mut events = crate::verify::feedback::FeedbackEventStore::open(&feedback.database)
            .map_err(code("check_feedback_store_unavailable"))?;
        let mut observer = crate::verify::feedback::FeedbackVerificationObserver::from_context(
            &mut events,
            &authority,
            &context,
            context.base_workspace_digest(),
        );
        let runner = self
            .check_runner
            .as_mut()
            .ok_or_else(|| "trusted_check_runner_unavailable".to_owned())?;
        let result = crate::verify::profiles::verify_current(
            &context,
            &self.root,
            &self.build,
            &self.temp,
            BTreeSet::new(),
            crate::verify::profiles::VerificationRequest {
                selection,
                registry: &self.verification_registry,
                authenticated: &self.authenticated,
                grants: &self.grants,
                config: &self.config,
                runner: Some(runner),
                observer: Some(&mut observer),
                artifacts: &self.artifacts,
                secrets: &self.secrets,
                on_check_failure: crate::verify::profiles::CheckFailureBehavior::Abort,
                model_outcome: None,
                cancellation: Some(&self.live_cancellation),
            },
            false,
        )
        .map_err(code("check_execution_failed"))?;
        drop(observer);
        let feedback_output = {
            let mut pipeline = crate::verify::feedback::FeedbackPipeline::new(
                &self.artifacts,
                &mut events,
                &self.authenticated,
                self.workspace_id.to_string(),
                ArtifactRetention::Forever,
                crate::store::artifacts::now_unix_micros()
                    .map_err(code("artifact_clock_unavailable"))?,
                &self.secrets,
                feedback.limits.clone(),
            )
            .map_err(code("check_feedback_unavailable"))?;
            let baseline = pipeline
                .capture_baseline(&authority, &context, &result, &feedback.adapters)
                .map_err(code("check_feedback_unavailable"))?;
            pipeline
                .process_result(
                    &authority,
                    Some(&baseline),
                    &context,
                    context.base_workspace_digest(),
                    &result,
                    &crate::verify::feedback::EditMapping::default(),
                    &feedback.adapters,
                )
                .map_err(code("check_feedback_unavailable"))?
        };
        let artifacts = vec![
            result.receipt().result_artifact.reference.clone(),
            feedback_output.payload_artifact.reference.clone(),
            feedback_output.report_artifact.reference.clone(),
        ];
        Ok((
            json!({
                "feedback": feedback_output.payload,
                "events": feedback_output.events,
                "verification": result,
            }),
            artifacts,
        ))
    }

    fn persist_log(&self, bytes: &[u8]) -> Result<String, String> {
        self.persist_artifact(
            bytes,
            "application/octet-stream",
            crate::store::artifacts::ArtifactClass::Log,
        )
    }

    fn persist_report(&self, bytes: &[u8]) -> Result<String, String> {
        self.persist_artifact(
            bytes,
            "application/json",
            crate::store::artifacts::ArtifactClass::Report,
        )
    }

    fn persist_artifact(
        &self,
        bytes: &[u8],
        media_type: &str,
        class: crate::store::artifacts::ArtifactClass,
    ) -> Result<String, String> {
        self.ensure_not_cancelled()?;
        let capture = self.custody.project(CaptureBoundary::Artifact, bytes);
        let bytes = capture.bytes().map_err(code("artifact_redaction_failed"))?;
        self.artifacts
            .put(
                bytes,
                crate::store::artifacts::ArtifactMetadata::new(
                    media_type,
                    class,
                    self.authenticated.principal_id().to_string(),
                    self.config.project_id().to_string(),
                    ArtifactRetention::Forever,
                    crate::store::artifacts::now_unix_micros()
                        .map_err(code("artifact_clock_unavailable"))?,
                )
                .map_err(code("artifact_metadata_failed"))?,
            )
            .map(|artifact| artifact.reference().to_string())
            .map_err(code("artifact_persistence_failed"))
    }

    fn ensure_not_cancelled(&self) -> Result<(), String> {
        if self.cancelled() {
            Err("cancelled".to_owned())
        } else {
            Ok(())
        }
    }
}

fn native_edit_error(
    error: crate::agent::adapters::grammar_edit::EditOrchestrationError,
) -> String {
    use crate::agent::adapters::grammar_edit::EditOrchestrationError;
    match error {
        EditOrchestrationError::Grammar(_) => "edit_input_rejected",
        EditOrchestrationError::Validation(error) => {
            return format!("edit_validation_failed:{}", validation_error_detail(&error));
        }
        EditOrchestrationError::Stage(_) => "edit_stage_failed",
        EditOrchestrationError::Verification(_) | EditOrchestrationError::VerificationRejected => {
            "edit_verification_failed"
        }
        EditOrchestrationError::Cancelled => "cancelled",
        EditOrchestrationError::Recovery(_) => "edit_recovery_failed",
        EditOrchestrationError::Feedback(_) => "edit_feedback_failed",
    }
    .to_owned()
}

fn validation_error_detail(
    error: &crate::workspace::edit::validate::ValidationError,
) -> &'static str {
    use crate::workspace::edit::validate::{ValidationError, ValidationLimit};
    match error {
        ValidationError::IdentityPolicyMismatch => "identity_policy_mismatch",
        ValidationError::StaleRevision => "stale_revision",
        ValidationError::ExternalEdit => "external_edit",
        ValidationError::AmbiguousAnchor(_) => "ambiguous_anchor",
        ValidationError::AnchorMismatch(_) => "anchor_mismatch",
        ValidationError::BaseDigestMismatch(_) => "base_digest_mismatch",
        ValidationError::InvalidUnicode(_) => "invalid_unicode",
        ValidationError::NewlineMismatch(_) => "newline_mismatch",
        ValidationError::FinalNewlineMismatch(_) => "final_newline_mismatch",
        ValidationError::BinaryFile(_) => "binary_file",
        ValidationError::RangeOutsideFile(_) => "range_outside_file",
        ValidationError::UnsafePath(_) => "unsafe_path",
        ValidationError::PathStateMismatch => "path_state_mismatch",
        ValidationError::LimitExceeded(limit) => match limit {
            ValidationLimit::Operations => "operations_limit",
            ValidationLimit::Path => "path_limit",
            ValidationLimit::Content => "content_limit",
            ValidationLimit::ReadBytes => "read_bytes_limit",
            ValidationLimit::Memory => "memory_limit",
            ValidationLimit::Time => "time_limit",
            ValidationLimit::Authorization => "authorization_limit",
        },
        ValidationError::Unavailable => "unavailable",
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DiscoverInput {
    Legacy(Box<LegacyDiscoverInput>),
    Map(Box<MapDiscoverInput>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDiscoverInput {
    expected_revision: String,
    terms: Vec<String>,
    roots: Vec<String>,
    languages: Vec<String>,
    #[serde(default)]
    cursor: Option<DiscoverCursor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MapDiscoverInput {
    expected_revision: String,
    map: NativeMapInput,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeMapInput {
    #[serde(default)]
    task_terms: Vec<String>,
    #[serde(default)]
    exact_identifiers: Vec<String>,
    #[serde(default)]
    stack_frames: Vec<NativeMapStackFrame>,
    #[serde(default)]
    recently_read_paths: Vec<String>,
    #[serde(default)]
    current_edit_paths: Vec<String>,
    #[serde(default)]
    path_prefixes: Vec<String>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    history_paths: Vec<String>,
    #[serde(default)]
    blame_paths: Vec<String>,
    #[serde(default)]
    relationships: Option<Vec<String>>,
    #[serde(default)]
    expansion_seeds: Vec<String>,
    #[serde(default)]
    graph_seeds: Vec<String>,
    #[serde(default)]
    expand_paths: Vec<String>,
    #[serde(default)]
    expand_symbols: Vec<String>,
    #[serde(default)]
    expand_packages: Vec<String>,
    #[serde(default)]
    expand_tests: Vec<String>,
    #[serde(default)]
    score_band: Option<NativeMapScoreBand>,
    #[serde(default)]
    purpose: Option<NativeMapPurpose>,
    #[serde(default)]
    budgets: NativeMapBudgets,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeMapStackFrame {
    path: String,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    line: Option<usize>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativeMapPurpose {
    Dependencies,
    Dependents,
    Neighborhood,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeMapBudgets {
    #[serde(default)]
    items: Option<usize>,
    #[serde(default)]
    estimated_tokens: Option<usize>,
    #[serde(default)]
    hops: Option<usize>,
    #[serde(default)]
    degree: Option<usize>,
    #[serde(default)]
    result_bytes: Option<usize>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeMapScoreBand {
    min: u64,
    max: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    expected_revision: String,
    text: String,
    mode: SearchModeInput,
    path_prefixes: Vec<String>,
    languages: Vec<String>,
    #[serde(default)]
    rewrite: Option<String>,
    #[serde(default)]
    cursor: Option<SearchCursor>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchModeInput {
    Path,
    Content,
    PathAndContent,
    Structural,
}

impl From<SearchModeInput> for SearchMode {
    fn from(value: SearchModeInput) -> Self {
        match value {
            SearchModeInput::Path => Self::Path,
            SearchModeInput::Content => Self::Content,
            SearchModeInput::PathAndContent => Self::PathAndContent,
            SearchModeInput::Structural => {
                unreachable!("structural search is dispatched separately")
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    expected_revision: String,
    path: String,
    range: ReadRangeInput,
    #[serde(default)]
    cursor: Option<ReadProjectionCursor>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadProjectionCursor {
    version: u16,
    projection_state: String,
    custody_revision: u64,
    tag: String,
}

struct ReadReplay {
    cursor: ReadProjectionCursor,
    binding: Vec<u8>,
    projection_state: crate::domain::secret::JsonProjectionState,
    data: Value,
    artifacts: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ReadRangeInput {
    Full,
    Bytes { start: usize, end: usize },
    Lines { start: usize, end: usize },
}

fn read_cursor_binding(
    input: &ReadInput,
    principal: &str,
    project: &str,
) -> Result<Vec<u8>, String> {
    let mut binding = b"KIT-NATIVE-READ-CURSOR\0\x01".to_vec();
    for value in [
        principal.as_bytes(),
        project.as_bytes(),
        input.expected_revision.as_bytes(),
        input.path.as_bytes(),
    ] {
        binding.extend_from_slice(&(value.len() as u64).to_be_bytes());
        binding.extend_from_slice(value);
    }
    match &input.range {
        ReadRangeInput::Full => binding.push(0),
        ReadRangeInput::Bytes { start, end } => {
            binding.push(1);
            binding.extend_from_slice(
                &u64::try_from(*start)
                    .map_err(|_| "read_cursor_invalid".to_owned())?
                    .to_be_bytes(),
            );
            binding.extend_from_slice(
                &u64::try_from(*end)
                    .map_err(|_| "read_cursor_invalid".to_owned())?
                    .to_be_bytes(),
            );
        }
        ReadRangeInput::Lines { start, end } => {
            binding.push(2);
            binding.extend_from_slice(
                &u64::try_from(*start)
                    .map_err(|_| "read_cursor_invalid".to_owned())?
                    .to_be_bytes(),
            );
            binding.extend_from_slice(
                &u64::try_from(*end)
                    .map_err(|_| "read_cursor_invalid".to_owned())?
                    .to_be_bytes(),
            );
        }
    }
    Ok(binding)
}

fn seal_read_cursor(
    state: &crate::domain::secret::JsonProjectionState,
    binding: &[u8],
    key: &[u8; 32],
) -> Result<ReadProjectionCursor, String> {
    let serialized = state
        .to_bounded_bytes()
        .ok_or_else(|| "read_projection_state_too_large".to_owned())?;
    let encrypted = xor_read_cursor_state(key, binding, &serialized);
    let version = crate::domain::secret::JsonProjectionState::VERSION;
    let revision = state.custody_revision();
    let tag = crate::domain::crypto::hmac_sha256_domain(
        key,
        b"KIT-NATIVE-READ-CURSOR-TAG\0",
        &[
            binding,
            &version.to_be_bytes(),
            &revision.to_be_bytes(),
            &encrypted,
        ],
    );
    Ok(ReadProjectionCursor {
        version,
        projection_state: hex(&encrypted),
        custody_revision: revision,
        tag: hex(&tag),
    })
}

fn open_read_cursor(
    cursor: &ReadProjectionCursor,
    binding: &[u8],
    key: &[u8; 32],
    custody: &crate::domain::secret::SecretCustody,
) -> Result<crate::domain::secret::JsonProjectionState, String> {
    if cursor.version != crate::domain::secret::JsonProjectionState::VERSION
        || cursor.custody_revision != custody.revision()
    {
        return Err("read_cursor_invalid".to_owned());
    }
    let encrypted = decode_hex_bytes(&cursor.projection_state)
        .ok_or_else(|| "read_cursor_invalid".to_owned())?;
    let actual_tag =
        decode_hex_bytes(&cursor.tag).ok_or_else(|| "read_cursor_invalid".to_owned())?;
    let expected_tag = crate::domain::crypto::hmac_sha256_domain(
        key,
        b"KIT-NATIVE-READ-CURSOR-TAG\0",
        &[
            binding,
            &cursor.version.to_be_bytes(),
            &cursor.custody_revision.to_be_bytes(),
            &encrypted,
        ],
    );
    if !crate::domain::crypto::constant_time_eq(&actual_tag, &expected_tag) {
        return Err("read_cursor_invalid".to_owned());
    }
    let serialized = xor_read_cursor_state(key, binding, &encrypted);
    let state = crate::domain::secret::JsonProjectionState::from_bounded_bytes(&serialized)
        .ok_or_else(|| "read_cursor_invalid".to_owned())?;
    (state.custody_revision() == cursor.custody_revision)
        .then_some(state)
        .ok_or_else(|| "read_cursor_invalid".to_owned())
}

fn xor_read_cursor_state(key: &[u8; 32], binding: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    for (counter, chunk) in bytes.chunks(32).enumerate() {
        let mask = crate::domain::crypto::hmac_sha256_domain(
            key,
            b"KIT-NATIVE-READ-CURSOR-MASK\0",
            &[binding, &(counter as u64).to_be_bytes()],
        );
        output.extend(chunk.iter().zip(mask).map(|(byte, mask)| byte ^ mask));
    }
    output
}

fn decode_hex_bytes(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) || !value.is_ascii() {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|digits| {
            let digits = std::str::from_utf8(digits).ok()?;
            u8::from_str_radix(digits, 16).ok()
        })
        .collect()
}

impl From<ReadRangeInput> for ReadRange {
    fn from(value: ReadRangeInput) -> Self {
        match value {
            ReadRangeInput::Full => Self::Full,
            ReadRangeInput::Bytes { start, end } => Self::Bytes { start, end },
            ReadRangeInput::Lines { start, end } => Self::Lines { start, end },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunInput {
    argv: Vec<String>,
    working_directory: String,
    mounts: RunMounts,
    environment: BTreeMap<String, String>,
    network: NetworkPolicy,
    limits: ResourceLimits,
    host_compatibility: bool,
    #[serde(rename = "background")]
    _background: RunBackground,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunBackground {
    Foreground,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct RunMounts {
    source: MountPolicy,
    build: MountPolicy,
    temp: MountPolicy,
}

impl RunMounts {
    const fn required() -> Self {
        Self {
            source: MountPolicy::ReadOnly,
            build: MountPolicy::ReadWrite,
            temp: MountPolicy::ReadWrite,
        }
    }
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum MountPolicy {
    ReadOnly,
    ReadWrite,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum NetworkPolicy {
    Deny,
    ProfileGrants,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckInput {
    profile: CheckProfile,
    targets: Vec<String>,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CheckProfile {
    Syntax,
    Fast,
    Targeted,
    Full,
}

fn bounded_discover_options() -> DiscoverOptions {
    DiscoverOptions {
        max_result_bytes: MAX_NATIVE_OUTPUT_BYTES / 2,
        max_time: std::time::Duration::from_secs(30),
        ..DiscoverOptions::default()
    }
}

fn native_structure_requested(request: &RepositoryMapRequest) -> bool {
    !request.expansion.graph_seeds.is_empty()
        || !request.expansion.packages.is_empty()
        || !request.expansion.tests.is_empty()
        || !request.history_paths.is_empty()
        || !request.blame_paths.is_empty()
        || request
            .expansion
            .relationships
            .iter()
            .any(|relationship| relationship.is_structure())
}

fn native_history_requested(request: &RepositoryMapRequest) -> bool {
    !request.history_paths.is_empty()
        || !request.blame_paths.is_empty()
        || request
            .expansion
            .relationships
            .contains(&RelationshipKind::ChangedWith)
}

fn native_history_request_digest(request: &HistoryRequest) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-native-history-request-v1\0");
    hash.update(&[u8::from(request.include_changed_with())]);
    for paths in [request.scope(), request.blame_paths()] {
        hash.update(&(paths.len() as u64).to_le_bytes());
        for path in paths {
            native_digest_frame(&mut hash, path.as_str().as_bytes());
        }
    }
    *hash.finalize().as_bytes()
}

fn native_semantic_digest(stored: &[NativeSemanticRelationship]) -> [u8; 32] {
    let mut facts = stored
        .iter()
        .map(|relationship| {
            let fact = &relationship.fact;
            let provenance = fact.provenance();
            let origin = provenance.origin();
            let mut hash = blake3::Hasher::new();
            hash.update(&relationship.source_declaration.as_bytes());
            hash.update(&[fact.relation() as u8]);
            native_digest_frame(&mut hash, fact.path().as_path().as_str().as_bytes());
            hash.update(&(fact.range().start() as u128).to_le_bytes());
            hash.update(&(fact.range().end() as u128).to_le_bytes());
            if let Some(range) = fact.target_range() {
                hash.update(&[1]);
                hash.update(&(range.start() as u128).to_le_bytes());
                hash.update(&(range.end() as u128).to_le_bytes());
            } else {
                hash.update(&[0]);
            }
            hash.update(&(fact.origin_point() as u128).to_le_bytes());
            hash.update(&(fact.origin_range().start() as u128).to_le_bytes());
            hash.update(&(fact.origin_range().end() as u128).to_le_bytes());
            hash.update(provenance.revision().as_bytes());
            native_digest_frame(&mut hash, origin.uri().as_bytes());
            hash.update(&origin.document_version().get().to_le_bytes());
            hash.update(&origin.request_generation().to_le_bytes());
            hash.update(&origin.request_id().get().to_le_bytes());
            native_digest_frame(
                &mut hash,
                provenance.server().server_artifact.as_str().as_bytes(),
            );
            native_digest_frame(
                &mut hash,
                provenance.server().configuration.as_str().as_bytes(),
            );
            hash.update(&[provenance.position_encoding() as u8]);
            *hash.finalize().as_bytes()
        })
        .collect::<Vec<_>>();
    facts.sort_unstable();
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-native-structure-semantic-v1\0");
    for fact in facts {
        hash.update(&fact);
    }
    *hash.finalize().as_bytes()
}

fn native_digest_frame(hash: &mut blake3::Hasher, value: &[u8]) {
    hash.update(&(value.len() as u64).to_le_bytes());
    hash.update(value);
}

fn native_map_request(
    input: NativeMapInput,
) -> Result<(RepositoryMapRequest, Option<MapCursor>), NativeMapError> {
    const MAX_PATH: usize = 4096;
    const MAX_TEXT: usize = 256;
    validate_map_values(&input.task_terms, 32, MAX_TEXT)?;
    validate_map_values(&input.languages, 32, 64)?;
    validate_map_paths(&input.recently_read_paths, 32, MAX_PATH)?;
    validate_map_paths(&input.current_edit_paths, 32, MAX_PATH)?;
    validate_map_paths(&input.path_prefixes, 32, MAX_PATH)?;
    validate_map_paths(
        &input.history_paths,
        NATIVE_MAP_MAX_EXPANSION_SELECTORS,
        MAX_PATH,
    )?;
    validate_map_paths(
        &input.blame_paths,
        NATIVE_MAP_MAX_EXPANSION_SELECTORS,
        MAX_PATH,
    )?;
    validate_map_paths(
        &input.expand_paths,
        NATIVE_MAP_MAX_EXPANSION_SELECTORS,
        MAX_PATH,
    )?;
    validate_map_values(
        &input.expand_symbols,
        NATIVE_MAP_MAX_EXPANSION_SELECTORS,
        MAX_TEXT,
    )?;
    validate_map_values(
        &input.expand_packages,
        NATIVE_MAP_MAX_EXPANSION_SELECTORS,
        MAX_PATH,
    )?;
    validate_map_values(
        &input.expand_tests,
        NATIVE_MAP_MAX_EXPANSION_SELECTORS,
        MAX_PATH,
    )?;
    if input.exact_identifiers.len() > 128
        || input.expansion_seeds.len() > NATIVE_MAP_MAX_EXPANSION_SELECTORS
        || input.graph_seeds.len() > NATIVE_MAP_MAX_EXPANSION_SELECTORS
        || input.stack_frames.len() > 32
        || input.score_band.is_some_and(|band| band.min > band.max)
    {
        return Err(NativeMapError::InvalidRequest);
    }
    let exact_declaration_ids = input
        .exact_identifiers
        .iter()
        .map(|value| DeclarationId::parse(value).ok_or(NativeMapError::InvalidRequest))
        .collect::<Result<Vec<_>, _>>()?;
    let seeds = input
        .expansion_seeds
        .iter()
        .map(|value| DeclarationId::parse(value).ok_or(NativeMapError::InvalidRequest))
        .collect::<Result<Vec<_>, _>>()?;
    let graph_seeds = input
        .graph_seeds
        .iter()
        .map(|value| {
            DeclarationId::parse(value)
                .map(|id| NodeId::from_bytes(id.as_bytes()))
                .ok_or(NativeMapError::InvalidRequest)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let paths = input
        .expand_paths
        .into_iter()
        .map(|value| {
            RootRelativePath::parse(value, MAX_PATH).map_err(|_| NativeMapError::InvalidRequest)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let history_paths = input
        .history_paths
        .into_iter()
        .map(|value| {
            RootRelativePath::parse(value, MAX_PATH).map_err(|_| NativeMapError::InvalidRequest)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let blame_paths = input
        .blame_paths
        .into_iter()
        .map(|value| {
            RootRelativePath::parse(value, MAX_PATH).map_err(|_| NativeMapError::InvalidRequest)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let stack_frames = input
        .stack_frames
        .into_iter()
        .map(|frame| {
            validate_map_path(&frame.path, MAX_PATH)?;
            if frame
                .symbol
                .as_ref()
                .is_some_and(|symbol| symbol.is_empty() || symbol.len() > MAX_TEXT)
                || frame
                    .line
                    .is_some_and(|line| !(1..=10_000_000).contains(&line))
            {
                return Err(NativeMapError::InvalidRequest);
            }
            Ok(StackFrame {
                path: frame.path.into(),
                symbol: frame.symbol,
                line: frame.line,
            })
        })
        .collect::<Result<Vec<_>, NativeMapError>>()?;
    let relationships = input.relationships.map_or_else(
        || {
            Ok(vec![
                RelationshipKind::Contains,
                RelationshipKind::ContainedBy,
            ])
        },
        |values| {
            if values.len() > NATIVE_MAP_MAX_RELATIONSHIPS
                || values.iter().collect::<BTreeSet<_>>().len() != values.len()
            {
                return Err(NativeMapError::InvalidRequest);
            }
            values
                .into_iter()
                .map(|value| match value.as_str() {
                    "contains" => Ok(RelationshipKind::Contains),
                    "contained_by" => Ok(RelationshipKind::ContainedBy),
                    "semantic_declaration" => Ok(RelationshipKind::SemanticDeclaration),
                    "semantic_definition" => Ok(RelationshipKind::SemanticDefinition),
                    "semantic_type_definition" => Ok(RelationshipKind::SemanticTypeDefinition),
                    "semantic_implementation" => Ok(RelationshipKind::SemanticImplementation),
                    "semantic_reference" => Ok(RelationshipKind::SemanticReference),
                    "defines" => Ok(RelationshipKind::Defines),
                    "imports" => Ok(RelationshipKind::Imports),
                    "exports" => Ok(RelationshipKind::Exports),
                    "references" => Ok(RelationshipKind::References),
                    "calls" => Ok(RelationshipKind::Calls),
                    "implements" => Ok(RelationshipKind::Implements),
                    "inherits" => Ok(RelationshipKind::Inherits),
                    "overrides" => Ok(RelationshipKind::Overrides),
                    "tests" => Ok(RelationshipKind::Tests),
                    "changed_with" => Ok(RelationshipKind::ChangedWith),
                    _ => Err(NativeMapError::InvalidRequest),
                })
                .collect()
        },
    )?;
    let budget = MapBudget {
        max_items: input.budgets.items.unwrap_or(NATIVE_MAP_MAX_ITEMS),
        max_estimated_tokens: input
            .budgets
            .estimated_tokens
            .unwrap_or(NATIVE_MAP_MAX_ESTIMATED_TOKENS),
        max_hops: input.budgets.hops.unwrap_or(NATIVE_MAP_MAX_HOPS),
        max_degree: input.budgets.degree.unwrap_or(NATIVE_MAP_MAX_DEGREE),
        max_result_bytes: input
            .budgets
            .result_bytes
            .unwrap_or(NATIVE_MAP_MAX_RESULT_BYTES),
    };
    validate_native_map_budget(budget)?;
    let cursor = input
        .cursor
        .as_deref()
        .map(MapCursor::from_token)
        .transpose()
        .map_err(|_| NativeMapError::CursorInvalid)?;
    Ok((
        RepositoryMapRequest {
            personalization: Personalization {
                task_terms: input.task_terms,
                exact_declaration_ids,
                stack_frames,
                recently_read_paths: input
                    .recently_read_paths
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
                current_edit_paths: input
                    .current_edit_paths
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
            },
            budget,
            expansion: ExpansionRequest {
                seeds,
                graph_seeds,
                paths,
                symbols: input.expand_symbols,
                packages: input.expand_packages,
                tests: input.expand_tests,
                score_band: input.score_band.map(|band| ScoreBand {
                    min: band.min,
                    max: band.max,
                }),
                purpose: match input.purpose.unwrap_or(NativeMapPurpose::Neighborhood) {
                    NativeMapPurpose::Dependencies => ExpansionPurpose::Dependencies,
                    NativeMapPurpose::Dependents => ExpansionPurpose::Dependents,
                    NativeMapPurpose::Neighborhood => ExpansionPurpose::Neighborhood,
                },
                relationships,
            },
            path_prefixes: input.path_prefixes.into_iter().map(PathBuf::from).collect(),
            languages: input.languages,
            history_paths,
            blame_paths,
        },
        cursor,
    ))
}

fn validate_map_values(
    values: &[String],
    maximum: usize,
    max_length: usize,
) -> Result<(), NativeMapError> {
    if values.len() > maximum
        || values
            .iter()
            .any(|value| value.is_empty() || value.len() > max_length)
    {
        Err(NativeMapError::InvalidRequest)
    } else {
        Ok(())
    }
}

fn validate_map_paths(
    values: &[String],
    maximum: usize,
    max_length: usize,
) -> Result<(), NativeMapError> {
    if values.len() > maximum {
        return Err(NativeMapError::InvalidRequest);
    }
    values
        .iter()
        .try_for_each(|value| validate_map_path(value, max_length))
}

fn validate_map_path(value: &str, max_length: usize) -> Result<(), NativeMapError> {
    RootRelativePath::parse(value.to_owned(), max_length)
        .map(|_| ())
        .map_err(|_| NativeMapError::InvalidRequest)
}

fn validate_native_map_budget(budget: MapBudget) -> Result<(), NativeMapError> {
    for (actual, maximum, allow_zero, bound) in [
        (
            budget.max_items,
            NATIVE_MAP_MAX_ITEMS,
            false,
            MapBound::Items,
        ),
        (
            budget.max_estimated_tokens,
            NATIVE_MAP_MAX_ESTIMATED_TOKENS,
            false,
            MapBound::EstimatedTokens,
        ),
        (budget.max_hops, NATIVE_MAP_MAX_HOPS, true, MapBound::Hops),
        (
            budget.max_degree,
            NATIVE_MAP_MAX_DEGREE,
            false,
            MapBound::Degree,
        ),
        (
            budget.max_result_bytes,
            NATIVE_MAP_MAX_RESULT_BYTES,
            false,
            MapBound::ResultBytes,
        ),
    ] {
        if actual > maximum || (actual == 0 && !allow_zero) {
            return Err(NativeMapError::Bound(bound));
        }
    }
    Ok(())
}

fn bounded_map_limits() -> MapLimits {
    MapLimits {
        max_items: NATIVE_MAP_MAX_ITEMS,
        max_estimated_tokens: NATIVE_MAP_MAX_ESTIMATED_TOKENS,
        max_hops: NATIVE_MAP_MAX_HOPS,
        max_degree: NATIVE_MAP_MAX_DEGREE,
        max_result_bytes: NATIVE_MAP_MAX_RESULT_BYTES,
        max_task_terms: 32,
        max_exact_ids: 128,
        max_stack_frames: 32,
        max_recent_paths: 32,
        max_current_edit_paths: 32,
        max_path_filters: 32,
        max_language_filters: 32,
        max_expansion_seeds: 128,
        max_expansion_paths: NATIVE_MAP_MAX_EXPANSION_SELECTORS,
        max_expansion_symbols: NATIVE_MAP_MAX_EXPANSION_SELECTORS,
        max_relationship_kinds: NATIVE_MAP_MAX_RELATIONSHIPS,
        max_semantic_relationships: NATIVE_MAP_MAX_SEMANTIC_RELATIONSHIPS,
        max_input_bytes: 256 * 1024,
        max_work: 1_000_000,
        max_memory_bytes: 64 * 1024 * 1024,
        max_candidates: 100_000,
        max_highlight_bytes: 1024,
        max_cursor_frontier: 10_000,
        max_time: Duration::from_secs(30),
    }
}

fn bounded_graph_map_limits(
    graph_requested: bool,
) -> Result<(GraphOptions, MapLimits), NativeMapError> {
    graph_map_limits_with_caps(
        graph_requested,
        NATIVE_MAP_TOTAL_WORK,
        NATIVE_MAP_TOTAL_MEMORY_BYTES,
        NATIVE_MAP_TOTAL_TIME,
    )
}

fn bounded_unified_map_limits(
    graph_requested: bool,
    history_requested: bool,
) -> Result<(GraphOptions, HistoryOptions, MapLimits), NativeMapError> {
    let (graph, mut map) = bounded_graph_map_limits(graph_requested)?;
    let mut history = HistoryOptions::default();
    if history_requested {
        if !graph_requested {
            return Err(NativeMapError::Unavailable);
        }
        let fence = ValidatedHistoryFence::conservative_metrics(4);
        history.max_work = NATIVE_HISTORY_TOTAL_WORK
            .checked_sub(graph.max_work)
            .and_then(|work| work.checked_sub(map.max_work))
            .and_then(|work| work.checked_sub(fence.work()))
            .and_then(|work| work.checked_sub(10_000_000))
            .filter(|work| *work != 0)
            .ok_or(NativeMapError::Unavailable)?;
        history.max_staging_bytes = (512_usize * 1024 * 1024)
            .checked_sub(fence.peak_memory_bytes())
            .filter(|bytes| *bytes != 0)
            .ok_or(NativeMapError::Unavailable)?;
        history.max_commands = history
            .max_commands
            .checked_sub(fence.commands())
            .filter(|commands| *commands != 0)
            .ok_or(NativeMapError::Unavailable)?;
        history.max_output_bytes = history
            .max_output_bytes
            .checked_sub(fence.output_bytes())
            .filter(|bytes| *bytes != 0)
            .ok_or(NativeMapError::Unavailable)?;
        history.max_cache_bytes = history.max_cache_bytes.min(history.max_staging_bytes / 2);
        history.max_time = Duration::from_secs(30);
        map.max_time = Duration::from_secs(30);
        if graph
            .max_work
            .checked_add(history.max_work)
            .and_then(|work| work.checked_add(map.max_work))
            .and_then(|work| work.checked_add(fence.work()))
            .is_none_or(|work| work > NATIVE_HISTORY_TOTAL_WORK)
            || graph
                .max_staging_bytes
                .checked_add(history.max_staging_bytes)
                .and_then(|bytes| bytes.checked_add(map.max_memory_bytes))
                .and_then(|bytes| bytes.checked_add(fence.peak_memory_bytes()))
                .is_none_or(|bytes| bytes > NATIVE_HISTORY_TOTAL_MEMORY_BYTES)
            || graph
                .max_time
                .checked_add(history.max_time)
                .and_then(|time| time.checked_add(map.max_time))
                .is_none_or(|time| time > NATIVE_HISTORY_TOTAL_TIME)
        {
            return Err(NativeMapError::Unavailable);
        }
    }
    Ok((graph, history, map))
}

fn graph_map_limits_with_caps(
    graph_requested: bool,
    total_work: usize,
    total_memory_bytes: usize,
    total_time: Duration,
) -> Result<(GraphOptions, MapLimits), NativeMapError> {
    let mut graph = GraphOptions::default();
    let mut map = bounded_map_limits();
    if graph_requested {
        graph.max_work = total_work
            .checked_sub(map.max_work)
            .filter(|work| *work != 0)
            .ok_or(NativeMapError::Unavailable)?;
        graph.max_staging_bytes = total_memory_bytes
            .checked_sub(map.max_memory_bytes)
            .filter(|bytes| *bytes != 0)
            .ok_or(NativeMapError::Unavailable)?;
        graph.max_cache_bytes = graph.max_cache_bytes.min(graph.max_staging_bytes / 2);
        graph.max_time = Duration::from_secs(5);
        map.max_time = total_time
            .checked_sub(graph.max_time)
            .filter(|time| !time.is_zero())
            .ok_or(NativeMapError::Unavailable)?;
    } else {
        if total_work == 0 || total_memory_bytes == 0 || total_time.is_zero() {
            return Err(NativeMapError::Unavailable);
        }
        map.max_work = total_work;
        map.max_memory_bytes = total_memory_bytes;
        map.max_time = total_time;
    }
    Ok((graph, map))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeMapError {
    Bound(MapBound),
    CursorInvalid,
    CursorMismatch,
    EvidenceInvalid,
    EvidenceStale,
    GraphEvidenceInvalid,
    GraphEvidenceStale,
    GraphEvidenceUnavailable,
    HistoryEvidenceStale,
    HistoryEvidenceUnavailable,
    InvalidRequest,
    RevisionStale,
    RevisionUnavailable,
    SemanticEvidenceUnavailable,
    SelectorNoMatch,
    Serialization,
    TimeLimit,
    Unavailable,
}

impl NativeMapError {
    fn code(self) -> String {
        match self {
            Self::Bound(MapBound::Items) => "map_items_bound_exceeded",
            Self::Bound(MapBound::EstimatedTokens) => "map_estimated_tokens_bound_exceeded",
            Self::Bound(MapBound::Hops) => "map_hops_bound_exceeded",
            Self::Bound(MapBound::Degree) => "map_degree_bound_exceeded",
            Self::Bound(MapBound::ResultBytes) => "map_result_bytes_bound_exceeded",
            Self::Bound(MapBound::Memory) => "map_memory_bound_exceeded",
            Self::CursorInvalid => "map_cursor_invalid",
            Self::CursorMismatch => "map_cursor_mismatch",
            Self::EvidenceInvalid => "map_semantic_evidence_invalid",
            Self::EvidenceStale => "map_semantic_evidence_stale",
            Self::GraphEvidenceInvalid => "map_graph_evidence_invalid",
            Self::GraphEvidenceStale => "map_graph_evidence_stale",
            Self::GraphEvidenceUnavailable => "map_graph_evidence_unavailable",
            Self::HistoryEvidenceStale => "map_history_evidence_stale",
            Self::HistoryEvidenceUnavailable => "map_history_evidence_unavailable",
            Self::InvalidRequest => "map_invalid_request",
            Self::RevisionStale => "stale_revision",
            Self::RevisionUnavailable => "map_revision_unavailable",
            Self::SemanticEvidenceUnavailable => "map_semantic_evidence_unavailable",
            Self::SelectorNoMatch => "map_selector_no_match",
            Self::Serialization => "map_serialization_failed",
            Self::TimeLimit => "map_time_limit_exceeded",
            Self::Unavailable => "map_unavailable",
        }
        .to_owned()
    }
}

impl From<MapError> for NativeMapError {
    fn from(error: MapError) -> Self {
        match error {
            MapError::BoundExceeded(bound) => Self::Bound(bound),
            MapError::CursorMismatch => Self::CursorMismatch,
            MapError::Revision(crate::workspace::revision::RevisionError::StaleRevision {
                ..
            }) => Self::RevisionStale,
            MapError::Revision(_) => Self::RevisionUnavailable,
            MapError::TimeLimit => Self::TimeLimit,
            MapError::InvalidRequest(_) => Self::InvalidRequest,
            MapError::SelectorNoMatch(_) => Self::SelectorNoMatch,
            MapError::InvalidLimits(_) | MapError::InvalidIndex(_) => Self::Unavailable,
            MapError::InvalidFact(_) => Self::EvidenceInvalid,
            MapError::StaleFact => Self::EvidenceStale,
            MapError::SemanticEvidenceUnavailable => Self::SemanticEvidenceUnavailable,
            MapError::GraphEvidenceUnavailable => Self::GraphEvidenceUnavailable,
            MapError::GraphEvidenceStale => Self::GraphEvidenceStale,
            MapError::HistoryEvidenceUnavailable => Self::HistoryEvidenceUnavailable,
            MapError::InvalidGraph(_) => Self::GraphEvidenceInvalid,
            MapError::Serialization(_) => Self::Serialization,
        }
    }
}

impl From<GraphError> for NativeMapError {
    fn from(error: GraphError) -> Self {
        match error {
            GraphError::StaleEvidence
            | GraphError::Revision(crate::workspace::revision::RevisionError::StaleRevision {
                ..
            }) => Self::GraphEvidenceStale,
            GraphError::MalformedManifest { .. }
            | GraphError::InvalidIndex(_)
            | GraphError::InvalidEvidence(_)
            | GraphError::ContainmentCycle
            | GraphError::HistoryMismatch
            | GraphError::UnsafePath(_)
            | GraphError::MissingWorkspaceMember { .. }
            | GraphError::MissingPathDependency { .. } => Self::GraphEvidenceInvalid,
            GraphError::Revision(_)
            | GraphError::InvalidOptions(_)
            | GraphError::BoundExceeded(_) => Self::GraphEvidenceUnavailable,
        }
    }
}

impl From<HistoryError> for NativeMapError {
    fn from(error: HistoryError) -> Self {
        match error {
            HistoryError::StaleRepositoryFence => Self::HistoryEvidenceStale,
            HistoryError::Revision(crate::workspace::revision::RevisionError::StaleRevision {
                ..
            }) => Self::HistoryEvidenceStale,
            HistoryError::InvalidRequest(_) | HistoryError::SelectorNoMatch(_) => {
                Self::InvalidRequest
            }
            HistoryError::InvalidIndex(_)
            | HistoryError::Malformed(_)
            | HistoryError::MissingObject(_)
            | HistoryError::RepositoryRootMismatch { .. }
            | HistoryError::UnsafeGitPath(_) => Self::GraphEvidenceInvalid,
            HistoryError::Unavailable(_)
            | HistoryError::Git { .. }
            | HistoryError::Revision(_)
            | HistoryError::InvalidOptions(_)
            | HistoryError::BoundExceeded(_) => Self::HistoryEvidenceUnavailable,
        }
    }
}

fn native_semantic_evidence(
    stored: &[NativeSemanticRelationship],
    revision: RevisionId,
) -> Result<Vec<SemanticRelationship<'_>>, NativeMapError> {
    validate_native_semantic_store(stored).map_err(|_| NativeMapError::EvidenceInvalid)?;
    if stored
        .iter()
        .any(|relationship| relationship.fact.provenance().revision() != revision)
    {
        return Err(NativeMapError::EvidenceStale);
    }
    Ok(stored
        .iter()
        .map(|relationship| {
            SemanticRelationship::new(relationship.source_declaration, &relationship.fact)
        })
        .collect())
}

fn validate_native_semantic_store(stored: &[NativeSemanticRelationship]) -> Result<(), String> {
    if stored.len() > NATIVE_MAP_MAX_SEMANTIC_RELATIONSHIPS {
        return Err("native semantic evidence count exceeds trusted bound".to_owned());
    }
    let bytes = stored.iter().try_fold(0_usize, |bytes, relationship| {
        let fact = &relationship.fact;
        bytes
            .checked_add(std::mem::size_of::<NativeSemanticRelationship>())
            .and_then(|bytes| bytes.checked_add(fact.path().as_path().as_str().len()))
            .and_then(|bytes| {
                bytes.checked_add(fact.provenance().origin().path().as_path().as_str().len())
            })
            .and_then(|bytes| bytes.checked_add(fact.provenance().origin().uri().len()))
            .and_then(|bytes| {
                bytes.checked_add(fact.provenance().server().server_artifact.as_str().len())
            })
            .and_then(|bytes| {
                bytes.checked_add(fact.provenance().server().configuration.as_str().len())
            })
            .and_then(|bytes| bytes.checked_add(6 * std::mem::size_of::<u64>()))
            .ok_or_else(|| "native semantic evidence byte count overflowed".to_owned())
    })?;
    if bytes > NATIVE_MAP_MAX_SEMANTIC_EVIDENCE_BYTES {
        Err("native semantic evidence bytes exceed trusted bound".to_owned())
    } else {
        Ok(())
    }
}

fn trusted_run_limits(limits: ResourceLimits) -> bool {
    limits.finite()
        && limits.cpu_millis <= MAX_RUN_CPU_MILLIS
        && limits.memory_bytes <= MAX_RUN_MEMORY_BYTES
        && limits.pids <= MAX_RUN_PIDS
        && limits.file_bytes <= MAX_RUN_FILE_BYTES
        && limits.disk_bytes <= MAX_RUN_DISK_BYTES
        && limits.io_bytes <= MAX_RUN_IO_BYTES
        && limits.output_bytes <= MAX_RUN_OUTPUT_BYTES
        && limits.wall_time_millis <= MAX_RUN_WALL_TIME_MILLIS
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|_| "invalid_arguments".to_owned())
}

fn revision(value: &str) -> Result<RevisionId, String> {
    RevisionId::parse(value).ok_or_else(|| "invalid_revision".to_owned())
}

fn code<E: std::fmt::Display>(prefix: &'static str) -> impl FnOnce(E) -> String {
    move |_| prefix.to_owned()
}

#[cfg(test)]
fn output(
    data: Value,
    artifacts: Vec<String>,
    store: &ArtifactStore,
    custody: &crate::domain::secret::SecretCustody,
) -> DispatchOutcome {
    projected_output(
        custody.project_json(CaptureBoundary::WorkspaceMetadata, &data),
        artifacts,
        store,
    )
}

fn streaming_output(
    data: Value,
    artifacts: Vec<String>,
    store: &ArtifactStore,
    custody: &crate::domain::secret::SecretCustody,
    state: &mut crate::domain::secret::JsonProjectionState,
) -> DispatchOutcome {
    projected_output(
        custody.project_json_stream(CaptureBoundary::WorkspaceMetadata, &data, state),
        artifacts,
        store,
    )
}

fn projected_output(data: Value, artifacts: Vec<String>, store: &ArtifactStore) -> DispatchOutcome {
    let artifact_digests = match artifacts
        .iter()
        .map(|artifact| {
            crate::domain::events::ArtifactRef::parse(artifact).or_else(|_| {
                let reference = crate::store::artifacts::ArtifactReference::parse(artifact)
                    .map_err(|_| crate::domain::events::DigestError)?;
                let digest = store
                    .open_reference(reference)
                    .map_err(|_| crate::domain::events::DigestError)?
                    .digest()
                    .to_string();
                crate::domain::events::ArtifactRef::parse(&digest)
            })
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(artifacts) => artifacts,
        Err(_) => return failed("invalid_artifact_digest"),
    };
    let body = serde_json::to_vec(&json!({
        "artifacts": artifacts,
        "data": data,
        "truncated": false,
        "version": 1,
    }))
    .expect("native output serializes");
    if body.len() > MAX_NATIVE_OUTPUT_BYTES {
        failed("native_output_too_large")
    } else {
        DispatchOutcome::Succeeded(CanonicalOutput {
            media_type: "application/json".to_owned(),
            body,
            artifact_digests,
        })
    }
}

fn committed_output(
    data: Value,
    artifacts: Vec<String>,
    store: &ArtifactStore,
    custody: &crate::domain::secret::SecretCustody,
    state: &mut crate::domain::secret::JsonProjectionState,
) -> DispatchOutcome {
    match streaming_output(data, artifacts, store, custody, state) {
        DispatchOutcome::Succeeded(output) => DispatchOutcome::DurablyCommitted(output),
        outcome => outcome,
    }
}

fn failed(code: &str) -> DispatchOutcome {
    DispatchOutcome::Failed {
        code: code.to_owned(),
    }
}

fn digest(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

fn structural_preview_token_digest(token: &str) -> [u8; 32] {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    hash.update(b"kit-structural-preview-token-v1\0");
    hash.update(token.as_bytes());
    hash.finalize().into()
}

fn settle_result_bytes(value: &mut Value) -> Result<(), String> {
    value["result_bytes"] = json!(0);
    let mut size = 0;
    loop {
        let next = serde_json::to_vec(value)
            .map_err(code("serialization_failed"))?
            .len();
        if next == size {
            return Ok(());
        }
        size = next;
        value["result_bytes"] = json!(size);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn host_platform() -> Result<Platform, String> {
    if cfg!(target_os = "macos") {
        Ok(Platform::MacOs)
    } else if cfg!(target_os = "linux") {
        Ok(Platform::Linux)
    } else if cfg!(target_os = "windows") {
        Ok(Platform::Windows)
    } else {
        Err("executor_host_unsupported".to_owned())
    }
}

fn host_architecture() -> Result<Architecture, String> {
    if cfg!(target_arch = "x86_64") {
        Ok(Architecture::X86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(Architecture::Aarch64)
    } else {
        Err("executor_architecture_unsupported".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        io::{Read as _, Write as _},
        net::TcpListener,
        process::Command,
        sync::atomic::AtomicU64,
        thread,
        time::Duration,
    };

    use agentkit_core::{Item, ItemKind};
    use agentkit_loop::{Agent, LoopInterrupt, LoopStep, ModelAdapter, SessionConfig};
    use url::Url;

    use crate::{
        agent::adapters::tool::{ToolBinding, ToolExecutorAdapter, ToolKernelContext},
        api::auth::contract::GrantSnapshot,
        capabilities::kernel::{
            grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot},
            identity::DigestAlgorithm,
        },
        domain::{
            config::{LayerStack, RunConfigContext},
            events::ContentDigest,
            ids::{AttemptId, PrincipalId, ProcessId, ProjectId, RunId, WorkspaceId},
            lifecycle::{AttemptOwnership, FencingToken, ProcessClaim, ProcessOwnership},
        },
        executor::{
            check::{CheckCommand, ConformanceCheck},
            profile::{ExecutorProfile, ProfileSpec, ResourceLimits, TrustTier},
        },
        runtime::scheduler::{budget::RunBudget, reserve::BudgetLedger},
        test_support,
        verify::{
            lsp::{
                facts::{
                    FactLimits, LspWorkspaceSnapshot, OpenDocument, SnapshotFile,
                    normalize_semantic_locations,
                },
                session::{
                    CodecLimits, DocumentVersion, ExecutionProfileIdentity, LaunchRequest,
                    LspCodec, LspSessionManager, OwnedLspLauncher, OwnedLspTransport,
                    PositionEncoding, ResponseDisposition, RevisionPolicy, SendContext,
                    ServerIdentity, SessionLimits, SessionPurpose, SessionScope, TransportError,
                },
            },
            profiles::{CheckClass, CheckRequirement, DeclaredCheck, VerificationRegistry},
        },
        workspace::{edit::ir::EditLimits, index::meta::IndexOptions},
    };

    use super::*;

    fn dispatcher(runner: Option<CheckRunner>) -> (PathBuf, NativeDispatcher) {
        dispatcher_with_semantic(runner, |_, _| (Vec::new(), None))
    }

    #[test]
    fn native_pages_cannot_reconstruct_any_secret_representation_split() {
        let (directory, dispatcher) = dispatcher(None);
        let custody = crate::domain::secret::SecretCustody::new([Arc::new(
            crate::domain::secret::SecretLease::new("cross-frame"),
        )]);
        for representation in [
            "cross-frame",
            "%63%72%6F%73%73%2D%66%72%61%6D%65",
            "63726f73732d6672616d65",
            "Y3Jvc3MtZnJhbWU=",
        ] {
            for split in 1..representation.len() {
                let mut state = crate::domain::secret::JsonProjectionState::default();
                let pages = [&representation[..split], &representation[split..]].map(|fragment| {
                    let DispatchOutcome::Succeeded(output) = streaming_output(
                        json!({"fragment": fragment}),
                        Vec::new(),
                        &dispatcher.artifacts,
                        &custody,
                        &mut state,
                    ) else {
                        panic!("native page projection failed");
                    };
                    output.body
                });
                assert!(pages.iter().any(|page| {
                    String::from_utf8_lossy(page).contains(crate::domain::secret::REDACTED)
                }));
                let mut reconstructed = custody.redactor().scanner();
                reconstructed.push(&pages[0]);
                reconstructed.push(&pages[1]);
                assert!(
                    !reconstructed.found(),
                    "native pages leaked {representation} at split {split}"
                );
            }
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_reads_share_projection_state_across_content_and_later_paths() {
        let (directory, mut dispatcher) = dispatcher(None);
        std::fs::write(dispatcher.root.join("first"), "cross-").unwrap();
        std::fs::write(dispatcher.root.join("read"), "").unwrap();
        dispatcher.custody.register(
            "read-test",
            "split",
            Arc::new(crate::domain::secret::SecretLease::new("cross-read")),
        );
        let revision = dispatcher.revision().unwrap();
        let read = |path: &str| {
            serde_json::to_vec(&json!({
                "expected_revision": revision,
                "path": path,
                "range": {"kind": "full"},
            }))
            .unwrap()
        };

        let (first, _) = dispatcher.read(&read("first")).unwrap();
        let (second, _) = dispatcher.read(&read("read")).unwrap();

        assert_eq!(first["path"], "first");
        assert_eq!(second["path"], crate::domain::secret::REDACTED);
        let mut scanner = dispatcher.custody.redactor().scanner();
        scanner.push(&serde_json::to_vec(&first).unwrap());
        scanner.push(&serde_json::to_vec(&second).unwrap());
        assert!(!scanner.found());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_read_cursor_replays_exactly_and_rejects_a_different_range() {
        let (directory, mut dispatcher) = dispatcher(None);
        std::fs::write(dispatcher.root.join("fragment"), "cross-").unwrap();
        dispatcher.custody.register(
            "read-cursor-test",
            "split",
            Arc::new(crate::domain::secret::SecretLease::new("cross-cross-")),
        );
        let revision = dispatcher.revision().unwrap();
        let request = json!({
            "expected_revision": revision,
            "path": "fragment",
            "range": {"kind": "bytes", "start": 0, "end": 6},
        });
        let (first, _) = dispatcher
            .read(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        let mut replay = request.clone();
        replay["cursor"] = first["cursor"].clone();
        let (replayed, _) = dispatcher
            .read(&serde_json::to_vec(&replay).unwrap())
            .unwrap();
        assert_eq!(replayed, first);
        assert_eq!(
            serde_json::to_vec(&replayed).unwrap(),
            serde_json::to_vec(&first).unwrap()
        );
        assert_eq!(replayed["content"], json!([99, 114, 111, 115, 115, 45]));

        std::fs::write(dispatcher.root.join("fragment"), "changed").unwrap();
        assert_eq!(
            dispatcher.read(&serde_json::to_vec(&replay).unwrap()),
            Err("stale_revision".to_owned())
        );
        assert!(dispatcher.read_replay.is_some());

        for changed in [
            ("range", json!({"kind": "bytes", "start": 0, "end": 5})),
            ("range", json!({"kind": "lines", "start": 1, "end": 1})),
            ("path", json!("lib.rs")),
            ("expected_revision", json!("other-read-revision")),
        ] {
            replay[changed.0] = changed.1;
            assert_eq!(
                dispatcher.read(&serde_json::to_vec(&replay).unwrap()),
                Err("read_cursor_invalid".to_owned())
            );
            replay = request.clone();
            replay["cursor"] = first["cursor"].clone();
        }

        let cursor: ReadProjectionCursor = serde_json::from_value(first["cursor"].clone()).unwrap();
        let input: ReadInput = serde_json::from_value(request).unwrap();
        for binding in [
            read_cursor_binding(
                &input,
                "other-principal",
                &dispatcher.config.project_id().to_string(),
            )
            .unwrap(),
            read_cursor_binding(
                &input,
                &dispatcher.authenticated.principal_id().to_string(),
                "other-project",
            )
            .unwrap(),
        ] {
            assert!(
                open_read_cursor(
                    &cursor,
                    &binding,
                    &dispatcher.cursor_key,
                    &dispatcher.custody
                )
                .is_err()
            );
        }

        std::fs::write(dispatcher.root.join("later"), "public").unwrap();
        let later = json!({
            "expected_revision": dispatcher.revision().unwrap(),
            "path": "later",
            "range": {"kind": "full"},
        });
        dispatcher
            .read(&serde_json::to_vec(&later).unwrap())
            .unwrap();
        let mut stale = replay;
        stale["cursor"] = serde_json::to_value(cursor).unwrap();
        assert_eq!(
            dispatcher.read(&serde_json::to_vec(&stale).unwrap()),
            Err("stale_revision".to_owned())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn dispatcher_with_semantic(
        runner: Option<CheckRunner>,
        evidence: impl FnOnce(
            &Path,
            &Path,
        ) -> (Vec<NativeSemanticRelationship>, Option<ManagedWorkspace>),
    ) -> (PathBuf, NativeDispatcher) {
        let directory = std::env::temp_dir().join(format!(
            "kit-native-check-{}",
            crate::domain::ids::RunId::generate().unwrap()
        ));
        let root = directory.join("source");
        let scratch = directory.join("scratch");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(root.join("lib.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("format.json"), "{}\n").unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let scratch = std::fs::canonicalize(scratch).unwrap();
        let (semantic_relationships, workspace_guard) = evidence(&root, &scratch);
        let semantic_evidence = NativeSemanticEvidenceStore::default();
        let artifacts = Arc::new(ArtifactStore::open(directory.join("artifacts")).unwrap());
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let grants = BTreeSet::from([
            Grant::ProcessSpawn,
            Grant::VerificationTargeted,
            Grant::WorkspaceRead,
            Grant::WorkspaceWrite,
        ]);
        let config = LayerStack::safe_defaults()
            .materialize(
                RunConfigContext {
                    principal_id: principal,
                    project_id: project,
                    run_id: RunId::generate().unwrap(),
                },
                &grants,
            )
            .unwrap();
        let authenticated =
            AuthenticatedPrincipal::from_grants(GrantSnapshot::new(principal, project, grants));
        let command = CheckCommand::new(
            "diagnostics",
            "cargo",
            vec!["check".to_owned()],
            "example.invalid/check@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            ResourceLimits::new(1_000, 1024 * 1024, 8, 1024, 1024, 1024, 1024, 1_000),
        )
        .unwrap();
        let typecheck = CheckCommand::new(
            "typecheck",
            "cargo",
            vec!["check".to_owned()],
            "example.invalid/check@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            ResourceLimits::new(1_000, 1024 * 1024, 8, 1024, 1024, 1024, 1024, 1_000),
        )
        .unwrap();
        let registry = VerificationRegistry::new(vec![
            DeclaredCheck::new(
                CheckClass::Diagnostics,
                command,
                CheckRequirement::Required,
                BTreeSet::new(),
                false,
            )
            .unwrap(),
            DeclaredCheck::new(
                CheckClass::Typecheck,
                typecheck,
                CheckRequirement::Required,
                BTreeSet::new(),
                false,
            )
            .unwrap(),
        ])
        .unwrap();
        let mut dispatcher = NativeDispatcher::open(
            root,
            &scratch,
            artifacts,
            authenticated,
            config,
            None,
            NativeRuntime {
                extension_guard: crate::capabilities::extensions::attest_native_extension(
                    &Arc::new(std::sync::RwLock::new(Default::default())),
                    crate::capabilities::extensions::ExtensionScope::new(principal, project),
                )
                .unwrap(),
                workspace_id: WorkspaceId::generate().unwrap(),
                process_registration: None,
                cancellation: SqliteCancellationCoordinator::new(directory.join("state.sqlite3")),
                live_cancellation: Arc::new(AtomicBool::new(false)),
                container_image: None,
                verification_registry: registry,
                check_runner: runner,
                custody: crate::domain::secret::SecretCustody::default(),
                secrets: Vec::new(),
                syntax_executors: vec![
                    crate::executor::syntax::SyntaxExecutor::debug(
                        "text",
                        crate::workspace::edit::format::NATIVE_TEXT_VERSION,
                        crate::executor::syntax::DebugSyntaxAction::Pass(None),
                    ),
                    crate::executor::syntax::SyntaxExecutor::debug(
                        "json",
                        crate::workspace::edit::format::NATIVE_JSON_VERSION,
                        crate::executor::syntax::DebugSyntaxAction::Pass(None),
                    ),
                    crate::executor::syntax::SyntaxExecutor::debug(
                        "rust",
                        crate::workspace::edit::format::RUST_GRAMMAR_VERSION,
                        crate::executor::syntax::DebugSyntaxAction::Pass(None),
                    ),
                ],
                formatter_required: false,
                formatter: None,
                feedback: Some(NativeFeedbackRuntime {
                    database: directory.join("feedback.sqlite3"),
                    adapters: BTreeMap::from([
                        (
                            "diagnostics".to_owned(),
                            crate::verify::feedback::DiagnosticAdapter::NormalizedJsonLinesV1,
                        ),
                        (
                            "typecheck".to_owned(),
                            crate::verify::feedback::DiagnosticAdapter::NormalizedJsonLinesV1,
                        ),
                    ]),
                    limits: crate::verify::feedback::FeedbackLimits::default(),
                }),
                semantic_evidence: semantic_evidence.clone(),
                edit_validation_time: crate::workspace::edit::ir::EditLimits::default()
                    .max_validation_time,
                cursor_key: [7; 32],
                run_runner: None,
            },
        )
        .unwrap();
        let revision = dispatcher.revision().unwrap();
        if !semantic_relationships.is_empty() {
            let (_, index) = dispatcher.workspace_index(&revision).unwrap();
            semantic_evidence
                .replace(&index, semantic_relationships)
                .unwrap();
        }
        drop(workspace_guard);
        (directory, dispatcher)
    }

    fn attempt(dispatcher: &NativeDispatcher) -> crate::domain::lifecycle::AttemptOwnership {
        crate::domain::lifecycle::AttemptOwnership::new(
            crate::domain::ids::AttemptId::generate().unwrap(),
            dispatcher.authenticated.principal_id(),
            crate::domain::lifecycle::FencingToken::new(1),
        )
    }

    fn discover_input(revision: &str, map: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "expected_revision": revision,
            "map": map,
        }))
        .unwrap()
    }

    struct LspLauncher;

    struct LspTransport {
        claim: ProcessClaim,
    }

    impl OwnedLspLauncher for LspLauncher {
        type Transport = LspTransport;

        fn launch(
            &mut self,
            request: LaunchRequest<'_>,
        ) -> Result<Self::Transport, TransportError> {
            Ok(LspTransport {
                claim: ProcessClaim::new(
                    ProcessId::generate().unwrap(),
                    ProcessOwnership::DaemonService(request.service.id),
                ),
            })
        }
    }

    impl OwnedLspTransport for LspTransport {
        fn claim(&self) -> ProcessClaim {
            self.claim
        }

        fn initialize(
            &mut self,
            _: &[u8],
            _: CodecLimits,
            _: SendContext,
        ) -> Result<(), TransportError> {
            Ok(())
        }

        fn send_frame(&mut self, _: &[u8], _: SendContext) -> Result<(), TransportError> {
            Ok(())
        }

        fn receive_frame(
            &mut self,
            _: CodecLimits,
            _: SendContext,
        ) -> Result<Vec<u8>, TransportError> {
            Err(TransportError::ReadFailed)
        }

        fn close_and_reap(&mut self, _: SendContext) -> Result<(), TransportError> {
            Ok(())
        }
    }

    fn test_digest(byte: u8) -> ContentDigest {
        ContentDigest::parse(&format!("blake3:{}", format!("{byte:02x}").repeat(32))).unwrap()
    }

    fn lsp_server() -> ServerIdentity {
        ServerIdentity {
            server_artifact: test_digest(1),
            configuration: test_digest(2),
        }
    }

    fn lsp_profile() -> ExecutionProfileIdentity {
        let profile = ExecutorProfile::new(ProfileSpec::isolated(
            TrustTier::TrustedLocal,
            host_platform().unwrap(),
            host_architecture().unwrap(),
            ResourceLimits::new(
                60_000,
                1024 * 1024 * 1024,
                64,
                64 * 1024 * 1024,
                1024 * 1024 * 1024,
                1024 * 1024 * 1024,
                64 * 1024 * 1024,
                60_000,
            ),
        ))
        .unwrap();
        ExecutionProfileIdentity::from_profile(&profile)
    }

    fn normalized_native_semantic_relationship(
        root: &Path,
        _scratch: &Path,
    ) -> (Vec<NativeSemanticRelationship>, Option<ManagedWorkspace>) {
        let source = "fn source() { target(); }\n";
        let target = "fn target() {}\n";
        std::fs::write(root.join("source.rs"), source).unwrap();
        std::fs::write(root.join("target.rs"), target).unwrap();
        let workspace = ManagedWorkspace::open(root).unwrap();
        let revision = workspace.current_revision().unwrap().id();
        let index = MetadataIndex::build(&workspace, revision, &IndexOptions::default()).unwrap();
        let source_declaration = index
            .entries()
            .iter()
            .flat_map(|entry| entry.syntax_records.iter())
            .find(|record| record.qualified_name().value().as_str() == "source")
            .map(|record| DeclarationId::from(record.declaration_id()))
            .unwrap();
        let source_uri = Url::from_file_path(root.join("source.rs"))
            .unwrap()
            .to_string();
        let target_uri = Url::from_file_path(root.join("target.rs"))
            .unwrap()
            .to_string();
        let server = lsp_server();
        let mut manager = LspSessionManager::new(LspLauncher, SessionLimits::default()).unwrap();
        let service = manager
            .open(
                SessionScope {
                    principal_id: PrincipalId::generate().unwrap(),
                    project_id: ProjectId::generate().unwrap(),
                    workspace_id: WorkspaceId::generate().unwrap(),
                    canonical_root_identity: test_digest(3),
                    purpose: SessionPurpose::Live,
                    revision_policy: RevisionPolicy::ManagedLive,
                    server: server.clone(),
                    position_encoding: PositionEncoding::Utf8,
                    execution_profile: lsp_profile(),
                },
                revision,
            )
            .unwrap();
        manager
            .open_document(
                service,
                source_uri.clone(),
                DocumentVersion::new(1),
                source.to_owned(),
            )
            .unwrap();
        let token = manager
            .request(
                service,
                revision,
                &source_uri,
                "textDocument/definition",
                json!({
                    "textDocument": {"uri": source_uri},
                    "position": {"line": 0, "character": 0}
                }),
                manager.now_tick() + 10_000,
            )
            .unwrap();
        let frame = LspCodec::encode(
            &json!({
                "jsonrpc": "2.0",
                "id": token.request_id.get(),
                "result": {
                    "uri": target_uri,
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 9}
                    }
                }
            }),
            SessionLimits::default().codec,
        )
        .unwrap();
        let ResponseDisposition::Accepted(accepted) = manager
            .receive_captured_response(service, &token, &frame)
            .unwrap()
        else {
            panic!("normalized LSP response was not accepted")
        };
        manager.shutdown().unwrap();
        let snapshot = LspWorkspaceSnapshot::new(
            root.to_owned(),
            revision,
            1,
            vec![
                SnapshotFile::new("source.rs", source.as_bytes().to_vec(), false),
                SnapshotFile::new("target.rs", target.as_bytes().to_vec(), false),
            ],
            vec![OpenDocument::new(
                source_uri,
                DocumentVersion::new(1),
                source.to_owned(),
            )],
            server,
            PositionEncoding::Utf8,
            EditLimits::default(),
            FactLimits::default(),
        )
        .unwrap();
        let fact = normalize_semantic_locations(&snapshot, &accepted)
            .unwrap()
            .remove(0);
        (
            vec![NativeSemanticRelationship {
                source_declaration,
                fact,
            }],
            Some(workspace),
        )
    }

    #[test]
    fn native_graph_and_map_limits_share_one_fixed_envelope() {
        let (graph, map) = graph_map_limits_with_caps(
            true,
            NATIVE_MAP_TOTAL_WORK,
            NATIVE_MAP_TOTAL_MEMORY_BYTES,
            NATIVE_MAP_TOTAL_TIME,
        )
        .unwrap();
        assert_eq!(graph.max_work + map.max_work, NATIVE_MAP_TOTAL_WORK);
        assert_eq!(
            graph.max_staging_bytes + map.max_memory_bytes,
            NATIVE_MAP_TOTAL_MEMORY_BYTES
        );
        assert_eq!(graph.max_time + map.max_time, NATIVE_MAP_TOTAL_TIME);
        assert!(
            graph_map_limits_with_caps(
                true,
                bounded_map_limits().max_work,
                bounded_map_limits().max_memory_bytes,
                Duration::from_secs(5),
            )
            .is_err()
        );
        let (graph, history, map) = bounded_unified_map_limits(true, true).unwrap();
        let fence = ValidatedHistoryFence::conservative_metrics(4);
        assert!(
            graph.max_work + history.max_work + map.max_work + fence.work()
                < NATIVE_HISTORY_TOTAL_WORK
        );
        assert_eq!(
            graph.max_staging_bytes
                + history.max_staging_bytes
                + map.max_memory_bytes
                + fence.peak_memory_bytes(),
            NATIVE_HISTORY_TOTAL_MEMORY_BYTES
        );
        assert_eq!(
            graph.max_time + history.max_time + map.max_time,
            NATIVE_HISTORY_TOTAL_TIME
        );
    }

    #[test]
    fn native_history_uses_real_git_and_reuses_provider_without_hooks() {
        let (directory, mut dispatcher) = dispatcher(None);
        let root = directory.join("source");
        let git = |arguments: &[&str]| {
            let output = Command::new("/usr/bin/git")
                .current_dir(&root)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("LC_ALL", "C")
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "Git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["add", "lib.rs", "format.json"]);
        git(&[
            "-c",
            "user.name=Kit",
            "-c",
            "user.email=kit@example.invalid",
            "commit",
            "-q",
            "-m",
            "first",
        ]);
        std::fs::write(root.join("lib.rs"), "fn main() { let _ = 1; }\n").unwrap();
        std::fs::write(root.join("format.json"), "{\"changed\":true}\n").unwrap();
        git(&["add", "lib.rs", "format.json"]);
        git(&[
            "-c",
            "user.name=Kit",
            "-c",
            "user.email=kit@example.invalid",
            "commit",
            "-q",
            "-m",
            "second",
        ]);
        let sentinel = directory.join("hook-ran");
        let hook = root.join("hostile-hook");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&["config", "core.hooksPath", hook.to_str().unwrap()]);

        let revision = dispatcher.revision().unwrap();
        let input = discover_input(
            &revision,
            json!({
                "historyPaths": ["format.json", "lib.rs"],
                "blamePaths": ["lib.rs"],
                "relationships": ["changed_with"],
                "expandPaths": ["lib.rs"],
                "purpose": "neighborhood"
            }),
        );
        let first = dispatcher.discover(&input).unwrap().0;
        assert_eq!(first["map"]["history"]["object_format"], "sha1");
        assert_eq!(first["map"]["history"]["commits_completeness"], "complete");
        assert!(
            first["map"]["blame"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
        assert_eq!(first["map"]["blame"][0]["path"], "lib.rs");
        assert!(first["map"]["blame"][0]["line_digest"].as_str().is_some());
        assert!(first["map"]["blame"][0].get("text").is_none());
        assert_eq!(
            first["map"]["graph_edges"][0]["relationship"],
            "changed_with"
        );
        assert!(!sentinel.exists());
        let metrics = dispatcher.history_graph.metrics().clone();
        let cache = dispatcher.history_graph.cache_usage();
        let first_unified = Arc::as_ptr(&dispatcher.unified_graph.as_ref().unwrap().1);
        let second = dispatcher.discover(&input).unwrap().0;
        let second_unified = Arc::as_ptr(&dispatcher.unified_graph.as_ref().unwrap().1);
        assert_eq!(first, second);
        assert_eq!(first_unified, second_unified);
        assert!(dispatcher.unified_peak_bytes <= NATIVE_HISTORY_TOTAL_MEMORY_BYTES);
        assert_eq!(dispatcher.history_graph.metrics(), &metrics);
        assert_eq!(dispatcher.history_graph.cache_usage(), cache);
        assert!(!sentinel.exists());
    }

    #[test]
    fn native_discover_map_ranks_syntax_and_returns_real_containment_without_semantic_claims() {
        let (directory, mut dispatcher) = dispatcher(None);
        std::fs::write(
            directory.join("source/lib.rs"),
            "fn parent() { fn child() {} }\nfn unrelated() {}\n",
        )
        .unwrap();
        let revision = dispatcher.revision().unwrap();
        let (_, index) = dispatcher.workspace_index(&revision).unwrap();
        let parent = index
            .entries()
            .iter()
            .flat_map(|entry| entry.syntax_records.iter())
            .find(|record| record.qualified_name().value().as_str() == "parent")
            .unwrap()
            .declaration_id();
        let input = discover_input(
            &revision,
            json!({
                "taskTerms": ["child"],
                "relationships": ["contains"],
                "expansionSeeds": [hex(&parent)],
                "budgets": {"items": 20}
            }),
        );
        let first = dispatcher.discover(&input).unwrap().0;
        let second = dispatcher.discover(&input).unwrap().0;
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );
        assert_eq!(first["mode"], "map");
        assert_eq!(first["semanticEvidenceAvailable"], false);
        assert!(
            first["map"]["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| {
                    entry["qualified_name"] == "parent::child"
                        && entry["reasons"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|reason| reason == "exact_task_term")
                })
        );
        assert!(
            first["map"]["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| {
                    edge["relationship"] == "contains"
                        && edge["provenance"]["source"] == "tree_sitter"
                })
        );
        let selectors = dispatcher
            .discover(&discover_input(
                &revision,
                json!({
                    "expandPaths": ["lib.rs"],
                    "expandSymbols": ["parent"],
                    "scoreBand": {"min": 0, "max": u64::MAX},
                    "relationships": ["contains"]
                }),
            ))
            .unwrap()
            .0;
        assert!(
            selectors["map"]["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["qualified_name"] == "parent::child")
        );
        let declaration_free_path = dispatcher
            .discover(&discover_input(
                &revision,
                json!({"expandPaths": ["format.json"], "relationships": []}),
            ))
            .unwrap()
            .0;
        assert_eq!(
            declaration_free_path["map"]["path_nodes"][0]["path"],
            "format.json"
        );
        assert_eq!(
            dispatcher.discover(&discover_input(
                &revision,
                json!({"expandSymbols": ["paren"]}),
            )),
            Err("map_selector_no_match".to_owned())
        );
        assert_eq!(
            dispatcher.discover(&discover_input(
                &revision,
                json!({"expandPaths": ["../lib.rs"]}),
            )),
            Err("map_invalid_request".to_owned())
        );
        for path in ["/lib.rs", "C:/lib.rs", "src\\lib.rs"] {
            assert_eq!(
                dispatcher.discover(&discover_input(&revision, json!({"expandPaths": [path]}),)),
                Err("map_invalid_request".to_owned())
            );
        }
        assert_eq!(
            dispatcher.discover(&discover_input(
                &revision,
                json!({"scoreBand": {"min": 2, "max": 1}}),
            )),
            Err("map_invalid_request".to_owned())
        );
        let DispatchOutcome::Succeeded(canonical) = output(
            first,
            Vec::new(),
            &dispatcher.artifacts,
            &dispatcher.custody,
        ) else {
            panic!("bounded map output failed");
        };
        assert!(canonical.body.len() <= MAX_NATIVE_OUTPUT_BYTES);

        let legacy = dispatcher
            .discover(
                &serde_json::to_vec(&json!({
                    "expected_revision": revision,
                    "terms": ["child"],
                    "roots": [],
                    "languages": ["rust"]
                }))
                .unwrap(),
            )
            .unwrap()
            .0;
        assert!(legacy.get("results").is_some());
        assert!(legacy.get("mode").is_none());
        for hybrid in [
            json!({
                "expected_revision": revision,
                "terms": [],
                "roots": [],
                "languages": [],
                "map": {}
            }),
            json!({
                "expected_revision": revision,
                "map": {},
                "terms": ["ignored"]
            }),
        ] {
            assert_eq!(
                dispatcher.discover(&serde_json::to_vec(&hybrid).unwrap()),
                Err("invalid_arguments".to_owned())
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_discover_builds_and_reuses_real_cargo_structure_graphs() {
        let (directory, mut dispatcher) = dispatcher(None);
        let source = directory.join("source");
        std::fs::create_dir_all(source.join("src")).unwrap();
        std::fs::create_dir_all(source.join("helper/src")).unwrap();
        std::fs::write(
            source.join("Cargo.toml"),
            "[package]\nname=\"root\"\nversion=\"0.1.0\"\n[dependencies]\nhelper={path=\"helper\"}\n",
        )
        .unwrap();
        std::fs::write(
            source.join("src/lib.rs"),
            "pub fn root() {}\n#[test]\nfn root_test() {}\n",
        )
        .unwrap();
        std::fs::write(
            source.join("helper/Cargo.toml"),
            "[package]\nname=\"helper\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(source.join("helper/src/lib.rs"), "pub fn helper() {}\n").unwrap();

        let revision = dispatcher.revision().unwrap();
        let request = discover_input(
            &revision,
            json!({
                "expandPackages": ["root"],
                "relationships": ["imports"],
                "purpose": "dependencies"
            }),
        );
        let first = dispatcher.discover(&request).unwrap().0;
        assert!(first.get("graphEvidenceAvailable").is_none());
        assert!(first.get("graphDigests").is_none());
        assert_eq!(first["map"]["graph_evidence_available"], true);
        assert!(
            first["map"]["graph_snapshot_digest"]
                .as_str()
                .unwrap()
                .len()
                == 64
        );
        assert!(
            first["map"]["graph_nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| { node["kind"] == "package" && node["name"] == "helper" })
        );
        assert!(
            first["map"]["graph_edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| {
                    edge["relationship"] == "imports"
                        && edge["provenance"]["source"] == "cargo_manifest"
                })
        );
        let first_metrics = dispatcher.structure_graph.metrics().clone();
        let second = dispatcher.discover(&request).unwrap().0;
        assert_eq!(first, second);
        assert_eq!(dispatcher.structure_graph.metrics(), &first_metrics);

        let dependents = dispatcher
            .discover(&discover_input(
                &revision,
                json!({
                    "expandPackages": ["helper/Cargo.toml"],
                    "relationships": ["imports"],
                    "purpose": "dependents"
                }),
            ))
            .unwrap()
            .0;
        let nodes = dependents["map"]["graph_nodes"].as_array().unwrap();
        let root = nodes.iter().find(|node| node["name"] == "root").unwrap()["node_id"].clone();
        let helper = nodes.iter().find(|node| node["name"] == "helper").unwrap()["node_id"].clone();
        assert!(
            dependents["map"]["graph_edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| {
                    edge["relationship"] == "imports"
                        && edge["source_node"] == root
                        && edge["target_node"] == helper
                })
        );

        std::fs::write(source.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        let next_revision = dispatcher.revision().unwrap();
        dispatcher
            .discover(&discover_input(
                &next_revision,
                json!({
                    "expandPackages": ["root"],
                    "relationships": ["imports"],
                    "purpose": "dependencies"
                }),
            ))
            .unwrap();
        assert_eq!(dispatcher.structure_graph.metrics().parsed_fragments(), 0);
        assert_eq!(dispatcher.structure_graph.metrics().reused_fragments(), 2);
        assert_eq!(
            dispatcher.structure_graph.metrics().changed_paths(),
            &[RootRelativePath::parse("src/lib.rs", 4096).unwrap()]
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_discover_map_uses_normalized_runtime_semantic_evidence() {
        let (directory, mut dispatcher) =
            dispatcher_with_semantic(None, normalized_native_semantic_relationship);
        let publisher = dispatcher.semantic_evidence.clone();
        let agent_native_runtime = dispatcher.semantic_evidence.clone();
        let http_native_service = dispatcher.semantic_evidence.clone();
        assert!(publisher.shares_state_with(&agent_native_runtime));
        assert!(publisher.shares_state_with(&http_native_service));
        let revision = dispatcher.revision().unwrap();
        assert_eq!(
            dispatcher
                .semantic_evidence
                .snapshot(RevisionId::parse(&revision).unwrap())
                .unwrap()[0]
                .fact
                .provenance()
                .revision()
                .to_string(),
            revision
        );
        let (_, index) = dispatcher.workspace_index(&revision).unwrap();
        let source = index
            .entries()
            .iter()
            .flat_map(|entry| entry.syntax_records.iter())
            .find(|record| record.qualified_name().value().as_str() == "source")
            .unwrap()
            .declaration_id();
        let response = dispatcher
            .discover(&discover_input(
                &revision,
                json!({
                    "relationships": ["semantic_definition"],
                    "expansionSeeds": [hex(&source)],
                    "purpose": "dependencies"
                }),
            ))
            .unwrap()
            .0;
        assert_eq!(response["semanticEvidenceAvailable"], true);
        assert!(
            response["map"]["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| {
                    edge["relationship"] == "semantic_definition"
                        && edge["provenance"]["source"] == "lsp"
                        && edge["provenance"]["fact"]["classification"] == "semantic"
                })
        );

        let stored = publisher.snapshot(index.revision()).unwrap().remove(0);
        http_native_service.clear().unwrap();
        assert_eq!(
            dispatcher.discover(&discover_input(
                &revision,
                json!({"relationships": ["semantic_definition"]}),
            )),
            Err("map_semantic_evidence_unavailable".to_owned())
        );
        publisher.publish(&index, stored.clone()).unwrap();
        assert_eq!(
            agent_native_runtime
                .snapshot(index.revision())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            http_native_service
                .snapshot(index.revision())
                .unwrap()
                .len(),
            1
        );
        agent_native_runtime
            .replace(&index, vec![stored.clone(), stored.clone()])
            .unwrap();
        assert_eq!(
            http_native_service
                .snapshot(index.revision())
                .unwrap()
                .len(),
            2
        );
        http_native_service.replace(&index, vec![stored]).unwrap();

        std::fs::write(directory.join("source/source.rs"), "fn changed() {}\n").unwrap();
        let next_revision = dispatcher.revision().unwrap();
        assert_eq!(
            dispatcher.discover(&discover_input(
                &next_revision,
                json!({"relationships": ["semantic_definition"]}),
            )),
            Err("map_semantic_evidence_stale".to_owned())
        );
        let containment = dispatcher
            .discover(&discover_input(
                &next_revision,
                json!({"relationships": ["contains"]}),
            ))
            .unwrap()
            .0;
        assert_eq!(containment["semanticEvidenceAvailable"], false);
        assert_eq!(containment["mode"], "map");
        assert_eq!(
            dispatcher.discover(&discover_input(
                &next_revision,
                json!({"relationships": ["semantic_definition"]}),
            )),
            Err("map_semantic_evidence_unavailable".to_owned())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_map_budget_relationship_and_cursor_failures_are_typed() {
        let (directory, mut dispatcher) = dispatcher(None);
        std::fs::write(
            directory.join("source/lib.rs"),
            "fn one() {}\nfn two() {}\nfn three() {}\n",
        )
        .unwrap();
        let revision = dispatcher.revision().unwrap();
        for (budget, expected) in [
            (json!({"items": 201}), "map_items_bound_exceeded"),
            (
                json!({"estimatedTokens": 16385}),
                "map_estimated_tokens_bound_exceeded",
            ),
            (json!({"hops": 5}), "map_hops_bound_exceeded"),
            (json!({"degree": 65}), "map_degree_bound_exceeded"),
            (
                json!({"resultBytes": 61441}),
                "map_result_bytes_bound_exceeded",
            ),
        ] {
            assert_eq!(
                dispatcher.discover(&discover_input(&revision, json!({"budgets": budget}))),
                Err(expected.to_owned())
            );
        }
        assert_eq!(
            dispatcher.discover(&discover_input(
                &revision,
                json!({"relationships": ["semantic_definition"]}),
            )),
            Err("map_semantic_evidence_unavailable".to_owned())
        );
        assert_eq!(
            dispatcher.discover(&discover_input(
                &revision,
                json!({"pathPrefixes": ["../outside"]}),
            )),
            Err("map_invalid_request".to_owned())
        );

        let first = dispatcher
            .discover(&discover_input(&revision, json!({"budgets": {"items": 1}})))
            .unwrap()
            .0;
        let token = first["map"]["cursor"].as_str().unwrap().to_owned();
        let first_id = first["map"]["entries"][0]["declaration_id"].clone();
        let second = dispatcher
            .discover(&discover_input(
                &revision,
                json!({"budgets": {"items": 1}, "cursor": token.clone()}),
            ))
            .unwrap()
            .0;
        assert_ne!(first_id, second["map"]["entries"][0]["declaration_id"]);
        assert_eq!(
            dispatcher.discover(&discover_input(
                &revision,
                json!({
                    "budgets": {"items": 1},
                    "cursor": first["map"]["cursor"],
                    "expandSymbols": ["one"]
                }),
            )),
            Err("map_cursor_mismatch".to_owned())
        );
        let mut tampered = token.into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        assert_eq!(
            dispatcher.discover(&discover_input(
                &revision,
                json!({
                    "budgets": {"items": 1},
                    "cursor": String::from_utf8(tampered).unwrap()
                }),
            )),
            Err("map_cursor_invalid".to_owned())
        );
        std::fs::write(directory.join("source/lib.rs"), "fn replacement() {}\n").unwrap();
        assert_eq!(
            dispatcher.discover(&discover_input(&revision, json!({}))),
            Err("stale_revision".to_owned())
        );
        let next_revision = dispatcher.revision().unwrap();
        assert_eq!(
            dispatcher.discover(&discover_input(
                &next_revision,
                json!({"budgets": {"items": 1}, "cursor": first["map"]["cursor"]}),
            )),
            Err("map_cursor_mismatch".to_owned())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_map_error_codes_are_exhaustive_and_stable() {
        for (error, expected) in [
            (
                NativeMapError::Bound(MapBound::Items),
                "map_items_bound_exceeded",
            ),
            (
                NativeMapError::Bound(MapBound::EstimatedTokens),
                "map_estimated_tokens_bound_exceeded",
            ),
            (
                NativeMapError::Bound(MapBound::Hops),
                "map_hops_bound_exceeded",
            ),
            (
                NativeMapError::Bound(MapBound::Degree),
                "map_degree_bound_exceeded",
            ),
            (
                NativeMapError::Bound(MapBound::ResultBytes),
                "map_result_bytes_bound_exceeded",
            ),
            (
                NativeMapError::Bound(MapBound::Memory),
                "map_memory_bound_exceeded",
            ),
            (NativeMapError::CursorInvalid, "map_cursor_invalid"),
            (NativeMapError::CursorMismatch, "map_cursor_mismatch"),
            (
                NativeMapError::SemanticEvidenceUnavailable,
                "map_semantic_evidence_unavailable",
            ),
            (NativeMapError::SelectorNoMatch, "map_selector_no_match"),
            (NativeMapError::EvidenceStale, "map_semantic_evidence_stale"),
            (
                NativeMapError::EvidenceInvalid,
                "map_semantic_evidence_invalid",
            ),
            (
                NativeMapError::GraphEvidenceUnavailable,
                "map_graph_evidence_unavailable",
            ),
            (
                NativeMapError::GraphEvidenceStale,
                "map_graph_evidence_stale",
            ),
            (
                NativeMapError::GraphEvidenceInvalid,
                "map_graph_evidence_invalid",
            ),
            (
                NativeMapError::HistoryEvidenceUnavailable,
                "map_history_evidence_unavailable",
            ),
            (
                NativeMapError::HistoryEvidenceStale,
                "map_history_evidence_stale",
            ),
            (NativeMapError::RevisionStale, "stale_revision"),
            (
                NativeMapError::RevisionUnavailable,
                "map_revision_unavailable",
            ),
            (NativeMapError::TimeLimit, "map_time_limit_exceeded"),
            (NativeMapError::InvalidRequest, "map_invalid_request"),
            (NativeMapError::Unavailable, "map_unavailable"),
            (NativeMapError::Serialization, "map_serialization_failed"),
        ] {
            assert_eq!(error.code(), expected);
        }
    }

    #[test]
    fn native_search_structural_rewrite_returns_bound_apply_without_writing() {
        let (directory, mut dispatcher) = dispatcher(None);
        let source = directory.join("source/lib.rs");
        std::fs::write(&source, "fn main() { let value = Some(1); }\n").unwrap();
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "expected_revision": revision.clone(),
            "text": "Some($A)",
            "mode": "structural",
            "rewrite": "Ok($A)",
            "path_prefixes": [],
            "languages": ["rust"]
        }))
        .unwrap();

        dispatcher.workspace_index(&revision).unwrap();
        let syntax_metrics = dispatcher.syntax_index.metrics();
        let (response, artifacts) = dispatcher.search(&input).unwrap();
        assert_eq!(dispatcher.syntax_index.metrics(), syntax_metrics);
        assert!(artifacts.is_empty());
        assert_eq!(response["matches"].as_array().unwrap().len(), 1);
        let apply = response["rewrite"]["apply"].as_object().unwrap();
        assert_eq!(apply.len(), 1);
        let token = apply["preview_token"].as_str().unwrap();
        assert!(token.starts_with("kitsp1_"));
        assert_eq!(token.len(), 71);
        assert_eq!(
            response["result_bytes"],
            serde_json::to_vec(&response).unwrap().len()
        );
        assert!(
            response["rewrite"]["change_diff"]
                .as_str()
                .unwrap()
                .contains("+fn main() { let value = Ok(1); }")
        );
        assert!(!response.to_string().contains("replacement"));
        assert!(!response.to_string().contains("expected_revision"));
        assert_eq!(
            std::fs::read_to_string(source).unwrap(),
            "fn main() { let value = Some(1); }\n"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn changed_structural_preview(dispatcher: &mut NativeDispatcher) -> Value {
        let revision = dispatcher.revision().unwrap();
        dispatcher
            .search(
                &serde_json::to_vec(&json!({
                    "expected_revision": revision,
                    "text": "Some($A)",
                    "mode": "structural",
                    "rewrite": "Ok($A)",
                    "path_prefixes": [],
                    "languages": ["rust"]
                }))
                .unwrap(),
            )
            .unwrap()
            .0
    }

    #[test]
    fn structural_preview_token_is_scoped_single_use_and_returns_the_same_change_diff_string() {
        let runner = CheckRunner::conformance([
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
        ]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        std::fs::write(
            directory.join("source/lib.rs"),
            "fn main() { let value = Some(1); }\n",
        )
        .unwrap();
        let preview = changed_structural_preview(&mut dispatcher);
        let token = preview["rewrite"]["apply"]["preview_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let change_diff = preview["rewrite"]["change_diff"]
            .as_str()
            .unwrap()
            .to_owned();
        let owner = dispatcher.authenticated.clone();
        let other = PrincipalId::generate().unwrap();
        dispatcher.authenticated = AuthenticatedPrincipal::from_grants(GrantSnapshot::new(
            other,
            dispatcher.config.project_id(),
            dispatcher.grants.grants().iter().copied(),
        ));
        assert_eq!(
            dispatcher.edit(
                &serde_json::to_vec(&json!({"preview_token": token})).unwrap(),
                attempt(&dispatcher),
            ),
            Err("structural_preview_invalid".to_owned())
        );
        dispatcher.authenticated = owner;
        let input = serde_json::to_vec(&json!({"preview_token": token})).unwrap();
        let owner_attempt = attempt(&dispatcher);
        let (result, _, committed) = dispatcher.edit(&input, owner_attempt).unwrap();
        assert!(committed);
        assert!(result["change_diff"].is_string());
        assert_eq!(result["change_diff"], change_diff);
        assert_eq!(
            std::fs::read_to_string(directory.join("source/lib.rs")).unwrap(),
            "fn main() { let value = Ok(1); }\n"
        );
        assert_eq!(
            dispatcher.edit(&input, attempt(&dispatcher)),
            Err("structural_preview_invalid".to_owned())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn identity_preview_has_no_token_and_expired_or_formatter_divergent_tokens_are_consumed() {
        let runner = CheckRunner::conformance([
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
        ]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let source = directory.join("source/lib.rs");
        std::fs::write(&source, "fn main() { let value = Some(1); }\n").unwrap();
        let revision = dispatcher.revision().unwrap();
        let identity = dispatcher
            .search(
                &serde_json::to_vec(&json!({
                    "expected_revision": revision,
                    "text": "Some($A)",
                    "mode": "structural",
                    "rewrite": "Some($A)",
                    "path_prefixes": [],
                    "languages": ["rust"]
                }))
                .unwrap(),
            )
            .unwrap()
            .0;
        assert_eq!(identity["rewrite"]["changed"], false);
        assert!(identity["rewrite"].get("apply").is_none());

        let expired = changed_structural_preview(&mut dispatcher);
        let expired = expired["rewrite"]["apply"]["preview_token"]
            .as_str()
            .unwrap()
            .to_owned();
        dispatcher
            .structural_previews
            .entries
            .values_mut()
            .for_each(|record| record.expires = Instant::now());
        assert_eq!(
            dispatcher.edit(
                &serde_json::to_vec(&json!({"preview_token": expired})).unwrap(),
                attempt(&dispatcher),
            ),
            Err("structural_preview_invalid".to_owned())
        );

        let preview = changed_structural_preview(&mut dispatcher);
        let token = preview["rewrite"]["apply"]["preview_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let path = crate::workspace::edit::ir::RootRelativePath::parse("lib.rs", 4096).unwrap();
        dispatcher.formatter_required = true;
        dispatcher.formatter = Some(NativeFormatterRuntime {
            descriptor: crate::workspace::edit::format::FormatterDescriptor::new(
                "rustfmt",
                "test",
                vec![path],
            )
            .unwrap(),
            executor: test_support::formatter_executor(test_support::FormatterTestAction::Rewrite(
                "lib.rs".to_owned(),
                b"fn main() { let value = Ok( 1 ); }\n".to_vec(),
            )),
        });
        let input = serde_json::to_vec(&json!({"preview_token": token})).unwrap();
        assert_eq!(
            dispatcher.edit(&input, attempt(&dispatcher)),
            Err("edit_recovery_failed".to_owned())
        );
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "fn main() { let value = Some(1); }\n"
        );
        assert_eq!(
            dispatcher.edit(&input, attempt(&dispatcher)),
            Err("structural_preview_invalid".to_owned())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn protocol_server(
        responses: Vec<String>,
    ) -> (String, thread::JoinHandle<Vec<serde_json::Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                let header_end = loop {
                    let read = stream.read(&mut buffer).unwrap();
                    assert_ne!(read, 0);
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap();
                while bytes.len() - header_end < length {
                    let read = stream.read(&mut buffer).unwrap();
                    assert_ne!(read, 0);
                    bytes.extend_from_slice(&buffer[..read]);
                }
                requests
                    .push(serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap());
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .unwrap();
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    fn native_inputs(revision: &str) -> Vec<(String, Value)> {
        let original = b"{}\n";
        vec![
            (
                "kit_discover".to_owned(),
                json!({"expected_revision":revision,"terms":["main"],"roots":[],"languages":["rust"]}),
            ),
            (
                "kit_search".to_owned(),
                json!({"expected_revision":revision,"text":"main","mode":"content","path_prefixes":[],"languages":["rust"]}),
            ),
            (
                "kit_read".to_owned(),
                json!({"expected_revision":revision,"path":"lib.rs","range":{"kind":"full"}}),
            ),
            (
                "kit_edit".to_owned(),
                json!({
                    "version":1,
                    "expected_revision":revision,
                    "operations":[{
                        "op":"replace_range",
                        "path":"format.json",
                        "base_digest":format!("blake3:{}", blake3::hash(original).to_hex()),
                        "range":{"start":0,"end":original.len()},
                        "expected":{"encoding":"utf8","newline":"lf","text":"{}","final_newline":true},
                        "replacement":{"encoding":"utf8","newline":"lf","text":"{\"x\":1}","final_newline":true},
                        "executable":"preserve"
                    }]
                }),
            ),
            (
                "kit_run".to_owned(),
                json!({
                    "argv":["cargo","metadata"],
                    "working_directory":".",
                    "mounts":{"source":"read_only","build":"read_write","temp":"read_write"},
                    "environment":{},
                    "network":"deny",
                    "host_compatibility":false,
                    "background":"foreground",
                    "limits":{"cpu_millis":1000,"memory_bytes":1048576,"pids":8,"file_bytes":1048576,"disk_bytes":1048576,"io_bytes":1048576,"output_bytes":65536,"wall_time_millis":1000}
                }),
            ),
            (
                "kit_check".to_owned(),
                json!({"profile":"fast","targets":[]}),
            ),
        ]
    }

    #[test]
    fn native_workspace_index_retains_tree_for_incremental_revision() {
        let (directory, mut dispatcher) = dispatcher(None);
        let first_revision = dispatcher.revision().unwrap();
        let (workspace, first_index) = dispatcher.workspace_index(&first_revision).unwrap();
        assert_eq!(dispatcher.syntax_index.metrics().full_parses, 1);
        let main = discover(
            &workspace,
            &first_index,
            &DiscoverQuery {
                terms: vec!["main".to_owned()],
                roots: Vec::new(),
                languages: vec!["rust".to_owned()],
            },
            &DiscoverOptions::default(),
            None,
        )
        .unwrap()
        .results
        .into_iter()
        .find(|result| result.symbol.as_deref() == Some("main"))
        .unwrap();
        assert_eq!(
            (main.line, main.byte_start, main.byte_end),
            (Some(1), Some(0), Some(12))
        );

        let updated_source = "fn updated_name() {\n    let changed = true;\n}\n";
        std::fs::write(directory.join("source/lib.rs"), updated_source).unwrap();
        let second_revision = dispatcher.revision().unwrap();
        assert_ne!(second_revision, first_revision);
        let (workspace, second_index) = dispatcher.workspace_index(&second_revision).unwrap();
        assert_eq!(dispatcher.syntax_index.metrics().incremental_parses, 1);
        assert_eq!(dispatcher.syntax_index.cache_usage().resident_files, 1);
        let updated = discover(
            &workspace,
            &second_index,
            &DiscoverQuery {
                terms: vec!["updated_name".to_owned()],
                roots: Vec::new(),
                languages: vec!["rust".to_owned()],
            },
            &DiscoverOptions::default(),
            None,
        )
        .unwrap()
        .results
        .into_iter()
        .find(|result| result.symbol.as_deref() == Some("updated_name"))
        .unwrap();
        assert_eq!(updated.line, Some(1));
        assert_eq!(updated.byte_start, Some(0));
        assert_eq!(updated.byte_end, Some(updated_source.trim_end().len()));
        assert_ne!(updated.byte_end, main.byte_end);
        let records = &second_index
            .entries()
            .iter()
            .find(|entry| entry.path == Path::new("lib.rs"))
            .unwrap()
            .syntax_records;
        let updated_record = records
            .iter()
            .find(|record| record.display_name().value().as_str() == "updated_name")
            .unwrap();
        assert_eq!(updated_record.range().start_byte, 0);
        assert_eq!(
            updated_record.range().end_byte,
            updated_source.trim_end().len()
        );
        assert_eq!(updated_record.range().start_line, 1);
        assert_eq!(updated_record.range().end_line, 3);
        assert!(
            records
                .iter()
                .all(|record| record.display_name().value().as_str() != "main")
        );
        let stale = discover(
            &workspace,
            &second_index,
            &DiscoverQuery {
                terms: vec!["main".to_owned()],
                roots: Vec::new(),
                languages: vec!["rust".to_owned()],
            },
            &DiscoverOptions::default(),
            None,
        )
        .unwrap();
        assert!(
            stale
                .results
                .iter()
                .all(|result| result.symbol.as_deref() != Some("main"))
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn anthropic_tool_stream(inputs: &[(String, Value)]) -> String {
        let mut stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-tools\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n"
        )
        .to_owned();
        for (index, (name, input)) in inputs.iter().enumerate() {
            stream.push_str(&format!(
                "event: content_block_start\ndata: {}\n\n",
                json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":format!("anthropic-call-{index}"),"name":name,"input":{}}})
            ));
            stream.push_str(&format!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}})
            ));
            stream.push_str(&format!(
                "event: content_block_stop\ndata: {}\n\n",
                json!({"type":"content_block_stop","index":index})
            ));
        }
        stream.push_str(concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        ));
        stream
    }

    fn anthropic_completion_stream() -> String {
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-done\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"complete\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )
        .to_owned()
    }

    fn completions_tool_stream(inputs: &[(String, Value)]) -> String {
        let calls = inputs
            .iter()
            .enumerate()
            .map(|(index, (name, input))| {
                json!({"index":index,"id":format!("completion-call-{index}"),"type":"function","function":{"name":name,"arguments":input.to_string()}})
            })
            .collect::<Vec<_>>();
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"id":"chatcmpl-tools","model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
        )
    }

    fn completions_completion_stream() -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"id":"chatcmpl-done","model":"gpt-test","choices":[{"index":0,"delta":{"content":"complete"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
        )
    }

    fn configure_formatter(dispatcher: &mut NativeDispatcher) {
        let path =
            crate::workspace::edit::ir::RootRelativePath::parse("format.json", 4096).unwrap();
        dispatcher.formatter_required = true;
        dispatcher.formatter = Some(NativeFormatterRuntime {
            descriptor: crate::workspace::edit::format::FormatterDescriptor::new(
                "route-formatter",
                "v1",
                vec![path],
            )
            .unwrap(),
            executor: test_support::formatter_executor(test_support::FormatterTestAction::Rewrite(
                "format.json".to_owned(),
                b"{\n  \"x\": 1\n}\n".to_vec(),
            )),
        });
    }

    async fn exercise_provider_route<M: ModelAdapter>(
        model: M,
        captured: thread::JoinHandle<Vec<Value>>,
        directory: PathBuf,
        mut dispatcher: NativeDispatcher,
    ) {
        dispatcher.run_runner = Some(CheckRunner::conformance([ConformanceCheck::pass(
            b"run-output",
            b"",
        )]));
        configure_formatter(&mut dispatcher);
        let revision = dispatcher.revision().unwrap();
        let inputs = native_inputs(&revision);
        let principal = dispatcher.authenticated.principal_id();
        let project = dispatcher.config.project_id();
        let run = dispatcher.config.run_id();
        let workspace = dispatcher.workspace_id;
        let attempt = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal,
            FencingToken::new(1),
        );
        let configured = NativeCatalog::all()
            .iter()
            .map(|descriptor| {
                let constraints = ArgumentConstraints::new([format!(
                    "native={}@{}",
                    descriptor.tool().short_name(),
                    descriptor.identity().version().as_str()
                )
                .into_bytes()]);
                (descriptor, constraints)
            })
            .collect::<Vec<_>>();
        let grants = CapabilityGrantSnapshot::new(
            &dispatcher.config,
            configured.iter().map(|(descriptor, constraints)| {
                CapabilityGrant::new(
                    principal,
                    project,
                    workspace,
                    descriptor.identity().clone(),
                    descriptor.schema().normalized_digest(),
                    descriptor.effect(),
                    constraints.clone(),
                )
            }),
            DigestAlgorithm::Sha256,
        );
        let bindings = configured
            .iter()
            .map(|(descriptor, constraints)| {
                let binding = ToolBinding::new(
                    descriptor.spec().clone(),
                    descriptor.identity().clone(),
                    descriptor.normalized_schema().clone(),
                    descriptor.schema().normalized_digest(),
                    descriptor.schema().normalized_digest(),
                    descriptor.effect(),
                    constraints.clone(),
                    descriptor.reservation(),
                    descriptor.retry_safety(),
                    descriptor.approval(),
                );
                if descriptor.tool() == NativeTool::Check {
                    binding.with_cost_estimator(|_| {
                        Ok(crate::runtime::scheduler::limits::Spend::new(0, 0, 0, 1, 2))
                    })
                } else {
                    binding
                }
            })
            .collect::<Vec<_>>();
        let database = directory.join("route.sqlite3");
        let mut store = test_support::open_sqlite_store(&database).unwrap();
        let claim = store
            .install_driver_claim_for_test(crate::api::service::AttemptDriverClaim {
                run_id: run,
                attempt_id: attempt.attempt_id,
                principal_id: principal,
                fence: attempt.fencing_token,
                lease_version: 1,
                expires_at_unix_micros: 0,
            })
            .unwrap();
        let budget = Arc::new(BudgetLedger::new(RunBudget::new(0, 0, 0, 256, 256)));
        let snapshot = dispatcher.config.clone();
        let tool = ToolExecutorAdapter::new(
            bindings,
            ToolKernelContext {
                authenticated: dispatcher.authenticated.clone(),
                config: dispatcher.config.clone(),
                grants,
                delegation: None,
                workspace_id: workspace,
                project_id: project,
                attempt,
                claim,
                current_fence: Arc::new(AtomicU64::new(1)),
                cancellation: Arc::new(AtomicBool::new(false)),
                cancellation_coordinator: Arc::new(SqliteCancellationCoordinator::new(&database)),
                budget: Arc::clone(&budget),
                custody: crate::domain::secret::SecretCustody::default(),
            },
            store,
            move |invocation| dispatcher.dispatch(invocation),
        )
        .unwrap();
        let agent = Agent::builder()
            .model(model)
            .tool_executor(tool)
            .input(vec![Item::text(ItemKind::User, "exercise native tools")])
            .build()
            .unwrap();
        let mut driver = agent
            .start(SessionConfig::new(run.to_string()))
            .await
            .unwrap();
        loop {
            match driver.next().await.unwrap() {
                LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                    pending.approve(&mut driver).unwrap();
                }
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {}
                LoopStep::Interrupt(other) => panic!("unexpected interrupt: {other:?}"),
                LoopStep::Finished(result) => {
                    assert_eq!(result.finish_reason, agentkit_core::FinishReason::Completed);
                    break;
                }
            }
        }

        let requests = captured.join().unwrap();
        assert_eq!(requests.len(), 2);
        let registered = requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                tool.get("name")
                    .or_else(|| tool.pointer("/function/name"))
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_owned()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            registered,
            NativeTool::ALL
                .into_iter()
                .map(NativeTool::provider_alias)
                .collect()
        );
        let events = test_support::open_sqlite_store(&database)
            .unwrap()
            .events()
            .unwrap();
        let payloads = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event.event_type.as_str(),
                    "capability.invocation_intent" | "capability.invocation_outcome"
                )
            })
            .map(|event| serde_json::from_slice::<Value>(&event.event.payload).unwrap())
            .collect::<Vec<_>>();
        for (alias, _) in &inputs {
            let descriptor = NativeCatalog::by_tool_name(alias).unwrap();
            assert!(payloads.iter().any(|payload| {
                payload["capability"]["name"] == descriptor.tool().short_name()
            }));
        }
        let outputs = payloads
            .iter()
            .filter_map(|payload| payload["result"]["output"]["body"].as_array())
            .map(|bytes| {
                serde_json::from_slice::<Value>(
                    &bytes
                        .iter()
                        .map(|byte| byte.as_u64().unwrap() as u8)
                        .collect::<Vec<_>>(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 6);
        assert!(outputs.iter().all(|output| {
            output["version"] == 1 && output["truncated"] == false && output["artifacts"].is_array()
        }));
        assert_eq!(budget.totals().committed.tools(), 6);
        assert_eq!(budget.totals().committed.processes(), 3);
        let restarted =
            crate::agent::executor::tool_budget_from_events(&events, &snapshot).unwrap();
        assert_eq!(restarted.remaining().tools(), 250);
        assert_eq!(restarted.remaining().processes(), 253);
        assert!(
            restarted
                .reserve(
                    crate::runtime::scheduler::reserve::ReservationId::new(1),
                    crate::runtime::scheduler::limits::Spend::new(0, 0, 0, 251, 0),
                )
                .is_err()
        );
        assert!(
            restarted
                .reserve(
                    crate::runtime::scheduler::reserve::ReservationId::new(2),
                    crate::runtime::scheduler::limits::Spend::new(0, 0, 0, 0, 254),
                )
                .is_err()
        );
        let durable = serde_json::to_string(&outputs).unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.join("source/format.json")).unwrap(),
            "{\n  \"x\": 1\n}\n",
            "{durable}"
        );
        for evidence in [
            "diff_artifact",
            "feedback",
            "process_artifact",
            "verification",
        ] {
            assert!(
                durable.contains(evidence),
                "missing route evidence: {evidence}"
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn anthropic_streamed_aliases_reach_native_dispatch_through_the_agent_loop() {
        let checks = (0..6).map(|_| ConformanceCheck::pass(b"", b""));
        let (directory, mut dispatcher) = dispatcher(Some(CheckRunner::conformance(checks)));
        let revision = dispatcher.revision().unwrap();
        let inputs = native_inputs(&revision);
        let (url, captured) = protocol_server(vec![
            anthropic_tool_stream(&inputs),
            anthropic_completion_stream(),
        ]);
        let mut config =
            agentkit_provider_anthropic::AnthropicConfig::new("test", "claude-test", 1024)
                .unwrap()
                .with_base_url(url);
        config.tool_choice = None;
        exercise_provider_route(
            agentkit_provider_anthropic::AnthropicAdapter::new(config).unwrap(),
            captured,
            directory,
            dispatcher,
        )
        .await;
    }

    #[tokio::test]
    async fn completions_streamed_aliases_reach_native_dispatch_through_the_agent_loop() {
        let checks = (0..6).map(|_| ConformanceCheck::pass(b"", b""));
        let (directory, mut dispatcher) = dispatcher(Some(CheckRunner::conformance(checks)));
        let revision = dispatcher.revision().unwrap();
        let inputs = native_inputs(&revision);
        let (url, captured) = protocol_server(vec![
            completions_tool_stream(&inputs),
            completions_completion_stream(),
        ]);
        exercise_provider_route(
            agentkit_provider_openai::OpenAIAdapter::new(
                agentkit_provider_openai::OpenAIConfig::new("test", "gpt-test").with_base_url(url),
            )
            .unwrap(),
            captured,
            directory,
            dispatcher,
        )
        .await;
    }

    #[test]
    fn trusted_check_runner_returns_bounded_artifacts() {
        let runner = CheckRunner::conformance([
            ConformanceCheck::pass(b"ok", b""),
            ConformanceCheck::pass(b"ok", b""),
        ]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let owner = attempt(&dispatcher);
        let (value, artifacts) = dispatcher
            .check(br#"{"profile":"fast","targets":[]}"#, owner)
            .unwrap();
        assert_eq!(value["verification"]["checks"][0]["status"], "pass");
        assert_eq!(artifacts.len(), 3);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn absent_trusted_check_runner_is_typed_unavailable() {
        let (directory, mut dispatcher) = dispatcher(None);
        let owner = attempt(&dispatcher);
        assert_eq!(
            dispatcher.check(br#"{"profile":"fast","targets":[]}"#, owner),
            Err("trusted_check_runner_unavailable".to_owned())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn all_bytes(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                bytes.extend(all_bytes(&entry.unwrap().path()));
            }
        } else if let Ok(file) = std::fs::read(path) {
            bytes.extend(file);
        }
        bytes
    }

    #[test]
    fn cancellation_during_native_run_reaps_the_protocol_service() {
        let (directory, mut dispatcher) = dispatcher(None);
        let cancellation = Arc::clone(&dispatcher.live_cancellation);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        dispatcher.run_runner = Some(CheckRunner::conformance([
            ConformanceCheck::CancelWhenSignalled {
                entered: entered_tx,
                cancellation: Arc::clone(&cancellation),
            },
        ]));
        let owner = attempt(&dispatcher);
        let result = thread::scope(|scope| {
            let worker = scope.spawn(move || {
                dispatcher.run(
                    br#"{"argv":["long-running"],"working_directory":".","mounts":{"source":"read_only","build":"read_write","temp":"read_write"},"environment":{},"network":"deny","host_compatibility":false,"background":"foreground","limits":{"cpu_millis":1000,"memory_bytes":1048576,"pids":8,"file_bytes":1048576,"disk_bytes":1048576,"io_bytes":1048576,"output_bytes":65536,"wall_time_millis":1000}}"#,
                    owner,
                )
            });
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            cancellation.store(true, Ordering::Release);
            worker.join().unwrap()
        });
        assert_eq!(result, Err("cancelled".to_owned()));
        let evidence = String::from_utf8_lossy(&all_bytes(&directory)).into_owned();
        assert!(evidence.contains("\"kill_attempted\":true"));
        assert!(evidence.contains("\"reaped\":true"));
        assert!(evidence.contains("\"survivors\":0"));
        assert!(evidence.contains("\"phase\":\"quiescent\""));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancellation_during_each_native_check_child_reaps_with_zero_survivors() {
        for child in 0..2 {
            let (directory, mut dispatcher) = dispatcher(None);
            let cancellation = Arc::clone(&dispatcher.live_cancellation);
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let mut checks = Vec::new();
            if child == 1 {
                checks.push(ConformanceCheck::pass(b"first", b""));
            }
            checks.push(ConformanceCheck::CancelWhenSignalled {
                entered: entered_tx,
                cancellation: Arc::clone(&cancellation),
            });
            dispatcher.check_runner = Some(CheckRunner::conformance(checks));
            let owner = attempt(&dispatcher);
            let result = thread::scope(|scope| {
                let worker = scope
                    .spawn(move || dispatcher.check(br#"{"profile":"fast","targets":[]}"#, owner));
                entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                cancellation.store(true, Ordering::Release);
                worker.join().unwrap()
            });
            let (result, artifacts) = result.unwrap();
            assert_eq!(result["verification"]["decision"], "abort");
            assert_eq!(
                result["verification"]["checks"][child]["status"],
                "cancelled"
            );
            assert!(result["verification"]["checks"][child]["process_artifact"].is_string());
            assert_eq!(artifacts.len(), 3);
            let evidence = String::from_utf8_lossy(&all_bytes(&directory)).into_owned();
            assert!(
                evidence.contains("\"kill_attempted\":true"),
                "child {child}"
            );
            assert!(evidence.contains("\"reaped\":true"), "child {child}");
            assert!(evidence.contains("\"survivors\":0"), "child {child}");
            assert!(
                evidence.contains("\"phase\":\"quiescent\""),
                "child {child}"
            );
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn native_edit_aborts_without_required_verification_services() {
        let (directory, mut dispatcher) = dispatcher(None);
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "version": 1,
            "expected_revision": revision,
            "operations": [{
                "op": "add_file",
                "path": "created.txt",
                "content": {
                    "encoding": "utf8",
                    "newline": "lf",
                    "text": "never materialized",
                    "final_newline": true
                },
                "executable": false
            }]
        }))
        .unwrap();
        assert_eq!(
            dispatcher.edit(&input, attempt(&dispatcher)),
            Err("trusted_edit_runner_unavailable".to_owned())
        );
        assert!(!dispatcher.root.join("created.txt").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_edit_uses_configured_check_runner_before_materialization() {
        let runner = CheckRunner::conformance([
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
        ]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "version": 1,
            "expected_revision": revision,
            "operations": [{
                "op": "add_file",
                "path": "created.txt",
                "content": {
                    "encoding": "utf8",
                    "newline": "lf",
                    "text": "verified",
                    "final_newline": true
                },
                "executable": false
            }]
        }))
        .unwrap();
        let owner = attempt(&dispatcher);
        let (result, artifacts, committed) = dispatcher.edit(&input, owner).unwrap();
        assert_eq!(
            std::fs::read_to_string(dispatcher.root.join("created.txt")).unwrap(),
            "verified\n"
        );
        assert!(!result["verification"].is_null());
        assert!(result["change_diff"].is_string());
        assert!(artifacts.len() >= 2);
        assert!(committed);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_empty_edit_creates_no_recovery_state_or_workspace_revision() {
        let runner = CheckRunner::conformance([
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
        ]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "version": 1,
            "expected_revision": revision,
            "operations": []
        }))
        .unwrap();
        assert_eq!(
            dispatcher.edit(&input, attempt(&dispatcher)),
            Err("edit_recovery_failed".to_owned())
        );
        assert_eq!(dispatcher.revision().unwrap(), revision);
        assert!(!dispatcher.root.join(".kit-edit-recovery.manifest").exists());
        assert!(!dispatcher.root.join(".kit-edit-recovery.ledger").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_edit_returns_feedback_and_preserves_revision_on_new_required_diagnostic() {
        let diagnostic = serde_json::to_vec(&json!({
            "schema_version": 1,
            "path": "created.txt",
            "range": {"start_line": 1, "start_column": 1, "end_line": 1, "end_column": 2},
            "code": "E1",
            "message": "new diagnostic",
            "severity": "error",
            "tool": "test"
        }))
        .unwrap();
        let runner = CheckRunner::conformance([
            ConformanceCheck::pass(b"", b""),
            ConformanceCheck::pass(b"", b""),
            ConformanceCheck::exit(1, diagnostic, b"failed"),
            ConformanceCheck::pass(b"", b""),
        ]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "version": 1,
            "expected_revision": revision,
            "operations": [{
                "op": "add_file",
                "path": "created.txt",
                "content": {
                    "encoding": "utf8",
                    "newline": "lf",
                    "text": "rejected",
                    "final_newline": true
                },
                "executable": false
            }]
        }))
        .unwrap();
        let owner = attempt(&dispatcher);
        let (result, artifacts, committed) = dispatcher.edit(&input, owner).unwrap();
        assert_eq!(result["outcome"], "aborted");
        assert!(!result["feedback"]["items"].as_array().unwrap().is_empty());
        assert_eq!(result["events"].as_array().unwrap().len(), 6);
        assert_eq!(artifacts.len(), 3);
        assert!(!committed);
        assert!(!dispatcher.root.join("created.txt").exists());
        assert_eq!(dispatcher.revision().unwrap(), revision);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn native_edit_rejects_missing_required_trusted_services_before_staging() {
        let runner = CheckRunner::conformance([]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "version": 1,
            "expected_revision": revision,
            "operations": [{
                "op": "add_file",
                "path": "created.txt",
                "content": {"encoding": "utf8", "newline": "lf", "text": "x", "final_newline": true},
                "executable": false
            }]
        }))
        .unwrap();
        let owner = attempt(&dispatcher);
        dispatcher.feedback = None;
        assert_eq!(
            dispatcher.edit(&input, owner),
            Err("trusted_edit_feedback_unavailable".to_owned())
        );
        assert!(!dispatcher.root.join("created.txt").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancellation_and_hard_run_limits_stop_before_effects() {
        let (directory, mut dispatcher) = dispatcher(None);
        dispatcher.live_cancellation.store(true, Ordering::Release);
        assert_eq!(
            dispatcher.run(
                br#"{"argv":["true"],"working_directory":".","mounts":{"source":"read_only","build":"read_write","temp":"read_write"},"environment":{},"network":"deny","host_compatibility":false,"background":"foreground","limits":{"cpu_millis":1,"memory_bytes":1,"pids":1,"file_bytes":1,"disk_bytes":1,"io_bytes":1,"output_bytes":1,"wall_time_millis":1}}"#,
                crate::domain::lifecycle::AttemptOwnership::new(
                    crate::domain::ids::AttemptId::generate().unwrap(),
                    dispatcher.authenticated.principal_id(),
                    crate::domain::lifecycle::FencingToken::new(1),
                ),
            ),
            Err("cancelled".to_owned())
        );
        dispatcher.live_cancellation.store(false, Ordering::Release);
        assert_eq!(
            dispatcher.run(
                br#"{"argv":["true"],"working_directory":".","mounts":{"source":"read_only","build":"read_write","temp":"read_write"},"environment":{},"network":"deny","host_compatibility":false,"background":"foreground","limits":{"cpu_millis":60001,"memory_bytes":1,"pids":1,"file_bytes":1,"disk_bytes":1,"io_bytes":1,"output_bytes":1,"wall_time_millis":1}}"#,
                crate::domain::lifecycle::AttemptOwnership::new(
                    crate::domain::ids::AttemptId::generate().unwrap(),
                    dispatcher.authenticated.principal_id(),
                    crate::domain::lifecycle::FencingToken::new(1),
                ),
            ),
            Err("run_request_rejected".to_owned())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
