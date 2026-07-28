//! Filesystem tools and session-scoped policies for agentkit.
//!
//! This crate provides a set of tools that let an agent interact with the
//! local filesystem: reading, writing, replacing, moving, deleting files,
//! listing directories, and creating directories. All tools implement the
//! [`Tool`](agentkit_tools_core::Tool) trait and can be registered with a
//! [`ToolRegistry`](agentkit_tools_core::ToolRegistry).
//!
//! The crate also provides [`FileSystemToolResources`] and
//! [`FileSystemToolPolicy`] for enforcing session-scoped access rules such
//! as requiring a path to be read before it can be modified.
//!
//! # Quick start
//!
//! ```rust
//! use agentkit_tool_fs::{registry, FileSystemToolPolicy, FileSystemToolResources};
//!
//! // Get a registry with all seven filesystem tools.
//! let reg = registry();
//! assert_eq!(reg.specs().len(), 7);
//!
//! // Optionally configure a policy to guard mutations.
//! let resources = FileSystemToolResources::new()
//!     .with_policy(
//!         FileSystemToolPolicy::new()
//!             .require_read_before_write(true),
//!     );
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use agentkit_core::{MetadataMap, SessionId, ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    FileSystemPermissionRequest, PermissionCode, PermissionDenial, PermissionRequest, Tool,
    ToolAnnotations, ToolContext, ToolError, ToolName, ToolOutputArtifact, ToolOutputArtifactId,
    ToolOutputArtifactSlice, ToolOutputArtifactStore, ToolOutputLimit, ToolOutputTruncationContext,
    ToolRegistry, ToolRequest, ToolResources, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use futures_lite::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

/// Default model-facing budget advertised by filesystem tools that can return
/// unbounded data. Hosts can override this per tool in their executor output
/// truncation strategy.
pub const DEFAULT_UNBOUNDED_OUTPUT_MAX_BYTES: usize = 150_000;

/// File-backed store for oversized tool-result artifacts.
///
/// This is intended for hosts that want stored results to survive outside the
/// current process. Pair it with `agentkit_tools_core::tool_result_readback_registry`
/// for bounded readback.
#[derive(Debug)]
pub struct FileToolOutputArtifactStore {
    root: PathBuf,
    next_id: AtomicU64,
}

impl FileToolOutputArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            next_id: AtomicU64::new(0),
        }
    }

    fn artifact_path(&self, id: &ToolOutputArtifactId) -> Result<PathBuf, ToolError> {
        if id.0.is_empty()
            || !id
                .0
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ToolError::InvalidInput(format!(
                "invalid tool result artifact id: {id}"
            )));
        }
        Ok(self.root.join(format!("{}.txt", id.0)))
    }
}

#[async_trait]
impl ToolOutputArtifactStore for FileToolOutputArtifactStore {
    async fn put(
        &self,
        ctx: &ToolOutputTruncationContext,
        body: String,
        original_bytes: usize,
    ) -> Result<ToolOutputArtifact, ToolError> {
        async_fs::create_dir_all(&self.root)
            .await
            .map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to create tool result artifact directory {}: {error}",
                    self.root.display()
                ))
            })?;
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = ToolOutputArtifactId(format!(
            "{}-{}-{n}",
            sanitize_artifact_id_component(ctx.session_id.0.as_str()),
            sanitize_artifact_id_component(ctx.call_id.0.as_str())
        ));
        async_fs::write(self.artifact_path(&id)?, body.as_bytes())
            .await
            .map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to write tool result artifact {id}: {error}"
                ))
            })?;
        Ok(ToolOutputArtifact {
            id,
            tool_name: ctx.tool_name.clone(),
            call_id: ctx.call_id.clone(),
            session_id: ctx.session_id.clone(),
            turn_id: ctx.turn_id.clone(),
            original_bytes,
            body,
        })
    }

    async fn read(
        &self,
        id: &ToolOutputArtifactId,
        offset: usize,
        max_bytes: usize,
    ) -> Result<ToolOutputArtifactSlice, ToolError> {
        let body = async_fs::read_to_string(self.artifact_path(id)?)
            .await
            .map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to read tool result artifact {id}: {error}"
                ))
            })?;
        if offset > body.len() || !body.is_char_boundary(offset) {
            return Err(ToolError::InvalidInput(format!(
                "offset {offset} is not a UTF-8 boundary in tool result artifact {id}"
            )));
        }
        let requested_end = offset.saturating_add(max_bytes).min(body.len());
        let end = body.floor_char_boundary(requested_end);
        Ok(ToolOutputArtifactSlice {
            id: id.clone(),
            offset,
            next_offset: end,
            original_bytes: body.len(),
            eof: end == body.len(),
            content: body[offset..end].to_string(),
        })
    }
}

fn sanitize_artifact_id_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "_".to_string()
    } else {
        cleaned
    }
}

