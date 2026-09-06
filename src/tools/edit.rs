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
use serde_json::{Map, Value};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct EditTool {
    root: PathBuf,
    spec: ToolSpec,
}

impl EditTool {
    pub fn new(root: PathBuf) -> Self {
        let input_schema = Value::Object(Map::from_iter([
            ("type".into(), Value::from("object")),
            (
                "oneOf".into(),
                Value::Array(vec![
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("object")),
                        (
                            "properties".into(),
                            Value::Object(Map::from_iter([
                                (
                                    "op".into(),
                                    Value::Object(Map::from_iter([
                                        ("type".into(), Value::from("string")),
                                        ("enum".into(), Value::Array(vec![Value::from("add")])),
                                    ])),
                                ),
                                (
                                    "path".into(),
                                    Value::Object(Map::from_iter([(
                                        "type".into(),
                                        Value::from("string"),
                                    )])),
                                ),
                                (
                                    "content".into(),
                                    Value::Object(Map::from_iter([(
                                        "type".into(),
                                        Value::from("string"),
                                    )])),
                                ),
                            ])),
                        ),
                        (
                            "required".into(),
                            Value::Array(vec![
                                Value::from("op"),
                                Value::from("path"),
                                Value::from("content"),
                            ]),
                        ),
                        ("additionalProperties".into(), Value::from(false)),
                    ])),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("object")),
                        (
                            "properties".into(),
                            Value::Object(Map::from_iter([
                                (
                                    "op".into(),
                                    Value::Object(Map::from_iter([
                                        ("type".into(), Value::from("string")),
                                        ("enum".into(), Value::Array(vec![Value::from("edit")])),
                                    ])),
                                ),
                                (
                                    "path".into(),
                                    Value::Object(Map::from_iter([(
                                        "type".into(),
                                        Value::from("string"),
                                    )])),
                                ),
                                (
                                    "hunks".into(),
                                    Value::Object(Map::from_iter([
                                        ("type".into(), Value::from("array")),
                                        ("minItems".into(), Value::from(1)),
                                        (
                                            "items".into(),
                                            Value::Object(Map::from_iter([
                                                ("type".into(), Value::from("object")),
                                                (
                                                    "properties".into(),
                                                    Value::Object(Map::from_iter([
                                                        (
                                                            "context_before".into(),
                                                            Value::Object(Map::from_iter([
                                                                (
                                                                    "type".into(),
                                                                    Value::from("string"),
                                                                ),
                                                                ("default".into(), Value::from("")),
                                                            ])),
                                                        ),
                                                        (
                                                            "old".into(),
                                                            Value::Object(Map::from_iter([(
                                                                "type".into(),
                                                                Value::from("string"),
                                                            )])),
                                                        ),
                                                        (
                                                            "new".into(),
                                                            Value::Object(Map::from_iter([(
                                                                "type".into(),
                                                                Value::from("string"),
                                                            )])),
                                                        ),
                                                        (
                                                            "context_after".into(),
                                                            Value::Object(Map::from_iter([
                                                                (
                                                                    "type".into(),
                                                                    Value::from("string"),
                                                                ),
                                                                ("default".into(), Value::from("")),
                                                            ])),
                                                        ),
                                                    ])),
                                                ),
                                                (
                                                    "required".into(),
                                                    Value::Array(vec![
                                                        Value::from("old"),
                                                        Value::from("new"),
                                                    ]),
                                                ),
                                                ("additionalProperties".into(), Value::from(false)),
                                            ])),
                                        ),
                                    ])),
                                ),
                            ])),
                        ),
                        (
                            "required".into(),
                            Value::Array(vec![
                                Value::from("op"),
                                Value::from("path"),
                                Value::from("hunks"),
                            ]),
                        ),
                        ("additionalProperties".into(), Value::from(false)),
                    ])),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("object")),
                        (
                            "properties".into(),
                            Value::Object(Map::from_iter([
                                (
                                    "op".into(),
                                    Value::Object(Map::from_iter([
                                        ("type".into(), Value::from("string")),
                                        ("enum".into(), Value::Array(vec![Value::from("delete")])),
                                    ])),
                                ),
                                (
                                    "path".into(),
                                    Value::Object(Map::from_iter([(
                                        "type".into(),
                                        Value::from("string"),
                                    )])),
                                ),
                            ])),
                        ),
                        (
                            "required".into(),
                            Value::Array(vec![Value::from("op"), Value::from("path")]),
                        ),
                        ("additionalProperties".into(), Value::from(false)),
                    ])),
                ]),
            ),
        ]));
        let output_schema = Value::Object(Map::from_iter([
            ("type".into(), Value::from("object")),
            (
                "properties".into(),
                Value::Object(Map::from_iter([
                    (
                        "path".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                    ),
                    (
                        "status".into(),
                        Value::Object(Map::from_iter([
                            ("type".into(), Value::from("string")),
                            (
                                "enum".into(),
                                Value::Array(vec![
                                    Value::from("added"),
                                    Value::from("edited"),
                                    Value::from("deleted"),
                                ]),
                            ),
                        ])),
                    ),
                ])),
            ),
            (
                "required".into(),
                Value::Array(vec![Value::from("path"), Value::from("status")]),
            ),
            ("additionalProperties".into(), Value::from(false)),
        ]));
        Self {
            root,
            spec: ToolSpec::new(
                ToolName::new("edit"),
                "Apply exact, git-style text hunks to one file. Anchors must match exactly once.",
                input_schema,
            )
            .with_output_schema(output_schema)
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
            ToolOutput::structured(Value::Object(Map::from_iter([
                (
                    "path".into(),
                    serde_json::to_value(path.strip_prefix(&self.root).unwrap_or(&path))
                        .map_err(|error| ToolError::Internal(error.to_string()))?,
                ),
                ("status".into(), Value::from(status)),
            ]))),
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
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
