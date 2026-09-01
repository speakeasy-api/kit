//! Deterministic full-text search over the documentation bundled with this Kit build.
//!
//! The index accepts an arbitrary document slice so the same search layer can
//! back a future `kit docs` command and focused tests.

use std::{cmp::Reverse, collections::BTreeSet, fmt};

use serde::Serialize;

const MAX_QUERY_CHARS: usize = 512;
const MAX_MATCHES: usize = 5;
const MAX_MATCH_CHARS: usize = 1_800;
const MAX_TOTAL_CHARS: usize = 7_000;
const MAX_METADATA_CHARS: usize = 256;

const BUNDLED_DOCUMENTS: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/bundled_docs.rs"));

#[derive(Clone, Copy, Debug)]
pub struct Document<'a> {
    pub path: &'a str,
    pub content: &'a str,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SearchResponse {
    pub query: String,
    pub version: &'static str,
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: String,
    pub title: String,
    pub section: String,
    pub score: usize,
    pub content: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SearchError {
    EmptyQuery,
    QueryTooLong,
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyQuery => "query must contain non-whitespace text",
            Self::QueryTooLong => "query must be at most 512 characters",
        })
    }
}

#[derive(Debug)]
struct Chunk<'a> {
    path: &'a str,
    title: String,
    section: String,
    content: String,
    ordinal: usize,
}

#[derive(Debug)]
struct Scored<'a> {
    chunk: &'a Chunk<'a>,
    score: usize,
}

pub struct SearchIndex<'a> {
    chunks: Vec<Chunk<'a>>,
}

impl<'a> SearchIndex<'a> {
    pub fn new(documents: &'a [Document<'a>]) -> Self {
        let chunks = documents
            .iter()
            .flat_map(|document| chunks(document))
            .collect();
        Self { chunks }
    }

    pub fn search(&self, query: &str) -> Result<SearchResponse, SearchError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(SearchError::EmptyQuery);
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(SearchError::QueryTooLong);
        }

        let normalized = query.to_lowercase();
        let mut terms = tokenize(query)
            .into_iter()
            .filter(|term| !is_stopword(term))
            .collect::<BTreeSet<_>>();
        if terms.is_empty() {
            terms.extend(tokenize(query));
        }

        let mut scored = self
            .chunks
            .iter()
            .filter_map(|chunk| {
                let score = score(chunk, &normalized, &terms);
                (score > 0).then_some(Scored { chunk, score })
            })
            .collect::<Vec<_>>();
        scored.sort_by_key(|item| {
            (
                Reverse(item.score),
                item.chunk.path,
                item.chunk.section.as_str(),
                item.chunk.ordinal,
            )
        });

        let available = scored.len();
        let mut used = 0;
        let mut matches = Vec::new();
        let mut content_truncated = false;
        for item in scored.into_iter().take(MAX_MATCHES) {
            let remaining = MAX_TOTAL_CHARS.saturating_sub(used);
            if remaining == 0 {
                content_truncated = true;
                break;
            }
            let limit = remaining.min(MAX_MATCH_CHARS);
            let (content, shortened) = excerpt(&item.chunk.content, &normalized, &terms, limit);
            let (path, path_shortened) = truncate(item.chunk.path, MAX_METADATA_CHARS);
            let (title, title_shortened) = truncate(&item.chunk.title, MAX_METADATA_CHARS);
            let (section, section_shortened) = truncate(&item.chunk.section, MAX_METADATA_CHARS);
            used += content.chars().count();
            content_truncated |=
                shortened || path_shortened || title_shortened || section_shortened;
            matches.push(SearchMatch {
                path,
                title,
                section,
                score: item.score,
                content,
            });
        }
        let truncated = content_truncated || available > matches.len();
        Ok(SearchResponse {
            query: query.to_owned(),
            version: env!("CARGO_PKG_VERSION"),
            matches,
            truncated,
        })
    }
}

pub fn bundled_search(query: &str) -> Result<SearchResponse, SearchError> {
    let documents = BUNDLED_DOCUMENTS
        .iter()
        .map(|(path, content)| Document { path, content })
        .collect::<Vec<_>>();
    SearchIndex::new(&documents).search(query)
}

