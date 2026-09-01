use std::{fmt, ops::Range, path::PathBuf, time::Duration};

use fff_search::{
    FFFMode, FilePicker, FilePickerOptions, FuzzySearchOptions, PaginationArgs, ParserConfig,
    QueryParser, SharedFilePicker, SharedFrecency,
};

const INDEX_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESULTS: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatch {
    pub relative_path: String,
    pub match_byte_offsets: Vec<Range<usize>>,
}

#[derive(Clone)]
pub struct WorkspaceFileSearch {
    picker: SharedFilePicker,
}

#[derive(Debug)]
pub struct FileSearchError(String);

impl fmt::Display for FileSearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FileSearchError {}

#[derive(Debug, Clone, Copy)]
struct LiteralPathConfig;

impl ParserConfig for LiteralPathConfig {
    fn enable_glob(&self) -> bool {
        false
    }

    fn enable_extension(&self) -> bool {
        false
    }

    fn enable_exclude(&self) -> bool {
        false
    }

    fn enable_path_segments(&self) -> bool {
        false
    }

    fn enable_type_filter(&self) -> bool {
        false
    }

    fn enable_git_status(&self) -> bool {
        false
    }

    fn enable_location(&self) -> bool {
        false
    }
}

impl WorkspaceFileSearch {
    pub fn start(root: PathBuf) -> Result<Self, FileSearchError> {
        if !root.is_dir() {
            return Err(FileSearchError(format!(
                "workspace root is not a directory: {}",
                root.display()
            )));
        }
        let base_path = root
            .into_os_string()
            .into_string()
            .map_err(|_| FileSearchError("workspace root is not valid UTF-8".into()))?;
        let picker = SharedFilePicker::default();
        FilePicker::new_with_shared_state(
            picker.clone(),
            SharedFrecency::noop(),
            FilePickerOptions {
                base_path,
                mode: FFFMode::Ai,
                ..FilePickerOptions::default()
            },
        )
        .map_err(|error| {
            FileSearchError(format!("could not start workspace file index: {error}"))
        })?;
        Ok(Self { picker })
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<FileMatch>, FileSearchError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if !self.picker.wait_for_scan(INDEX_TIMEOUT) {
            return Err(FileSearchError("workspace file index timed out".into()));
        }

        let guard = self.picker.read().map_err(|error| {
            FileSearchError(format!("could not read workspace file index: {error}"))
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| FileSearchError("workspace file index is unavailable".into()))?;
        let parsed = QueryParser::new(LiteralPathConfig).parse(query);
        let results = picker.fuzzy_search(
            &parsed,
            None,
            FuzzySearchOptions {
                pagination: PaginationArgs {
                    offset: 0,
                    limit: limit.min(MAX_RESULTS),
                },
                ..FuzzySearchOptions::default()
            },
        );

        Ok(results
            .items
            .into_iter()
            .zip(results.match_byte_offsets)
            .map(|(item, offsets)| {
                let relative_path = item.relative_path(picker);
                let mut match_byte_offsets: Vec<_> = offsets
                    .into_iter()
                    .filter_map(|(start, end)| {
                        let range = start as usize..end as usize;
                        (range.start < range.end
                            && relative_path.is_char_boundary(range.start)
                            && relative_path.is_char_boundary(range.end))
                        .then_some(range)
                    })
                    .collect();
                match_byte_offsets.sort_by_key(|range| (range.start, range.end));
                let mut normalized: Vec<Range<usize>> = Vec::new();
                for range in match_byte_offsets {
                    if let Some(previous) = normalized.last_mut()
                        && range.start <= previous.end
                    {
                        previous.end = previous.end.max(range.end);
                    } else {
                        normalized.push(range);
                    }
                }
                FileMatch {
                    relative_path,
                    match_byte_offsets: normalized,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::TempDir;

    use super::*;

    fn workspace() -> TempDir {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(workspace.path())
                .status()
                .expect("run git init")
                .success()
        );
        fs::create_dir(workspace.path().join("src")).expect("create source directory");
        fs::create_dir(workspace.path().join("ignored")).expect("create ignored directory");
        fs::write(workspace.path().join(".gitignore"), "ignored/\n").expect("write ignore file");
        fs::write(workspace.path().join("README.md"), "tracked").expect("write tracked file");
        fs::write(workspace.path().join("src/untracked.rs"), "untracked")
            .expect("write untracked file");
        fs::write(workspace.path().join("src/caf\u{e9}.rs"), "unicode")
            .expect("write Unicode file");
        fs::write(
            workspace.path().join("src/status:modified.rs"),
            "punctuation",
        )
        .expect("write punctuation file");
        fs::write(workspace.path().join("ignored/secret.rs"), "ignored")
            .expect("write ignored file");
        assert!(
            Command::new("git")
                .current_dir(workspace.path())
                .args(["add", "README.md", ".gitignore"])
                .status()
                .expect("run git add")
                .success()
        );
        workspace
    }

    #[test]
    fn normalizes_workspace_search_contract() {
        let workspace = workspace();
        let search = WorkspaceFileSearch::start(
            workspace
                .path()
                .canonicalize()
                .expect("canonical workspace"),
        )
        .expect("start search");

        let all = search.search("", 100).expect("list files");
        assert!(all.iter().any(|item| item.relative_path == "README.md"));
        assert!(
            all.iter()
                .any(|item| item.relative_path == "src/untracked.rs")
        );
        assert!(
            all.iter()
                .any(|item| item.relative_path == "src/caf\u{e9}.rs")
        );
        assert!(!all.iter().any(|item| item.relative_path.contains("secret")));

        let short = search.search("r", 100).expect("short query");
        assert!(!short.is_empty());

        let punctuation = search
            .search("status:modified", 100)
            .expect("literal punctuation query");
        assert_eq!(punctuation[0].relative_path, "src/status:modified.rs");

        let unicode = search.search("caf\u{e9}", 100).expect("Unicode query");
        assert_eq!(unicode[0].relative_path, "src/caf\u{e9}.rs");
        assert!(unicode[0].match_byte_offsets.iter().all(|range| {
            unicode[0].relative_path.is_char_boundary(range.start)
                && unicode[0].relative_path.is_char_boundary(range.end)
        }));

        assert_eq!(search.search("", 1).expect("bounded query").len(), 1);
        assert!(search.search("", 0).expect("zero limit").is_empty());
    }

    #[test]
    fn rejects_invalid_roots() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let file = workspace.path().join("file");
        fs::write(&file, "not a directory").expect("write file");

        assert!(WorkspaceFileSearch::start(file).is_err());
        assert!(WorkspaceFileSearch::start(workspace.path().join("missing")).is_err());
    }
}