/// Creates a [`ToolRegistry`] pre-populated with all filesystem tools.
///
/// The returned registry contains [`ReadFileTool`], [`WriteFileTool`],
/// [`ReplaceInFileTool`], [`MoveTool`], [`DeleteTool`], [`ListDirectoryTool`],
/// and [`CreateDirectoryTool`], each with default configuration.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::registry;
///
/// let reg = registry();
/// let specs = reg.specs();
/// assert_eq!(specs.len(), 7);
/// ```
pub fn registry() -> ToolRegistry {
    ToolRegistry::new()
        .with(ReadFileTool::default())
        .with(WriteFileTool::default())
        .with(ReplaceInFileTool::default())
        .with(MoveTool::default())
        .with(DeleteTool::default())
        .with(ListDirectoryTool::default())
        .with(CreateDirectoryTool::default())
}

/// Errors specific to filesystem tool operations.
///
/// These are domain errors that arise from invalid arguments or unsupported
/// paths rather than I/O failures. They are typically converted into
/// [`ToolError::InvalidInput`](agentkit_tools_core::ToolError::InvalidInput)
/// before being returned to the caller.
#[derive(Debug, Error)]
pub enum FileSystemToolError {
    /// The given path cannot be represented as valid UTF-8.
    #[error("path {0} is not valid UTF-8")]
    InvalidUtf8Path(PathBuf),
    /// The requested line range is invalid (e.g. `from` exceeds `to`).
    #[error("invalid line range: from={from:?} to={to:?}")]
    InvalidLineRange {
        /// 1-based inclusive start line, if specified.
        from: Option<usize>,
        /// 1-based inclusive end line, if specified.
        to: Option<usize>,
    },
}

/// Policy governing session-scoped filesystem access rules.
///
/// Policies are enforced by [`FileSystemToolResources`] on a per-session basis.
/// The primary policy today is `require_read_before_write`, which prevents an
/// agent from mutating a path it has not first inspected (via read or list).
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::FileSystemToolPolicy;
///
/// let policy = FileSystemToolPolicy::new()
///     .require_read_before_write(true);
/// ```
#[derive(Clone, Debug, Default)]
pub struct FileSystemToolPolicy {
    require_read_before_write: bool,
}

impl FileSystemToolPolicy {
    /// Creates a new policy with all rules disabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// When enabled, the agent must read (or list) a path before it can write,
    /// replace, move, or delete it. This helps prevent accidental overwrites.
    ///
    /// Defaults to `false`.
    pub fn require_read_before_write(mut self, value: bool) -> Self {
        self.require_read_before_write = value;
        self
    }
}

#[derive(Default)]
struct SessionAccessState {
    inspected_paths: BTreeSet<PathBuf>,
}

/// Session-scoped resource state for filesystem tools.
///
/// `FileSystemToolResources` implements
/// [`ToolResources`](agentkit_tools_core::ToolResources) and tracks which paths
/// each session has inspected. Combined with a [`FileSystemToolPolicy`], it
/// enforces rules such as requiring a read before a write.
///
/// Pass an instance as the `resources` field of
/// [`ToolContext`](agentkit_tools_core::ToolContext) so that filesystem tools
/// can record and check access.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::{FileSystemToolPolicy, FileSystemToolResources};
///
/// let resources = FileSystemToolResources::new()
///     .with_policy(
///         FileSystemToolPolicy::new()
///             .require_read_before_write(true),
///     );
/// ```
#[derive(Default)]
pub struct FileSystemToolResources {
    policy: FileSystemToolPolicy,
    sessions: Mutex<BTreeMap<SessionId, SessionAccessState>>,
}

impl FileSystemToolResources {
    /// Creates a new resource instance with all policies disabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the [`FileSystemToolPolicy`] that governs mutation guards.
    pub fn with_policy(mut self, policy: FileSystemToolPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Records that the given path was read during `session_id`.
    ///
    /// This marks the path as inspected, satisfying any
    /// `require_read_before_write` policy for subsequent mutations.
    pub fn record_read(&self, session_id: &SessionId, path: &Path) {
        self.record_inspected_path(session_id, path);
    }

    /// Records that the given directory was listed during `session_id`.
    ///
    /// Like [`record_read`](Self::record_read), this marks the path as
    /// inspected.
    pub fn record_list(&self, session_id: &SessionId, path: &Path) {
        self.record_inspected_path(session_id, path);
    }

    /// Records that the given path was written during `session_id`.
    ///
    /// After a write the path is considered inspected, so subsequent mutations
    /// are allowed without an additional read.
    pub fn record_written(&self, session_id: &SessionId, path: &Path) {
        self.record_inspected_path(session_id, path);
    }

    /// Records that a path was moved from `from` to `to` during `session_id`.
    ///
    /// The old path is removed from the inspected set and the new path is
    /// added.
    pub fn record_moved(&self, session_id: &SessionId, from: &Path, to: &Path) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|err| err.into_inner());
        let state = sessions.entry(session_id.clone()).or_default();
        state.inspected_paths.remove(from);
        state.inspected_paths.insert(to.to_path_buf());
    }

    fn ensure_mutation_allowed(
        &self,
        session_id: Option<&SessionId>,
        action: &'static str,
        path: &Path,
        target_exists: bool,
    ) -> Result<(), ToolError> {
        if !self.policy.require_read_before_write || !target_exists {
            return Ok(());
        }

        let Some(session_id) = session_id else {
            return Err(read_before_write_denial(action, path));
        };

        let sessions = self.sessions.lock().unwrap_or_else(|err| err.into_inner());
        let Some(state) = sessions.get(session_id) else {
            return Err(read_before_write_denial(action, path));
        };

        if state
            .inspected_paths
            .iter()
            .any(|inspected| path == inspected || path.starts_with(inspected))
        {
            Ok(())
        } else {
            Err(read_before_write_denial(action, path))
        }
    }

    fn record_inspected_path(&self, session_id: &SessionId, path: &Path) {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .entry(session_id.clone())
            .or_default()
            .inspected_paths
            .insert(path.to_path_buf());
    }
}

