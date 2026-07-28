use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    fmt,
    io::{self, Write},
    mem::size_of,
    path::{Component, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::workspace::{
    index::meta::{MetadataEntry, MetadataIndex},
    revision::{EntryKind, LimitKind, ManagedWorkspace, RevisionError, RevisionId},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoverQuery {
    pub terms: Vec<String>,
    pub roots: Vec<PathBuf>,
    pub languages: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoverOptions {
    pub max_terms: usize,
    pub max_query_bytes: usize,
    pub max_scanned_entries: usize,
    pub max_scanned_bytes: u64,
    pub max_rank_work: usize,
    pub max_candidate_bytes: usize,
    pub max_results: usize,
    pub max_results_per_path: usize,
    pub max_result_bytes: usize,
    pub max_excerpt_bytes: usize,
    pub max_cursor_offset: usize,
    pub max_time: Duration,
}

impl Default for DiscoverOptions {
    fn default() -> Self {
        Self {
            max_terms: 32,
            max_query_bytes: 4 * 1024,
            max_scanned_entries: 100_000,
            max_scanned_bytes: 256 * 1024 * 1024,
            max_rank_work: 1_000_000,
            max_candidate_bytes: 16 * 1024 * 1024,
            max_results: 50,
            max_results_per_path: 2,
            max_result_bytes: 256 * 1024,
            max_excerpt_bytes: 512,
            max_cursor_offset: 10_000,
            max_time: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverKind {
    Path,
    Symbol,
    Content,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiscoverResult {
    pub path: PathBuf,
    pub kind: DiscoverKind,
    pub score: u16,
    pub rationale: &'static str,
    pub symbol: Option<String>,
    pub line: Option<usize>,
    pub byte_start: Option<usize>,
    pub byte_end: Option<usize>,
    pub excerpt: Option<String>,
    pub excerpt_truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiscoverCursor {
    revision: RevisionId,
    #[serde(serialize_with = "serialize_hex")]
    index_digest: [u8; 32],
    #[serde(serialize_with = "serialize_hex")]
    query_digest: [u8; 32],
    #[serde(serialize_with = "serialize_hex")]
    options_digest: [u8; 32],
    frontier: usize,
    #[serde(serialize_with = "serialize_hex")]
    canonical_digest: [u8; 32],
}

impl DiscoverCursor {
    pub fn canonical_digest(&self) -> [u8; 32] {
        self.canonical_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiscoverResponse {
    pub revision: RevisionId,
    pub results: Vec<DiscoverResult>,
    pub scanned_entries: usize,
    pub scanned_bytes: u64,
    pub omitted: usize,
    pub omitted_complete: bool,
    pub truncated: bool,
    pub result_bytes: usize,
    pub cursor: Option<DiscoverCursor>,
}

impl DiscoverResponse {
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Debug)]
pub enum DiscoverError {
    Revision(RevisionError),
    TimeLimit,
    InvalidQuery(&'static str),
    InvalidOptions(&'static str),
    CursorMismatch,
    Serialization(serde_json::Error),
}

impl From<RevisionError> for DiscoverError {
    fn from(value: RevisionError) -> Self {
        match value {
            RevisionError::LimitExceeded(LimitKind::Time) => Self::TimeLimit,
            value => Self::Revision(value),
        }
    }
}

impl fmt::Display for DiscoverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::TimeLimit => formatter.write_str("discover time limit exceeded"),
            Self::InvalidQuery(reason) => write!(formatter, "invalid discover query: {reason}"),
            Self::InvalidOptions(reason) => {
                write!(formatter, "invalid discover options: {reason}")
            }
            Self::CursorMismatch => formatter.write_str("discover cursor does not match request"),
            Self::Serialization(error) => write!(formatter, "serialize discover response: {error}"),
        }
    }
}

impl std::error::Error for DiscoverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

pub fn discover(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    query: &DiscoverQuery,
    options: &DiscoverOptions,
    cursor: Option<&DiscoverCursor>,
) -> Result<DiscoverResponse, DiscoverError> {
    let started = Instant::now();
    let deadline = started.checked_add(options.max_time).unwrap_or(started);
    validate(query, options, deadline)?;
    workspace.validate_revision_until(index.revision(), deadline)?;
    let query_digest = digest_query(query, deadline)?;
    let options_digest = digest_options(options, deadline)?;
    let frontier = cursor.map_or(Ok(0), |cursor| {
        check_deadline(deadline)?;
        let expected = cursor_digest(
            cursor.revision,
            cursor.index_digest,
            cursor.query_digest,
            cursor.options_digest,
            cursor.frontier,
        );
        if cursor.revision != index.revision()
            || cursor.index_digest != *index.index_digest()
            || cursor.query_digest != query_digest
            || cursor.options_digest != options_digest
            || cursor.frontier > options.max_cursor_offset
            || cursor.canonical_digest != expected
        {
            Err(DiscoverError::CursorMismatch)
        } else {
            Ok(cursor.frontier)
        }
    })?;
    let capacity = options.max_cursor_offset;
    let structural_bytes = structural_candidate_bytes(options)?;
    let dynamic_budget = options.max_candidate_bytes - structural_bytes;
    let mut candidates = BinaryHeap::new();
    candidates
        .try_reserve_exact(capacity)
        .map_err(|_| DiscoverError::InvalidOptions("candidate allocation failed"))?;
    let mut scanned_entries = 0;
    let mut scanned_bytes = 0_u64;
    let mut rank_work = 0_usize;
    let mut rank_omitted = 0_usize;
    let mut candidate_bytes = 0_usize;
    let mut work_cut = index.truncated();
    let mut retention_cut = false;
    let mut excerpt_cut = false;

    'entries: for entry in index.entries() {
        check_deadline(deadline)?;
        if !selected(entry, query, deadline)? || entry.kind != EntryKind::File {
            continue;
        }
        if scanned_entries == options.max_scanned_entries
            || scanned_bytes.saturating_add(entry.size) > options.max_scanned_bytes
        {
            work_cut = true;
            break;
        }
        scanned_entries += 1;
        scanned_bytes += entry.size;

        let path = entry.path.to_str().ok_or(DiscoverError::InvalidOptions(
            "index contains a non-UTF-8 path",
        ))?;
        if let Some((score, rationale)) = path_relevance(path, &query.terms, deadline)? {
            if rank_work == options.max_rank_work {
                work_cut = true;
                break;
            }
            retention_cut |= retain(
                &mut candidates,
                Candidate {
                    path: &entry.path,
                    kind: DiscoverKind::Path,
                    score,
                    rationale,
                    symbol: None,
                    line: None,
                    byte_start: None,
                    byte_end: None,
                    excerpt: Some(path),
                    excerpt_truncated: false,
                },
                capacity,
                options.max_results_per_path,
                &mut candidate_bytes,
                dynamic_budget,
                &mut rank_omitted,
            )?;
            rank_work += 1;
        }
        for symbol in &entry.symbols {
            check_deadline(deadline)?;
            let Some((score, rationale)) = term_relevance(&symbol.name, &query.terms, deadline)?
            else {
                continue;
            };
            if rank_work == options.max_rank_work {
                work_cut = true;
                break 'entries;
            }
            retention_cut |= retain(
                &mut candidates,
                Candidate {
                    path: &entry.path,
                    kind: DiscoverKind::Symbol,
                    score,
                    rationale,
                    symbol: Some(&symbol.name),
                    line: Some(symbol.line),
                    byte_start: None,
                    byte_end: None,
                    excerpt: None,
                    excerpt_truncated: false,
                },
                capacity,
                options.max_results_per_path,
                &mut candidate_bytes,
                dynamic_budget,
                &mut rank_omitted,
            )?;
            rank_work += 1;
        }
        if let Some(text) = entry.text()
            && let Some((start, term)) = first_content_match(text, &query.terms, deadline)?
        {
            if rank_work == options.max_rank_work {
                work_cut = true;
                break;
            }
            let end = start + term.len();
            let boundary = identifier_boundary(text, start, end);
            let (line, excerpt, excerpt_truncated) =
                excerpt(text, start, end, options.max_excerpt_bytes, deadline)?;
            excerpt_cut |= excerpt_truncated;
            retention_cut |= retain(
                &mut candidates,
                Candidate {
                    path: &entry.path,
                    kind: DiscoverKind::Content,
                    score: if boundary { 800 } else { 700 },
                    rationale: if boundary {
                        "exact identifier in content"
                    } else {
                        "exact term in content"
                    },
                    symbol: None,
                    line: Some(line),
                    byte_start: Some(start),
                    byte_end: Some(end),
                    excerpt: Some(excerpt),
                    excerpt_truncated,
                },
                capacity,
                options.max_results_per_path,
                &mut candidate_bytes,
                dynamic_budget,
                &mut rank_omitted,
            )?;
            rank_work += 1;
        }
        if rank_work == options.max_rank_work {
            work_cut = true;
            break;
        }
    }

    let mut ranked = candidates.into_vec();
    ranked.sort();
    if frontier > ranked.len() {
        return Err(DiscoverError::CursorMismatch);
    }
    let requested_take = options.max_results.min(ranked.len() - frontier);
    let mut take = requested_take;
    let mut clone_bytes = 0_usize;
    for (offset, ranked) in ranked[frontier..frontier + requested_take]
        .iter()
        .enumerate()
    {
        let Some(next) =
            candidate_dynamic_size(&ranked.0).and_then(|bytes| clone_bytes.checked_add(bytes))
        else {
            take = offset;
            retention_cut = true;
            break;
        };
        if candidate_bytes.saturating_add(next) > dynamic_budget {
            take = offset;
            retention_cut = true;
            break;
        }
        clone_bytes = next;
    }
    let mut results = Vec::new();
    results
        .try_reserve_exact(take)
        .map_err(|_| DiscoverError::InvalidOptions("result allocation failed"))?;
    for ranked in &ranked[frontier..frontier + take] {
        results.push(clone_result(&ranked.0)?);
    }
    let mut response = DiscoverResponse {
        revision: index.revision(),
        results,
        scanned_entries,
        scanned_bytes,
        omitted: 0,
        omitted_complete: !work_cut,
        truncated: false,
        result_bytes: 0,
        cursor: None,
    };
    loop {
        set_page(
            &mut response,
            ranked.len(),
            rank_omitted,
            work_cut,
            retention_cut || excerpt_cut,
            frontier,
            *index.index_digest(),
            query_digest,
            options_digest,
            options,
            deadline,
        )?;
        settle_size(&mut response, deadline)?;
        if response.result_bytes <= options.max_result_bytes {
            break;
        }
        if response.results.pop().is_none() {
            return Err(DiscoverError::InvalidOptions(
                "result byte bound is smaller than response metadata",
            ));
        }
    }
    workspace.validate_revision_until(index.revision(), deadline)?;
    Ok(response)
}

fn validate(
    query: &DiscoverQuery,
    options: &DiscoverOptions,
    deadline: Instant,
) -> Result<(), DiscoverError> {
    if options.max_terms == 0
        || options.max_query_bytes == 0
        || options.max_scanned_entries == 0
        || options.max_scanned_bytes == 0
        || options.max_rank_work == 0
        || options.max_candidate_bytes == 0
        || options.max_results == 0
        || options.max_results_per_path == 0
        || options.max_results_per_path > options.max_cursor_offset
        || options.max_result_bytes == 0
        || options.max_excerpt_bytes == 0
        || options.max_cursor_offset < options.max_results
        || options.max_time.is_zero()
    {
        return Err(DiscoverError::InvalidOptions(
            "all bounds must be nonzero and consistent",
        ));
    }
    structural_candidate_bytes(options)?;
    let value_count = query
        .terms
        .len()
        .checked_add(query.roots.len())
        .and_then(|count| count.checked_add(query.languages.len()))
        .ok_or(DiscoverError::InvalidQuery("query has too many values"))?;
    if value_count > options.max_terms {
        return Err(DiscoverError::InvalidQuery("query exceeds its bound"));
    }
    let mut query_bytes = 0_usize;
    for bytes in query
        .terms
        .iter()
        .map(|term| term.len())
        .chain(
            query
                .roots
                .iter()
                .map(|root| root.as_os_str().as_encoded_bytes().len()),
        )
        .chain(query.languages.iter().map(String::len))
    {
        check_deadline(deadline)?;
        query_bytes = query_bytes
            .checked_add(bytes)
            .ok_or(DiscoverError::InvalidQuery("query is too long"))?;
    }
    if query.terms.is_empty() || query.terms.iter().any(|term| term.is_empty()) {
        return Err(DiscoverError::InvalidQuery("terms must not be empty"));
    }
    if query_bytes > options.max_query_bytes {
        return Err(DiscoverError::InvalidQuery("query exceeds its bound"));
    }
    if query
        .terms
        .iter()
        .any(|term| term.chars().any(char::is_control))
    {
        return Err(DiscoverError::InvalidQuery(
            "control characters are not allowed",
        ));
    }
    if query.roots.iter().any(|path| {
        path.as_os_str().is_empty()
            || path.is_absolute()
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
    }) {
        return Err(DiscoverError::InvalidQuery(
            "roots must be canonical and root-relative",
        ));
    }
    if query.languages.iter().any(String::is_empty) {
        return Err(DiscoverError::InvalidQuery("languages must not be empty"));
    }
    Ok(())
}

fn selected(
    entry: &MetadataEntry,
    query: &DiscoverQuery,
    deadline: Instant,
) -> Result<bool, DiscoverError> {
    let mut root_selected = query.roots.is_empty();
    for root in &query.roots {
        check_deadline(deadline)?;
        root_selected |= entry.path.starts_with(root);
    }
    let mut language_selected = query.languages.is_empty();
    if let Some(language) = &entry.language {
        for selected in &query.languages {
            check_deadline(deadline)?;
            language_selected |= selected == language;
        }
    }
    Ok(root_selected && language_selected)
}

fn path_relevance(
    path: &str,
    terms: &[String],
    deadline: Instant,
) -> Result<Option<(u16, &'static str)>, DiscoverError> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut best: Option<(u16, &'static str)> = None;
    for term in terms {
        check_deadline(deadline)?;
        let found = if path == term {
            Some((950, "exact path match"))
        } else if name == term {
            Some((925, "exact file-name match"))
        } else if name.starts_with(term) {
            Some((875, "file-name prefix match"))
        } else if path.contains(term) {
            Some((825, "term in path"))
        } else {
            None
        };
        if found.is_some_and(|found| best.is_none_or(|best| found.0 > best.0)) {
            best = found;
        }
    }
    Ok(best)
}

fn term_relevance(
    value: &str,
    terms: &[String],
    deadline: Instant,
) -> Result<Option<(u16, &'static str)>, DiscoverError> {
    let mut contains = false;
    for term in terms {
        check_deadline(deadline)?;
        if value == term {
            return Ok(Some((1_000, "exact symbol match")));
        }
        contains |= value.contains(term);
    }
    Ok(contains.then_some((900, "term in symbol")))
}

fn first_content_match<'a>(
    text: &str,
    terms: &'a [String],
    deadline: Instant,
) -> Result<Option<(usize, &'a str)>, DiscoverError> {
    let mut best: Option<(usize, &'a str)> = None;
    for term in terms {
        check_deadline(deadline)?;
        if let Some(start) = text.find(term) {
            let candidate = (start, term.as_str());
            if best.is_none_or(|best| {
                candidate
                    .0
                    .cmp(&best.0)
                    .then_with(|| candidate.1.cmp(best.1))
                    .is_lt()
            }) {
                best = Some(candidate);
            }
        }
    }
    Ok(best)
}

fn excerpt(
    text: &str,
    start: usize,
    end: usize,
    max: usize,
    deadline: Instant,
) -> Result<(usize, &str, bool), DiscoverError> {
    let line_start = text[..start].rfind('\n').map_or(0, |at| at + 1);
    check_deadline(deadline)?;
    let mut line_end = text[end..].find('\n').map_or(text.len(), |at| end + at);
    if line_end > line_start && text.as_bytes()[line_end - 1] == b'\r' {
        line_end -= 1;
    }
    let mut line = 1;
    for chunk in text.as_bytes()[..line_start].chunks(4096) {
        check_deadline(deadline)?;
        line += chunk.iter().filter(|byte| **byte == b'\n').count();
    }
    if line_end - line_start <= max {
        return Ok((line, &text[line_start..line_end], false));
    }
    let mut from = start
        .saturating_sub(max.saturating_sub(end - start) / 2)
        .max(line_start);
    let mut to = from.saturating_add(max).min(line_end);
    while from < text.len() && !text.is_char_boundary(from) {
        from += 1;
    }
    while to > from && !text.is_char_boundary(to) {
        to -= 1;
    }
    if to < end {
        to = end;
        from = end.saturating_sub(max).max(line_start);
        while from < end && !text.is_char_boundary(from) {
            from += 1;
        }
    }
    check_deadline(deadline)?;
    Ok((line, &text[from..to], true))
}

fn copy_path(path: &std::path::Path) -> Result<PathBuf, DiscoverError> {
    let text = path.to_str().ok_or(DiscoverError::InvalidOptions(
        "index contains a non-UTF-8 path",
    ))?;
    Ok(PathBuf::from(copy_string(text)?))
}

fn copy_string(value: &str) -> Result<String, DiscoverError> {
    let mut copied = String::new();
    copied
        .try_reserve_exact(value.len())
        .map_err(|_| DiscoverError::InvalidOptions("candidate allocation failed"))?;
    copied.push_str(value);
    Ok(copied)
}

fn clone_result(value: &DiscoverResult) -> Result<DiscoverResult, DiscoverError> {
    Ok(DiscoverResult {
        path: copy_path(&value.path)?,
        kind: value.kind,
        score: value.score,
        rationale: value.rationale,
        symbol: value.symbol.as_deref().map(copy_string).transpose()?,
        line: value.line,
        byte_start: value.byte_start,
        byte_end: value.byte_end,
        excerpt: value.excerpt.as_deref().map(copy_string).transpose()?,
        excerpt_truncated: value.excerpt_truncated,
    })
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

fn retain(
    heap: &mut BinaryHeap<Ranked>,
    value: Candidate<'_>,
    capacity: usize,
    max_per_path: usize,
    retained_bytes: &mut usize,
    max_bytes: usize,
    omitted: &mut usize,
) -> Result<bool, DiscoverError> {
    let Some(value_bytes) = value.dynamic_size() else {
        *omitted += 1;
        return Ok(true);
    };
    if heap
        .iter()
        .filter(|candidate| candidate.0.path == value.path)
        .count()
        == max_per_path
    {
        let mut values = std::mem::take(heap).into_vec();
        let worst = values
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.0.path == value.path)
            .max_by(|(_, left), (_, right)| left.cmp(right))
            .map(|(index, _)| index)
            .expect("the retained path count is nonzero");
        if value.compare_result(&values[worst].0).is_lt() {
            let replaced_bytes = candidate_dynamic_size(&values[worst].0)
                .expect("owned candidate size was checked before allocation");
            let Some(next_bytes) = retained_bytes
                .checked_sub(replaced_bytes)
                .and_then(|bytes| bytes.checked_add(value_bytes))
            else {
                *heap = BinaryHeap::from(values);
                *omitted += 1;
                return Ok(true);
            };
            if next_bytes > max_bytes {
                *heap = BinaryHeap::from(values);
                *omitted += 1;
                return Ok(true);
            }
            drop(values.swap_remove(worst));
            values.push(Ranked(value.to_owned()?));
            *retained_bytes = next_bytes;
        }
        *heap = BinaryHeap::from(values);
        *omitted += 1;
        return Ok(false);
    }
    if heap.len() < capacity {
        let Some(next_bytes) = retained_bytes.checked_add(value_bytes) else {
            *omitted += 1;
            return Ok(true);
        };
        if next_bytes > max_bytes {
            *omitted += 1;
            return Ok(true);
        }
        heap.push(Ranked(value.to_owned()?));
        *retained_bytes = next_bytes;
    } else {
        if value
            .compare_result(&heap.peek().expect("full candidate heap is nonempty").0)
            .is_lt()
        {
            let replaced = heap.pop().expect("full candidate heap is nonempty");
            let replaced_bytes = candidate_dynamic_size(&replaced.0)
                .expect("owned candidate size was checked before allocation");
            let Some(next_bytes) = retained_bytes
                .checked_sub(replaced_bytes)
                .and_then(|bytes| bytes.checked_add(value_bytes))
            else {
                heap.push(replaced);
                *omitted += 1;
                return Ok(true);
            };
            if next_bytes > max_bytes {
                heap.push(replaced);
                *omitted += 1;
                return Ok(true);
            }
            drop(replaced);
            heap.push(Ranked(value.to_owned()?));
            *retained_bytes = next_bytes;
        }
        *omitted += 1;
    }
    Ok(false)
}

fn structural_candidate_bytes(options: &DiscoverOptions) -> Result<usize, DiscoverError> {
    options
        .max_cursor_offset
        .checked_mul(size_of::<Ranked>())
        .and_then(|bytes| {
            options
                .max_results
                .checked_mul(size_of::<DiscoverResult>())
                .and_then(|results| bytes.checked_add(results))
        })
        .filter(|bytes| *bytes <= options.max_candidate_bytes)
        .ok_or(DiscoverError::InvalidOptions(
            "candidate byte bound cannot hold requested cursor and result capacities",
        ))
}

fn candidate_dynamic_size(result: &DiscoverResult) -> Option<usize> {
    result
        .path
        .as_os_str()
        .as_encoded_bytes()
        .len()
        .checked_add(result.symbol.as_ref().map_or(0, String::len))?
        .checked_add(result.excerpt.as_ref().map_or(0, String::len))
}

struct Candidate<'a> {
    path: &'a std::path::Path,
    kind: DiscoverKind,
    score: u16,
    rationale: &'static str,
    symbol: Option<&'a str>,
    line: Option<usize>,
    byte_start: Option<usize>,
    byte_end: Option<usize>,
    excerpt: Option<&'a str>,
    excerpt_truncated: bool,
}

impl Candidate<'_> {
    fn dynamic_size(&self) -> Option<usize> {
        self.path
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .checked_add(self.symbol.map_or(0, str::len))?
            .checked_add(self.excerpt.map_or(0, str::len))
    }

    fn compare_result(&self, other: &DiscoverResult) -> Ordering {
        other
            .score
            .cmp(&self.score)
            .then_with(|| self.path.cmp(&other.path))
            .then_with(|| self.kind.cmp(&other.kind))
            .then_with(|| self.line.cmp(&other.line))
            .then_with(|| self.byte_start.cmp(&other.byte_start))
            .then_with(|| self.symbol.cmp(&other.symbol.as_deref()))
    }

    fn to_owned(&self) -> Result<DiscoverResult, DiscoverError> {
        Ok(DiscoverResult {
            path: copy_path(self.path)?,
            kind: self.kind,
            score: self.score,
            rationale: self.rationale,
            symbol: self.symbol.map(copy_string).transpose()?,
            line: self.line,
            byte_start: self.byte_start,
            byte_end: self.byte_end,
            excerpt: self.excerpt.map(copy_string).transpose()?,
            excerpt_truncated: self.excerpt_truncated,
        })
    }
}

#[derive(Eq, PartialEq)]
struct Ranked(DiscoverResult);

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_results(&self.0, &other.0)
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_results(left: &DiscoverResult, right: &DiscoverResult) -> Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.kind.cmp(&right.kind))
        .then_with(|| left.line.cmp(&right.line))
        .then_with(|| left.byte_start.cmp(&right.byte_start))
        .then_with(|| left.symbol.cmp(&right.symbol))
}

#[allow(clippy::too_many_arguments)]
fn set_page(
    response: &mut DiscoverResponse,
    retained: usize,
    rank_omitted: usize,
    work_cut: bool,
    nonresumable_cut: bool,
    frontier: usize,
    index_digest: [u8; 32],
    query_digest: [u8; 32],
    options_digest: [u8; 32],
    options: &DiscoverOptions,
    deadline: Instant,
) -> Result<(), DiscoverError> {
    let consumed = frontier + response.results.len();
    response.omitted = rank_omitted + retained - consumed;
    response.omitted_complete = !work_cut;
    response.truncated = work_cut
        || nonresumable_cut
        || response.omitted != 0
        || response
            .results
            .iter()
            .any(|result| result.excerpt_truncated);
    response.cursor =
        (!work_cut && !nonresumable_cut && consumed < retained && consumed > frontier).then(|| {
            let canonical_digest = cursor_digest(
                response.revision,
                index_digest,
                query_digest,
                options_digest,
                consumed,
            );
            DiscoverCursor {
                revision: response.revision,
                index_digest,
                query_digest,
                options_digest,
                frontier: consumed,
                canonical_digest,
            }
        });
    if consumed >= options.max_cursor_offset {
        response.cursor = None;
    }
    check_deadline(deadline)
}

fn settle_size(response: &mut DiscoverResponse, deadline: Instant) -> Result<(), DiscoverError> {
    for _ in 0..usize::MAX.to_string().len() + 2 {
        check_deadline(deadline)?;
        let bytes = serialized_bytes(response, deadline)?;
        if bytes == response.result_bytes {
            return Ok(());
        }
        response.result_bytes = bytes;
    }
    Err(DiscoverError::InvalidOptions(
        "serialized result size did not converge",
    ))
}

fn serialized_bytes(value: &impl Serialize, deadline: Instant) -> Result<usize, DiscoverError> {
    let mut writer = CountingWriter { bytes: 0, deadline };
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        if error.io_error_kind() == Some(io::ErrorKind::TimedOut) {
            DiscoverError::TimeLimit
        } else {
            DiscoverError::Serialization(error)
        }
    })?;
    Ok(writer.bytes)
}

