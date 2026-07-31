use crate::{
    ArmConfig, CandidateRange, CandidateSemantics, CorpusUnit, ProjectedCandidate, ProtocolError,
    RawCandidate, RawTrial, Result, SourceStatus, TrialGrade, TrialTerminal, canonical,
    protocol::validated_unit_files, sha256, valid_digest,
};
use std::{cmp::Ordering, collections::BTreeMap, path::Path};

pub fn project(raw: &RawTrial, config: &ArmConfig) -> Result<Vec<ProjectedCandidate>> {
    validate_raw(raw, config)?;
    if raw.terminal != TrialTerminal::Complete {
        return Ok(Vec::new());
    }
    let mut unique = BTreeMap::<(String, usize, usize, usize, usize), &RawCandidate>::new();
    for candidate in raw
        .observations
        .iter()
        .flat_map(|observation| &observation.candidates)
    {
        let key = (
            candidate.range.path.clone(),
            candidate.range.start_byte,
            candidate.range.end_byte,
            candidate.range.start_line,
            candidate.range.end_line,
        );
        match unique.get(&key) {
            Some(current) if compare_raw(candidate, current) != Ordering::Less => {}
            _ => {
                unique.insert(key, candidate);
            }
        }
    }
    let mut candidates = unique.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| compare_raw(left, right));
    let mut projected = Vec::new();
    let mut remaining_tokens = config.token_budget;
    for (rank, candidate) in candidates
        .into_iter()
        .take(config.normalization.top_k)
        .enumerate()
    {
        let maximum_bytes = remaining_tokens.saturating_mul(4);
        let mut end = candidate.snippet.len().min(maximum_bytes);
        while !candidate.snippet.is_char_boundary(end) {
            end -= 1;
        }
        let snippet = candidate.snippet[..end].to_owned();
        remaining_tokens = remaining_tokens.saturating_sub(snippet.len().div_ceil(4));
        projected.push(ProjectedCandidate {
            rank: rank + 1,
            range: candidate.range.clone(),
            symbol: candidate.symbol.clone(),
            snippet,
            snippet_truncated: candidate.snippet_truncated || end != candidate.snippet.len(),
            semantics: candidate.semantics,
            source: candidate.source,
            source_revision_digest: candidate.source_revision_digest.clone(),
            provenance_digest: candidate.provenance_digest.clone(),
            score_micros: candidate.score_micros(),
            response_ordinal: candidate.response_ordinal,
        });
    }
    Ok(projected)
}

pub fn grade(unit: &CorpusUnit, raw: &RawTrial, source: &Path) -> Result<TrialGrade> {
    if raw.unit_id != unit.unit_id
        || raw.repository_class != unit.repository_class
        || raw.source_digest != unit.rust_source_digest
        || raw.task_query_digest != unit.task.query_digest
    {
        return Err(ProtocolError("raw trial does not bind the frozen unit/task".into()).into());
    }
    validate_lexical_context_bytes(raw, source)?;
    let config = ArmConfig::frozen(raw.arm);
    let projected_top_k = project(raw, &config)?;
    let target = &unit.oracle.target;
    let target_candidate = projected_top_k
        .iter()
        .find(|candidate| localizes(candidate, target));
    let localization_success = target_candidate.is_some();
    let wrong_decoy_success = localization_success
        && !projected_top_k.iter().any(|candidate| {
            unit.oracle
                .decoys
                .iter()
                .any(|decoy| localizes(candidate, decoy))
        });
    let downstream_mechanical_success = target_candidate
        .map(|candidate| downstream_edit(unit, candidate, source))
        .transpose()?
        .unwrap_or(false);
    let freshness_success = raw.observations.iter().all(|observation| {
        observation.source_revision_digest == unit.rust_source_digest
            && observation
                .candidates
                .iter()
                .all(|candidate| candidate.source_revision_digest == unit.rust_source_digest)
    });
    let latency_success = raw.index_latency_ms <= 10_000 && raw.query_latency_ms <= 3_000;
    let provenance_success = projected_top_k.iter().all(|candidate| {
        valid_digest(&candidate.provenance_digest)
            && valid_digest(&candidate.source_revision_digest)
    });
    let token_count = projected_top_k
        .iter()
        .map(|candidate| candidate.snippet.len().div_ceil(4))
        .sum::<usize>();
    let token_budget_success = token_count <= config.token_budget && raw.token_count == token_count;
    let terminal_success = raw.terminal == TrialTerminal::Complete
        && raw.worker_error.is_none()
        && raw
            .observations
            .iter()
            .all(|observation| observation.status != SourceStatus::Error);
    Ok(TrialGrade {
        schema_version: "2.0".into(),
        kind: "m005_w07_trial_grade".into(),
        unit_id: unit.unit_id.clone(),
        arm: raw.arm,
        raw_trial_digest: sha256(&canonical(raw)?),
        projected_top_k,
        localization_success,
        wrong_decoy_success,
        downstream_mechanical_success,
        freshness_success,
        latency_success,
        provenance_success,
        token_budget_success,
        terminal_success,
    })
}