impl ToolResources for FileSystemToolResources {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Reads a UTF-8 text file, optionally limited to a 1-based inclusive line range.
///
/// Tool name: `fs_read_file`
///
/// When [`FileSystemToolResources`] is available in the tool context, a
/// successful read marks the path as inspected for the current session.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::ReadFileTool;
/// use agentkit_tools_core::Tool;
///
/// let tool = ReadFileTool::default();
/// assert_eq!(&tool.spec().name.0, "fs_read_file");
/// ```
#[derive(Clone, Debug)]
pub struct ReadFileTool {
    spec: ToolSpec,
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self {
            spec: ToolSpec {
                name: ToolName::new("fs_read_file"),
                description: "Read a UTF-8 text file from disk, optionally limited to a 1-based inclusive line range."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "from": { "type": "integer", "minimum": 1 },
                        "to": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                output_schema: Some(json!({
                    "type": "string",
                    "description": "Raw file contents (possibly sliced to the requested line range)."
                })),
                annotations: ToolAnnotations {
                    read_only_hint: true,
                    idempotent_hint: true,
                    ..ToolAnnotations::default()
                },
                metadata: MetadataMap::new(),
            },
        }
        .with_default_output_limit()
    }
}

impl ReadFileTool {
    fn with_default_output_limit(mut self) -> Self {
        self.spec = self
            .spec
            .with_output_limit(ToolOutputLimit::store_for_readback(
                DEFAULT_UNBOUNDED_OUTPUT_MAX_BYTES,
            ));
        self
    }
}

#[derive(Deserialize)]
struct ReadFileInput {
    path: PathBuf,
    from: Option<usize>,
    to: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        let input: ReadFileInput = parse_input(&request.input)?;
        Ok(vec![Box::new(FileSystemPermissionRequest::Read {
            path: input.path,
            metadata: request.metadata.clone(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let started = Instant::now();
        let input: ReadFileInput = parse_input(&request.input)?;
        validate_line_range(input.from, input.to)?;

        let contents = async_fs::read_to_string(&input.path)
            .await
            .map_err(|error| ToolError::ExecutionFailed(format!("failed to read file: {error}")))?;
        let sliced = slice_lines(&contents, input.from, input.to)?;

        if let (Some(session_id), Some(resources)) = (
            ctx.capability.session_id,
            file_system_resources(ctx.resources),
        ) {
            resources.record_read(session_id, &input.path);
        }

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Text(sliced),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: Some(started.elapsed()),
            metadata: MetadataMap::new(),
        })
    }
}

/// Writes UTF-8 text to a file, creating parent directories if needed.
///
/// Tool name: `fs_write_file`
///
/// If `require_read_before_write` is active and the target file already exists,
/// this tool will refuse to execute unless the path was previously inspected
/// during the same session.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::WriteFileTool;
/// use agentkit_tools_core::Tool;
///
/// let tool = WriteFileTool::default();
/// assert_eq!(&tool.spec().name.0, "fs_write_file");
/// ```
#[derive(Clone, Debug)]
pub struct WriteFileTool {
    spec: ToolSpec,
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self {
            spec: ToolSpec {
                name: ToolName::new("fs_write_file"),
                description: "Write UTF-8 text to a file, creating parent directories if needed."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "contents": { "type": "string" },
                        "create_parents": { "type": "boolean", "default": true }
                    },
                    "required": ["path", "contents"],
                    "additionalProperties": false
                }),
                output_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "bytes_written": { "type": "integer" },
                        "created": { "type": "boolean", "description": "true if the file did not exist before this call" }
                    },
                    "required": ["path", "bytes_written", "created"]
                })),
                annotations: ToolAnnotations {
                    destructive_hint: true,
                    idempotent_hint: false,
                    ..ToolAnnotations::default()
                },
                metadata: MetadataMap::new(),
            },
        }
    }
}

#[derive(Deserialize)]
struct WriteFileInput {
    path: PathBuf,
    contents: String,
    #[serde(default = "default_true")]
    create_parents: bool,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        let input: WriteFileInput = parse_input(&request.input)?;
        Ok(vec![Box::new(FileSystemPermissionRequest::Write {
            path: input.path,
            metadata: request.metadata.clone(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let started = Instant::now();
        let input: WriteFileInput = parse_input(&request.input)?;
        let existed = path_exists(&input.path).await?;
        enforce_mutation_policy(ctx, "write", &input.path, existed)?;

        if input.create_parents
            && let Some(parent) = input.path.parent()
        {
            async_fs::create_dir_all(parent).await.map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to create parent directories for {}: {error}",
                    input.path.display()
                ))
            })?;
        }

        async_fs::write(&input.path, input.contents.as_bytes())
            .await
            .map_err(|error| {
                ToolError::ExecutionFailed(format!("failed to write file: {error}"))
            })?;

        if let (Some(session_id), Some(resources)) = (
            ctx.capability.session_id,
            file_system_resources(ctx.resources),
        ) {
            resources.record_written(session_id, &input.path);
        }

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Structured(json!({
                    "path": input.path.display().to_string(),
                    "bytes_written": input.contents.len(),
                    "created": !existed,
                })),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: Some(started.elapsed()),
            metadata: MetadataMap::new(),
        })
    }
}

