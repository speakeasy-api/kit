use crate::{
    ArmConfig, CandidateRange, CandidateSemantics, ExecutorEvidence, ProtocolError, RawCandidate,
    RawTrial, Result, RetrievalSource, SourceObservation, SourceStatus, TrialTerminal,
    WorkerArmRequest, WorkerQuery, canonical, sha256,
};
use kit::workspace::{
    edit::ir::RootRelativePath,
    graph::{
        history::{HistoryGraphProvider, HistoryOptions, HistoryRequest},
        structure::{GraphOptions, NodeKind, StructureGraph, StructureGraphProvider},
    },
    index::meta::{IndexOptions, MetadataIndex},
    map::{
        MapBudget, MapLimits, Personalization, RepositoryMapRequest, build_repository_map,
        build_repository_map_with_structure,
    },
    revision::{EntryKind, ManagedWorkspace, RevisionOptions},
    search::{
        lexical::{SearchMode, SearchOptions, SearchQuery, search as lexical_search},
        structural::{StructuralOptions, StructuralQuery, search as structural_search},
    },
    syntax::SyntaxIndex,
};
use serde_json::Value;
use sha2::Digest as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, Instant},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const MAX_WORKER_INPUT: u64 = 256 * 1024;
const MAX_CANDIDATES: usize = 100_000;
const LEXICAL_CONTEXT_BYTES: usize = 1_024;
const MAX_HISTORY_PATHS: usize = 32;

pub fn run_worker(
    source: &Path,
    git_metadata_root: &Path,
    query_path: &Path,
    request_path: &Path,
    output: &Path,
    cache: &Path,
) -> Result<()> {
    if !source.is_dir() || !cache.is_dir() || output.exists() {
        return Err(ProtocolError("invalid worker materialized paths".into()).into());
    }
    let query: WorkerQuery = read_json(query_path, MAX_WORKER_INPUT)?;
    let request: WorkerArmRequest = read_json(request_path, MAX_WORKER_INPUT)?;
    validate_worker_inputs(&query, &request)?;
    let trial = execute(source, git_metadata_root, cache, query, request)?;
    let mut bytes = serde_json::to_vec(&trial)?;
    if bytes.len() > crate::MAX_JSON_BYTES {
        return Err(ProtocolError("raw worker output exceeds bound".into()).into());
    }
    bytes.push(b'\n');
    let mut options = fs::OpenOptions::new();
    options
        .create_new(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW);
    crate::reject_symlink_components(output, true)?;
    let mut file = options.open(output)?;
    std::io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()?;
    Ok(())
}

fn validate_worker_inputs(query: &WorkerQuery, request: &WorkerArmRequest) -> Result<()> {
    if query.task_id.is_empty()
        || query.task_id.len() > 128
        || query.query.is_empty()
        || query.query.len() > 4096
        || query.query_digest != sha256(query.query.as_bytes())
        || request.config != ArmConfig::frozen(request.config.arm)
        || request.unit_id.is_empty()
        || request.cache_id.is_empty()
        || !crate::valid_digest(&request.worker_executable_digest)
        || !matches!(
            request.executor_evidence,
            ExecutorEvidence::LocalSandboxNotTrusted | ExecutorEvidence::M004ProductionTrusted
        )
    {
        return Err(ProtocolError("invalid bounded worker request".into()).into());
    }
    Ok(())
}

