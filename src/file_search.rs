use std::{
    ops::Range,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use ignore::{WalkBuilder, WalkState};
use neo_frizbee::{Config, match_list, match_list_indices};
use serde::{Deserialize, Serialize};

const MAX_RESULTS: usize = 100;

// Preserve fff-search’s non-Git safeguards for home and other broad roots.
const NON_GIT_IGNORED_DIRS: &[&str] = &[
    // various dev tools that can be meet in the developer app
    "node_modules",
    "__pycache__",
    "venv",
    ".venv",
    "target/debug",
    "target/release",
    "target/rust-analyzer",
    "target/criterion",
    // Language package caches in non-git roots.
    "go/pkg/mod",
    ".cargo/registry",
    ".rustup/toolchains",
    ".gradle/caches",
    ".m2/repository",
    ".npm/_cacache",
    ".pub-cache",
    #[cfg(not(target_os = "windows"))]
    ".local/state", // this contains tons of logs which generate too much watcher noise
    #[cfg(target_os = "macos")]
    "Library/Application Support",
    #[cfg(target_os = "macos")]
    "Library/Caches",
    #[cfg(target_os = "macos")]
    "Library/Containers", // sandboxed apps data
    #[cfg(target_os = "macos")]
    "Library/Group Containers", // random application data and networking
    #[cfg(target_os = "macos")]
    "Library/pnpm",
    #[cfg(target_os = "macos")]
    "Library/Metadata",
    #[cfg(target_os = "macos")]
    "Library/Developer/CoreSimulator",
    #[cfg(target_os = "macos")]
    "Library/Android",
    #[cfg(target_os = "macos")]
    "Library/Logs",
    #[cfg(target_os = "macos")]
    "Library/Daemon Containers",
    #[cfg(target_os = "macos")]
    "Library/Trial",
    #[cfg(target_os = "macos")]
    "Library/Preferences",
    #[cfg(target_os = "macos")]
    "Library/Messages",
    #[cfg(target_os = "macos")]
    "Library/IdentityServices",
    #[cfg(target_os = "windows")]
    "bin/Debug",
    #[cfg(target_os = "windows")]
    "bin/Release",
    #[cfg(target_os = "windows")]
    "Program Files",
    #[cfg(target_os = "windows")]
    "Program Files (x86)",
    #[cfg(target_os = "windows")]
    "AppData/Local",
    #[cfg(target_os = "windows")]
    "AppData/Roaming",
];

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct FileMatch {
    pub relative_path: String,
    pub match_byte_offsets: Vec<Range<usize>>,
}

pub struct WorkspaceFileSearch {
    files: Vec<String>,
}

pub struct WorkspaceFileSearchState {
    activation: u64,
    search: WorkspaceFileSearch,
}

impl WorkspaceFileSearch {
    pub fn start(root: PathBuf) -> Result<Self, String> {
        if !root.is_dir() {
            return Err(format!(
                "workspace root is not a directory: {}",
                root.display()
            ));
        }
        if root.parent().is_none() {
            return Err(format!(
                "refusing to index filesystem root: {}",
                root.display()
            ));
        }
        if root.to_str().is_none() {
            return Err("workspace root is not valid UTF-8".to_string());
        }

        let is_git_repo = root
            .ancestors()
            .any(|ancestor| ancestor.join(".git").exists());
        let mut builder = WalkBuilder::new(&root);
        builder
            .hidden(!is_git_repo)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .ignore(true)
            .follow_links(false)
            .filter_entry(|entry| entry.file_name() != ".git");

        if !is_git_repo {
            let mut overrides = ignore::overrides::OverrideBuilder::new(&root);
            for dir in NON_GIT_IGNORED_DIRS {
                overrides
                    .add(&format!("!**/{dir}/"))
                    .map_err(|error| format!("invalid non-Git exclusion: {error}"))?;
            }
            builder.overrides(
                overrides
                    .build()
                    .map_err(|error| format!("could not build non-Git exclusions: {error}"))?,
            );
        }

        let files = Mutex::new(Vec::new());
        let scan_error = Mutex::new(None);
        builder.build_parallel().run(|| {
            let files = &files;
            let scan_error = &scan_error;
            let root = &root;
            Box::new(move |result| {
                let entry = match result {
                    Ok(entry) if entry.error().is_none() => entry,
                    result => {
                        let error = match result {
                            Ok(entry) => entry.error().expect("entry has an error").to_string(),
                            Err(error) => error.to_string(),
                        };
                        scan_error
                            .lock()
                            .expect("scan error mutex poisoned")
                            .get_or_insert(error);
                        return WalkState::Quit;
                    }
                };
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    return WalkState::Continue;
                }
                let Ok(relative) = entry.path().strip_prefix(root) else {
                    return WalkState::Continue;
                };
                let relative_path = relative
                    .iter()
                    .map(|part| part.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                files
                    .lock()
                    .expect("file list mutex poisoned")
                    .push(relative_path);
                WalkState::Continue
            })
        });
        if let Some(error) = scan_error
            .into_inner()
            .map_err(|error| format!("could not collect workspace scan error: {error}"))?
        {
            return Err(format!("could not scan workspace files: {error}"));
        }
        let mut files = files
            .into_inner()
            .map_err(|error| format!("could not collect workspace files: {error}"))?;
        files.sort_unstable();
        Ok(Self { files })
    }

    pub fn search(&self, query: &str) -> Result<Vec<FileMatch>, String> {
        if query.is_empty() {
            return Ok(self
                .files
                .iter()
                .take(MAX_RESULTS)
                .cloned()
                .map(|relative_path| FileMatch {
                    relative_path,
                    match_byte_offsets: Vec::new(),
                })
                .collect());
        }

        let config = Config {
            max_typos: Some((query.len() as u16 / 4).clamp(2, 6)),
            ..Config::default()
        };
        let candidates: Vec<_> = match_list(query, &self.files, &config)
            .into_iter()
            .take(MAX_RESULTS)
            .filter_map(|matched| self.files.get(matched.index as usize))
            .map(String::as_str)
            .collect();

        Ok(match_list_indices(query, &candidates, &config)
            .into_iter()
            .filter_map(|matched| {
                let relative_path = candidates.get(matched.index as usize)?.to_string();
                let mut offsets = matched.indices;
                offsets.sort_unstable();
                let mut match_byte_offsets: Vec<Range<usize>> = Vec::new();
                for byte_index in offsets {
                    if byte_index >= relative_path.len() {
                        continue;
                    }
                    // neo_frizbee returns byte offsets, including continuation bytes.
                    // Expand each one to the containing character before merging.
                    let mut start = byte_index;
                    while !relative_path.is_char_boundary(start) {
                        start -= 1;
                    }
                    let mut end = byte_index + 1;
                    while !relative_path.is_char_boundary(end) {
                        end += 1;
                    }
                    if let Some(previous) = match_byte_offsets.last_mut()
                        && start <= previous.end
                    {
                        previous.end = previous.end.max(end);
                    } else {
                        match_byte_offsets.push(start..end);
                    }
                }
                Some(FileMatch {
                    relative_path,
                    match_byte_offsets,
                })
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
        assert_eq!(unicode[0].match_byte_offsets, vec![4..9]);
    }

    #[test]
    fn unicode_highlights_use_byte_offsets() {
        let search = WorkspaceFileSearch {
            files: vec!["é.rs".into()],
        };
        for (query, expected) in [("é", 0..2), ("rs", 3..5), ("é.rs", 0..5)] {
            let results = search.search(query).expect("Unicode search");
            assert_eq!(results.len(), 1, "query: {query}");
            assert_eq!(results[0].match_byte_offsets.len(), 1, "query: {query}");
            assert_eq!(results[0].match_byte_offsets[0], expected, "query: {query}");
        }
    }

    #[test]
    fn non_git_roots_exclude_machine_state_but_keep_source() {
        let workspace = tempfile::tempdir().expect("temporary non-Git workspace");
        for prefix in ["", "nested/"] {
            for dir in NON_GIT_IGNORED_DIRS {
                let directory = workspace.path().join(format!("{prefix}{dir}"));
                fs::create_dir_all(&directory).expect("create excluded directory");
                fs::write(directory.join("excluded.txt"), "state").expect("write state");
            }
        }
        let sources = [
            "dev/project/src/main.rs",
            "dev/myproj/pkg/mod/thing.go",
            "Documents/notes/todo.md",
            "target/source.rs",
            "node_modules_backup/source.js",
        ];
        for source in sources {
            let path = workspace.path().join(source);
            fs::create_dir_all(path.parent().unwrap()).expect("create source directory");
            fs::write(path, "source").expect("write source");
        }
        let search = WorkspaceFileSearch::start(workspace.path().to_path_buf())
            .expect("start non-Git search");
        let mut expected: Vec<_> = sources.into_iter().map(String::from).collect();
        expected.sort();
        assert_eq!(search.files, expected);
    }

    #[test]
    fn git_roots_do_not_apply_non_git_exclusions() {
        let workspace = workspace();
        fs::create_dir(workspace.path().join("node_modules")).expect("create directory");
        fs::write(workspace.path().join("node_modules/source.js"), "source").expect("write source");
        let search =
            WorkspaceFileSearch::start(workspace.path().to_path_buf()).expect("start Git search");
        assert!(search.files.contains(&"node_modules/source.js".to_string()));
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
    fn prunes_git_metadata_before_scanning() {
        let workspace = workspace();
        // An invalid ignore rule would fail the scan if metadata were traversed.
        fs::write(workspace.path().join(".git/.ignore"), "[z-a]\n")
            .expect("write invalid metadata ignore rule");
        let search = WorkspaceFileSearch::start(workspace.path().to_path_buf())
            .expect("metadata must not be scanned");
        assert!(search.files.iter().all(|path| !path.starts_with(".git/")));
    }

    #[test]
    fn reports_invalid_ignore_rules() {
        let workspace = workspace();
        fs::write(workspace.path().join(".ignore"), "[z-a]\n").expect("write invalid ignore rule");
        let error = WorkspaceFileSearch::start(workspace.path().to_path_buf())
            .err()
            .expect("invalid ignore rule must fail the scan");
        assert!(error.contains("could not scan workspace files"), "{error}");
    }

    #[test]
    fn rejects_invalid_roots() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let file = workspace.path().join("file");
        fs::write(&file, "not a directory").expect("write file");

        assert!(WorkspaceFileSearch::start(file).is_err());
        assert!(WorkspaceFileSearch::start(workspace.path().join("missing")).is_err());
        assert!(WorkspaceFileSearch::start(PathBuf::from(std::path::MAIN_SEPARATOR_STR)).is_err());
    }
}