pub fn projected_token_count(raw: &RawTrial, config: &ArmConfig) -> Result<usize> {
    Ok(project(raw, config)?
        .iter()
        .map(|candidate| candidate.snippet.len().div_ceil(4))
        .sum())
}

pub(crate) fn validate_raw(raw: &RawTrial, config: &ArmConfig) -> Result<()> {
    if raw.schema_version != "2.0"
        || raw.kind != "m005_w07_raw_arm_trial"
        || raw.arm != config.arm
        || raw.unit_id.is_empty()
        || raw.unit_id.len() > 128
        || !valid_digest(&raw.admission_digest)
        || !valid_digest(&raw.source_digest)
        || !valid_digest(&raw.task_query_digest)
        || !valid_digest(&raw.arm_config_digest)
        || !valid_digest(&raw.worker_executable_digest)
        || raw.arm_config_digest != sha256(&canonical(config)?)
        || raw.cache_id.is_empty()
        || raw.cache_id.len() > 256
        || raw.elapsed_ns > 120_000_000_000
        || raw.index_latency_ms > 120_000
        || raw.query_latency_ms > 120_000
        || raw.observations.len() > 16
        || (raw.terminal == TrialTerminal::Complete && raw.process_id == 0)
    {
        return Err(ProtocolError("invalid bounded raw trial".into()).into());
    }
    if matches!(raw.arm, crate::Arm::L | crate::Arm::FS) && raw.syntax_initializations != 0 {
        return Err(ProtocolError("syntax-free arm initialized syntax".into()).into());
    }
    if raw.terminal != TrialTerminal::Complete {
        if raw.worker_error.is_none() || !raw.observations.is_empty() {
            return Err(ProtocolError("invalid terminal failure record".into()).into());
        }
        return Ok(());
    }
    if raw.worker_error.is_some()
        || raw
            .observations
            .iter()
            .map(|observation| observation.source)
            .collect::<Vec<_>>()
            != config.enabled_sources
    {
        return Err(ProtocolError("raw source set disagrees with arm".into()).into());
    }
    let mut aggregate = 0_usize;
    for observation in &raw.observations {
        if observation.api.is_empty()
            || observation.api.len() > 256
            || observation
                .error_code
                .as_ref()
                .is_some_and(|code| code.len() > 128)
            || observation
                .error
                .as_ref()
                .is_some_and(|error| error.len() > 512)
            || observation.error.as_ref().is_some_and(|error| {
                error.contains(['/', '\\', '=']) || error.chars().any(char::is_control)
            })
            || observation.complete_candidate_count != observation.candidates.len()
            || observation.candidates.len() > 100_000
            || observation.elapsed_ns > 120_000_000_000
            || !valid_digest(&observation.source_revision_digest)
        {
            return Err(ProtocolError("invalid bounded source observation".into()).into());
        }
        aggregate = aggregate
            .checked_add(observation.candidates.len())
            .ok_or_else(|| ProtocolError("candidate aggregate overflow".into()))?;
        if aggregate > 200_000 {
            return Err(ProtocolError("candidate aggregate bound exceeded".into()).into());
        }
        validate_source_semantics(observation)?;
        for candidate in &observation.candidates {
            validate_candidate(candidate, observation)?;
        }
    }
    Ok(())
}