pub(crate) fn execute(
    source: &Path,
    git_metadata_root: &Path,
    cache: &Path,
    query: WorkerQuery,
    request: WorkerArmRequest,
) -> Result<RawTrial> {
    let measured_started_at = timestamp()?;
    let trial_started = Instant::now();
    let repository = repository_root(source, git_metadata_root)?;
    let source_prefix = source.strip_prefix(&repository)?.to_path_buf();
    validate_filtered_repository(&repository, &source_prefix)?;
    let revision_options = RevisionOptions {
        metadata_path: Some(cache.join("revision.state")),
        max_scan_time: Duration::from_secs(10),
        ..RevisionOptions::default()
    };
    let workspace = ManagedWorkspace::open_with_options(source, revision_options)?;
    let revision = workspace.current_revision()?.id();
    let index_started = Instant::now();
    let mut syntax_initializations = 0;
    let mut syntax = None;
    let index_options = IndexOptions {
        max_symbol_bytes: 4_096,
        ..IndexOptions::default()
    };
    let index = if request.config.syntax_initialization_permitted {
        syntax_initializations += 1;
        let mut value = SyntaxIndex::new();
        let index =
            MetadataIndex::build_with_syntax(&workspace, revision, &index_options, &mut value)?;
        syntax = Some(value);
        index
    } else {
        MetadataIndex::build_lexical(&workspace, revision, &index_options)?
    };
    let index_latency_ms = millis(index_started.elapsed());
    let revision_digest = rust_tree_digest(source, Path::new(""), &index)?;
    if revision_digest != request.source_digest {
        return Err(
            ProtocolError("worker Rust tree differs from frozen source digest".into()).into(),
        );
    }
    let query_started = Instant::now();
    let mut observations = BTreeMap::new();

    if enabled(&request.config, RetrievalSource::Lexical) {
        observations.insert(
            RetrievalSource::Lexical,
            lexical_observation(
                source,
                Path::new(""),
                &workspace,
                &index,
                &query,
                &revision_digest,
            )?,
        );
    }
    if enabled(&request.config, RetrievalSource::TreeSitter) {
        observations.insert(
            RetrievalSource::TreeSitter,
            syntax_observation(&index, &query, &revision_digest)?,
        );
    }
    if enabled(&request.config, RetrievalSource::Structural) {
        let observation = match syntax.as_mut() {
            Some(syntax) => structural_observation(
                Path::new(""),
                &workspace,
                &index,
                syntax,
                &query,
                &revision_digest,
            )?,
            None => unavailable(
                RetrievalSource::Structural,
                "kit::workspace::search::structural::search",
                &revision_digest,
                "DEPENDENCY_CLOSED_SYNTAX_DISABLED",
            )?,
        };
        observations.insert(RetrievalSource::Structural, observation);
    }
    if enabled(&request.config, RetrievalSource::Lsp) {
        observations.insert(
            RetrievalSource::Lsp,
            unavailable(
                RetrievalSource::Lsp,
                "kit::verify::lsp",
                &revision_digest,
                "BLK-14_NO_PINNED_RUST_LSP_SERVER",
            )?,
        );
    }
    if enabled(&request.config, RetrievalSource::FilesystemMetadata) {
        observations.insert(
            RetrievalSource::FilesystemMetadata,
            metadata_observation(&index, &query, &revision_digest)?,
        );
    }
    if enabled(
        &request.config,
        RetrievalSource::CargoMetadataWithoutSourceParse,
    ) {
        observations.insert(
            RetrievalSource::CargoMetadataWithoutSourceParse,
            unavailable(
                RetrievalSource::CargoMetadataWithoutSourceParse,
                "kit::workspace public API",
                &revision_digest,
                "NO_PINNED_PARSE_FREE_CARGO_METADATA_ADAPTER",
            )?,
        );
    }

    let mut structure = None;
    if enabled(&request.config, RetrievalSource::StructureGraph) {
        let (observation, graph) = graph_observation(&workspace, &index, &query, &revision_digest)?;
        observations.insert(RetrievalSource::StructureGraph, observation);
        structure = graph;
    }
    let history_paths = history_paths(&observations, &source_prefix, &index)?;
    let history = if request.config.enabled_sources.iter().any(|source| {
        matches!(
            source,
            RetrievalSource::History | RetrievalSource::GitPathHistory
        )
    }) {
        let history_options = RevisionOptions {
            metadata_path: Some(cache.join("history-revision.state")),
            max_scan_time: Duration::from_secs(10),
            ..RevisionOptions::default()
        };
        let history_workspace = ManagedWorkspace::open_with_options(&repository, history_options)?;
        let history_revision = history_workspace.current_revision()?.id();
        let history_index =
            MetadataIndex::build_lexical(&history_workspace, history_revision, &index_options)?;
        Some((history_workspace, history_index))
    } else {
        None
    };
    for source_kind in [RetrievalSource::History, RetrievalSource::GitPathHistory] {
        if enabled(&request.config, source_kind) {
            let (history_workspace, history_index) = history
                .as_ref()
                .ok_or_else(|| ProtocolError("history workspace was not initialized".into()))?;
            observations.insert(
                source_kind,
                history_observation(
                    history_workspace,
                    history_index,
                    &query,
                    &revision_digest,
                    source_kind,
                    &history_paths,
                )?,
            );
        }
    }
    if enabled(&request.config, RetrievalSource::PersonalizedMap) {
        observations.insert(
            RetrievalSource::PersonalizedMap,
            map_observation(
                &workspace,
                &index,
                structure.as_ref(),
                &query,
                &revision_digest,
            )?,
        );
    }

    let mut observations = request
        .config
        .enabled_sources
        .iter()
        .map(|source| {
            observations.remove(source).ok_or_else(|| {
                ProtocolError(format!("enabled source {source:?} produced no observation")).into()
            })
        })
        .collect::<Result<Vec<_>>>()?;
    scope_observations(&mut observations, &source_prefix)?;
    let query_latency_ms = millis(query_started.elapsed());
    let measured_ended_at = timestamp()?;
    let mut raw = RawTrial {
        schema_version: "2.0".into(),
        kind: "m005_w07_raw_arm_trial".into(),
        unit_id: request.unit_id,
        task_id: query.task_id,
        arm: request.config.arm,
        executor_evidence: request.executor_evidence,
        admission_digest: request.admission_digest,
        source_digest: request.source_digest,
        task_query_digest: query.query_digest,
        arm_config_digest: sha256(&canonical(&request.config)?),
        worker_executable_digest: request.worker_executable_digest,
        process_id: std::process::id(),
        cache_id: request.cache_id,
        measured_started_at,
        measured_ended_at,
        elapsed_ns: nanos(trial_started.elapsed()),
        index_latency_ms,
        query_latency_ms,
        token_count: 0,
        syntax_initializations,
        terminal: TrialTerminal::Complete,
        observations,
        worker_error: None,
    };
    raw.token_count = crate::grader::projected_token_count(&raw, &request.config)?;
    Ok(raw)
}