/// Replaces exact text within a UTF-8 file.
///
/// Tool name: `fs_replace_in_file`
///
/// Fails if the search text is not found. Supports replacing only the first
/// occurrence (default) or all occurrences via the `replace_all` input flag.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::ReplaceInFileTool;
/// use agentkit_tools_core::Tool;
///
/// let tool = ReplaceInFileTool::default();
/// assert_eq!(&tool.spec().name.0, "fs_replace_in_file");
/// ```
#[derive(Clone, Debug)]
pub struct ReplaceInFileTool {
    spec: ToolSpec,
}

impl Default for ReplaceInFileTool {
    fn default() -> Self {
        Self {
            spec: ToolSpec {
                name: ToolName::new("fs_replace_in_file"),
                description:
                    "Replace exact text in a UTF-8 file. Fails if the search text is not found."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "find": { "type": "string" },
                        "replace": { "type": "string" },
                        "replace_all": { "type": "boolean", "default": false }
                    },
                    "required": ["path", "find", "replace"],
                    "additionalProperties": false
                }),
                output_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "replacements": { "type": "integer" }
                    },
                    "required": ["path", "replacements"]
                })),
                annotations: ToolAnnotations {
                    destructive_hint: true,
                    idempotent_hint: false,
                    ..ToolAnnotations::default()
                },
                metadata: MetadataMap::new(),
            },
        }
    }
}

impl ListDirectoryTool {
    fn with_default_output_limit(mut self) -> Self {
        self.spec = self
            .spec
            .with_output_limit(ToolOutputLimit::store_for_readback(
                DEFAULT_UNBOUNDED_OUTPUT_MAX_BYTES,
            ));
        self
    }
}

#[derive(Deserialize)]
struct ReplaceInFileInput {
    path: PathBuf,
    find: String,
    replace: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for ReplaceInFileTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        let input: ReplaceInFileInput = parse_input(&request.input)?;
        Ok(vec![Box::new(FileSystemPermissionRequest::Edit {
            path: input.path,
            metadata: request.metadata.clone(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let started = Instant::now();
        let input: ReplaceInFileInput = parse_input(&request.input)?;
        enforce_mutation_policy(ctx, "edit", &input.path, true)?;

        let contents = async_fs::read_to_string(&input.path)
            .await
            .map_err(|error| ToolError::ExecutionFailed(format!("failed to read file: {error}")))?;

        let replacement_count = contents.matches(&input.find).count();
        if replacement_count == 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "search text not found in {}",
                input.path.display()
            )));
        }

        let updated = if input.replace_all {
            contents.replace(&input.find, &input.replace)
        } else {
            contents.replacen(&input.find, &input.replace, 1)
        };
        let applied = if input.replace_all {
            replacement_count
        } else {
            1
        };

        async_fs::write(&input.path, updated.as_bytes())
            .await
            .map_err(|error| {
                ToolError::ExecutionFailed(format!("failed to write file: {error}"))
            })?;

        if let (Some(session_id), Some(resources)) = (
            ctx.capability.session_id,
            file_system_resources(ctx.resources),
        ) {
            resources.record_written(session_id, &input.path);
        }

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Structured(json!({
                    "path": input.path.display().to_string(),
                    "replacements": applied,
                })),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: Some(started.elapsed()),
            metadata: MetadataMap::new(),
        })
    }
}

/// Moves or renames a file or directory.
///
/// Tool name: `fs_move`
///
/// Optionally creates parent directories for the destination and can overwrite
/// an existing target when `overwrite` is set. Subject to `require_read_before_write`
/// policy on the source path.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::MoveTool;
/// use agentkit_tools_core::Tool;
///
/// let tool = MoveTool::default();
/// assert_eq!(&tool.spec().name.0, "fs_move");
/// ```
#[derive(Clone, Debug)]
pub struct MoveTool {
    spec: ToolSpec,
}

impl Default for MoveTool {
    fn default() -> Self {
        Self {
            spec: ToolSpec {
                name: ToolName::new("fs_move"),
                description: "Move or rename a file or directory.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" },
                        "create_parents": { "type": "boolean", "default": true },
                        "overwrite": { "type": "boolean", "default": false }
                    },
                    "required": ["from", "to"],
                    "additionalProperties": false
                }),
                output_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" },
                        "moved": { "type": "boolean", "const": true }
                    },
                    "required": ["from", "to", "moved"]
                })),
                annotations: ToolAnnotations {
                    destructive_hint: true,
                    idempotent_hint: false,
                    ..ToolAnnotations::default()
                },
                metadata: MetadataMap::new(),
            },
        }
    }
}

