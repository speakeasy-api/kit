use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    fmt,
    io::{self, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::workspace::{
    index::meta::{ContentState, MetadataEntry, MetadataIndex},
    revision::{EpochId, LimitKind, ManagedWorkspace, RevisionError, RevisionId},
};

const SEARCH_CURSOR_STATE_TAG_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchMode {
    Path,
    Content,
    PathAndContent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchQuery {
    pub text: String,
    pub mode: SearchMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchOptions {
    pub path_prefixes: Vec<PathBuf>,
    pub languages: Vec<String>,
    pub max_query_bytes: usize,
    pub max_scanned_files: usize,
    pub max_scanned_bytes: u64,
    pub max_time: Duration,
    pub max_results: usize,
    pub max_result_bytes: usize,
    pub max_snippet_bytes: usize,
    pub max_cursor_offset: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            path_prefixes: Vec::new(),
            languages: Vec::new(),
            max_query_bytes: 4 * 1024,
            max_scanned_files: 100_000,
            max_scanned_bytes: 256 * 1024 * 1024,
            max_time: Duration::from_secs(2),
            max_results: 100,
            max_result_bytes: 256 * 1024,
            max_snippet_bytes: 2 * 1024,
            max_cursor_offset: 10_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchField {
    Path,
    Content,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SearchMatch {
    pub path: PathBuf,
    pub field: MatchField,
    pub score: u16,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub byte_start: usize,
    pub byte_end: usize,
    pub excerpt: String,
    pub excerpt_truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SkippedFiles {
    pub binary: usize,
    pub invalid_utf8: usize,
    pub too_large: usize,
    pub index_limited: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchCursor {
    epoch: EpochId,
    revision: RevisionId,
    digest: String,
    #[serde(serialize_with = "serialize_hex", deserialize_with = "deserialize_hex")]
    index_digest: [u8; 32],
    #[serde(serialize_with = "serialize_hex", deserialize_with = "deserialize_hex")]
    query_digest: [u8; 32],
    #[serde(serialize_with = "serialize_hex", deserialize_with = "deserialize_hex")]
    options_digest: [u8; 32],
    frontier: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projection_state_version: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projection_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    custody_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projection_state_tag: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SearchResponse {
    pub epoch: EpochId,
    pub revision: RevisionId,
    pub digest: String,
    pub matches: Vec<SearchMatch>,
    pub scanned_files: usize,
    pub scanned_bytes: u64,
    pub skipped: SkippedFiles,
    pub omitted: usize,
    pub omitted_complete: bool,
    pub result_bytes: usize,
    pub truncated: bool,
    pub cursor: Option<SearchCursor>,
}

impl SearchResponse {
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Debug)]
pub enum SearchError {
    Revision(RevisionError),
    TimeLimit,
    InvalidQuery(&'static str),
    InvalidOptions(&'static str),
    CursorMismatch,
    SingleResultTooLarge,
    Serialization(serde_json::Error),
}

impl From<RevisionError> for SearchError {
    fn from(value: RevisionError) -> Self {
        match value {
            RevisionError::LimitExceeded(LimitKind::Time) => Self::TimeLimit,
            value => Self::Revision(value),
        }
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::TimeLimit => formatter.write_str("search time limit exceeded"),
            Self::InvalidQuery(reason) => write!(formatter, "invalid lexical query: {reason}"),
            Self::InvalidOptions(reason) => write!(formatter, "invalid search options: {reason}"),
            Self::CursorMismatch => {
                formatter.write_str("search cursor does not match the query or options")
            }
            Self::SingleResultTooLarge => {
                formatter.write_str("one lexical search result exceeds the result byte bound")
            }
            Self::Serialization(error) => write!(formatter, "serialize search response: {error}"),
        }
    }
}

impl std::error::Error for SearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

pub fn search(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &SearchQuery,
    options: &SearchOptions,
    cursor: Option<&SearchCursor>,
) -> Result<SearchResponse, SearchError> {
    search_inner(workspace, index, query, options, cursor, true)
}

fn search_inner(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &SearchQuery,
    options: &SearchOptions,
    cursor: Option<&SearchCursor>,
    enforce_result_bytes: bool,
) -> Result<SearchResponse, SearchError> {
    let started = Instant::now();
    let deadline = started.checked_add(options.max_time).unwrap_or(started);
    validate_query(query, options)?;
    validate_options(options)?;
    workspace.validate_revision_until(index.revision(), deadline)?;

    let query_digest = digest_query(query);
    let options_digest = digest_options(options);
    let frontier = if let Some(cursor) = cursor {
        if cursor.epoch != index.epoch()
            || cursor.revision != index.revision()
            || cursor.digest != index.digest().as_str()
            || cursor.index_digest != *index.index_digest()
            || cursor.query_digest != query_digest
            || cursor.options_digest != options_digest
            || cursor.frontier > options.max_cursor_offset
        {
            return Err(SearchError::CursorMismatch);
        }
        cursor.frontier
    } else {
        0
    };
    let capacity = frontier
        .checked_add(options.max_results)
        .map(|value| value.min(options.max_cursor_offset))
        .ok_or(SearchError::InvalidOptions("result frontier overflow"))?;
    if capacity == frontier {
        return Err(SearchError::CursorMismatch);
    }

    let mut candidates = BinaryHeap::new();
    candidates
        .try_reserve(capacity)
        .map_err(|_| SearchError::InvalidOptions("candidate allocation failed"))?;
    let mut scanned_files = 0;
    let mut scanned_bytes = 0_u64;
    let mut skipped = SkippedFiles::default();
    let mut omitted = 0_usize;
    let mut work_cut = index.truncated();
    let mut snippet_cut = false;

    'files: for entry in index.entries() {
        if Instant::now() >= deadline {
            work_cut = true;
            break;
        }
        if entry.kind != crate::workspace::revision::EntryKind::File || !selected(entry, options) {
            continue;
        }
        if Instant::now() >= deadline
            || scanned_files == options.max_scanned_files
            || scanned_bytes.saturating_add(entry.size) > options.max_scanned_bytes
        {
            work_cut = true;
            break;
        }
        scanned_files += 1;
        scanned_bytes += entry.size;

        let path = entry.path.to_string_lossy();
        if matches!(query.mode, SearchMode::Path | SearchMode::PathAndContent)
            && let Some(start) = path.find(&query.text)
        {
            let end = start + query.text.len();
            retain(
                &mut candidates,
                RankedMatch(SearchMatch {
                    path: entry.path.clone(),
                    field: MatchField::Path,
                    score: path_score(&path, &query.text),
                    line: None,
                    column: None,
                    byte_start: start,
                    byte_end: end,
                    excerpt: path.into_owned(),
                    excerpt_truncated: false,
                }),
                capacity,
                &mut omitted,
            );
        }
        if !matches!(query.mode, SearchMode::Content | SearchMode::PathAndContent) {
            continue;
        }
        let Some(text) = entry.text() else {
            match entry.content_state {
                ContentState::Binary => skipped.binary += 1,
                ContentState::InvalidUtf8 => skipped.invalid_utf8 += 1,
                ContentState::TooLarge => skipped.too_large += 1,
                ContentState::IndexLimit => {
                    skipped.index_limited += 1;
                    work_cut = true;
                }
                ContentState::Directory | ContentState::Text => {}
            }
            continue;
        };
        for (start, _) in text.match_indices(&query.text) {
            if Instant::now() >= deadline {
                work_cut = true;
                break 'files;
            }
            let end = start + query.text.len();
            let (line, column, excerpt, excerpt_truncated) =
                excerpt(text, start, end, options.max_snippet_bytes);
            check_deadline(deadline)?;
            snippet_cut |= excerpt_truncated;
            retain(
                &mut candidates,
                RankedMatch(SearchMatch {
                    path: entry.path.clone(),
                    field: MatchField::Content,
                    score: if identifier_boundary(text, start, end) {
                        800
                    } else {
                        600
                    },
                    line: Some(line),
                    column: Some(column),
                    byte_start: start,
                    byte_end: end,
                    excerpt,
                    excerpt_truncated,
                }),
                capacity,
                &mut omitted,
            );
        }
        if Instant::now() >= deadline {
            work_cut = true;
            break;
        }
    }

    check_deadline(deadline)?;
    let mut candidates = candidates
        .into_vec()
        .into_iter()
        .map(|candidate| candidate.0)
        .collect::<Vec<_>>();
    candidates.sort_by(compare_matches);
    check_deadline(deadline)?;
    let retained = candidates.len();
    if frontier > retained {
        return Err(SearchError::CursorMismatch);
    }
    let mut matches = Vec::new();
    matches
        .try_reserve_exact(options.max_results.min(candidates.len()))
        .map_err(|_| SearchError::InvalidOptions("result allocation failed"))?;
    let mut response = SearchResponse {
        epoch: index.epoch(),
        revision: index.revision(),
        digest: index.digest().to_string(),
        matches,
        scanned_files,
        scanned_bytes,
        skipped,
        omitted: 0,
        omitted_complete: false,
        result_bytes: 0,
        truncated: false,
        cursor: None,
    };
    let mut consumed = frontier;
    for candidate in candidates.into_iter().skip(frontier) {
        check_deadline(deadline)?;
        if response.matches.len() == options.max_results {
            break;
        }
        consumed += 1;
        response.matches.push(candidate);
        set_page_metadata(
            &mut response,
            work_cut,
            snippet_cut,
            *index.index_digest(),
            query_digest,
            options_digest,
            options,
            frontier,
            consumed,
            retained,
            omitted,
        );
        set_result_bytes(&mut response, deadline)?;
        if enforce_result_bytes && response.result_bytes > options.max_result_bytes {
            response.matches.pop();
            consumed -= 1;
            if response.matches.is_empty() {
                return Err(SearchError::SingleResultTooLarge);
            }
            break;
        }
    }
    set_page_metadata(
        &mut response,
        work_cut,
        snippet_cut,
        *index.index_digest(),
        query_digest,
        options_digest,
        options,
        frontier,
        consumed,
        retained,
        omitted,
    );
    set_result_bytes(&mut response, deadline)?;
    if enforce_result_bytes && response.result_bytes > options.max_result_bytes {
        return Err(SearchError::InvalidOptions(
            "result byte bound is smaller than response metadata",
        ));
    }
    workspace.validate_revision_until(index.revision(), deadline)?;
    Ok(response)
}

pub fn search_projected(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &SearchQuery,
    options: &SearchOptions,
    cursor: Option<&SearchCursor>,
    custody: &crate::domain::secret::SecretCustody,
    cursor_key: &[u8; 32],
) -> Result<serde_json::Value, SearchError> {
    search_projected_with_state(
        workspace,
        index,
        query,
        options,
        cursor,
        custody,
        &mut crate::domain::secret::JsonProjectionState::default(),
        cursor_key,
        "",
        "",
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn search_projected_with_state(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &SearchQuery,
    options: &SearchOptions,
    cursor: Option<&SearchCursor>,
    custody: &crate::domain::secret::SecretCustody,
    projection_state: &mut crate::domain::secret::JsonProjectionState,
    cursor_key: &[u8; 32],
    principal: &str,
    project: &str,
) -> Result<serde_json::Value, SearchError> {
    validate_query(query, options)?;
    validate_options(options)?;
    if projection_state.custody_revision() != custody.revision() {
        *projection_state = custody.projection_state();
    }
    let mut initial_state = projection_state.clone();
    if let Some(cursor) = cursor {
        let cursor_state = open_projected_cursor(cursor, custody, cursor_key, principal, project)?;
        if !initial_state.merge_forward(cursor_state) {
            return Err(SearchError::CursorMismatch);
        }
    }
    let frontier = cursor.map_or(0, |cursor| cursor.frontier);
    let mut response = search_inner(workspace, index, query, options, cursor, false)?;
    response.cursor = None;
    let source_matches = std::mem::take(&mut response.matches);
    let source_omitted = response.omitted;
    let mut accepted = None;
    for count in 0..=source_matches.len() {
        let mut candidate = response.clone();
        candidate.matches = source_matches[..count].to_vec();
        candidate.omitted = source_omitted + source_matches.len() - count;
        candidate.truncated |= candidate.omitted != 0;
        let has_omitted = candidate.omitted != 0;
        let mut state = initial_state.clone();
        let value = serde_json::to_value(candidate).map_err(SearchError::Serialization)?;
        let mut projected = custody.project_json_stream(
            crate::telemetry::redact::CaptureBoundary::WorkspaceMetadata,
            &value,
            &mut state,
        );
        let next_frontier = frontier + count;
        let projected_cursor = (has_omitted && next_frontier < options.max_cursor_offset)
            .then(|| SearchCursor {
                epoch: index.epoch(),
                revision: index.revision(),
                digest: index.digest().to_string(),
                index_digest: *index.index_digest(),
                query_digest: digest_query(query),
                options_digest: digest_options(options),
                frontier: next_frontier,
                projection_state_version: None,
                projection_state: None,
                custody_revision: None,
                projection_state_tag: None,
            })
            .map(|cursor| seal_projected_cursor(cursor, &state, cursor_key, principal, project))
            .transpose()?;
        projected["cursor"] = projected_cursor
            .map(serde_json::to_value)
            .transpose()
            .map_err(SearchError::Serialization)?
            .unwrap_or(serde_json::Value::Null);
        settle_projected_size(&mut projected)?;
        if projected["result_bytes"].as_u64().unwrap_or(u64::MAX) <= options.max_result_bytes as u64
        {
            accepted = Some((projected, state));
        } else {
            break;
        }
    }
    let Some((projected, state)) = accepted else {
        return Err(SearchError::InvalidOptions(
            "result byte bound is smaller than projected response metadata",
        ));
    };
    if projected["matches"].as_array().is_some_and(Vec::is_empty) && !source_matches.is_empty() {
        return Err(SearchError::SingleResultTooLarge);
    }
    *projection_state = state;
    Ok(projected)
}

fn seal_projected_cursor(
    mut cursor: SearchCursor,
    state: &crate::domain::secret::JsonProjectionState,
    key: &[u8; 32],
    principal: &str,
    project: &str,
) -> Result<SearchCursor, SearchError> {
    let serialized = state.to_bounded_bytes().ok_or(SearchError::InvalidOptions(
        "projection state exceeds cursor bound",
    ))?;
    let associated = cursor_associated(&cursor, principal, project);
    let encrypted = xor_cursor_state(key, &associated, &serialized);
    let revision = state.custody_revision();
    let version = crate::domain::secret::JsonProjectionState::VERSION;
    let tag = crate::domain::crypto::hmac_sha256_domain(
        key,
        b"KIT-LEXICAL-CURSOR-STATE-TAG\0",
        &[
            &associated,
            &version.to_be_bytes(),
            &revision.to_be_bytes(),
            &encrypted,
        ],
    );
    cursor.projection_state_version = Some(version);
    cursor.projection_state = Some(hex(&encrypted));
    cursor.custody_revision = Some(revision);
    cursor.projection_state_tag = Some(hex(&tag[..SEARCH_CURSOR_STATE_TAG_BYTES]));
    Ok(cursor)
}

fn open_projected_cursor(
    cursor: &SearchCursor,
    custody: &crate::domain::secret::SecretCustody,
    key: &[u8; 32],
    principal: &str,
    project: &str,
) -> Result<crate::domain::secret::JsonProjectionState, SearchError> {
    let fields = (
        cursor.projection_state_version,
        cursor.projection_state.as_deref(),
        cursor.custody_revision,
        cursor.projection_state_tag.as_deref(),
    );
    let (Some(version), Some(encoded), Some(revision), Some(encoded_tag)) = fields else {
        return if fields == (None, None, None, None) && custody.is_empty() {
            Ok(crate::domain::secret::JsonProjectionState::default())
        } else {
            Err(SearchError::CursorMismatch)
        };
    };
    if version != crate::domain::secret::JsonProjectionState::VERSION
        || revision != custody.revision()
    {
        return Err(SearchError::CursorMismatch);
    }
    let encrypted = decode_hex(encoded).ok_or(SearchError::CursorMismatch)?;
    let actual_tag = decode_hex(encoded_tag).ok_or(SearchError::CursorMismatch)?;
    let associated = cursor_associated(cursor, principal, project);
    let expected_tag = crate::domain::crypto::hmac_sha256_domain(
        key,
        b"KIT-LEXICAL-CURSOR-STATE-TAG\0",
        &[
            &associated,
            &version.to_be_bytes(),
            &revision.to_be_bytes(),
            &encrypted,
        ],
    );
    if !crate::domain::crypto::constant_time_eq(
        &actual_tag,
        &expected_tag[..SEARCH_CURSOR_STATE_TAG_BYTES],
    ) {
        return Err(SearchError::CursorMismatch);
    }
    let serialized = xor_cursor_state(key, &associated, &encrypted);
    let state = crate::domain::secret::JsonProjectionState::from_bounded_bytes(&serialized)
        .ok_or(SearchError::CursorMismatch)?;
    (state.custody_revision() == revision)
        .then_some(state)
        .ok_or(SearchError::CursorMismatch)
}

fn xor_cursor_state(key: &[u8; 32], associated: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    for (counter, chunk) in bytes.chunks(32).enumerate() {
        let mask = crate::domain::crypto::hmac_sha256_domain(
            key,
            b"KIT-LEXICAL-CURSOR-STATE-MASK\0",
            &[associated, &(counter as u64).to_be_bytes()],
        );
        output.extend(chunk.iter().zip(mask).map(|(byte, mask)| byte ^ mask));
    }
    output
}

fn cursor_associated(cursor: &SearchCursor, principal: &str, project: &str) -> Vec<u8> {
    let mut bytes = b"KIT-LEXICAL-CURSOR\0\x02".to_vec();
    put_bytes(&mut bytes, principal.as_bytes());
    put_bytes(&mut bytes, project.as_bytes());
    bytes.extend_from_slice(cursor.epoch.as_bytes());
    bytes.extend_from_slice(cursor.revision.as_bytes());
    put_bytes(&mut bytes, cursor.digest.as_bytes());
    bytes.extend_from_slice(&cursor.index_digest);
    bytes.extend_from_slice(&cursor.query_digest);
    bytes.extend_from_slice(&cursor.options_digest);
    bytes.extend_from_slice(&(cursor.frontier as u64).to_be_bytes());
    bytes
}

fn put_bytes(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    push_hex(&mut output, bytes);
    output
}

fn push_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
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

fn settle_projected_size(value: &mut serde_json::Value) -> Result<(), SearchError> {
    for _ in 0..usize::MAX.to_string().len() + 2 {
        let bytes = serde_json::to_vec(value)
            .map_err(SearchError::Serialization)?
            .len();
        if value["result_bytes"].as_u64() == Some(bytes as u64) {
            return Ok(());
        }
        value["result_bytes"] = serde_json::Value::from(bytes);
    }
    Err(SearchError::Serialization(serde_json::Error::io(
        io::Error::other("projected search response length did not converge"),
    )))
}

fn validate_query(query: &SearchQuery, options: &SearchOptions) -> Result<(), SearchError> {
    if query.text.is_empty() {
        return Err(SearchError::InvalidQuery("query is empty"));
    }
    if query.text.len() > options.max_query_bytes {
        return Err(SearchError::InvalidQuery("query is too long"));
    }
    if query.text.len() > options.max_snippet_bytes {
        return Err(SearchError::InvalidOptions(
            "snippet bound is smaller than the query",
        ));
    }
    if query.text.chars().any(char::is_control) {
        return Err(SearchError::InvalidQuery(
            "control characters are not allowed",
        ));
    }
    Ok(())
}

fn validate_options(options: &SearchOptions) -> Result<(), SearchError> {
    if options.max_query_bytes == 0
        || options.max_scanned_files == 0
        || options.max_scanned_bytes == 0
        || options.max_time.is_zero()
        || options.max_results == 0
        || options.max_result_bytes == 0
        || options.max_snippet_bytes == 0
        || options.max_cursor_offset < options.max_results
    {
        return Err(SearchError::InvalidOptions(
            "all bounds must be nonzero and consistent",
        ));
    }
    if options.languages.iter().any(|value| value.is_empty()) {
        return Err(SearchError::InvalidOptions(
            "language filters must not be empty",
        ));
    }
    if options.path_prefixes.iter().any(|path| {
        path.as_os_str().is_empty()
            || path.is_absolute()
            || !path
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
    }) {
        return Err(SearchError::InvalidOptions(
            "path filters must be canonical and root-relative",
        ));
    }
    Ok(())
}

fn selected(entry: &MetadataEntry, options: &SearchOptions) -> bool {
    (options.path_prefixes.is_empty()
        || options
            .path_prefixes
            .iter()
            .any(|prefix| entry.path.starts_with(prefix)))
        && (options.languages.is_empty()
            || entry.language.as_ref().is_some_and(|language| {
                options
                    .languages
                    .iter()
                    .any(|selected| selected == language)
            }))
}

fn path_score(path: &str, query: &str) -> u16 {
    let name = path.rsplit('/').next().unwrap_or(path);
    if path == query {
        1_000
    } else if name == query {
        950
    } else if path.split('/').any(|part| part == query) {
        900
    } else if name.starts_with(query) {
        850
    } else {
        700
    }
}

fn identifier_boundary(text: &str, start: usize, end: usize) -> bool {
    let identifier = |value: char| value.is_alphanumeric() || value == '_';
    text[..start]
        .chars()
        .next_back()
        .is_none_or(|value| !identifier(value))
        && text[end..]
            .chars()
            .next()
            .is_none_or(|value| !identifier(value))
}

fn excerpt(text: &str, start: usize, end: usize, max_bytes: usize) -> (usize, usize, String, bool) {
    let line_start = text[..start].rfind('\n').map_or(0, |at| at + 1);
    let mut line_end = text[end..].find('\n').map_or(text.len(), |at| end + at);
    if line_end > line_start && text.as_bytes()[line_end - 1] == b'\r' {
        line_end -= 1;
    }
    let line = text[..line_start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    let column = text[line_start..start].chars().count() + 1;
    let (snippet_start, snippet_end) = if line_end - line_start <= max_bytes {
        (line_start, line_end)
    } else {
        let before = max_bytes.saturating_sub(end - start) / 2;
        let mut from = start.saturating_sub(before).max(line_start);
        let mut to = from.saturating_add(max_bytes).min(line_end);
        from = floor_char_boundary(text, from);
        to = floor_char_boundary(text, to);
        if to < end {
            to = end;
            from = ceil_char_boundary(text, end.saturating_sub(max_bytes)).max(line_start);
        }
        (from, to)
    };
    (
        line,
        column,
        text[snippet_start..snippet_end].to_owned(),
        snippet_start != line_start || snippet_end != line_end,
    )
}

fn floor_char_boundary(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_char_boundary(text: &str, mut at: usize) -> usize {
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

fn retain(
    candidates: &mut BinaryHeap<RankedMatch>,
    candidate: RankedMatch,
    capacity: usize,
    omitted: &mut usize,
) {
    if capacity == 0 {
        *omitted += 1;
        return;
    }
    if candidates.len() < capacity {
        candidates.push(candidate);
        return;
    }
    if candidate < *candidates.peek().expect("a full candidate set is nonempty") {
        candidates.pop();
        candidates.push(candidate);
    }
    *omitted += 1;
}

#[derive(Eq, PartialEq)]
struct RankedMatch(SearchMatch);

impl Ord for RankedMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_matches(&self.0, &other.0)
    }
}

impl PartialOrd for RankedMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_matches(left: &SearchMatch, right: &SearchMatch) -> Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.line.cmp(&right.line))
        .then_with(|| left.byte_start.cmp(&right.byte_start))
        .then_with(|| left.field.cmp(&right.field))
}

#[allow(clippy::too_many_arguments)]
fn set_page_metadata(
    response: &mut SearchResponse,
    work_cut: bool,
    snippet_cut: bool,
    index_digest: [u8; 32],
    query_digest: [u8; 32],
    options_digest: [u8; 32],
    options: &SearchOptions,
    frontier: usize,
    consumed: usize,
    retained: usize,
    rank_omitted: usize,
) {
    response.omitted = rank_omitted + retained - frontier - response.matches.len();
    response.omitted_complete = !work_cut;
    response.truncated = work_cut || snippet_cut || response.omitted != 0;
    response.cursor = (!work_cut
        && !snippet_cut
        && (consumed < retained || rank_omitted != 0)
        && consumed > frontier
        && consumed < options.max_cursor_offset)
        .then(|| SearchCursor {
            epoch: response.epoch,
            revision: response.revision,
            digest: response.digest.clone(),
            index_digest,
            query_digest,
            options_digest,
            frontier: consumed,
            projection_state_version: None,
            projection_state: None,
            custody_revision: None,
            projection_state_tag: None,
        });
}

fn set_result_bytes(response: &mut SearchResponse, deadline: Instant) -> Result<(), SearchError> {
    for _ in 0..usize::MAX.to_string().len() + 2 {
        check_deadline(deadline)?;
        let bytes = serialized_bytes(response, deadline)?;
        if response.result_bytes == bytes {
            return Ok(());
        }
        response.result_bytes = bytes;
    }
    Err(SearchError::Serialization(serde_json::Error::io(
        io::Error::other("serialized search response length did not converge"),
    )))
}

fn serialized_bytes(value: &impl Serialize, deadline: Instant) -> Result<usize, SearchError> {
    serialized_bytes_with(value, deadline, || {})
}

fn serialized_bytes_with(
    value: &impl Serialize,
    deadline: Instant,
    before_write: impl FnMut(),
) -> Result<usize, SearchError> {
    let mut writer = CountingWriter {
        bytes: 0,
        deadline,
        before_write,
    };
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        if error.io_error_kind() == Some(io::ErrorKind::TimedOut) {
            SearchError::TimeLimit
        } else {
            SearchError::Serialization(error)
        }
    })?;
    Ok(writer.bytes)
}

struct CountingWriter<F> {
    bytes: usize,
    deadline: Instant,
    before_write: F,
}

impl<F: FnMut()> Write for CountingWriter<F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (self.before_write)();
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "search time limit exceeded",
            ));
        }
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("serialized search response length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn check_deadline(deadline: Instant) -> Result<(), SearchError> {
    if Instant::now() >= deadline {
        Err(SearchError::TimeLimit)
    } else {
        Ok(())
    }
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

fn deserialize_hex<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.len() != N * 2 {
        return Err(de::Error::custom("invalid hex digest length"));
    }
    let mut output = [0_u8; N];
    for (byte, digits) in output.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let digits =
            std::str::from_utf8(digits).map_err(|_| de::Error::custom("invalid hex digest"))?;
        *byte =
            u8::from_str_radix(digits, 16).map_err(|_| de::Error::custom("invalid hex digest"))?;
    }
    Ok(output)
}

#[cfg(test)]
mod projection_state_tests {
    use super::*;
    use crate::domain::secret::{REDACTED, SecretCustody, SecretLease};
    use std::sync::Arc;

    #[test]
    fn cursorless_search_continues_the_callers_projection_state_and_seals_it() {
        let root = std::env::temp_dir().join(format!(
            "kit-search-state-{}",
            crate::domain::ids::RunId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "needle first\n").unwrap();
        std::fs::write(root.join("b.txt"), "needle second\n").unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let workspace = ManagedWorkspace::open(&root).unwrap();
        let revision = workspace.current_revision().unwrap();
        let index = MetadataIndex::build(
            &workspace,
            revision.id(),
            &crate::workspace::index::meta::IndexOptions::default(),
        )
        .unwrap();
        let custody = SecretCustody::new([Arc::new(SecretLease::new(format!(
            "priornull{}",
            index.digest()
        )))]);
        let mut state = custody.projection_state();
        custody.project_json_stream(
            crate::telemetry::redact::CaptureBoundary::WorkspaceMetadata,
            &serde_json::json!("prior"),
            &mut state,
        );

        let projected = search_projected_with_state(
            &workspace,
            &index,
            &SearchQuery {
                text: "needle".to_owned(),
                mode: SearchMode::Content,
            },
            &SearchOptions {
                max_results: 1,
                ..SearchOptions::default()
            },
            None,
            &custody,
            &mut state,
            &[7; 32],
            "principal",
            "project",
        )
        .unwrap();

        assert_eq!(projected["digest"], REDACTED);
        assert!(projected["cursor"]["projection_state"].is_string());
        assert_eq!(state.custody_revision(), custody.revision());
        let cursor: SearchCursor = serde_json::from_value(projected["cursor"].clone()).unwrap();
        custody.project_json_stream(
            crate::telemetry::redact::CaptureBoundary::WorkspaceMetadata,
            &serde_json::json!("intervening output"),
            &mut state,
        );
        assert!(matches!(
            search_projected_with_state(
                &workspace,
                &index,
                &SearchQuery {
                    text: "needle".to_owned(),
                    mode: SearchMode::Content,
                },
                &SearchOptions {
                    max_results: 1,
                    ..SearchOptions::default()
                },
                Some(&cursor),
                &custody,
                &mut state,
                &[7; 32],
                "principal",
                "project",
            ),
            Err(SearchError::CursorMismatch)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn digest_query(query: &SearchQuery) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-lexical-query-v1\0");
    hash.update(&[query.mode as u8]);
    hash.update(query.text.as_bytes());
    *hash.finalize().as_bytes()
}

fn digest_options(options: &SearchOptions) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-lexical-options-v1\0");
    for path in &options.path_prefixes {
        hash.update(&(path.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
        hash.update(path.as_os_str().as_encoded_bytes());
    }
    for language in &options.languages {
        hash.update(&(language.len() as u64).to_le_bytes());
        hash.update(language.as_bytes());
    }
    for value in [
        options.max_query_bytes as u128,
        options.max_scanned_files as u128,
        options.max_scanned_bytes as u128,
        options.max_time.as_nanos(),
        options.max_results as u128,
        options.max_result_bytes as u128,
        options.max_snippet_bytes as u128,
        options.max_cursor_offset as u128,
    ] {
        hash.update(&value.to_le_bytes());
    }
    *hash.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, thread};

    #[test]
    fn counting_serialization_stops_at_the_search_deadline() {
        let writes = Cell::new(0);
        let started = Instant::now();
        let result = serialized_bytes_with(
            &vec!["candidate"; 10_000],
            started + Duration::from_millis(1),
            || {
                writes.set(writes.get() + 1);
                thread::sleep(Duration::from_millis(5));
            },
        );

        assert!(matches!(result, Err(SearchError::TimeLimit)));
        assert_eq!(writes.get(), 1);
        assert!(started.elapsed() < Duration::from_millis(250));
    }
}
