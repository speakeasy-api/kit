//! Context loaders for workspace-local agent instructions.
//!
//! This crate discovers and loads `AGENTS.md` files (project-level
//! instructions) into [`agentkit_core::Item`]s with [`ItemKind::Context`]. The
//! resulting items slot directly into a transcript alongside system, user, and
//! assistant messages, so the agent loop and providers do not need a separate
//! context path.
//!
//! # Overview
//!
//! * [`AgentsMd`] -- walks ancestor directories to find `AGENTS.md` files.
//! * [`ContextLoader`] -- combines multiple [`ContextSource`] implementations
//!   and loads them in order.
//!
//! # Example
//!
//! ```rust,no_run
//! use agentkit_context::{AgentsMd, ContextLoader};
//!
//! # async fn run() -> Result<(), agentkit_context::ContextError> {
//! let items = ContextLoader::new()
//!     .with_source(AgentsMd::discover("."))
//!     .load()
//!     .await?;
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agentkit_core::{Item, ItemKind, MetadataMap, Part, TextPart};
use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

const DEFAULT_AGENTS_FILE: &str = "AGENTS.md";

/// Controls how many `AGENTS.md` files [`AgentsMd`] returns during ancestor
/// discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentsMdMode {
    /// Stop at the first (nearest) `AGENTS.md` found while walking upward.
    Nearest,
    /// Collect every `AGENTS.md` from the filesystem root down to the start
    /// directory, ordered from outermost to innermost.
    All,
}

/// A source of context [`Item`]s.
///
/// Implement this trait to create custom context loaders that can be plugged
/// into a [`ContextLoader`]. Each call to [`load`](ContextSource::load) should
/// return zero or more [`Item`]s with [`ItemKind::Context`].
#[async_trait]
pub trait ContextSource: Send + Sync {
    /// Load context items from this source.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError`] if the underlying filesystem operations fail.
    async fn load(&self) -> Result<Vec<Item>, ContextError>;
}

/// Composable loader that gathers context [`Item`]s from multiple
/// [`ContextSource`] implementations.
///
/// Sources are loaded in the order they were added and the resulting items are
/// concatenated into a single `Vec<Item>`. These items carry
/// [`ItemKind::Context`] and can be prepended to the transcript before the
/// user message.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_context::{AgentsMd, ContextLoader};
///
/// # async fn run() -> Result<(), agentkit_context::ContextError> {
/// let items = ContextLoader::new()
///     .with_source(AgentsMd::discover("."))
///     .load()
///     .await?;
///
/// println!("loaded {} context items", items.len());
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct ContextLoader {
    sources: Vec<Box<dyn ContextSource>>,
}

impl ContextLoader {
    /// Create an empty loader with no sources.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a [`ContextSource`] to this loader.
    ///
    /// Sources are loaded in the order they are added. This method consumes
    /// and returns `self` so calls can be chained.
    pub fn with_source(mut self, source: impl ContextSource + 'static) -> Self {
        self.sources.push(Box::new(source));
        self
    }

    /// Load all registered sources and return a combined list of context
    /// [`Item`]s.
    ///
    /// # Errors
    ///
    /// Returns the first [`ContextError`] encountered while loading. Sources
    /// that appear before the failing source will have already been loaded.
    pub async fn load(&self) -> Result<Vec<Item>, ContextError> {
        let mut items = Vec::new();

        for source in &self.sources {
            items.extend(source.load().await?);
        }

        Ok(items)
    }
}

/// Discovers and loads `AGENTS.md` files by walking ancestor directories.
///
/// `AgentsMd` is the primary way to inject project-level instructions into an
/// agent session. It walks upward from a given starting directory, collecting
/// `AGENTS.md` files according to the configured [`AgentsMdMode`]. Explicit
/// paths and extra search directories can be added for cases that fall outside
/// simple ancestor discovery.
///
/// Loaded items carry metadata under the `agentkit.context.*` namespace:
///
/// | Key                        | Value                         |
/// |----------------------------|-------------------------------|
/// | `agentkit.context.source`  | `"agents_md"`                 |
/// | `agentkit.context.path`    | Filesystem path of the file   |
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_context::AgentsMd;
/// use agentkit_context::ContextSource; // for `.load()`
///
/// # async fn run() -> Result<(), agentkit_context::ContextError> {
/// // Find the nearest AGENTS.md starting from the current directory.
/// let items = AgentsMd::discover(".").load().await?;
///
/// // Or collect all ancestor AGENTS.md files, with an extra search dir.
/// let items = AgentsMd::discover_all(".")
///     .with_search_dir("./.agent")
///     .load()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct AgentsMd {
    start_dir: PathBuf,
    mode: AgentsMdMode,
    file_name: String,
    explicit_paths: Vec<PathBuf>,
    search_dirs: Vec<PathBuf>,
}

