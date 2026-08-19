use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct EditTool {
    root: PathBuf,
    spec: ToolSpec,
}

impl EditTool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            spec: ToolSpec::new(
                ToolName::new("edit"),
                "Apply exact, git-style text hunks to one file. Anchors must match exactly once.",
                json!({
                    "type": "object",
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "op": {"type": "string", "enum": ["add"]},
                                "path": {"type": "string"},
                                "content": {"type": "string"}
                            },
                            "required": ["op", "path", "content"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "op": {"type": "string", "enum": ["edit"]},
                                "path": {"type": "string"},
                                "hunks": {
                                    "type": "array",
                                    "minItems": 1,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "context_before": {"type": "string", "default": ""},
                                            "old": {"type": "string"},
                                            "new": {"type": "string"},
                                            "context_after": {"type": "string", "default": ""}
                                        },
                                        "required": ["old", "new"],
                                        "additionalProperties": false
                                    }
                                }
                            },
                            "required": ["op", "path", "hunks"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "op": {"type": "string", "enum": ["delete"]},
                                "path": {"type": "string"}
                            },
                            "required": ["op", "path"],
                            "additionalProperties": false
                        }
                    ]
                }),
            )
            .with_output_schema(json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "status": {"type": "string", "enum": ["added", "edited", "deleted"]}
                },
                "required": ["path", "status"],
                "additionalProperties": false
            }))
            .with_annotations(ToolAnnotations::new()),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum EditInput {
    Add { path: String, content: String },
    Edit { path: String, hunks: Vec<Hunk> },
    Delete { path: String },
}

#[derive(Deserialize)]
struct Hunk {
    #[serde(default)]
    context_before: String,
    old: String,
    new: String,
    #[serde(default)]
    context_after: String,
}

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: EditInput = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let path = match &input {
            EditInput::Add { path, .. }
            | EditInput::Edit { path, .. }
            | EditInput::Delete { path } => rooted(&self.root, path)?,
        };
        let status = match input {
            EditInput::Add { content, .. } => {
                if path.exists() {
                    return Err(ToolError::ExecutionFailed(format!(
                        "{} already exists",
                        path.display()
                    )));
                }
                write_atomic(&path, content.as_bytes())?;
                "added"
            }
            EditInput::Edit { hunks, .. } => {
                let original = fs::read_to_string(&path).map_err(io_error)?;
                let crlf = original.contains("\r\n");
                let mut content = normalize_newlines(&original);
                for hunk in hunks {
                    content = apply_hunk(content, hunk)?;
                }
                if crlf {
                    content = content.replace('\n', "\r\n");
                }
                write_atomic(&path, content.as_bytes())?;
                "edited"
            }
            EditInput::Delete { .. } => {
                fs::remove_file(&path).map_err(io_error)?;
                "deleted"
            }
        };
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(json!({
                "path": path.strip_prefix(&self.root).unwrap_or(&path),
                "status": status
            })),
        )))
    }
}

fn apply_hunk(mut content: String, hunk: Hunk) -> Result<String, ToolError> {
    let before = normalize_newlines(&hunk.context_before);
    let old = normalize_newlines(&hunk.old);
    let new = normalize_newlines(&hunk.new);
    let after = normalize_newlines(&hunk.context_after);
    let anchor = format!("{before}{old}{after}");
    if anchor.is_empty() {
        return Err(ToolError::InvalidInput(
            "an edit hunk needs an anchor".into(),
        ));
    }
    let matches = content
        .match_indices(&anchor)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let [start] = matches.as_slice() else {
        return Err(ToolError::ExecutionFailed(if matches.is_empty() {
            "hunk anchor did not match".into()
        } else {
            "hunk anchor is ambiguous".into()
        }));
    };
    let old_start = start + before.len();
    content.replace_range(old_start..old_start + old.len(), &new);
    Ok(content)
}

fn rooted(root: &Path, value: &str) -> Result<PathBuf, ToolError> {
    if value.is_empty() {
        return Err(ToolError::InvalidInput("path must be non-empty".into()));
    }
    let path = Path::new(value);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    })
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::InvalidInput("file has no parent directory".into()))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("edit");
    let temp = parent.join(format!(
        ".{name}.kit-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&temp, bytes).map_err(io_error)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&temp, metadata.permissions()).map_err(io_error)?;
    }
    fs::rename(&temp, path).map_err(io_error)
}

fn io_error(error: std::io::Error) -> ToolError {
    ToolError::ExecutionFailed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_requires_one_exact_anchor() {
        let hunk = || Hunk {
            context_before: "a\n".into(),
            old: "b\n".into(),
            new: "x\n".into(),
            context_after: "c\n".into(),
        };
        assert_eq!(apply_hunk("a\nb\nc\n".into(), hunk()).unwrap(), "a\nx\nc\n");
        assert!(apply_hunk("missing\n".into(), hunk()).is_err());
        assert!(apply_hunk("a\nb\nc\na\nb\nc\n".into(), hunk()).is_err());
    }

    #[test]
    fn accepts_paths_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        assert_eq!(
            rooted(root.path(), "../outside").unwrap(),
            root.path().join("../outside")
        );
        assert_eq!(
            rooted(root.path(), outside.path().to_str().unwrap()).unwrap(),
            outside.path()
        );
        assert!(rooted(root.path(), "").is_err());
    }
}