struct CountingWriter {
    bytes: usize,
    deadline: Instant,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "discover time limit exceeded",
            ));
        }
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("serialized discover response length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn check_deadline(deadline: Instant) -> Result<(), DiscoverError> {
    if Instant::now() >= deadline {
        Err(DiscoverError::TimeLimit)
    } else {
        Ok(())
    }
}

fn digest_query(query: &DiscoverQuery, deadline: Instant) -> Result<[u8; 32], DiscoverError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-discover-query-v1\0");
    for term in &query.terms {
        check_deadline(deadline)?;
        frame(&mut hash, term.as_bytes());
    }
    for root in &query.roots {
        check_deadline(deadline)?;
        frame(&mut hash, root.as_os_str().as_encoded_bytes());
    }
    for language in &query.languages {
        check_deadline(deadline)?;
        frame(&mut hash, language.as_bytes());
    }
    check_deadline(deadline)?;
    Ok(*hash.finalize().as_bytes())
}

fn digest_options(options: &DiscoverOptions, deadline: Instant) -> Result<[u8; 32], DiscoverError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-discover-options-v1\0");
    for value in [
        options.max_terms as u128,
        options.max_query_bytes as u128,
        options.max_scanned_entries as u128,
        options.max_scanned_bytes as u128,
        options.max_rank_work as u128,
        options.max_candidate_bytes as u128,
        options.max_results as u128,
        options.max_results_per_path as u128,
        options.max_result_bytes as u128,
        options.max_excerpt_bytes as u128,
        options.max_cursor_offset as u128,
        options.max_time.as_nanos(),
    ] {
        check_deadline(deadline)?;
        hash.update(&value.to_le_bytes());
    }
    check_deadline(deadline)?;
    Ok(*hash.finalize().as_bytes())
}

fn cursor_digest(
    revision: RevisionId,
    index: [u8; 32],
    query: [u8; 32],
    options: [u8; 32],
    frontier: usize,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-discover-cursor-v1\0");
    hash.update(revision.to_string().as_bytes());
    hash.update(&index);
    hash.update(&query);
    hash.update(&options);
    hash.update(&(frontier as u128).to_le_bytes());
    *hash.finalize().as_bytes()
}

fn frame(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn serialize_hex<S, const N: usize>(value: &[u8; N], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(N * 2);
    for byte in value {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    serializer.serialize_str(&output)
}