impl AgentsMd {
    /// Create a new `AgentsMd` that searches for the nearest `AGENTS.md`
    /// starting from `start_dir` and walking upward.
    ///
    /// This uses [`AgentsMdMode::Nearest`] by default. Call
    /// [`with_mode`](Self::with_mode) or use [`discover_all`](Self::discover_all)
    /// to collect every ancestor match instead.
    pub fn discover(start_dir: impl Into<PathBuf>) -> Self {
        Self {
            start_dir: start_dir.into(),
            mode: AgentsMdMode::Nearest,
            file_name: DEFAULT_AGENTS_FILE.into(),
            explicit_paths: Vec::new(),
            search_dirs: Vec::new(),
        }
    }

    /// Shorthand for `AgentsMd::discover(start_dir).with_mode(AgentsMdMode::All)`.
    ///
    /// Collects every `AGENTS.md` from the filesystem root down to `start_dir`,
    /// ordered outermost-first so that more specific instructions appear last.
    pub fn discover_all(start_dir: impl Into<PathBuf>) -> Self {
        Self::discover(start_dir).with_mode(AgentsMdMode::All)
    }

    /// Set the discovery mode.
    ///
    /// See [`AgentsMdMode`] for the available options.
    pub fn with_mode(mut self, mode: AgentsMdMode) -> Self {
        self.mode = mode;
        self
    }

    /// Override the file name to look for (default: `AGENTS.md`).
    ///
    /// Useful when a project uses a different convention such as `CLAUDE.md`.
    pub fn with_file_name(mut self, file_name: impl Into<String>) -> Self {
        self.file_name = file_name.into();
        self
    }

    /// Add an explicit file path to include.
    ///
    /// The path is checked for existence at load time; if it does not exist it
    /// is silently skipped. Explicit paths are loaded before ancestor discovery
    /// results.
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.explicit_paths.push(path.into());
        self
    }

    /// Add a directory to search for the configured file name.
    ///
    /// Unlike ancestor discovery, this checks only the given directory (not its
    /// ancestors). This is useful for well-known sidecar locations like
    /// `.agent/` or `.config/`.
    pub fn with_search_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.search_dirs.push(dir.into());
        self
    }

    /// Resolve the first matching path without reading its contents.
    ///
    /// Returns `None` when no `AGENTS.md` file is found. This is a convenience
    /// wrapper around [`resolve_all`](Self::resolve_all).
    ///
    /// # Errors
    ///
    /// Returns [`ContextError`] if a filesystem metadata check fails.
    pub async fn resolve(&self) -> Result<Option<PathBuf>, ContextError> {
        Ok(self.resolve_all().await?.into_iter().next())
    }

    /// Resolve all matching paths without reading their contents.
    ///
    /// The returned paths are deduplicated and ordered from outermost to
    /// innermost. When the mode is [`AgentsMdMode::Nearest`], at most one path
    /// is returned.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError`] if a filesystem metadata check fails.
    pub async fn resolve_all(&self) -> Result<Vec<PathBuf>, ContextError> {
        let mut paths = Vec::new();

        for path in &self.explicit_paths {
            if path_exists(path).await? {
                paths.push(path.clone());
            }
        }

        for dir in &self.search_dirs {
            let candidate = dir.join(&self.file_name);
            if path_exists(&candidate).await? {
                paths.push(candidate);
            }
        }

        paths.extend(
            find_in_ancestors_with_mode(
                &self.start_dir,
                &self.file_name,
                self.mode == AgentsMdMode::All,
            )
            .await?,
        );

        let mut seen = BTreeSet::new();
        paths.retain(|path| seen.insert(path.clone()));
        if self.mode == AgentsMdMode::Nearest {
            Ok(paths.into_iter().rev().take(1).collect())
        } else {
            Ok(paths)
        }
    }
}