fn lexical_observation(
    repository: &Path,
    source_prefix: &Path,
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &WorkerQuery,
    revision_digest: &str,
) -> Result<SourceObservation> {
    observe(
        RetrievalSource::Lexical,
        "kit::workspace::search::lexical::search",
        revision_digest,
        || {
            let text = query_payload(&query.query);
            let response = lexical_search(
                workspace,
                index,
                &SearchQuery {
                    text,
                    mode: SearchMode::Content,
                },
                &SearchOptions {
                    path_prefixes: (!source_prefix.as_os_str().is_empty())
                        .then(|| source_prefix.to_path_buf())
                        .into_iter()
                        .collect(),
                    max_results: 10_000,
                    max_cursor_offset: 10_000,
                    max_result_bytes: 8 * 1024 * 1024,
                    max_time: Duration::from_secs(3),
                    ..SearchOptions::default()
                },
                None,
            )?;
            let candidates = response
                .matches
                .iter()
                .enumerate()
                .map(|(ordinal, found)| {
                    let (range, snippet, snippet_truncated) =
                        lexical_context(repository, &found.path, found.byte_start, found.byte_end)?;
                    Ok(RawCandidate {
                        range,
                        symbol: None,
                        snippet: snippet.clone(),
                        snippet_truncated,
                        semantics: CandidateSemantics::LexicalContext,
                        source: RetrievalSource::Lexical,
                        source_revision_digest: revision_digest.into(),
                        provenance_digest: sha256(&serde_json::to_vec(found)?),
                        raw_score_micros: i64::from(found.score) * 1_000,
                        token_overlap_micros: overlap(&query.query, &snippet),
                        response_ordinal: ordinal,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((candidates, response.truncated, None))
        },
    )
}

fn syntax_observation(
    index: &MetadataIndex,
    query: &WorkerQuery,
    revision_digest: &str,
) -> Result<SourceObservation> {
    observe(
        RetrievalSource::TreeSitter,
        "kit::workspace::index::meta::MetadataIndex::build_with_syntax",
        revision_digest,
        || {
            let mut candidates = Vec::new();
            for entry in index.entries() {
                for record in entry.syntax_records.iter() {
                    if candidates.len() == MAX_CANDIDATES {
                        return Ok((candidates, true, None));
                    }
                    let range = record.range();
                    let snippet = record.declaration().value().text().to_owned();
                    candidates.push(RawCandidate {
                        range: CandidateRange {
                            path: path_string(record.canonical_path()),
                            start_byte: range.start_byte,
                            end_byte: range.end_byte,
                            start_line: range.start_line,
                            end_line: range.end_line,
                        },
                        symbol: Some(record.display_name().value().to_string()),
                        snippet: snippet.clone(),
                        snippet_truncated: record.declaration().value().truncated(),
                        semantics: CandidateSemantics::ExactItem,
                        source: RetrievalSource::TreeSitter,
                        source_revision_digest: revision_digest.into(),
                        provenance_digest: sha256(&record.declaration_id()),
                        raw_score_micros: 500_000,
                        token_overlap_micros: overlap(&query.query, &snippet),
                        response_ordinal: candidates.len(),
                    });
                }
            }
            Ok((candidates, index.truncated(), None))
        },
    )
}

fn structural_observation(
    source_prefix: &Path,
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    syntax: &mut SyntaxIndex,
    query: &WorkerQuery,
    revision_digest: &str,
) -> Result<SourceObservation> {
    observe(
        RetrievalSource::Structural,
        "kit::workspace::search::structural::search",
        revision_digest,
        || {
            let patterns = [
                "pub fn $NAME($$$ARGS) { $$$BODY }",
                "pub struct $NAME { $$$FIELDS }",
                "pub enum $NAME { $$$VARIANTS }",
                "pub trait $NAME { $$$BODY }",
                "pub type $NAME = $TYPE;",
                "pub const $NAME: $TYPE = $VALUE;",
                "pub static $NAME: $TYPE = $VALUE;",
                "pub mod $NAME { $$$BODY }",
            ];
            let mut candidates = Vec::new();
            let mut truncated = false;
            let mut error = None;
            for pattern in patterns {
                let response = match structural_search(
                    workspace,
                    index,
                    syntax,
                    &StructuralQuery {
                        pattern: pattern.into(),
                        rewrite: None,
                    },
                    &StructuralOptions {
                        path_prefixes: (!source_prefix.as_os_str().is_empty())
                            .then(|| source_prefix.to_path_buf())
                            .into_iter()
                            .collect(),
                        max_matches: 10_000,
                        max_output_bytes: 8 * 1024 * 1024,
                        max_time: Duration::from_secs(3),
                        ..StructuralOptions::default()
                    },
                ) {
                    Ok(response) => response,
                    Err(source_error) => {
                        error = Some(source_error.to_string());
                        continue;
                    }
                };
                truncated |= response.truncated;
                for found in response.matches {
                    if candidates.len() == MAX_CANDIDATES {
                        return Ok((candidates, true, error));
                    }
                    let (snippet, snippet_truncated) = bounded_snippet(&found.text);
                    candidates.push(RawCandidate {
                        range: CandidateRange {
                            path: path_string(&found.path),
                            start_byte: found.range.start_byte,
                            end_byte: found.range.end_byte,
                            start_line: found.range.start_line,
                            end_line: found.range.end_line,
                        },
                        symbol: found
                            .captures
                            .iter()
                            .find(|capture| capture.name == "NAME")
                            .map(|capture| capture.text.clone()),
                        snippet,
                        snippet_truncated,
                        semantics: CandidateSemantics::ExactItem,
                        source: RetrievalSource::Structural,
                        source_revision_digest: revision_digest.into(),
                        provenance_digest: sha256(&serde_json::to_vec(&found)?),
                        raw_score_micros: 400_000,
                        token_overlap_micros: overlap(&query.query, &found.text),
                        response_ordinal: candidates.len(),
                    });
                }
            }
            Ok((candidates, truncated, error))
        },
    )
}

fn metadata_observation(
    index: &MetadataIndex,
    query: &WorkerQuery,
    revision_digest: &str,
) -> Result<SourceObservation> {
    observe(
        RetrievalSource::FilesystemMetadata,
        "kit::workspace::index::meta::MetadataIndex::build_lexical",
        revision_digest,
        || {
            let candidates = index
                .entries()
                .iter()
                .filter(|entry| entry.kind == EntryKind::File && entry.size > 0)
                .take(MAX_CANDIDATES)
                .enumerate()
                .map(|(ordinal, entry)| {
                    let path = path_string(&entry.path);
                    RawCandidate {
                        range: CandidateRange {
                            path: path.clone(),
                            start_byte: 0,
                            end_byte: usize::try_from(entry.size).unwrap_or(usize::MAX),
                            start_line: 1,
                            end_line: 1,
                        },
                        symbol: None,
                        snippet: path.clone(),
                        snippet_truncated: false,
                        semantics: CandidateSemantics::OtherContext,
                        source: RetrievalSource::FilesystemMetadata,
                        source_revision_digest: revision_digest.into(),
                        provenance_digest: sha256(path.as_bytes()),
                        raw_score_micros: 0,
                        token_overlap_micros: overlap(&query.query, &path),
                        response_ordinal: ordinal,
                    }
                })
                .collect::<Vec<_>>();
            Ok((candidates, index.entries().len() > MAX_CANDIDATES, None))
        },
    )
}

fn graph_observation(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &WorkerQuery,
    revision_digest: &str,
) -> Result<(SourceObservation, Option<StructureGraph>)> {
    let started_at = timestamp()?;
    let started = Instant::now();
    let mut provider = StructureGraphProvider::new();
    let options = GraphOptions {
        max_nodes: MAX_CANDIDATES,
        ..GraphOptions::default()
    };
    match provider.refresh(workspace, index, &options, &[], &[]) {
        Ok(graph) => {
            let candidates = graph
                .nodes()
                .iter()
                .filter(|node| node.kind() == NodeKind::Symbol)
                .filter_map(|node| Some((node.path()?, node.range()?, node)))
                .take(MAX_CANDIDATES)
                .enumerate()
                .map(|(ordinal, (path, range, node))| RawCandidate {
                    range: CandidateRange {
                        path: path.to_string(),
                        start_byte: range.start_byte(),
                        end_byte: range.end_byte(),
                        start_line: range.start_line(),
                        end_line: range.end_line(),
                    },
                    symbol: Some(node.name().into()),
                    snippet: node.name().into(),
                    snippet_truncated: false,
                    semantics: CandidateSemantics::ExactItem,
                    source: RetrievalSource::StructureGraph,
                    source_revision_digest: revision_digest.into(),
                    provenance_digest: sha256(&node.id().as_bytes()),
                    raw_score_micros: 300_000,
                    token_overlap_micros: overlap(&query.query, node.name()),
                    response_ordinal: ordinal,
                })
                .collect::<Vec<_>>();
            let graph = graph.clone();
            Ok((
                SourceObservation {
                    source: RetrievalSource::StructureGraph,
                    api: "kit::workspace::graph::structure::StructureGraphProvider::refresh".into(),
                    status: SourceStatus::Available,
                    started_at,
                    ended_at: timestamp()?,
                    elapsed_ns: nanos(started.elapsed()),
                    complete_candidate_count: candidates.len(),
                    candidates,
                    truncated: false,
                    source_revision_digest: revision_digest.into(),
                    error_code: None,
                    error: None,
                },
                Some(graph),
            ))
        }
        Err(_) => Ok((
            error_observation(
                RetrievalSource::StructureGraph,
                "kit::workspace::graph::structure::StructureGraphProvider::refresh",
                revision_digest,
                started_at,
                started,
                "STRUCTURE_GRAPH_ERROR",
                SourceStatus::Error,
            )?,
            None,
        )),
    }
}

fn history_observation(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &WorkerQuery,
    revision_digest: &str,
    source_kind: RetrievalSource,
    paths: &[RootRelativePath],
) -> Result<SourceObservation> {
    let started_at = timestamp()?;
    let started = Instant::now();
    let mut provider = HistoryGraphProvider::new();
    match provider.refresh(
        workspace,
        index,
        &HistoryRequest::all(paths.to_vec()),
        &HistoryOptions::default(),
    ) {
        Ok(graph) => {
            let mut candidates = graph
                .blame_hunks()
                .iter()
                .map(|hunk| {
                    let range = hunk.range();
                    let path = hunk.path().to_string();
                    RawCandidate {
                        range: CandidateRange {
                            path: path.clone(),
                            start_byte: range.start_byte(),
                            end_byte: range.end_byte(),
                            start_line: range.start_line(),
                            end_line: range.end_line(),
                        },
                        symbol: None,
                        snippet: path.clone(),
                        snippet_truncated: false,
                        semantics: CandidateSemantics::OtherContext,
                        source: source_kind,
                        source_revision_digest: revision_digest.into(),
                        provenance_digest: sha256(&hunk.evidence_digest()),
                        raw_score_micros: i64::from(hunk.confidence_millis()) * 1_000,
                        token_overlap_micros: overlap(&query.query, &path),
                        response_ordinal: 0,
                    }
                })
                .collect::<Vec<_>>();
            for change in graph.changes() {
                let path = change.current_path().unwrap_or_else(|| change.path());
                if let Some(candidate) = whole_file_candidate(
                    index,
                    path.as_str(),
                    source_kind,
                    revision_digest,
                    change.commit().as_str().as_bytes(),
                    i64::from(change.parent().is_some()) * 100_000,
                ) {
                    candidates.push(candidate);
                }
            }
            for fact in graph.changed_with() {
                for path in [fact.left(), fact.right()] {
                    if let Some(candidate) = whole_file_candidate(
                        index,
                        path.as_str(),
                        source_kind,
                        revision_digest,
                        &fact.provenance().evidence_digest(),
                        i64::from(fact.strength_millis()) * 1_000,
                    ) {
                        candidates.push(candidate);
                    }
                }
            }
            candidates.truncate(MAX_CANDIDATES);
            for (ordinal, candidate) in candidates.iter_mut().enumerate() {
                candidate.response_ordinal = ordinal;
                candidate.token_overlap_micros = overlap(&query.query, &candidate.snippet);
            }
            Ok(SourceObservation {
                source: source_kind,
                api: "kit::workspace::graph::history::HistoryGraphProvider::refresh".into(),
                status: SourceStatus::Available,
                started_at,
                ended_at: timestamp()?,
                elapsed_ns: nanos(started.elapsed()),
                complete_candidate_count: candidates.len(),
                candidates,
                truncated: graph.blame_hunks().len()
                    + graph.changes().len()
                    + graph.changed_with().len().saturating_mul(2)
                    > MAX_CANDIDATES,
                source_revision_digest: revision_digest.into(),
                error_code: None,
                error: None,
            })
        }
        Err(error) => Err(ProtocolError(format!(
            "pinned checkout did not provide W06b history: {error}"
        ))
        .into()),
    }
}

fn whole_file_candidate(
    index: &MetadataIndex,
    path: &str,
    source: RetrievalSource,
    revision_digest: &str,
    provenance: &[u8],
    score: i64,
) -> Option<RawCandidate> {
    let entry = index.entries().iter().find(|entry| {
        entry.path == Path::new(path) && entry.kind == EntryKind::File && entry.size > 0
    })?;
    Some(RawCandidate {
        range: CandidateRange {
            path: path.to_owned(),
            start_byte: 0,
            end_byte: usize::try_from(entry.size).ok()?,
            start_line: 1,
            end_line: 1,
        },
        symbol: None,
        snippet: path.to_owned(),
        snippet_truncated: false,
        semantics: CandidateSemantics::OtherContext,
        source,
        source_revision_digest: revision_digest.into(),
        provenance_digest: sha256(provenance),
        raw_score_micros: score.clamp(-1_000_000, 1_000_000),
        token_overlap_micros: 0,
        response_ordinal: 0,
    })
}

fn history_paths(
    observations: &BTreeMap<RetrievalSource, SourceObservation>,
    source_prefix: &Path,
    index: &MetadataIndex,
) -> Result<Vec<RootRelativePath>> {
    let prefix = path_string(source_prefix);
    let repository_path = |path: &str| {
        if prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{prefix}/{path}")
        }
    };
    let mut ordered = Vec::new();
    if let Some(lexical) = observations.get(&RetrievalSource::Lexical) {
        ordered.extend(
            lexical
                .candidates
                .iter()
                .map(|candidate| repository_path(&candidate.range.path)),
        );
    }
    ordered.extend(
        index
            .entries()
            .iter()
            .filter(|entry| {
                entry.kind == EntryKind::File
                    && entry.path.extension().is_some_and(|value| value == "rs")
            })
            .map(|entry| repository_path(&path_string(&entry.path))),
    );
    let mut seen = BTreeSet::new();
    ordered
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .take(MAX_HISTORY_PATHS)
        .map(|path| RootRelativePath::parse(path, 4_096).map_err(Into::into))
        .collect()
}

fn rust_tree_digest(root: &Path, prefix: &Path, index: &MetadataIndex) -> Result<String> {
    let mut rust = BTreeMap::new();
    for entry in index.entries().iter().filter(|entry| {
        entry.kind == EntryKind::File
            && entry.path.extension().is_some_and(|value| value == "rs")
            && (prefix.as_os_str().is_empty() || entry.path.starts_with(prefix))
    }) {
        let relative = entry.path.strip_prefix(prefix).unwrap_or(&entry.path);
        let bytes = read_bounded(&root.join(&entry.path), crate::MAX_SOURCE_FILE_BYTES)?;
        rust.insert(
            path_string(relative),
            format!("{:x}", sha2::Sha256::digest(&bytes)),
        );
    }
    if rust.is_empty() {
        return Err(ProtocolError("worker package contains no Rust files".into()).into());
    }
    Ok(sha256(&canonical(&rust)?))
}

fn map_observation(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    structure: Option<&StructureGraph>,
    query: &WorkerQuery,
    revision_digest: &str,
) -> Result<SourceObservation> {
    observe(
        RetrievalSource::PersonalizedMap,
        "kit::workspace::map::build_repository_map",
        revision_digest,
        || {
            let request = RepositoryMapRequest {
                personalization: Personalization {
                    task_terms: tokens(&query.query).into_iter().collect(),
                    ..Personalization::default()
                },
                budget: MapBudget {
                    max_estimated_tokens: 2_048,
                    ..MapBudget::default()
                },
                languages: vec!["rust".into()],
                ..RepositoryMapRequest::default()
            };
            let response = if let Some(graph) = structure {
                build_repository_map_with_structure(
                    workspace,
                    index,
                    &request,
                    &[],
                    MapLimits::default(),
                    None,
                    Some(graph),
                )?
            } else {
                build_repository_map(workspace, index, &request, &[], MapLimits::default(), None)?
            };
            let value: Value = serde_json::from_slice(&response.to_canonical_json()?)?;
            let candidates = map_candidates(&value, query, revision_digest)?;
            Ok((candidates, response.truncated(), None))
        },
    )
}

fn map_candidates(
    value: &Value,
    query: &WorkerQuery,
    revision_digest: &str,
) -> Result<Vec<RawCandidate>> {
    value
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_CANDIDATES)
        .enumerate()
        .map(|(ordinal, entry)| {
            let range = entry
                .get("source_range")
                .ok_or_else(|| ProtocolError("map entry lacks source range".into()))?;
            let path = json_string(entry, "path")?;
            let (snippet, snippet_truncated) =
                bounded_snippet(&json_string(entry, "signature").unwrap_or_default());
            Ok(RawCandidate {
                range: CandidateRange {
                    path,
                    start_byte: json_usize(range, "start_byte")?,
                    end_byte: json_usize(range, "end_byte")?,
                    start_line: json_usize(range, "start_line")?,
                    end_line: json_usize(range, "end_line")?,
                },
                symbol: Some(json_string(entry, "display_name")?),
                snippet: snippet.clone(),
                snippet_truncated,
                semantics: CandidateSemantics::ExactItem,
                source: RetrievalSource::PersonalizedMap,
                source_revision_digest: revision_digest.into(),
                provenance_digest: sha256(&serde_json::to_vec(entry)?),
                raw_score_micros: 1_000_000_i64
                    .saturating_sub(i64::try_from(json_usize(entry, "rank")?).unwrap_or(i64::MAX))
                    .clamp(-1_000_000, 1_000_000),
                token_overlap_micros: overlap(&query.query, &snippet),
                response_ordinal: ordinal,
            })
        })
        .collect()
}

fn observe<F>(
    source: RetrievalSource,
    api: &str,
    revision_digest: &str,
    action: F,
) -> Result<SourceObservation>
where
    F: FnOnce() -> Result<(Vec<RawCandidate>, bool, Option<String>)>,
{
    let started_at = timestamp()?;
    let started = Instant::now();
    match action() {
        Ok((candidates, truncated, error)) => {
            if candidates.len() > MAX_CANDIDATES {
                return Err(ProtocolError("source candidate bound exceeded".into()).into());
            }
            Ok(SourceObservation {
                source,
                api: api.into(),
                status: SourceStatus::Available,
                started_at,
                ended_at: timestamp()?,
                elapsed_ns: nanos(started.elapsed()),
                complete_candidate_count: candidates.len(),
                candidates,
                truncated,
                source_revision_digest: revision_digest.into(),
                error_code: None,
                error,
            })
        }
        Err(_) => error_observation(
            source,
            api,
            revision_digest,
            started_at,
            started,
            "KIT_API_ERROR",
            SourceStatus::Error,
        ),
    }
}

fn unavailable(
    source: RetrievalSource,
    api: &str,
    revision_digest: &str,
    code: &str,
) -> Result<SourceObservation> {
    let at = timestamp()?;
    Ok(SourceObservation {
        source,
        api: api.into(),
        status: SourceStatus::TerminalUnavailable,
        started_at: at.clone(),
        ended_at: at,
        elapsed_ns: 0,
        complete_candidate_count: 0,
        candidates: Vec::new(),
        truncated: false,
        source_revision_digest: revision_digest.into(),
        error_code: Some(code.into()),
        error: Some(code.into()),
    })
}

#[allow(clippy::too_many_arguments)]
fn error_observation(
    source: RetrievalSource,
    api: &str,
    revision_digest: &str,
    started_at: String,
    started: Instant,
    code: &str,
    status: SourceStatus,
) -> Result<SourceObservation> {
    Ok(SourceObservation {
        source,
        api: api.into(),
        status,
        started_at,
        ended_at: timestamp()?,
        elapsed_ns: nanos(started.elapsed()),
        complete_candidate_count: 0,
        candidates: Vec::new(),
        truncated: false,
        source_revision_digest: revision_digest.into(),
        error_code: Some(code.into()),
        error: Some(code.into()),
    })
}

fn enabled(config: &ArmConfig, source: RetrievalSource) -> bool {
    config.enabled_sources.contains(&source)
}

fn query_payload(query: &str) -> String {
    query
        .split_once('"')
        .and_then(|(_, value)| value.rsplit_once('"').map(|(value, _)| value))
        .filter(|value| !value.is_empty())
        .unwrap_or(query)
        .to_owned()
}

fn repository_root(source: &Path, git_metadata_root: &Path) -> Result<std::path::PathBuf> {
    let source = source.canonicalize()?;
    crate::reject_symlink_components(git_metadata_root, false)?;
    let git_metadata_root = git_metadata_root.canonicalize()?;
    if !git_metadata_root.is_dir() {
        return Err(ProtocolError("worker Git metadata root is not a directory".into()).into());
    }
    for ancestor in source.ancestors() {
        let dot_git = ancestor.join(".git");
        let Ok(metadata) = fs::symlink_metadata(&dot_git) else {
            continue;
        };
        if metadata.file_type().is_dir() && dot_git.canonicalize()? == git_metadata_root {
            return Ok(ancestor.to_path_buf());
        }
        if metadata.file_type().is_file() {
            let pointer = String::from_utf8(read_bounded(&dot_git, 4096)?)?;
            let target = pointer
                .strip_suffix('\n')
                .and_then(|value| value.strip_prefix("gitdir: "))
                .filter(|value| !value.is_empty() && !value.contains(['\n', '\r']))
                .map(Path::new)
                .filter(|path| path.is_absolute())
                .ok_or_else(|| ProtocolError("invalid linked-worktree Git pointer".into()))?;
            crate::reject_symlink_components(target, false)?;
            let canonical_target = target.canonicalize()?;
            if target != canonical_target
                || canonical_target == git_metadata_root
                || !canonical_target.starts_with(&git_metadata_root)
                || !canonical_target.is_dir()
            {
                return Err(ProtocolError(
                    "linked-worktree Git pointer escapes metadata root".into(),
                )
                .into());
            }
            return Ok(ancestor.to_path_buf());
        }
    }
    Err(ProtocolError("worker source is not inside a genuine Git checkout".into()).into())
}

fn validate_filtered_repository(repository: &Path, source_prefix: &Path) -> Result<()> {
    let mut pending = vec![repository.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)?.collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries.into_iter().rev() {
            let path = entry.path();
            let relative = path.strip_prefix(repository)?;
            if relative
                .components()
                .next()
                .is_some_and(|component| component.as_os_str() == ".git")
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(ProtocolError("symlink in worker source is forbidden".into()).into());
            }
            let in_package = source_prefix.as_os_str().is_empty()
                || relative.starts_with(source_prefix)
                || source_prefix.starts_with(relative);
            if !in_package {
                return Err(ProtocolError(
                    "worker repository contains sibling worktree source".into(),
                )
                .into());
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if !metadata.is_file() {
                return Err(ProtocolError("worker source entry is not regular".into()).into());
            }
        }
    }
    Ok(())
}

fn scope_observations(observations: &mut [SourceObservation], prefix: &Path) -> Result<()> {
    if prefix.as_os_str().is_empty() {
        return Ok(());
    }
    for observation in observations {
        if !matches!(
            observation.source,
            RetrievalSource::History | RetrievalSource::GitPathHistory
        ) {
            continue;
        }
        observation.candidates.retain_mut(|candidate| {
            let path = Path::new(&candidate.range.path);
            let Ok(relative) = path.strip_prefix(prefix) else {
                return false;
            };
            if relative.as_os_str().is_empty() {
                return false;
            }
            candidate.range.path = path_string(relative);
            true
        });
        observation.complete_candidate_count = observation.candidates.len();
    }
    Ok(())
}

fn lexical_context(
    root: &Path,
    path: &Path,
    match_start: usize,
    match_end: usize,
) -> Result<(CandidateRange, String, bool)> {
    let bytes = read_bounded(&root.join(path), crate::MAX_SOURCE_FILE_BYTES)?;
    let text = std::str::from_utf8(&bytes)?;
    if match_start >= match_end || match_end > text.len() {
        return Err(ProtocolError("lexical API returned an invalid match range".into()).into());
    }
    let line_starts = std::iter::once(0)
        .chain(
            text.bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
        )
        .collect::<Vec<_>>();
    let mut start = match_start.saturating_sub(LEXICAL_CONTEXT_BYTES);
    let mut end = match_end
        .saturating_add(LEXICAL_CONTEXT_BYTES)
        .min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let start_line = line_starts.partition_point(|line| *line <= start);
    let end_line = line_starts
        .partition_point(|line| *line < end)
        .max(start_line);
    Ok((
        CandidateRange {
            path: path_string(path),
            start_byte: start,
            end_byte: end,
            start_line,
            end_line,
        },
        text[start..end].to_owned(),
        start != 0 || end != text.len(),
    ))
}

fn overlap(query: &str, candidate: &str) -> i64 {
    let query = tokens(query);
    let candidate = tokens(candidate);
    let common = query.intersection(&candidate).count() as i64;
    if query.is_empty() {
        0
    } else {
        common.saturating_mul(100_000) / query.len() as i64
    }
}

fn tokens(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| token.len() > 1)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn json_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ProtocolError(format!("map field {key} is not a string")).into())
}