#[derive(Deserialize)]
struct MoveInput {
    from: PathBuf,
    to: PathBuf,
    #[serde(default = "default_true")]
    create_parents: bool,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait]
impl Tool for MoveTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        let input: MoveInput = parse_input(&request.input)?;
        Ok(vec![Box::new(FileSystemPermissionRequest::Move {
            from: input.from,
            to: input.to,
            metadata: request.metadata.clone(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let started = Instant::now();
        let input: MoveInput = parse_input(&request.input)?;
        enforce_mutation_policy(ctx, "move", &input.from, true)?;

        if path_exists(&input.to).await? {
            if input.overwrite {
                remove_path(&input.to, true, true).await?;
            } else {
                return Err(ToolError::ExecutionFailed(format!(
                    "destination {} already exists",
                    input.to.display()
                )));
            }
        }

        if input.create_parents
            && let Some(parent) = input.to.parent()
        {
            async_fs::create_dir_all(parent).await.map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to create parent directories for {}: {error}",
                    input.to.display()
                ))
            })?;
        }

        async_fs::rename(&input.from, &input.to)
            .await
            .map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to move {} to {}: {error}",
                    input.from.display(),
                    input.to.display()
                ))
            })?;

        if let (Some(session_id), Some(resources)) = (
            ctx.capability.session_id,
            file_system_resources(ctx.resources),
        ) {
            resources.record_moved(session_id, &input.from, &input.to);
        }

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Structured(json!({
                    "from": input.from.display().to_string(),
                    "to": input.to.display().to_string(),
                    "moved": true,
                })),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: Some(started.elapsed()),
            metadata: MetadataMap::new(),
        })
    }
}

/// Deletes a file or directory.
///
/// Tool name: `fs_delete`
///
/// For directories, set `recursive` to remove non-empty directories. The
/// `missing_ok` flag suppresses errors when the target does not exist.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::DeleteTool;
/// use agentkit_tools_core::Tool;
///
/// let tool = DeleteTool::default();
/// assert_eq!(&tool.spec().name.0, "fs_delete");
/// ```
#[derive(Clone, Debug)]
pub struct DeleteTool {
    spec: ToolSpec,
}

impl Default for DeleteTool {
    fn default() -> Self {
        Self {
            spec: ToolSpec {
                name: ToolName::new("fs_delete"),
                description: "Delete a file or directory.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "recursive": { "type": "boolean", "default": false },
                        "missing_ok": { "type": "boolean", "default": false }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                output_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "deleted": { "type": "boolean" },
                        "missing": { "type": "boolean", "description": "true when missing_ok=true and the path did not exist" }
                    },
                    "required": ["path", "deleted"]
                })),
                annotations: ToolAnnotations {
                    destructive_hint: true,
                    idempotent_hint: false,
                    ..ToolAnnotations::default()
                },
                metadata: MetadataMap::new(),
            },
        }
    }
}

#[derive(Deserialize)]
struct DeleteInput {
    path: PathBuf,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    missing_ok: bool,
}

#[async_trait]
impl Tool for DeleteTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        let input: DeleteInput = parse_input(&request.input)?;
        Ok(vec![Box::new(FileSystemPermissionRequest::Delete {
            path: input.path,
            metadata: request.metadata.clone(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let started = Instant::now();
        let input: DeleteInput = parse_input(&request.input)?;
        let existed = path_exists(&input.path).await?;
        if !existed && input.missing_ok {
            return Ok(ToolResult {
                result: ToolResultPart {
                    call_id: request.call_id,
                    output: ToolOutput::Structured(json!({
                        "path": input.path.display().to_string(),
                        "deleted": false,
                        "missing": true,
                    })),
                    is_error: false,
                    metadata: MetadataMap::new(),
                },
                duration: Some(started.elapsed()),
                metadata: MetadataMap::new(),
            });
        }

        enforce_mutation_policy(ctx, "delete", &input.path, existed)?;
        remove_path(&input.path, input.recursive, false).await?;

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Structured(json!({
                    "path": input.path.display().to_string(),
                    "deleted": true,
                })),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: Some(started.elapsed()),
            metadata: MetadataMap::new(),
        })
    }
}

/// Lists entries in a directory.
///
/// Tool name: `fs_list_directory`
///
/// Returns a JSON array of objects with `name`, `path`, and `kind` (one of
/// `"file"`, `"directory"`, or `"symlink"`) for each entry. A successful list
/// marks the directory as inspected for `require_read_before_write` purposes.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::ListDirectoryTool;
/// use agentkit_tools_core::Tool;
///
/// let tool = ListDirectoryTool::default();
/// assert_eq!(&tool.spec().name.0, "fs_list_directory");
/// ```
#[derive(Clone, Debug)]
pub struct ListDirectoryTool {
    spec: ToolSpec,
}

impl Default for ListDirectoryTool {
    fn default() -> Self {
        Self {
            spec: ToolSpec {
                name: ToolName::new("fs_list_directory"),
                description: "List the entries in a directory.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                output_schema: Some(json!({
                    "type": "array",
                    "description": "Direct entries (non-recursive) in the requested directory.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Entry file name, without any leading directory." },
                            "path": { "type": "string", "description": "Absolute path to the entry." },
                            "kind": { "type": "string", "enum": ["file", "directory", "symlink"] }
                        },
                        "required": ["name", "path", "kind"]
                    }
                })),
                annotations: ToolAnnotations {
                    read_only_hint: true,
                    idempotent_hint: true,
                    ..ToolAnnotations::default()
                },
                metadata: MetadataMap::new(),
            },
        }
        .with_default_output_limit()
    }
}

