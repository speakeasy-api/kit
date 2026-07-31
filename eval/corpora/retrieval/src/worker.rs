use crate::{
    ArmConfig, CandidateRange, CandidateSemantics, ExecutorEvidence, ProtocolError, RawCandidate,
    RawTrial, Result, RetrievalSource, SourceObservation, SourceStatus, TrialTerminal,
    WorkerArmRequest, WorkerQuery, canonical, sha256,
};
use kit::workspace::{
    edit::ir::RootRelativePath,
    graph::{
        history::{
            HistoryBound, HistoryError, HistoryGraph, HistoryGraphProvider, HistoryOptions,
            HistoryRequest,
        },
        structure::{
            GraphBound, GraphError, GraphOptions, NodeKind, StructureGraph, StructureGraphProvider,
        },
    },
    index::meta::{IndexOptions, MetadataIndex},
    map::{
        MapBudget, MapError, MapLimits, Personalization, RepositoryMapRequest,
        build_repository_map, build_repository_map_with_structure,
    },
    revision::{EntryKind, LimitKind, ManagedWorkspace, RevisionError, RevisionOptions},
    search::{
        lexical::{SearchError, SearchMode, SearchOptions, SearchQuery, search as lexical_search},
        structural::{
            StructuralError, StructuralOptions, StructuralQuery, search as structural_search,
        },
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
        || !Path::new(&request.git_path).is_absolute()
        || !crate::valid_digest(&request.git_executable_digest)
        || request.git_version.is_empty()
        || request.git_version.len() > 4096
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
        max_indexed_bytes: crate::MAX_SNAPSHOT_BYTES,
        max_file_bytes: crate::MAX_SOURCE_FILE_BYTES,
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
    let primary_index_elapsed = index_started.elapsed();
    let revision_digest = rust_tree_digest(source, Path::new(""), &index)?;
    if revision_digest != request.source_digest {
        return Err(
            ProtocolError("worker Rust tree differs from frozen source digest".into()).into(),
        );
    }
    let history_enabled = request.config.enabled_sources.iter().any(|source| {
        matches!(
            source,
            RetrievalSource::History | RetrievalSource::GitPathHistory
        )
    });
    let (history_owner, nested_index_elapsed) = if history_enabled
        && !source_prefix.as_os_str().is_empty()
    {
        let nested_started = Instant::now();
        let history_options = RevisionOptions {
            metadata_path: Some(cache.join("history-revision.state")),
            max_scan_time: Duration::from_secs(10),
            ..RevisionOptions::default()
        };
        let history_workspace = ManagedWorkspace::open_with_options(&repository, history_options)?;
        let history_revision = history_workspace.current_revision()?.id();
        let history_index =
            MetadataIndex::build_lexical(&history_workspace, history_revision, &index_options)?;
        (
            Some((history_workspace, history_index)),
            Some(nested_started.elapsed()),
        )
    } else {
        (None, None)
    };
    let index_latency_ms = combined_index_latency_ms(primary_index_elapsed, nested_index_elapsed);
    let query_started = Instant::now();
    let source_limits = request.config.source_limits;
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
                query_started,
                source_limits,
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
                query_started,
                source_limits,
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
        let (observation, graph) = graph_observation(
            &workspace,
            &index,
            &query,
            &revision_digest,
            query_started,
            source_limits,
        )?;
        observations.insert(RetrievalSource::StructureGraph, observation);
        structure = graph;
    }
    if history_enabled {
        let paths = history_paths(&observations, &source_prefix, &index)?;
        if let Some((history_workspace, history_index)) = &history_owner {
            insert_history_observations(
                &mut observations,
                history_workspace,
                history_index,
                &query,
                &revision_digest,
                &paths,
                &request,
                query_started,
            )?;
        } else {
            insert_history_observations(
                &mut observations,
                &workspace,
                &index,
                &query,
                &revision_digest,
                &paths,
                &request,
                query_started,
            )?;
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
                query_started,
                source_limits,
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
        repository_class: request.repository_class,
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

#[allow(clippy::too_many_arguments)]
fn lexical_observation(
    repository: &Path,
    source_prefix: &Path,
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &WorkerQuery,
    revision_digest: &str,
    query_started: Instant,
    limits: crate::SourceLimits,
) -> Result<SourceObservation> {
    observe(
        RetrievalSource::Lexical,
        "kit::workspace::search::lexical::search",
        revision_digest,
        || {
            let max_time = source_duration(query_started, limits, limits.lexical_ms)
                .ok_or_else(|| typed_failure(RetrievalSource::Lexical, FailureKind::Time))?;
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
                    max_time,
                    ..SearchOptions::default()
                },
                None,
            )
            .map_err(lexical_failure)?;
            let candidates = response
                .matches
                .iter()
                .enumerate()
                .map(|(ordinal, found)| {
                    let (range, snippet, snippet_truncated) =
                        lexical_context(repository, &found.path, found.byte_start, found.byte_end)
                            .map_err(|_| {
                                typed_failure(
                                    RetrievalSource::Lexical,
                                    FailureKind::InvalidContract,
                                )
                            })?;
                    Ok(RawCandidate {
                        range,
                        symbol: None,
                        snippet: snippet.clone(),
                        snippet_truncated,
                        semantics: CandidateSemantics::LexicalContext,
                        source: RetrievalSource::Lexical,
                        source_revision_digest: revision_digest.into(),
                        provenance_digest: sha256(&serde_json::to_vec(found).map_err(|_| {
                            typed_failure(RetrievalSource::Lexical, FailureKind::InvalidContract)
                        })?),
                        raw_score_micros: i64::from(found.score) * 1_000,
                        token_overlap_micros: overlap(&query.query, &snippet),
                        response_ordinal: ordinal,
                    })
                })
                .collect::<std::result::Result<Vec<_>, SourceFailure>>()?;
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
                    let declaration = record.declaration().value();
                    let (snippet, bounded) = bounded_snippet(declaration.text());
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
                        snippet_truncated: declaration.truncated() || bounded,
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

#[allow(clippy::too_many_arguments)]
fn structural_observation(
    source_prefix: &Path,
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    syntax: &mut SyntaxIndex,
    query: &WorkerQuery,
    revision_digest: &str,
    query_started: Instant,
    limits: crate::SourceLimits,
) -> Result<SourceObservation> {
    let mut attempted_pattern_count = 0;
    let mut successful_pattern_count = 0;
    let mut failures = Vec::new();
    let structural_started = Instant::now();
    let observation = observe(
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
            for pattern in patterns {
                attempted_pattern_count += 1;
                let max_time = source_duration(query_started, limits, limits.structural_pattern_ms)
                    .and_then(|duration| {
                        remaining_duration(structural_started, limits.structural_total_ms)
                            .map(|remaining| duration.min(remaining))
                    });
                let Some(max_time) = max_time else {
                    failures.push(typed_failure(
                        RetrievalSource::Structural,
                        FailureKind::Time,
                    ));
                    continue;
                };
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
                        max_time,
                        ..StructuralOptions::default()
                    },
                ) {
                    Ok(response) => response,
                    Err(error) => {
                        failures.push(structural_failure(error));
                        continue;
                    }
                };
                successful_pattern_count += 1;
                truncated |= response.truncated;
                for found in response.matches {
                    if candidates.len() == MAX_CANDIDATES {
                        truncated = true;
                        continue;
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
                        provenance_digest: sha256(&serde_json::to_vec(&found).map_err(|_| {
                            typed_failure(RetrievalSource::Structural, FailureKind::InvalidContract)
                        })?),
                        raw_score_micros: 400_000,
                        token_overlap_micros: overlap(&query.query, &found.text),
                        response_ordinal: candidates.len(),
                    });
                }
            }
            Ok((candidates, truncated, None))
        },
    )?;
    Ok(finalize_structural_observation(
        observation,
        attempted_pattern_count,
        successful_pattern_count,
        &failures,
    ))
}