fn validate_source_semantics(observation: &crate::SourceObservation) -> Result<()> {
    use crate::RetrievalSource::*;

    let diagnostic = || {
        observation.error.as_ref().is_some_and(|message| {
            !message.is_empty()
                && message.len() <= 512
                && !message.contains(['/', '\\', '='])
                && !message.chars().any(char::is_control)
        })
    };
    let no_results = observation.candidates.is_empty()
        && observation.complete_candidate_count == 0
        && !observation.truncated;
    let patterns_are_zero =
        observation.attempted_pattern_count == 0 && observation.successful_pattern_count == 0;
    let code = observation.error_code.as_deref();
    let valid = match observation.status {
        SourceStatus::Available if observation.source == Structural => {
            observation.attempted_pattern_count == 8
                && observation.successful_pattern_count > 0
                && observation.successful_pattern_count <= observation.attempted_pattern_count
                && if observation.successful_pattern_count < observation.attempted_pattern_count {
                    code.is_some_and(|code| error_code_is_valid(Structural, code)) && diagnostic()
                } else {
                    code.is_none() && observation.error.is_none()
                }
        }
        SourceStatus::Available => {
            patterns_are_zero && code.is_none() && observation.error.is_none()
        }
        SourceStatus::TerminalUnavailable => {
            patterns_are_zero
                && no_results
                && code.is_some_and(|code| {
                    matches!(
                        (observation.source, code),
                        (Lsp, "BLK-14_NO_PINNED_RUST_LSP_SERVER")
                            | (
                                CargoMetadataWithoutSourceParse,
                                "NO_PINNED_PARSE_FREE_CARGO_METADATA_ADAPTER"
                            )
                            | (Structural, "DEPENDENCY_CLOSED_SYNTAX_DISABLED")
                    )
                })
                && diagnostic()
        }
        SourceStatus::Error => {
            no_results
                && diagnostic()
                && code.is_some_and(|code| error_code_is_valid(observation.source, code))
                && if observation.source == Structural {
                    observation.attempted_pattern_count == 8
                        && observation.successful_pattern_count == 0
                } else {
                    patterns_are_zero
                }
        }
    };
    let history_digest_is_valid = if matches!(observation.source, History | GitPathHistory) {
        observation.status == SourceStatus::Available
            && observation
                .git_executable_digest
                .as_deref()
                .is_some_and(valid_digest)
            || observation.status == SourceStatus::Error
                && observation.git_executable_digest.is_none()
    } else {
        observation.git_executable_digest.is_none()
    };
    if !valid || !history_digest_is_valid {
        return Err(ProtocolError(format!(
            "invalid typed source contract for {:?}",
            observation.source
        ))
        .into());
    }
    Ok(())
}

fn error_code_is_valid(source: crate::RetrievalSource, code: &str) -> bool {
    use crate::RetrievalSource::*;
    let suffix = match source {
        Lexical => code.strip_prefix("LEXICAL_"),
        Structural => code.strip_prefix("STRUCTURAL_"),
        PersonalizedMap => code.strip_prefix("MAP_"),
        StructureGraph => code.strip_prefix("GRAPH_"),
        History => code.strip_prefix("HISTORY_"),
        GitPathHistory => code.strip_prefix("GIT_PATH_HISTORY_"),
        _ => None,
    };
    suffix.is_some_and(|suffix| {
        matches!(
            suffix,
            "TIME_LIMIT"
                | "BOUND_EXCEEDED"
                | "INVALID_REQUEST"
                | "INVALID_INDEX"
                | "INVALID_CONTRACT"
        ) || source == Structural && suffix == "MULTIPLE_ERRORS"
    }) && !(source == StructureGraph && code.ends_with("INVALID_REQUEST"))
}