#[derive(Deserialize)]
struct ListDirectoryInput {
    path: PathBuf,
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        let input: ListDirectoryInput = parse_input(&request.input)?;
        Ok(vec![Box::new(FileSystemPermissionRequest::List {
            path: input.path,
            metadata: request.metadata.clone(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let started = Instant::now();
        let input: ListDirectoryInput = parse_input(&request.input)?;
        let mut entries = Vec::new();
        let mut dir = async_fs::read_dir(&input.path).await.map_err(|error| {
            ToolError::ExecutionFailed(format!(
                "failed to list directory {}: {error}",
                input.path.display()
            ))
        })?;

        while let Some(entry) = dir.next().await {
            let entry = entry.map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to read directory entry in {}: {error}",
                    input.path.display()
                ))
            })?;
            let file_type = entry.file_type().await.map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to inspect directory entry in {}: {error}",
                    input.path.display()
                ))
            })?;
            entries.push(json!({
                "name": path_name(entry.path())?,
                "path": entry.path().display().to_string(),
                "kind": file_kind_label(&file_type),
            }));
        }

        if let (Some(session_id), Some(resources)) = (
            ctx.capability.session_id,
            file_system_resources(ctx.resources),
        ) {
            resources.record_list(session_id, &input.path);
        }

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Structured(Value::Array(entries)),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: Some(started.elapsed()),
            metadata: MetadataMap::new(),
        })
    }
}

/// Creates a directory and any missing parent directories.
///
/// Tool name: `fs_create_directory`
///
/// This is idempotent: calling it on an already-existing directory succeeds
/// without error.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_fs::CreateDirectoryTool;
/// use agentkit_tools_core::Tool;
///
/// let tool = CreateDirectoryTool::default();
/// assert_eq!(&tool.spec().name.0, "fs_create_directory");
/// ```
#[derive(Clone, Debug)]
pub struct CreateDirectoryTool {
    spec: ToolSpec,
}

impl Default for CreateDirectoryTool {
    fn default() -> Self {
        Self {
            spec: ToolSpec {
                name: ToolName::new("fs_create_directory"),
                description: "Create a directory and any missing parent directories.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                output_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "created": { "type": "boolean", "const": true }
                    },
                    "required": ["path", "created"]
                })),
                annotations: ToolAnnotations {
                    destructive_hint: true,
                    idempotent_hint: true,
                    ..ToolAnnotations::default()
                },
                metadata: MetadataMap::new(),
            },
        }
    }
}

#[derive(Deserialize)]
struct CreateDirectoryInput {
    path: PathBuf,
}

#[async_trait]
impl Tool for CreateDirectoryTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        let input: CreateDirectoryInput = parse_input(&request.input)?;
        Ok(vec![Box::new(FileSystemPermissionRequest::CreateDir {
            path: input.path,
            metadata: request.metadata.clone(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let started = Instant::now();
        let input: CreateDirectoryInput = parse_input(&request.input)?;
        async_fs::create_dir_all(&input.path)
            .await
            .map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to create directory {}: {error}",
                    input.path.display()
                ))
            })?;

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Structured(json!({
                    "path": input.path.display().to_string(),
                    "created": true,
                })),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: Some(started.elapsed()),
            metadata: MetadataMap::new(),
        })
    }
}

fn parse_input<T>(value: &Value) -> Result<T, ToolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(value.clone())
        .map_err(|error| ToolError::InvalidInput(format!("invalid tool input: {error}")))
}

fn default_true() -> bool {
    true
}

fn validate_line_range(from: Option<usize>, to: Option<usize>) -> Result<(), ToolError> {
    if matches!((from, to), (Some(start), Some(end)) if end < start) {
        return Err(ToolError::InvalidInput(
            FileSystemToolError::InvalidLineRange { from, to }.to_string(),
        ));
    }
    Ok(())
}

fn slice_lines(
    contents: &str,
    from: Option<usize>,
    to: Option<usize>,
) -> Result<String, ToolError> {
    validate_line_range(from, to)?;
    if from.is_none() && to.is_none() {
        return Ok(contents.to_string());
    }

    let start = from.unwrap_or(1);
    let end = to.unwrap_or(usize::MAX);
    let selected = contents
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line_number = index + 1;
            (line_number >= start && line_number <= end).then_some(line)
        })
        .collect::<Vec<_>>();

    Ok(selected.join("\n"))
}

fn file_system_resources(resources: &dyn ToolResources) -> Option<&FileSystemToolResources> {
    resources.as_any().downcast_ref::<FileSystemToolResources>()
}