fn finalize_structural_observation(
    mut observation: SourceObservation,
    attempted_pattern_count: usize,
    successful_pattern_count: usize,
    failures: &[SourceFailure],
) -> SourceObservation {
    observation.attempted_pattern_count = attempted_pattern_count;
    observation.successful_pattern_count = successful_pattern_count;
    if observation.status != SourceStatus::Available {
        return observation;
    }
    if successful_pattern_count == 0 && attempted_pattern_count > 0 {
        observation.status = SourceStatus::Error;
        observation.candidates.clear();
        observation.complete_candidate_count = 0;
        observation.truncated = false;
        let failure = combined_structural_failure(failures);
        observation.error_code = Some(failure.code);
        observation.error = Some(format!(
            "all {attempted_pattern_count} structural patterns failed: {}",
            failure.message
        ));
    } else if successful_pattern_count < attempted_pattern_count {
        let failure = combined_structural_failure(failures);
        observation.error_code = Some(failure.code);
        observation.error = Some(format!(
            "{} of {attempted_pattern_count} structural patterns failed: {}",
            attempted_pattern_count - successful_pattern_count,
            failure.message
        ));
    }
    observation
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
                    let (snippet, snippet_truncated) = bounded_snippet(&path);
                    RawCandidate {
                        range: CandidateRange {
                            path: path.clone(),
                            start_byte: 0,
                            end_byte: usize::try_from(entry.size).unwrap_or(usize::MAX),
                            start_line: 1,
                            end_line: 1,
                        },
                        symbol: None,
                        snippet,
                        snippet_truncated,
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
    query_started: Instant,
    limits: crate::SourceLimits,
) -> Result<(SourceObservation, Option<StructureGraph>)> {
    let started_at = timestamp()?;
    let started = Instant::now();
    let mut provider = StructureGraphProvider::new();
    let max_time = source_duration(query_started, limits, limits.graph_ms);
    let response = max_time
        .ok_or_else(|| typed_failure(RetrievalSource::StructureGraph, FailureKind::Time))
        .and_then(|max_time| {
            provider
                .refresh(
                    workspace,
                    index,
                    &GraphOptions {
                        max_nodes: MAX_CANDIDATES,
                        max_time,
                        ..GraphOptions::default()
                    },
                    &[],
                    &[],
                )
                .map_err(graph_failure)
        });
    match response {
        Ok(graph) => {
            let candidates = graph
                .nodes()
                .iter()
                .filter(|node| node.kind() == NodeKind::Symbol)
                .filter_map(|node| Some((node.path()?, node.range()?, node)))
                .take(MAX_CANDIDATES)
                .enumerate()
                .map(|(ordinal, (path, range, node))| {
                    let (snippet, snippet_truncated) = bounded_snippet(node.name());
                    RawCandidate {
                        range: CandidateRange {
                            path: path.to_string(),
                            start_byte: range.start_byte(),
                            end_byte: range.end_byte(),
                            start_line: range.start_line(),
                            end_line: range.end_line(),
                        },
                        symbol: Some(node.name().into()),
                        snippet,
                        snippet_truncated,
                        semantics: CandidateSemantics::ExactItem,
                        source: RetrievalSource::StructureGraph,
                        source_revision_digest: revision_digest.into(),
                        provenance_digest: sha256(&node.id().as_bytes()),
                        raw_score_micros: 300_000,
                        token_overlap_micros: overlap(&query.query, node.name()),
                        response_ordinal: ordinal,
                    }
                })
                .collect::<Vec<_>>();
            let graph = graph.clone();
            Ok((
                SourceObservation {
                    source: RetrievalSource::StructureGraph,
                    api: "kit::workspace::graph::structure::StructureGraphProvider::refresh".into(),
                    status: SourceStatus::Available,
                    attempted_pattern_count: 0,
                    successful_pattern_count: 0,
                    started_at,
                    ended_at: timestamp()?,
                    elapsed_ns: nanos(started.elapsed()),
                    complete_candidate_count: candidates.len(),
                    candidates,
                    truncated: false,
                    source_revision_digest: revision_digest.into(),
                    git_executable_digest: None,
                    error_code: None,
                    error: None,
                },
                Some(graph),
            ))
        }
        Err(failure) => Ok((
            error_observation(
                RetrievalSource::StructureGraph,
                "kit::workspace::graph::structure::StructureGraphProvider::refresh",
                revision_digest,
                started_at,
                started,
                failure,
            )?,
            None,
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn history_observation(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &WorkerQuery,
    revision_digest: &str,
    source_kind: RetrievalSource,
    paths: &[RootRelativePath],
    request: &WorkerArmRequest,
    query_started: Instant,
) -> Result<SourceObservation> {
    let started_at = timestamp()?;
    let started = Instant::now();
    let mut provider = HistoryGraphProvider::new();
    let max_time = source_duration(
        query_started,
        request.config.source_limits,
        request.config.source_limits.history_ms,
    );
    let response = max_time
        .ok_or_else(|| typed_failure(source_kind, FailureKind::Time))
        .and_then(|max_time| {
            provider
                .refresh(
                    workspace,
                    index,
                    &HistoryRequest::all(paths.to_vec()),
                    &HistoryOptions {
                        max_time,
                        ..HistoryOptions::default()
                    },
                )
                .map_err(|error| history_failure(source_kind, error))
        });
    match response {
        Ok(graph) => {
            let git_executable_digest = validate_history_git(graph, request)?;
            let mut candidates = graph
                .blame_hunks()
                .iter()
                .map(|hunk| {
                    let range = hunk.range();
                    let path = hunk.path().to_string();
                    let (snippet, snippet_truncated) = bounded_snippet(&path);
                    RawCandidate {
                        range: CandidateRange {
                            path: path.clone(),
                            start_byte: range.start_byte(),
                            end_byte: range.end_byte(),
                            start_line: range.start_line(),
                            end_line: range.end_line(),
                        },
                        symbol: None,
                        snippet,
                        snippet_truncated,
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
                attempted_pattern_count: 0,
                successful_pattern_count: 0,
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
                git_executable_digest: Some(git_executable_digest),
                error_code: None,
                error: None,
            })
        }
        Err(failure) => error_observation(
            source_kind,
            "kit::workspace::graph::history::HistoryGraphProvider::refresh",
            revision_digest,
            started_at,
            started,
            failure,
        ),
    }
}

fn validate_history_git(graph: &HistoryGraph, request: &WorkerArmRequest) -> Result<String> {
    let selected = kit::workspace::acquire::trusted_git_executable()?;
    let implementation = graph.git_implementation();
    let bytes = read_bounded(&selected, 128 << 20)?;
    let executable_digest = sha256(&bytes);
    let version = request
        .git_version
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if Path::new(implementation.executable()) != selected
        || Path::new(&request.git_path) != selected
        || implementation.executable_digest() != *blake3::hash(&bytes).as_bytes()
        || implementation.version_digest() != *blake3::hash(version.as_bytes()).as_bytes()
        || executable_digest != request.git_executable_digest
    {
        return Err(ProtocolError("W06b history Git implementation pin mismatch".into()).into());
    }
    Ok(executable_digest)
}

#[allow(clippy::too_many_arguments)]
fn insert_history_observations(
    observations: &mut BTreeMap<RetrievalSource, SourceObservation>,
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &WorkerQuery,
    revision_digest: &str,
    paths: &[RootRelativePath],
    request: &WorkerArmRequest,
    query_started: Instant,
) -> Result<()> {
    for source in [RetrievalSource::History, RetrievalSource::GitPathHistory] {
        if enabled(&request.config, source) {
            observations.insert(
                source,
                history_observation(
                    workspace,
                    index,
                    query,
                    revision_digest,
                    source,
                    paths,
                    request,
                    query_started,
                )?,
            );
        }
    }
    Ok(())
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
    let (snippet, snippet_truncated) = bounded_snippet(path);
    Some(RawCandidate {
        range: CandidateRange {
            path: path.to_owned(),
            start_byte: 0,
            end_byte: usize::try_from(entry.size).ok()?,
            start_line: 1,
            end_line: 1,
        },
        symbol: None,
        snippet,
        snippet_truncated,
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
    query_started: Instant,
    limits: crate::SourceLimits,
) -> Result<SourceObservation> {
    observe(
        RetrievalSource::PersonalizedMap,
        "kit::workspace::map::build_repository_map",
        revision_digest,
        || {
            let max_time =
                source_duration(query_started, limits, limits.map_ms).ok_or_else(|| {
                    typed_failure(RetrievalSource::PersonalizedMap, FailureKind::Time)
                })?;
            let request = RepositoryMapRequest {
                personalization: Personalization {
                    task_terms: tokens(&query.query).into_iter().collect(),
                    ..Personalization::default()
                },
                budget: MapBudget {
                    max_items: 10_000,
                    max_estimated_tokens: 1_000_000,
                    max_hops: 32,
                    max_degree: 10_000,
                    max_result_bytes: 4 * 1024 * 1024,
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
                    MapLimits {
                        max_time,
                        ..MapLimits::default()
                    },
                    None,
                    Some(graph),
                )
                .map_err(map_failure)?
            } else {
                build_repository_map(
                    workspace,
                    index,
                    &request,
                    &[],
                    MapLimits {
                        max_time,
                        ..MapLimits::default()
                    },
                    None,
                )
                .map_err(map_failure)?
            };
            let bytes = response.to_canonical_json().map_err(|_| {
                typed_failure(
                    RetrievalSource::PersonalizedMap,
                    FailureKind::InvalidContract,
                )
            })?;
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
                typed_failure(
                    RetrievalSource::PersonalizedMap,
                    FailureKind::InvalidContract,
                )
            })?;
            let candidates = map_candidates(&value, query, revision_digest).map_err(|_| {
                typed_failure(
                    RetrievalSource::PersonalizedMap,
                    FailureKind::InvalidContract,
                )
            })?;
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

#[derive(Clone, Copy)]
enum FailureKind {
    Time,
    Bound,
    InvalidRequest,
    InvalidIndex,
    InvalidContract,
}

#[derive(Clone)]
struct SourceFailure {
    code: String,
    message: String,
}

fn typed_failure(source: RetrievalSource, kind: FailureKind) -> SourceFailure {
    let prefix = match source {
        RetrievalSource::Lexical => "LEXICAL",
        RetrievalSource::Structural => "STRUCTURAL",
        RetrievalSource::PersonalizedMap => "MAP",
        RetrievalSource::StructureGraph => "GRAPH",
        RetrievalSource::History => "HISTORY",
        RetrievalSource::GitPathHistory => "GIT_PATH_HISTORY",
        _ => "SOURCE",
    };
    let (suffix, message) = match kind {
        FailureKind::Time => ("TIME_LIMIT", "source API time limit exceeded"),
        FailureKind::Bound => ("BOUND_EXCEEDED", "source API deterministic bound exceeded"),
        FailureKind::InvalidRequest => ("INVALID_REQUEST", "source API request is invalid"),
        FailureKind::InvalidIndex => ("INVALID_INDEX", "source API index is invalid or stale"),
        FailureKind::InvalidContract => (
            "INVALID_CONTRACT",
            "source API response or execution contract is invalid",
        ),
    };
    SourceFailure {
        code: format!("{prefix}_{suffix}"),
        message: message.into(),
    }
}

fn revision_failure(source: RetrievalSource, error: RevisionError) -> SourceFailure {
    match error {
        RevisionError::LimitExceeded(LimitKind::Time) => typed_failure(source, FailureKind::Time),
        RevisionError::LimitExceeded(_) => typed_failure(source, FailureKind::Bound),
        _ => typed_failure(source, FailureKind::InvalidIndex),
    }
}

fn lexical_failure(error: SearchError) -> SourceFailure {
    match error {
        SearchError::TimeLimit => typed_failure(RetrievalSource::Lexical, FailureKind::Time),
        SearchError::InvalidQuery(_) => {
            typed_failure(RetrievalSource::Lexical, FailureKind::InvalidRequest)
        }
        SearchError::Revision(error) => revision_failure(RetrievalSource::Lexical, error),
        SearchError::InvalidOptions(_)
        | SearchError::CursorMismatch
        | SearchError::Serialization(_) => {
            typed_failure(RetrievalSource::Lexical, FailureKind::InvalidContract)
        }
    }
}

fn structural_failure(error: StructuralError) -> SourceFailure {
    match error {
        StructuralError::TimeLimit => typed_failure(RetrievalSource::Structural, FailureKind::Time),
        StructuralError::InvalidQuery(_) => {
            typed_failure(RetrievalSource::Structural, FailureKind::InvalidRequest)
        }
        StructuralError::Revision(error) => revision_failure(RetrievalSource::Structural, error),
        StructuralError::InvalidOptions(_)
        | StructuralError::MalformedSource(_)
        | StructuralError::IncompleteRewrite(_)
        | StructuralError::AmbiguousRewrite(_)
        | StructuralError::EditIr(_)
        | StructuralError::Syntax(_)
        | StructuralError::Serialization(_) => {
            typed_failure(RetrievalSource::Structural, FailureKind::InvalidContract)
        }
    }
}

fn map_failure(error: MapError) -> SourceFailure {
    match error {
        MapError::TimeLimit => typed_failure(RetrievalSource::PersonalizedMap, FailureKind::Time),
        MapError::BoundExceeded(_) => {
            typed_failure(RetrievalSource::PersonalizedMap, FailureKind::Bound)
        }
        MapError::InvalidRequest(_) => typed_failure(
            RetrievalSource::PersonalizedMap,
            FailureKind::InvalidRequest,
        ),
        MapError::InvalidIndex(_) => {
            typed_failure(RetrievalSource::PersonalizedMap, FailureKind::InvalidIndex)
        }
        MapError::Revision(error) => revision_failure(RetrievalSource::PersonalizedMap, error),
        MapError::InvalidLimits(_)
        | MapError::InvalidFact(_)
        | MapError::SelectorNoMatch(_)
        | MapError::StaleFact
        | MapError::SemanticEvidenceUnavailable
        | MapError::GraphEvidenceUnavailable
        | MapError::GraphEvidenceStale
        | MapError::HistoryEvidenceUnavailable
        | MapError::InvalidGraph(_)
        | MapError::CursorMismatch
        | MapError::Serialization(_) => typed_failure(
            RetrievalSource::PersonalizedMap,
            FailureKind::InvalidContract,
        ),
    }
}

fn graph_failure(error: GraphError) -> SourceFailure {
    match error {
        GraphError::BoundExceeded(GraphBound::Time) => {
            typed_failure(RetrievalSource::StructureGraph, FailureKind::Time)
        }
        GraphError::BoundExceeded(_) => {
            typed_failure(RetrievalSource::StructureGraph, FailureKind::Bound)
        }
        GraphError::InvalidIndex(_) => {
            typed_failure(RetrievalSource::StructureGraph, FailureKind::InvalidIndex)
        }
        GraphError::Revision(error) => revision_failure(RetrievalSource::StructureGraph, error),
        GraphError::InvalidOptions(_)
        | GraphError::StaleEvidence
        | GraphError::UnsafePath(_)
        | GraphError::MalformedManifest { .. }
        | GraphError::MissingWorkspaceMember { .. }
        | GraphError::MissingPathDependency { .. }
        | GraphError::InvalidEvidence(_)
        | GraphError::ContainmentCycle
        | GraphError::HistoryMismatch => typed_failure(
            RetrievalSource::StructureGraph,
            FailureKind::InvalidContract,
        ),
    }
}

fn history_failure(source: RetrievalSource, error: HistoryError) -> SourceFailure {
    match error {
        HistoryError::BoundExceeded(HistoryBound::Time) => typed_failure(source, FailureKind::Time),
        HistoryError::BoundExceeded(_) => typed_failure(source, FailureKind::Bound),
        HistoryError::InvalidRequest(_) => typed_failure(source, FailureKind::InvalidRequest),
        HistoryError::InvalidIndex(_) => typed_failure(source, FailureKind::InvalidIndex),
        HistoryError::Revision(error) => revision_failure(source, error),
        HistoryError::InvalidOptions(_)
        | HistoryError::SelectorNoMatch(_)
        | HistoryError::Unavailable(_)
        | HistoryError::StaleRepositoryFence
        | HistoryError::Git { .. }
        | HistoryError::Malformed(_)
        | HistoryError::MissingObject(_)
        | HistoryError::RepositoryRootMismatch { .. }
        | HistoryError::UnsafeGitPath(_) => typed_failure(source, FailureKind::InvalidContract),
    }
}

fn combined_structural_failure(failures: &[SourceFailure]) -> SourceFailure {
    let first = failures.first().cloned().unwrap_or_else(|| {
        typed_failure(RetrievalSource::Structural, FailureKind::InvalidContract)
    });
    if failures.iter().all(|failure| failure.code == first.code) {
        first
    } else {
        SourceFailure {
            code: "STRUCTURAL_MULTIPLE_ERRORS".into(),
            message: "structural patterns failed with multiple typed source errors".into(),
        }
    }
}

fn source_duration(
    started: Instant,
    limits: crate::SourceLimits,
    source_ms: u64,
) -> Option<Duration> {
    remaining_duration(started, limits.total_ms)
        .map(|remaining| remaining.min(Duration::from_millis(source_ms)))
}

fn remaining_duration(started: Instant, limit_ms: u64) -> Option<Duration> {
    Duration::from_millis(limit_ms)
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
}

fn observe<F>(
    source: RetrievalSource,
    api: &str,
    revision_digest: &str,
    action: F,
) -> Result<SourceObservation>
where
    F: FnOnce() -> std::result::Result<(Vec<RawCandidate>, bool, Option<String>), SourceFailure>,
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
                attempted_pattern_count: 0,
                successful_pattern_count: 0,
                started_at,
                ended_at: timestamp()?,
                elapsed_ns: nanos(started.elapsed()),
                complete_candidate_count: candidates.len(),
                candidates,
                truncated,
                source_revision_digest: revision_digest.into(),
                git_executable_digest: None,
                error_code: None,
                error,
            })
        }
        Err(failure) => {
            error_observation(source, api, revision_digest, started_at, started, failure)
        }
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
        attempted_pattern_count: 0,
        successful_pattern_count: 0,
        started_at: at.clone(),
        ended_at: at,
        elapsed_ns: 0,
        complete_candidate_count: 0,
        candidates: Vec::new(),
        truncated: false,
        source_revision_digest: revision_digest.into(),
        git_executable_digest: None,
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
    failure: SourceFailure,
) -> Result<SourceObservation> {
    Ok(SourceObservation {
        source,
        api: api.into(),
        status: SourceStatus::Error,
        attempted_pattern_count: 0,
        successful_pattern_count: 0,
        started_at,
        ended_at: timestamp()?,
        elapsed_ns: nanos(started.elapsed()),
        complete_candidate_count: 0,
        candidates: Vec::new(),
        truncated: false,
        source_revision_digest: revision_digest.into(),
        git_executable_digest: None,
        error_code: Some(failure.code),
        error: Some(failure.message),
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

fn combined_index_latency_ms(primary: Duration, nested: Option<Duration>) -> u64 {
    millis(primary.saturating_add(nested.unwrap_or_default()))
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
    fn tree_sitter_keeps_large_unicode_item_range_and_bounds_snippet() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-w07-large-declaration-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let source_root = root.join("source");
        let cache = root.join("cache");
        fs::create_dir_all(source_root.join("src")).unwrap();
        fs::create_dir(&cache).unwrap();
        let source = format!(
            "/// Large declaration.\npub const HUGE: &str = \"{}\";\n",
            "é".repeat(3_000)
        );
        fs::write(source_root.join("src/lib.rs"), &source).unwrap();

        let workspace = ManagedWorkspace::open_with_options(
            &source_root,
            RevisionOptions {
                metadata_path: Some(cache.join("revision.state")),
                ..RevisionOptions::default()
            },
        )
        .unwrap();
        let revision = workspace.current_revision().unwrap().id();
        let mut syntax = SyntaxIndex::new();
        let index = MetadataIndex::build_with_syntax(
            &workspace,
            revision,
            &IndexOptions::default(),
            &mut syntax,
        )
        .unwrap();
        let digest = sha256(b"large-declaration");
        let query = WorkerQuery {
            task_id: "large-declaration".into(),
            query: "Locate Large declaration".into(),
            query_digest: sha256(b"Locate Large declaration"),
        };
        let observation = syntax_observation(&index, &query, &digest).unwrap();
        let candidate = observation
            .candidates
            .iter()
            .find(|candidate| candidate.symbol.as_deref() == Some("HUGE"))
            .unwrap();
        let expected_start = source.find("pub const HUGE").unwrap();
        let expected_end = source.trim_end().len();
        assert_eq!(
            (candidate.range.start_byte, candidate.range.end_byte),
            (expected_start, expected_end)
        );
        assert!(candidate.range.end_byte - candidate.range.start_byte > 4_096);
        assert!(candidate.snippet.len() <= 4_096);
        assert!(candidate.snippet_truncated);
        assert!(candidate.snippet.is_char_boundary(candidate.snippet.len()));

        let projected = crate::ProjectedCandidate {
            rank: 1,
            range: candidate.range.clone(),
            symbol: candidate.symbol.clone(),
            snippet: candidate.snippet.clone(),
            snippet_truncated: candidate.snippet_truncated,
            semantics: candidate.semantics,
            source: candidate.source,
            source_revision_digest: candidate.source_revision_digest.clone(),
            provenance_digest: candidate.provenance_digest.clone(),
            score_micros: candidate.score_micros(),
            response_ordinal: candidate.response_ordinal,
        };
        assert!(crate::grader::localizes(
            &projected,
            &crate::SymbolPin {
                path: "src/lib.rs".into(),
                symbol: "HUGE".into(),
                symbol_kind: "const".into(),
                start_byte: expected_start,
                end_byte: expected_end,
                start_line: 2,
                end_line: 2,
                doc_digest: sha256(b"Large declaration."),
            }
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_repository_index_build_is_charged_to_index_latency() {
        assert_eq!(
            combined_index_latency_ms(
                Duration::from_micros(1_500),
                Some(Duration::from_micros(2_500))
            ),
            4
        );
        assert_eq!(
            combined_index_latency_ms(Duration::from_micros(1_500), None),
            1
        );
    }

    #[test]
    fn source_errors_keep_typed_sanitized_variants() {
        let failures = [
            lexical_failure(SearchError::TimeLimit),
            lexical_failure(SearchError::InvalidQuery("secret")),
            structural_failure(StructuralError::InvalidQuery("secret".into())),
            map_failure(MapError::BoundExceeded(
                kit::workspace::map::MapBound::Memory,
            )),
            map_failure(MapError::InvalidIndex("secret")),
            graph_failure(GraphError::BoundExceeded(GraphBound::Time)),
            graph_failure(GraphError::UnsafePath(std::path::PathBuf::from(
                "/private/secret",
            ))),
            history_failure(
                RetrievalSource::History,
                HistoryError::BoundExceeded(HistoryBound::Work),
            ),
            history_failure(
                RetrievalSource::GitPathHistory,
                HistoryError::Git {
                    operation: "show",
                    message: "/private/secret".into(),
                },
            ),
        ];
        let codes = failures
            .iter()
            .map(|failure| failure.code.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            [
                "LEXICAL_TIME_LIMIT",
                "LEXICAL_INVALID_REQUEST",
                "STRUCTURAL_INVALID_REQUEST",
                "MAP_BOUND_EXCEEDED",
                "MAP_INVALID_INDEX",
                "GRAPH_TIME_LIMIT",
                "GRAPH_INVALID_CONTRACT",
                "HISTORY_BOUND_EXCEEDED",
                "GIT_PATH_HISTORY_INVALID_CONTRACT",
            ]
        );
        assert!(failures.iter().all(|failure| {
            failure.code.len() <= 128
                && failure.message.len() <= 512
                && !failure.message.contains("secret")
                && !failure.message.contains(['/', '\\', '='])
                && !failure.message.chars().any(char::is_control)
        }));
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
    fn tiny_fixture_matches_canary_shape_for_every_arm_and_both_roots() {
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
        let root_source_digest = sha256(
            &canonical(&BTreeMap::from([(
                "crate/src/lib.rs".to_owned(),
                format!(
                    "{:x}",
                    sha2::Sha256::digest(b"/// Finds alpha.\npub fn alpha() {}\n")
                ),
            )]))
            .unwrap(),
        );
        let mut shape_count = 0;
        for (root_name, worker_source, worker_source_digest, lexical_path, history_states) in [
            (
                "root",
                repository.as_path(),
                root_source_digest.as_str(),
                "crate/src/lib.rs",
                1,
            ),
            (
                "nested",
                source.as_path(),
                source_digest.as_str(),
                "src/lib.rs",
                2,
            ),
        ] {
            for arm in [Arm::L, Arm::C, Arm::F, Arm::FS, Arm::FP, Arm::FG, Arm::FH] {
                let cache = root.join(format!("cache-{root_name}-{arm:?}"));
                fs::create_dir(&cache).unwrap();
                let config = ArmConfig::frozen(arm);
                let mut raw = execute(
                    worker_source,
                    &git_metadata_root,
                    &cache,
                    WorkerQuery {
                        task_id: "task".into(),
                        query: query_text.into(),
                        query_digest: sha256(query_text.as_bytes()),
                    },
                    WorkerArmRequest {
                        unit_id: "unit".into(),
                        repository_class: crate::RepositoryClass::Small,
                        source_digest: worker_source_digest.into(),
                        admission_digest:
                            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                                .into(),
                        executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
                        cache_id: format!("cache-{root_name}-{arm:?}"),
                        worker_executable_digest:
                            "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                                .into(),
                        git_path: crate::run::git_path(&git).to_string_lossy().into_owned(),
                        git_executable_digest: crate::run::git_digest(&git).into(),
                        git_version: crate::run::git_version(&git).into(),
                        config: config.clone(),
                    },
                )
                .unwrap();
                assert!(crate::run::canary_raw_shape_is_valid(
                    &raw,
                    &config,
                    worker_source_digest,
                    crate::run::git_digest(&git),
                ));
                assert_eq!(
                    raw.observations
                        .iter()
                        .map(|item| item.source)
                        .collect::<Vec<_>>(),
                    config.enabled_sources
                );
                let lexical = raw
                    .observations
                    .iter()
                    .find(|observation| observation.source == RetrievalSource::Lexical)
                    .unwrap();
                assert!(lexical.candidates.iter().any(|candidate| {
                    candidate.range.path == lexical_path
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
                    assert_eq!(
                        history
                            .and_then(|observation| observation.git_executable_digest.as_deref()),
                        Some(crate::run::git_digest(&git))
                    );
                    assert_eq!(revision_state_count(&cache), history_states);
                    assert_eq!(
                        cache.join("history-revision.state").exists(),
                        root_name == "nested"
                    );
                }
                if root_name == "root" && arm == Arm::FG {
                    let structural = raw
                        .observations
                        .iter_mut()
                        .find(|observation| observation.source == RetrievalSource::Structural)
                        .unwrap();
                    *structural = finalize_structural_observation(
                        structural.clone(),
                        8,
                        7,
                        &[typed_failure(
                            RetrievalSource::Structural,
                            FailureKind::Time,
                        )],
                    );
                    assert!(crate::run::canary_raw_shape_is_valid(
                        &raw,
                        &config,
                        worker_source_digest,
                        crate::run::git_digest(&git),
                    ));

                    let mut successful_empty = raw.clone();
                    let structural = successful_empty
                        .observations
                        .iter_mut()
                        .find(|observation| observation.source == RetrievalSource::Structural)
                        .unwrap();
                    structural.candidates.clear();
                    structural.complete_candidate_count = 0;
                    structural.error_code = None;
                    structural.error = None;
                    *structural = finalize_structural_observation(structural.clone(), 8, 8, &[]);
                    assert_eq!(structural.status, SourceStatus::Available);
                    assert!(structural.candidates.is_empty());
                    assert!(crate::run::canary_raw_shape_is_valid(
                        &successful_empty,
                        &config,
                        worker_source_digest,
                        crate::run::git_digest(&git),
                    ));

                    let mut all_failed = raw.clone();
                    let structural = all_failed
                        .observations
                        .iter_mut()
                        .find(|observation| observation.source == RetrievalSource::Structural)
                        .unwrap();
                    *structural = finalize_structural_observation(
                        structural.clone(),
                        8,
                        0,
                        &vec![
                            typed_failure(
                                RetrievalSource::Structural,
                                FailureKind::InvalidRequest,
                            );
                            8
                        ],
                    );
                    assert_eq!(structural.status, SourceStatus::Error);
                    assert!(structural.candidates.is_empty());
                    assert_eq!(structural.complete_candidate_count, 0);
                    assert!(!crate::run::canary_raw_shape_is_valid(
                        &all_failed,
                        &config,
                        worker_source_digest,
                        crate::run::git_digest(&git),
                    ));
                }
                shape_count += 1;
            }
        }
        assert_eq!(shape_count, 14);
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
                    repository_class: crate::RepositoryClass::Small,
                    source_digest,
                    admission_digest:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .into(),
                    executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
                    cache_id: "cache-sibling".into(),
                    worker_executable_digest:
                        "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                            .into(),
                    git_path: crate::run::git_path(&git).to_string_lossy().into_owned(),
                    git_executable_digest: crate::run::git_digest(&git).into(),
                    git_version: crate::run::git_version(&git).into(),
                    config: ArmConfig::frozen(Arm::L),
                },
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn revision_state_count(cache: &Path) -> usize {
        fs::read_dir(cache)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "state")
            })
            .count()
    }
}