fn chunks<'a>(document: &'a Document<'a>) -> Vec<Chunk<'a>> {
    let title = document
        .content
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .unwrap_or(document.path)
        .trim()
        .to_owned();
    let mut result = Vec::new();
    let mut section = title.clone();
    let mut body = String::new();
    let mut ordinal = 0;
    for line in document.content.lines() {
        let heading = line
            .strip_prefix("## ")
            .or_else(|| line.strip_prefix("### "));
        if let Some(heading) = heading {
            push_chunk(&mut result, document.path, &title, &section, &body, ordinal);
            ordinal += 1;
            section = heading.trim().to_owned();
            body.clear();
        }
        body.push_str(line);
        body.push('\n');
    }
    push_chunk(&mut result, document.path, &title, &section, &body, ordinal);
    result
}

fn push_chunk<'a>(
    chunks: &mut Vec<Chunk<'a>>,
    path: &'a str,
    title: &str,
    section: &str,
    body: &str,
    ordinal: usize,
) {
    let content = body.trim();
    if !content.is_empty() {
        chunks.push(Chunk {
            path,
            title: title.to_owned(),
            section: section.to_owned(),
            content: content.to_owned(),
            ordinal,
        });
    }
}

fn score(chunk: &Chunk<'_>, query: &str, terms: &BTreeSet<String>) -> usize {
    let title = chunk.title.to_lowercase();
    let section = chunk.section.to_lowercase();
    let content = chunk.content.to_lowercase();
    let mut score = usize::from(content.contains(query)) * 100;
    for term in terms {
        score += usize::from(title.contains(term)) * 20;
        score += usize::from(section.contains(term)) * 12;
        score += content.matches(term).count().min(8) * 2;
    }
    if !terms.is_empty() && terms.iter().all(|term| content.contains(term)) {
        score += 25;
    }
    score
}

fn tokenize(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn is_stopword(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "are"
            | "do"
            | "for"
            | "how"
            | "i"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "the"
            | "to"
            | "what"
            | "with"
    )
}

fn excerpt(value: &str, query: &str, terms: &BTreeSet<String>, limit: usize) -> (String, bool) {
    let characters = value.chars().collect::<Vec<_>>();
    if limit == 0 {
        return (String::new(), !characters.is_empty());
    }
    if limit <= 2 && characters.len() > limit {
        return ("…".to_owned(), true);
    }
    if characters.len() <= limit {
        return (value.to_owned(), false);
    }

    let normalized = value.to_lowercase();
    let match_byte = normalized
        .find(query)
        .or_else(|| terms.iter().filter_map(|term| normalized.find(term)).min());
    let match_character = match_byte
        .map(|byte| normalized[..byte].chars().count())
        .unwrap_or_default();
    let payload = limit.saturating_sub(2);
    let start = match_character
        .saturating_sub(payload / 3)
        .min(characters.len().saturating_sub(payload));
    let end = (start + payload).min(characters.len());

    let mut result = String::new();
    if start > 0 {
        result.push('…');
    }
    result.extend(&characters[start..end]);
    if end < characters.len() {
        result.push('…');
    }
    (result, true)
}