fn enforce_mutation_policy(
    ctx: &ToolContext<'_>,
    action: &'static str,
    path: &Path,
    target_exists: bool,
) -> Result<(), ToolError> {
    let Some(resources) = file_system_resources(ctx.resources) else {
        return Ok(());
    };

    resources.ensure_mutation_allowed(ctx.capability.session_id, action, path, target_exists)
}

fn read_before_write_denial(action: &'static str, path: &Path) -> ToolError {
    ToolError::PermissionDenied(PermissionDenial {
        code: PermissionCode::CustomPolicyDenied,
        message: format!(
            "filesystem policy requires reading {} before attempting to {} it",
            path.display(),
            action
        ),
        metadata: MetadataMap::new(),
    })
}

async fn path_exists(path: &Path) -> Result<bool, ToolError> {
    Ok(async_fs::metadata(path).await.is_ok())
}

async fn remove_path(path: &Path, recursive: bool, overwrite: bool) -> Result<(), ToolError> {
    let metadata = async_fs::metadata(path).await.map_err(|error| {
        ToolError::ExecutionFailed(format!(
            "failed to inspect {} before deletion: {error}",
            path.display()
        ))
    })?;

    if metadata.is_dir() {
        if recursive || overwrite {
            async_fs::remove_dir_all(path).await.map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to remove directory {}: {error}",
                    path.display()
                ))
            })?;
        } else {
            async_fs::remove_dir(path).await.map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "failed to remove directory {}: {error}",
                    path.display()
                ))
            })?;
        }
    } else {
        async_fs::remove_file(path).await.map_err(|error| {
            ToolError::ExecutionFailed(format!("failed to remove file {}: {error}", path.display()))
        })?;
    }

    Ok(())
}

fn path_name(path: impl AsRef<Path>) -> Result<String, ToolError> {
    let path = path.as_ref();
    let name = path.file_name().ok_or_else(|| {
        ToolError::ExecutionFailed(format!("path {} has no file name", path.display()))
    })?;

    name.to_str().map(|value| value.to_string()).ok_or_else(|| {
        ToolError::ExecutionFailed(
            FileSystemToolError::InvalidUtf8Path(path.to_path_buf()).to_string(),
        )
    })
}