fn json_usize(value: &Value, key: &str) -> Result<usize> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| ProtocolError(format!("map field {key} is not bounded integer")).into())
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn bounded_snippet(value: &str) -> (String, bool) {
    if value.len() <= 4_096 {
        return (value.to_owned(), false);
    }
    let mut end = 4_096;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

fn timestamp() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, maximum: u64) -> Result<T> {
    let bytes = read_bounded(path, maximum)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    crate::reject_symlink_components(path, false)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > maximum {
        return Err(ProtocolError("invalid bounded worker JSON input".into()).into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref().take(maximum + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != metadata.len()
        || after.len() != metadata.len()
        || after.modified()? != metadata.modified()?
    {
        return Err(ProtocolError("worker JSON changed while read".into()).into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arm;
    use std::fs;

    #[test]
    fn syntax_free_arms_are_dependency_closed() {
        for arm in [Arm::L, Arm::FS] {
            let config = ArmConfig::frozen(arm);
            assert!(!config.syntax_initialization_permitted);
            assert!(!config.enabled_sources.iter().any(|source| matches!(
                source,
                RetrievalSource::TreeSitter
                    | RetrievalSource::Structural
                    | RetrievalSource::Lsp
                    | RetrievalSource::StructureGraph
            )));
        }
        for arm in [Arm::C, Arm::F, Arm::FP, Arm::FG, Arm::FH] {
            assert!(ArmConfig::frozen(arm).syntax_initialization_permitted);
        }
    }

    #[test]
    fn normalization_is_integer_and_deterministic() {
        assert_eq!(query_payload("Locate: \"Alpha beta.\""), "Alpha beta.");
        assert_eq!(overlap("alpha beta", "BETA gamma"), 50_000);
    }

    #[test]
    fn linked_worktree_pointer_cannot_escape_allowed_metadata_root() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-w07-worker-pointer-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let allowed = root.join("allowed");
        let arbitrary = root.join("arbitrary");
        let source = root.join("source");
        for directory in [&allowed, &arbitrary, &source] {
            fs::create_dir_all(directory).unwrap();
        }
        let git = crate::run::preregistration_git().unwrap();
        crate::run::trusted_git_status(&git, &allowed, &["init"]).unwrap();
        crate::run::trusted_git_status(&git, &arbitrary, &["init"]).unwrap();
        fs::write(
            source.join(".git"),
            format!(
                "gitdir: {}\n",
                arbitrary.join(".git").canonicalize().unwrap().display()
            ),
        )
        .unwrap();
        assert!(repository_root(&source, &allowed.join(".git")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tiny_fixture_executes_exact_source_set_for_every_arm() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-w07-worker-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let upstream = root.join("upstream");
        let repository = root.join("repository");
        let upstream_source = upstream.join("crate");
        let source = repository.join("crate");
        fs::create_dir_all(upstream_source.join("src")).unwrap();
        fs::write(
            upstream_source.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(
            upstream_source.join("src/lib.rs"),
            "/// Finds alpha.\npub fn alpha() {}\n",
        )
        .unwrap();
        let git = crate::run::preregistration_git().unwrap();
        crate::run::trusted_git_status(&git, &upstream, &["init"]).unwrap();
        crate::run::trusted_git_status(&git, &upstream, &["add", "."]).unwrap();
        crate::run::trusted_git_status(
            &git,
            &upstream,
            &[
                "-c",
                "user.name=W07 Test",
                "-c",
                "user.email=w07@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        )
        .unwrap();
        crate::run::trusted_git_status(
            &git,
            &upstream,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                repository.to_str().unwrap(),
                "HEAD",
            ],
        )
        .unwrap();
        let git_metadata_root = upstream.join(".git").canonicalize().unwrap();
        let query_text = "Locate the public Rust item documented as: \"Finds alpha.\"";
        let source_digest = sha256(
            &canonical(&BTreeMap::from([(
                "src/lib.rs".to_owned(),
                format!(
                    "{:x}",
                    sha2::Sha256::digest(b"/// Finds alpha.\npub fn alpha() {}\n")
                ),
            )]))
            .unwrap(),
        );
        for arm in [Arm::L, Arm::C, Arm::F, Arm::FS, Arm::FP, Arm::FG, Arm::FH] {
            let cache = root.join(format!("cache-{arm:?}"));
            fs::create_dir(&cache).unwrap();
            let config = ArmConfig::frozen(arm);
            let raw = execute(
                &source,
                &git_metadata_root,
                &cache,
                WorkerQuery {
                    task_id: "task".into(),
                    query: query_text.into(),
                    query_digest: sha256(query_text.as_bytes()),
                },
                WorkerArmRequest {
                    unit_id: "unit".into(),
                    source_digest: source_digest.clone(),
                    admission_digest:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .into(),
                    executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
                    cache_id: format!("cache-{arm:?}"),
                    worker_executable_digest:
                        "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                            .into(),
                    config: config.clone(),
                },
            )
            .unwrap();
            assert_eq!(
                raw.observations
                    .iter()
                    .map(|item| item.source)
                    .collect::<Vec<_>>(),
                config.enabled_sources
            );
            assert_eq!(
                raw.syntax_initializations,
                usize::from(config.syntax_initialization_permitted)
            );
            let lexical = raw
                .observations
                .iter()
                .find(|observation| observation.source == RetrievalSource::Lexical)
                .unwrap();
            assert!(lexical.candidates.iter().any(|candidate| {
                candidate.range.path == "src/lib.rs"
                    && candidate.range.start_byte <= 17
                    && 17 < candidate.range.end_byte
                    && candidate.snippet.contains("pub fn alpha")
            }));
            if matches!(arm, Arm::F | Arm::FP | Arm::FG | Arm::FS) {
                let history = raw.observations.iter().find(|observation| {
                    matches!(
                        observation.source,
                        RetrievalSource::History | RetrievalSource::GitPathHistory
                    )
                });
                assert!(history.is_some_and(|observation| !observation.candidates.is_empty()));
            }
            assert_eq!(
                raw.observations
                    .iter()
                    .any(|observation| observation.source == RetrievalSource::History),
                matches!(arm, Arm::F | Arm::FP | Arm::FG)
            );
        }
        fs::write(repository.join("sibling.rs"), "pub fn unpublished() {}\n").unwrap();
        let cache = root.join("cache-sibling");
        fs::create_dir(&cache).unwrap();
        assert!(
            execute(
                &source,
                &git_metadata_root,
                &cache,
                WorkerQuery {
                    task_id: "task".into(),
                    query: query_text.into(),
                    query_digest: sha256(query_text.as_bytes()),
                },
                WorkerArmRequest {
                    unit_id: "unit".into(),
                    source_digest,
                    admission_digest:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .into(),
                    executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
                    cache_id: "cache-sibling".into(),
                    worker_executable_digest:
                        "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                            .into(),
                    config: ArmConfig::frozen(Arm::L),
                },
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