#[async_trait]
impl ContextSource for AgentsMd {
    async fn load(&self) -> Result<Vec<Item>, ContextError> {
        let paths = self.resolve_all().await?;
        let mut items = Vec::with_capacity(paths.len());

        for path in paths {
            let body = async_fs::read_to_string(&path).await.map_err(|error| {
                ContextError::ReadFailed {
                    path: path.clone(),
                    error,
                }
            })?;

            items.push(context_item(
                format!(
                    "[Loaded AGENTS]\nPath: {}\n\n{}",
                    path.display(),
                    body.trim_end()
                ),
                metadata_for("agents_md", &path, None),
            ));
        }

        Ok(items)
    }
}

fn context_item(text: String, metadata: MetadataMap) -> Item {
    Item {
        id: None,
        kind: ItemKind::Context,
        parts: vec![Part::Text(TextPart {
            text,
            metadata: MetadataMap::new(),
        })],
        metadata,
        usage: None,
        finish_reason: None,
        created_at: None,
    }
}

fn metadata_for(source_kind: &str, path: &Path, name: Option<String>) -> MetadataMap {
    let mut metadata = MetadataMap::new();
    metadata.insert(
        "agentkit.context.source".into(),
        Value::String(source_kind.into()),
    );
    metadata.insert(
        "agentkit.context.path".into(),
        Value::String(path.display().to_string()),
    );
    if let Some(name) = name {
        metadata.insert("agentkit.context.name".into(), Value::String(name));
    }
    metadata
}

async fn path_exists(path: &Path) -> Result<bool, ContextError> {
    match async_fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ContextError::InspectFailed {
            path: path.to_path_buf(),
            error,
        }),
    }
}

async fn find_in_ancestors_with_mode(
    start_dir: &Path,
    file_name: &str,
    include_all: bool,
) -> Result<Vec<PathBuf>, ContextError> {
    let mut current = start_dir.to_path_buf();
    let mut matches = Vec::new();

    loop {
        let candidate = current.join(file_name);
        if path_exists(&candidate).await? {
            matches.push(candidate);
            if !include_all {
                break;
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }

    matches.reverse();
    Ok(matches)
}

/// Errors that can occur while discovering or reading context files.
#[derive(Debug, Error)]
pub enum ContextError {
    /// A filesystem metadata or directory-listing operation failed.
    ///
    /// This typically means the path exists but is not accessible (permission
    /// denied, broken symlink, etc.).
    #[error("failed to inspect {path}: {error}")]
    InspectFailed {
        /// The path that could not be inspected.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        error: std::io::Error,
    },
    /// Reading the contents of a discovered file failed.
    #[error("failed to read {path}: {error}")]
    ReadFailed {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        error: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[tokio::test]
    async fn discovers_agents_file_in_ancestors() {
        let root = temp_path("agentkit-context-agents");
        let nested = root.join("nested/project");
        async_fs::create_dir_all(&nested).await.unwrap();
        let agents_path = root.join("AGENTS.md");
        async_fs::write(&agents_path, "project = lantern")
            .await
            .unwrap();

        let items = AgentsMd::discover(&nested).load().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Context);
        assert_eq!(
            items[0].metadata.get("agentkit.context.source"),
            Some(&Value::String("agents_md".into()))
        );

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn discovers_all_agents_files_when_requested() {
        let root = temp_path("agentkit-context-agents-all");
        let nested = root.join("nested/project");
        async_fs::create_dir_all(&nested).await.unwrap();
        async_fs::write(root.join("AGENTS.md"), "project = lantern")
            .await
            .unwrap();
        async_fs::write(root.join("nested/AGENTS.md"), "team = orbit")
            .await
            .unwrap();

        let items = AgentsMd::discover_all(&nested).load().await.unwrap();
        assert_eq!(items.len(), 2);

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn loads_agents_from_explicit_search_paths() {
        let root = temp_path("agentkit-context-agents-explicit");
        let nested = root.join("nested/project");
        let shared = root.join("shared");
        async_fs::create_dir_all(&nested).await.unwrap();
        async_fs::create_dir_all(&shared).await.unwrap();
        async_fs::write(shared.join("AGENTS.md"), "policy = explicit")
            .await
            .unwrap();

        let items = AgentsMd::discover(&nested)
            .with_search_dir(&shared)
            .load()
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert!(
            items[0]
                .metadata
                .get("agentkit.context.path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("/shared/AGENTS.md"))
        );

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    fn temp_path(prefix: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{suffix}"))
    }
}
