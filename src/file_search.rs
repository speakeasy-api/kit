use std::{
    ops::Range,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use fff_search::{
    FFFMode, FilePicker, FilePickerOptions, FuzzySearchOptions, PaginationArgs, ParserConfig,
    QueryParser,
};
use serde::{Deserialize, Serialize};

const MAX_RESULTS: usize = 100;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct FileMatch {
    pub relative_path: String,
    pub match_byte_offsets: Vec<Range<usize>>,
}

pub struct WorkspaceFileSearch {
    picker: FilePicker,
}

pub struct WorkspaceFileSearchState {
    activation: u64,
    search: WorkspaceFileSearch,
}

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
    pub fn start(root: PathBuf) -> Result<Self, String> {
        if !root.is_dir() {
            return Err(format!(
                "workspace root is not a directory: {}",
                root.display()
            ));
        }
        let base_path = root
            .into_os_string()
            .into_string()
            .map_err(|_| "workspace root is not valid UTF-8".to_string())?;
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path,
            mode: FFFMode::Ai,
            watch: false,
            enable_home_dir_scanning: true,
            ..FilePickerOptions::default()
        })
        .map_err(|error| format!("could not start workspace file index: {error}"))?;
        // The synchronous path-only scan avoids fff-search's background
        // content-classification pass and cannot overlap a replacement scan.
        picker
            .collect_files()
            .map_err(|error| format!("could not scan workspace files: {error}"))?;
        Ok(Self { picker })
    }

    pub fn search(&self, query: &str) -> Result<Vec<FileMatch>, String> {
        let picker = &self.picker;
        let parsed = QueryParser::new(LiteralPathConfig).parse(query);
        let results = picker.fuzzy_search(
            &parsed,
            None,
            FuzzySearchOptions {
                pagination: PaginationArgs {
                    offset: 0,
                    limit: MAX_RESULTS,
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

pub async fn search_workspace(
    state: Arc<Mutex<Option<WorkspaceFileSearchState>>>,
    root: PathBuf,
    query: String,
    activation: u64,
) -> Result<Vec<FileMatch>, String> {
    tokio::task::spawn_blocking(move || {
        let mut state = state
            .lock()
            .map_err(|error| format!("file search state is unavailable: {error}"))?;
        // The activation identifies a picker instance independently of its query.
        // This lets a later query initialize the snapshot if it wins the request
        // race, while older or same-activation requests cannot trigger a rescan.
        let refresh = state
            .as_ref()
            .is_none_or(|cached| activation > cached.activation);
        if refresh {
            *state = Some(WorkspaceFileSearchState {
                activation,
                search: WorkspaceFileSearch::start(root)?,
            });
        }
        state
            .as_ref()
            .expect("initialized above")
            .search
            .search(&query)
    })
    .await
    .map_err(|error| format!("file search worker failed: {error}"))?
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

        let all = search.search("").expect("list files");
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

        let short = search.search("r").expect("short query");
        assert!(!short.is_empty());

        let punctuation = search
            .search("status:modified")
            .expect("literal punctuation query");
        assert_eq!(punctuation[0].relative_path, "src/status:modified.rs");

        let unicode = search.search("caf\u{e9}").expect("Unicode query");
        assert_eq!(unicode[0].relative_path, "src/caf\u{e9}.rs");
        assert!(unicode[0].match_byte_offsets.iter().all(|range| {
            unicode[0].relative_path.is_char_boundary(range.start)
                && unicode[0].relative_path.is_char_boundary(range.end)
        }));
    }

    #[tokio::test]
    async fn activation_refreshes_even_when_a_later_query_arrives_first() {
        let workspace = workspace();
        let root = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace");
        let state = Arc::new(Mutex::new(None));

        search_workspace(state.clone(), root.clone(), String::new(), 1)
            .await
            .expect("initial scan");
        fs::write(workspace.path().join("new.rs"), "new").expect("write new file");

        let raced = search_workspace(state.clone(), root.clone(), "new".into(), 2)
            .await
            .expect("racing query");
        assert!(raced.iter().any(|item| item.relative_path == "new.rs"));

        fs::write(workspace.path().join("later.rs"), "later").expect("write later file");
        let delayed_activation = search_workspace(state, root, String::new(), 2)
            .await
            .expect("delayed activation query");
        assert!(
            !delayed_activation
                .iter()
                .any(|item| item.relative_path == "later.rs")
        );
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