fn validate_candidate(
    candidate: &RawCandidate,
    observation: &crate::SourceObservation,
) -> Result<()> {
    let range = &candidate.range;
    if candidate.source != observation.source
        || candidate.source_revision_digest != observation.source_revision_digest
        || range.path.is_empty()
        || range.path.len() > 4096
        || Path::new(&range.path).is_absolute()
        || range
            .path
            .split('/')
            .any(|part| part.is_empty() || part == "..")
        || range.start_byte >= range.end_byte
        || range.end_byte > crate::MAX_SOURCE_FILE_BYTES as usize
        || range.start_line == 0
        || range.start_line > range.end_line
        || candidate
            .symbol
            .as_ref()
            .is_some_and(|symbol| symbol.len() > 512)
        || candidate.snippet.len() > 4096
        || !valid_digest(&candidate.provenance_digest)
        || !(-1_000_000..=1_000_000).contains(&candidate.raw_score_micros)
        || !(-1_000_000..=1_000_000).contains(&candidate.token_overlap_micros)
        || candidate.response_ordinal > 100_000
        || (candidate.semantics == CandidateSemantics::LexicalContext
            && candidate.source != crate::RetrievalSource::Lexical)
        || (candidate.source == crate::RetrievalSource::Lexical
            && candidate.semantics != CandidateSemantics::LexicalContext)
    {
        return Err(ProtocolError(format!(
            "invalid bounded raw candidate from {:?} at {}:{}-{} (scores {}, {})",
            candidate.source,
            range.path,
            range.start_byte,
            range.end_byte,
            candidate.raw_score_micros,
            candidate.token_overlap_micros
        ))
        .into());
    }
    Ok(())
}

fn validate_lexical_context_bytes(raw: &RawTrial, source: &Path) -> Result<()> {
    for candidate in raw
        .observations
        .iter()
        .flat_map(|observation| &observation.candidates)
        .filter(|candidate| candidate.semantics == CandidateSemantics::LexicalContext)
    {
        let bytes = crate::protocol::read_bounded_allow_empty(
            &source.join(&candidate.range.path),
            crate::MAX_SOURCE_FILE_BYTES,
        )?;
        if candidate.range.end_byte > bytes.len()
            || bytes.get(candidate.range.start_byte..candidate.range.end_byte)
                != Some(candidate.snippet.as_bytes())
        {
            return Err(
                ProtocolError("lexical context does not equal its source range".into()).into(),
            );
        }
    }
    Ok(())
}

fn compare_raw(left: &RawCandidate, right: &RawCandidate) -> Ordering {
    right
        .score_micros()
        .cmp(&left.score_micros())
        .then_with(|| left.range.path.as_bytes().cmp(right.range.path.as_bytes()))
        .then_with(|| left.range.start_byte.cmp(&right.range.start_byte))
        .then_with(|| left.range.end_byte.cmp(&right.range.end_byte))
        .then_with(|| left.source.cmp(&right.source))
        .then_with(|| left.response_ordinal.cmp(&right.response_ordinal))
        .then_with(|| left.provenance_digest.cmp(&right.provenance_digest))
}

fn exact_symbol_range(range: &CandidateRange, symbol: &crate::SymbolPin) -> bool {
    range.path == symbol.path
        && range.start_byte == symbol.start_byte
        && range.end_byte == symbol.end_byte
        && range.start_line == symbol.start_line
        && range.end_line == symbol.end_line
}

pub(crate) fn localizes(candidate: &ProjectedCandidate, symbol: &crate::SymbolPin) -> bool {
    match candidate.semantics {
        CandidateSemantics::ExactItem => exact_symbol_range(&candidate.range, symbol),
        CandidateSemantics::LexicalContext => {
            candidate.range.path == symbol.path
                && candidate.range.start_byte <= symbol.start_byte
                && symbol.start_byte < candidate.range.end_byte
        }
        CandidateSemantics::OtherContext => false,
    }
}