fn file_kind_label(file_type: &std::fs::FileType) -> &'static str {
    if file_type.is_dir() {
        "directory"
    } else if file_type.is_symlink() {
        "symlink"
    } else {
        "file"
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use agentkit_capabilities::CapabilityContext;
    use agentkit_core::{SessionId, ToolCallId, TurnId};
    use agentkit_tools_core::{
        BasicToolExecutor, PermissionChecker, PermissionDecision, TOOL_OUTPUT_LIMIT_METADATA_KEY,
        ToolExecutionOutcome, ToolExecutor,
    };

    use super::*;

    struct AllowAll;

    impl PermissionChecker for AllowAll {
        fn evaluate(&self, _request: &dyn PermissionRequest) -> PermissionDecision {
            PermissionDecision::Allow
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time moved backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("agentkit-{name}-{nanos}"))
    }

    fn tool_context<'a>(
        session_id: &'a SessionId,
        turn_id: &'a TurnId,
        metadata: &'a MetadataMap,
        resources: &'a dyn ToolResources,
    ) -> ToolContext<'a> {
        ToolContext {
            capability: CapabilityContext {
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                metadata,
            },
            permissions: &AllowAll,
            resources,
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        }
    }

    fn request(
        tool_name: &str,
        input: Value,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> ToolRequest {
        ToolRequest {
            call_id: ToolCallId::new(format!("call-{tool_name}")),
            tool_name: ToolName::new(tool_name),
            input,
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            metadata: MetadataMap::new(),
        }
    }

    #[test]
    fn registry_exposes_expected_tools() {
        let specs = registry().specs();
        let names: Vec<_> = specs.into_iter().map(|spec| spec.name.0).collect();
        assert!(names.contains(&"fs_read_file".into()));
        assert!(names.contains(&"fs_write_file".into()));
        assert!(names.contains(&"fs_replace_in_file".into()));
        assert!(names.contains(&"fs_move".into()));
        assert!(names.contains(&"fs_delete".into()));
        assert!(names.contains(&"fs_list_directory".into()));
        assert!(names.contains(&"fs_create_directory".into()));
    }

    #[test]
    fn unbounded_read_tools_advertise_store_for_readback_output_limit() {
        let specs = registry().specs();
        for name in ["fs_read_file", "fs_list_directory"] {
            let spec = specs.iter().find(|spec| spec.name.0 == name).expect("spec");
            let limit: ToolOutputLimit = serde_json::from_value(
                spec.metadata
                    .get(TOOL_OUTPUT_LIMIT_METADATA_KEY)
                    .expect("output limit metadata")
                    .clone(),
            )
            .expect("valid output limit metadata");
            assert_eq!(limit, ToolOutputLimit::store_for_readback(150_000));
        }
    }

    #[tokio::test]
    async fn file_tool_output_artifact_store_roundtrips_bounded_slices() {
        let root = temp_dir("artifact-store");
        let store = FileToolOutputArtifactStore::new(&root);
        let ctx = ToolOutputTruncationContext {
            tool_name: ToolName::new("big"),
            call_id: ToolCallId::new("call"),
            session_id: SessionId::new("session"),
            turn_id: TurnId::new("turn"),
            tool_spec: ToolSpec::new("big", "big output", json!({"type": "object"})),
        };

        let artifact = store
            .put(&ctx, "abcdef".to_string(), 6)
            .await
            .expect("store artifact");
        let slice = store
            .read(&artifact.id, 2, 3)
            .await
            .expect("read artifact slice");
        assert_eq!(slice.content, "cde");
        assert_eq!(slice.next_offset, 5);
        assert!(!slice.eof);
    }

    #[tokio::test]
    async fn file_tool_output_artifact_store_rejects_path_like_ids() {
        let root = temp_dir("artifact-store-escape");
        let store = FileToolOutputArtifactStore::new(&root);

        let err = store
            .read(&ToolOutputArtifactId("../secret".to_string()), 0, 1)
            .await
            .expect_err("path-like artifact ids must be rejected");
        match err {
            ToolError::InvalidInput(message) => assert!(message.contains("invalid")),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_then_ranged_read_roundtrip() {
        let root = temp_dir("fs");
        async_fs::create_dir_all(&root).await.unwrap();
        let target = root.join("note.txt");
        let session_id = SessionId::new("session-1");
        let turn_id = TurnId::new("turn-1");

        let executor = BasicToolExecutor::from_registry(registry());
        let metadata = MetadataMap::new();
        let mut ctx = tool_context(&session_id, &turn_id, &metadata, &());

        let write = executor
            .execute(
                request(
                    "fs_write_file",
                    json!({
                        "path": target.display().to_string(),
                        "contents": "alpha\nbeta\ngamma"
                    }),
                    &session_id,
                    &turn_id,
                ),
                &mut ctx,
            )
            .await;
        assert!(matches!(write, ToolExecutionOutcome::Completed(_)));

        let read = executor
            .execute(
                request(
                    "fs_read_file",
                    json!({
                        "path": target.display().to_string(),
                        "from": 2,
                        "to": 3
                    }),
                    &session_id,
                    &turn_id,
                ),
                &mut ctx,
            )
            .await;

        match read {
            ToolExecutionOutcome::Completed(result) => {
                assert_eq!(result.result.output, ToolOutput::Text("beta\ngamma".into()));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }

        let _ = async_fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn replace_move_and_delete_work() {
        let root = temp_dir("fs-edit");
        async_fs::create_dir_all(&root).await.unwrap();
        let source = root.join("source.txt");
        let destination = root.join("archive").join("renamed.txt");
        async_fs::write(&source, "hello world").await.unwrap();

        let resources = FileSystemToolResources::new()
            .with_policy(FileSystemToolPolicy::new().require_read_before_write(true));
        let session_id = SessionId::new("session-2");
        let turn_id = TurnId::new("turn-2");
        let metadata = MetadataMap::new();
        let executor = BasicToolExecutor::from_registry(registry());
        let mut ctx = tool_context(&session_id, &turn_id, &metadata, &resources);

        let denied_edit = executor
            .execute(
                request(
                    "fs_replace_in_file",
                    json!({
                        "path": source.display().to_string(),
                        "find": "world",
                        "replace": "agentkit"
                    }),
                    &session_id,
                    &turn_id,
                ),
                &mut ctx,
            )
            .await;
        // The read-before-write policy is enforced inside the tool body (it
        // needs resource state), so a denial is a plain `Failed`, not
        // `FailedBeforeInvocation` — that classification is reserved for
        // checker denials that stop the tool from ever starting.
        assert!(matches!(
            denied_edit,
            ToolExecutionOutcome::Failed(ToolError::PermissionDenied(_))
        ));

        let read = executor
            .execute(
                request(
                    "fs_read_file",
                    json!({
                        "path": source.display().to_string()
                    }),
                    &session_id,
                    &turn_id,
                ),
                &mut ctx,
            )
            .await;
        assert!(matches!(read, ToolExecutionOutcome::Completed(_)));

        let replace = executor
            .execute(
                request(
                    "fs_replace_in_file",
                    json!({
                        "path": source.display().to_string(),
                        "find": "world",
                        "replace": "agentkit"
                    }),
                    &session_id,
                    &turn_id,
                ),
                &mut ctx,
            )
            .await;
        assert!(matches!(replace, ToolExecutionOutcome::Completed(_)));

        let move_result = executor
            .execute(
                request(
                    "fs_move",
                    json!({
                        "from": source.display().to_string(),
                        "to": destination.display().to_string()
                    }),
                    &session_id,
                    &turn_id,
                ),
                &mut ctx,
            )
            .await;
        assert!(matches!(move_result, ToolExecutionOutcome::Completed(_)));

        let read_moved = async_fs::read_to_string(&destination).await.unwrap();
        assert_eq!(read_moved, "hello agentkit");

        let delete = executor
            .execute(
                request(
                    "fs_delete",
                    json!({
                        "path": destination.display().to_string()
                    }),
                    &session_id,
                    &turn_id,
                ),
                &mut ctx,
            )
            .await;
        assert!(matches!(delete, ToolExecutionOutcome::Completed(_)));
        assert!(!path_exists(&destination).await.unwrap());

        let _ = async_fs::remove_dir_all(root).await;
    }
}