fn truncate(value: &str, limit: usize) -> (String, bool) {
    if value.chars().count() <= limit {
        return (value.to_owned(), false);
    }
    let keep = limit.saturating_sub(1);
    let mut shortened = value.chars().take(keep).collect::<String>();
    shortened.push('…');
    (shortened, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUMENTS: &[Document<'_>] = &[
        Document {
            path: "docs/user/mcp.md",
            content: "# MCP setup\n\nAll configured servers start connecting in the background at startup using stored credentials only. The config reloads before tool_search and auth, and tool_search waits for any servers that are still uninitialized before searching every connected server's tools globally; the exact query mcp returns a compact server-status list and reports any cap-driven tail omission. Remote Streamable HTTP servers infer OAuth from a WWW-Authenticate Bearer challenge, so an explicit auth block is optional and only supplies client or scope overrides. Stdio servers do not use OAuth.\n\n## OAuth failures\n\nIf a server reports authentication_required, call auth and open the returned URL. The originating ACP session resumes when the callback completes. One-shot prompt preserves authentication_required status but cannot launch interactive authentication.",
        },
        Document {
            path: "docs/user/sessions.md",
            content: "# Sessions\n\nResume a persisted session by id.\n\n## Stale locks\n\nUse force only after checking no other Kit process owns the session.",
        },
    ];

    #[test]
    fn ranks_relevant_sections_deterministically() {
        let index = SearchIndex::new(DOCUMENTS);
        let first = index.search("Kit stale session lock").unwrap();
        let second = index.search("Kit stale session lock").unwrap();
        assert_eq!(first, second);
        assert_eq!(first.matches[0].path, "docs/user/sessions.md");
        assert_eq!(first.matches[0].section, "Stale locks");
    }

    #[test]
    fn rejects_unbounded_or_empty_queries() {
        let index = SearchIndex::new(DOCUMENTS);
        assert_eq!(index.search("  "), Err(SearchError::EmptyQuery));
        assert_eq!(
            index.search(&"x".repeat(MAX_QUERY_CHARS + 1)),
            Err(SearchError::QueryTooLong)
        );
    }

    #[test]
    fn bounds_match_count_and_content() {
        let long = format!("# Needle\n\n{}", "needle content ".repeat(1_000));
        let documents = vec![
            Document {
                path: "docs/user/large.md",
                content: &long,
            };
            MAX_MATCHES + 3
        ];
        let result = SearchIndex::new(&documents).search("needle").unwrap();
        assert!(!result.matches.is_empty());
        assert!(result.matches.len() <= MAX_MATCHES);
        assert!(
            result
                .matches
                .iter()
                .all(|item| item.content.chars().count() <= MAX_MATCH_CHARS)
        );
        assert!(
            result
                .matches
                .iter()
                .map(|item| item.content.chars().count())
                .sum::<usize>()
                <= MAX_TOTAL_CHARS
        );
        assert!(result.truncated);
    }

    #[test]
    fn excerpts_include_matches_near_the_end_of_long_sections() {
        let content = format!("# Long section\n\n{}needle answer", "prefix ".repeat(1_000));
        let documents = [Document {
            path: "docs/user/long.md",
            content: &content,
        }];
        let result = SearchIndex::new(&documents)
            .search("needle answer")
            .unwrap();
        assert!(result.matches[0].content.contains("needle answer"));
        assert!(result.matches[0].content.starts_with('…'));
    }

    #[test]
    fn bounds_result_metadata() {
        let heading = "x".repeat(MAX_METADATA_CHARS + 100);
        let content = format!("# {heading}\n\nneedle");
        let documents = [Document {
            path: &heading,
            content: &content,
        }];
        let result = SearchIndex::new(&documents).search("needle").unwrap();
        let item = &result.matches[0];
        assert!(item.path.chars().count() <= MAX_METADATA_CHARS);
        assert!(item.title.chars().count() <= MAX_METADATA_CHARS);
        assert!(item.section.chars().count() <= MAX_METADATA_CHARS);
        assert!(result.truncated);
    }

    #[test]
    fn bundled_corpus_contains_every_user_guide() {
        let paths = BUNDLED_DOCUMENTS
            .iter()
            .map(|(path, _)| *path)
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "docs/user/agent-plugins.md",
                "docs/user/compose-and-local-tools.md",
                "docs/user/getting-started-and-configuration.md",
                "docs/user/mcp.md",
                "docs/user/migrating-from-claude-code-and-codex.md",
                "docs/user/reporting-kit-issues.md",
                "docs/user/security-limits-and-troubleshooting.md",
                "docs/user/subagents-and-acp-harnesses.md",
                "docs/user/tui-and-sessions.md",
            ]
        );
        let result = bundled_search("MCP OAuth credential store").unwrap();
        assert_eq!(result.version, env!("CARGO_PKG_VERSION"));
        assert!(
            result
                .matches
                .iter()
                .any(|item| item.path == "docs/user/mcp.md")
        );
    }
}