fn downstream_edit(
    unit: &CorpusUnit,
    candidate: &ProjectedCandidate,
    source: &Path,
) -> Result<bool> {
    if !localizes(candidate, &unit.oracle.target) {
        return Ok(false);
    }
    let mut files = validated_unit_files(source, unit)?;
    let target = &unit.oracle.target;
    let path = source.join(&target.path);
    let mut bytes = crate::protocol::read_bounded_allow_empty(&path, crate::MAX_SOURCE_FILE_BYTES)?;
    if target.end_byte > bytes.len() || !bytes.is_char_boundary(target.start_byte) {
        return Ok(false);
    }
    bytes.splice(
        target.start_byte..target.start_byte,
        unit.oracle.reference_edit.utf8_text.bytes(),
    );
    files.insert(target.path.clone(), hex_sha256(&bytes));
    Ok(sha256(&canonical(&files)?) == unit.oracle.expected_post_edit_tree_digest)
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

trait ByteBoundary {
    fn is_char_boundary(&self, index: usize) -> bool;
}

impl ByteBoundary for Vec<u8> {
    fn is_char_boundary(&self, index: usize) -> bool {
        std::str::from_utf8(self)
            .map(|text| text.is_char_boundary(index))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CorpusUnit, ExecutorEvidence, OraclePin, PackagePin, ReferenceEdit, RepositoryClass,
        RetrievalSource, SourceObservation, SymbolPin, TaskPin,
    };
    use sha2::{Digest as _, Sha256};
    use std::{collections::BTreeMap, fs};

    fn candidate(path: &str, start: usize, score: i64, source: RetrievalSource) -> RawCandidate {
        RawCandidate {
            range: CandidateRange {
                path: path.into(),
                start_byte: start,
                end_byte: start + 1,
                start_line: 1,
                end_line: 1,
            },
            symbol: None,
            snippet: path.into(),
            snippet_truncated: false,
            semantics: if source == RetrievalSource::Lexical {
                CandidateSemantics::LexicalContext
            } else {
                CandidateSemantics::OtherContext
            },
            source,
            source_revision_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            provenance_digest: format!("sha256:{:064x}", start + 1),
            raw_score_micros: score,
            token_overlap_micros: 0,
            response_ordinal: start,
        }
    }

    #[test]
    fn projection_deduplicates_and_breaks_ties() {
        let config = ArmConfig::frozen(crate::Arm::L);
        let mut raw = RawTrial {
            schema_version: "2.0".into(),
            kind: "m005_w07_raw_arm_trial".into(),
            unit_id: "u".into(),
            task_id: "t".into(),
            repository_class: RepositoryClass::Small,
            arm: crate::Arm::L,
            executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
            admission_digest:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            source_digest:
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
            task_query_digest:
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".into(),
            arm_config_digest: sha256(&canonical(&config).unwrap()),
            worker_executable_digest:
                "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
            process_id: 1,
            cache_id: "cache".into(),
            measured_started_at: "2026-01-01T00:00:00Z".into(),
            measured_ended_at: "2026-01-01T00:00:01Z".into(),
            elapsed_ns: 1,
            index_latency_ms: 1,
            query_latency_ms: 1,
            token_count: 1,
            syntax_initializations: 0,
            terminal: TrialTerminal::Complete,
            observations: vec![SourceObservation {
                source: RetrievalSource::Lexical,
                api: "api".into(),
                status: SourceStatus::Available,
                attempted_pattern_count: 0,
                successful_pattern_count: 0,
                started_at: "2026-01-01T00:00:00Z".into(),
                ended_at: "2026-01-01T00:00:01Z".into(),
                elapsed_ns: 1,
                complete_candidate_count: 3,
                candidates: vec![
                    candidate("b.rs", 0, 10, RetrievalSource::Lexical),
                    candidate("a.rs", 0, 10, RetrievalSource::Lexical),
                    candidate("a.rs", 0, 9, RetrievalSource::Lexical),
                ],
                truncated: false,
                source_revision_digest:
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                git_executable_digest: None,
                error_code: None,
                error: None,
            }],
            worker_error: None,
        };
        let projected = project(&raw, &config).unwrap();
        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].range.path, "a.rs");
        assert_eq!(projected[1].range.path, "b.rs");
        raw.syntax_initializations = 1;
        assert!(project(&raw, &config).is_err());
    }

    #[test]
    fn oracle_grade_rejects_decoy_and_applies_edit_to_retrieved_target() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-w07-grade-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        let source = b"/// target docs\npub fn target() {}\n/// decoy docs\npub fn decoy() {}\n";
        fs::write(root.join("src/lib.rs"), source).unwrap();
        let mut files = BTreeMap::from([(
            "src/lib.rs".to_owned(),
            format!("{:x}", Sha256::digest(source)),
        )]);
        let checksum = serde_json::json!({
            "files": files,
            "package": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });
        let checksum_bytes = serde_json::to_vec(&checksum).unwrap();
        fs::write(root.join(".cargo-checksum.json"), &checksum_bytes).unwrap();
        let git = crate::run::preregistration_git().unwrap();
        crate::run::trusted_git_status(&git, &root, &["init"]).unwrap();
        crate::run::trusted_git_status(&git, &root, &["add", "."]).unwrap();
        crate::run::trusted_git_status(
            &git,
            &root,
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
        let target_start = source
            .windows(b"pub fn target".len())
            .position(|window| window == b"pub fn target")
            .unwrap();
        let target_end = source[target_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap()
            + target_start;
        let decoy_start = source
            .windows(b"pub fn decoy".len())
            .position(|window| window == b"pub fn decoy")
            .unwrap();
        let decoy_end = source[decoy_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap()
            + decoy_start;
        let insertion = "#[doc = \"M005-W07 downstream localization check.\"]\n";
        let mut edited = source.to_vec();
        edited.splice(target_start..target_start, insertion.bytes());
        files.insert(
            "src/lib.rs".into(),
            format!("{:x}", Sha256::digest(&edited)),
        );
        let symbol = |name: &str, start, end, line| SymbolPin {
            path: "src/lib.rs".into(),
            symbol: name.into(),
            symbol_kind: "function".into(),
            start_byte: start,
            end_byte: end,
            start_line: line,
            end_line: line,
            doc_digest: "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                .into(),
        };
        let unit = CorpusUnit {
            schedule_index: 0,
            unit_id: "fixture".into(),
            repository_class: RepositoryClass::Small,
            package: PackagePin {
                name: "fixture".into(),
                version: "1.0.0".into(),
                normalized_repository_url: "https://example.invalid/fixture".into(),
                vcs_commit: "1111111111111111111111111111111111111111".into(),
                path_in_vcs: String::new(),
                license: "MIT".into(),
                cargo_lock_checksum:
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                registry_source: "crates.io".into(),
            },
            rust_sloc: 2,
            source_file_count: 1,
            source_bytes: source.len() as u64,
            source_digest: sha256(
                &canonical(&BTreeMap::from([(
                    "src/lib.rs".to_owned(),
                    format!("{:x}", Sha256::digest(source)),
                )]))
                .unwrap(),
            ),
            rust_source_digest: sha256(
                &canonical(&BTreeMap::from([(
                    "src/lib.rs".to_owned(),
                    format!("{:x}", Sha256::digest(source)),
                )]))
                .unwrap(),
            ),
            checksum_manifest_digest: sha256(&checksum_bytes),
            task: TaskPin {
                task_id: "task".into(),
                query: "target docs".into(),
                query_digest: sha256(b"target docs"),
            },
            oracle: OraclePin {
                selection_hash:
                    "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
                target: symbol("target", target_start, target_end, 2),
                decoys: vec![symbol("decoy", decoy_start, decoy_end, 4)],
                reference_edit: ReferenceEdit {
                    operation: "insert_utf8_before_registered_target_after_context_authorization"
                        .into(),
                    utf8_text: insertion.into(),
                },
                expected_post_edit_tree_digest: sha256(&canonical(&files).unwrap()),
            },
            arm_order: vec![crate::Arm::L],
        };
        let config = ArmConfig::frozen(crate::Arm::L);
        let cache = root.with_extension("target-cache");
        fs::create_dir(&cache).unwrap();
        let mut raw =
            crate::worker::execute(
                &root,
                &root.join(".git"),
                &cache,
                crate::WorkerQuery {
                    task_id: "task".into(),
                    query: "target docs".into(),
                    query_digest: sha256(b"target docs"),
                },
                crate::WorkerArmRequest {
                    unit_id: "fixture".into(),
                    repository_class: RepositoryClass::Small,
                    source_digest: unit.rust_source_digest.clone(),
                    admission_digest:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .into(),
                    executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
                    cache_id: "target-cache".into(),
                    worker_executable_digest:
                        "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                            .into(),
                    git_path: "/usr/bin/git".into(),
                    git_executable_digest:
                        "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                            .into(),
                    git_version: "git version fixture".into(),
                    config: config.clone(),
                },
            )
            .unwrap();
        raw.source_digest = unit.rust_source_digest.clone();
        raw.task_query_digest = unit.task.query_digest.clone();
        let clean = grade(&unit, &raw, &root).unwrap();
        assert!(clean.localization_success);
        assert!(!clean.wrong_decoy_success);
        assert!(clean.downstream_mechanical_success);

        let mut latency_failure = raw.clone();
        latency_failure.query_latency_ms = 3_001;
        let latency_grade = grade(&unit, &latency_failure, &root).unwrap();
        assert!(!latency_grade.latency_success);
        assert!(latency_grade.terminal_success);

        let mut source_time_limit = raw.clone();
        source_time_limit.query_latency_ms = 1;
        source_time_limit.token_count = 0;
        let lexical = &mut source_time_limit.observations[0];
        lexical.status = SourceStatus::Error;
        lexical.candidates.clear();
        lexical.complete_candidate_count = 0;
        lexical.truncated = false;
        lexical.error_code = Some("LEXICAL_TIME_LIMIT".into());
        lexical.error = Some("source API time limit exceeded".into());
        let source_grade = grade(&unit, &source_time_limit, &root).unwrap();
        assert!(source_grade.latency_success);
        assert!(!source_grade.terminal_success);

        let structural_config = ArmConfig::frozen(crate::Arm::C);
        let mut structural_raw = raw.clone();
        structural_raw.arm = crate::Arm::C;
        structural_raw.arm_config_digest = sha256(&canonical(&structural_config).unwrap());
        structural_raw.syntax_initializations = 1;
        let mut empty_observation = structural_raw.observations[0].clone();
        empty_observation.candidates.clear();
        empty_observation.complete_candidate_count = 0;
        empty_observation.api = "api".into();
        let mut tree_sitter = empty_observation.clone();
        tree_sitter.source = RetrievalSource::TreeSitter;
        let mut structural = empty_observation.clone();
        structural.source = RetrievalSource::Structural;
        structural.attempted_pattern_count = 8;
        structural.successful_pattern_count = 8;
        let mut lsp = empty_observation;
        lsp.source = RetrievalSource::Lsp;
        lsp.status = SourceStatus::TerminalUnavailable;
        lsp.error_code = Some("BLK-14_NO_PINNED_RUST_LSP_SERVER".into());
        lsp.error = lsp.error_code.clone();
        structural_raw.observations = vec![
            structural_raw.observations[0].clone(),
            tree_sitter,
            structural,
            lsp,
        ];
        structural_raw.token_count =
            projected_token_count(&structural_raw, &structural_config).unwrap();
        crate::protocol::validate_schema(
            include_bytes!("../schema/v2/raw-trial.schema.json"),
            &canonical(&structural_raw).unwrap(),
        )
        .unwrap();
        assert!(crate::run::canary_raw_shape_is_valid(
            &structural_raw,
            &structural_config,
            &unit.rust_source_digest,
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        ));
        assert!(
            grade(&unit, &structural_raw, &root)
                .unwrap()
                .terminal_success
        );

        let mut arbitrary_code = structural_raw.clone();
        let lsp = arbitrary_code.observations.last_mut().unwrap();
        lsp.error_code = Some("ARBITRARY".into());
        assert!(validate_raw(&arbitrary_code, &structural_config).is_err());

        let mut source_mismatch = structural_raw.clone();
        let lsp = source_mismatch.observations.last_mut().unwrap();
        lsp.error_code = Some("NO_PINNED_PARSE_FREE_CARGO_METADATA_ADAPTER".into());
        assert!(validate_raw(&source_mismatch, &structural_config).is_err());

        let mut unknown_unavailable = structural_raw.clone();
        let lsp = unknown_unavailable.observations.last_mut().unwrap();
        lsp.error_code = Some("LSP_UNKNOWN_UNAVAILABLE".into());
        assert!(validate_raw(&unknown_unavailable, &structural_config).is_err());

        let mut available_generic_error = structural_raw.clone();
        let lexical = available_generic_error.observations.first_mut().unwrap();
        lexical.error_code = Some("LEXICAL_TIME_LIMIT".into());
        lexical.error = Some("source API time limit exceeded".into());
        assert!(validate_raw(&available_generic_error, &structural_config).is_err());

        let structural = structural_raw
            .observations
            .iter_mut()
            .find(|observation| observation.source == RetrievalSource::Structural)
            .unwrap();
        structural.successful_pattern_count = 7;
        structural.error_code = Some("STRUCTURAL_TIME_LIMIT".into());
        structural.error =
            Some("1 of 8 structural patterns failed: source API time limit exceeded".into());
        assert!(validate_raw(&structural_raw, &structural_config).is_ok());
        crate::protocol::validate_schema(
            include_bytes!("../schema/v2/raw-trial.schema.json"),
            &canonical(&structural_raw).unwrap(),
        )
        .unwrap();
        assert!(crate::run::canary_raw_shape_is_valid(
            &structural_raw,
            &structural_config,
            &unit.rust_source_digest,
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        ));
        assert!(
            grade(&unit, &structural_raw, &root)
                .unwrap()
                .terminal_success
        );

        let structural = structural_raw
            .observations
            .iter_mut()
            .find(|observation| observation.source == RetrievalSource::Structural)
            .unwrap();
        structural.successful_pattern_count = 0;
        structural.status = SourceStatus::Error;
        structural.error_code = Some("STRUCTURAL_INVALID_REQUEST".into());
        structural.error =
            Some("all 8 structural patterns failed: source API request is invalid".into());
        crate::protocol::validate_schema(
            include_bytes!("../schema/v2/raw-trial.schema.json"),
            &canonical(&structural_raw).unwrap(),
        )
        .unwrap();
        assert!(!crate::run::canary_raw_shape_is_valid(
            &structural_raw,
            &structural_config,
            &unit.rust_source_digest,
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        ));
        assert!(
            !grade(&unit, &structural_raw, &root)
                .unwrap()
                .terminal_success
        );

        let decoy_cache = root.with_extension("decoy-cache");
        fs::create_dir(&decoy_cache).unwrap();
        let decoy_raw =
            crate::worker::execute(
                &root,
                &root.join(".git"),
                &decoy_cache,
                crate::WorkerQuery {
                    task_id: "task".into(),
                    query: "decoy docs".into(),
                    query_digest: sha256(b"decoy docs"),
                },
                crate::WorkerArmRequest {
                    unit_id: "fixture".into(),
                    repository_class: RepositoryClass::Small,
                    source_digest: unit.rust_source_digest.clone(),
                    admission_digest:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .into(),
                    executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
                    cache_id: "decoy-cache".into(),
                    worker_executable_digest:
                        "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                            .into(),
                    git_path: "/usr/bin/git".into(),
                    git_executable_digest:
                        "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                            .into(),
                    git_version: "git version fixture".into(),
                    config: config.clone(),
                },
            )
            .unwrap();
        let decoy_candidate = decoy_raw.observations[0].candidates[0].clone();
        raw.observations[0].candidates.push(decoy_candidate);
        raw.observations[0].complete_candidate_count = 2;
        raw.token_count = project(&raw, &config)
            .unwrap()
            .iter()
            .map(|item| item.snippet.len().div_ceil(4))
            .sum();
        let contaminated = grade(&unit, &raw, &root).unwrap();
        assert!(contaminated.localization_success);
        assert!(!contaminated.wrong_decoy_success);
        raw.observations[0].candidates[0].source_revision_digest =
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into();
        assert!(grade(&unit, &raw, &root).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(cache).unwrap();
        fs::remove_dir_all(decoy_cache).unwrap();
    }
}
