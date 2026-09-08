use std::path::PathBuf;

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::managed_files::FileStore;

#[derive(Clone)]
pub struct ReadFileTool {
    root: PathBuf,
    store: FileStore,
    spec: ToolSpec,
}

impl ReadFileTool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            store: FileStore::new(&root),
            root,
            spec: ToolSpec::new(
                ToolName::new("read_file"),
                "Import a regular local PNG or JPEG image as a durable immutable File reference. Only File references reachable from the final compose return deliver pixels; intermediate references remain private. Maximum 8 MiB, 8192 pixels per dimension and 16 megapixels; animation is unsupported. Source bytes, orientation and accepted metadata are preserved; PNG iCCP/zTXt/iTXt metadata is rejected to bound expansion. Files are session-scoped and survive restart and source deletion; other sessions have no implicit access. This is not a text-file reader.",
                object_schema([ ("path", object([("type", Value::from("string")), ("minLength", Value::from(1)), ("maxLength", Value::from(4096))])) ]),
            )
            .with_output_schema(file_schema())
            .with_annotations(ToolAnnotations::read_only()),
        }
    }
}

pub(super) fn file_schema() -> Value {
    object_schema([
        (
            "$kit",
            object([
                ("type", Value::from("string")),
                ("enum", Value::Array(vec![Value::from("file")])),
            ]),
        ),
        (
            "version",
            object([
                ("type", Value::from("integer")),
                ("enum", Value::Array(vec![Value::from(1)])),
            ]),
        ),
        (
            "id",
            object([
                ("type", Value::from("string")),
                ("pattern", Value::from("^file_[0-9a-f]{64}$")),
            ]),
        ),
        (
            "name",
            object([
                ("type", Value::from("string")),
                ("minLength", Value::from(1)),
                ("maxLength", Value::from(255)),
            ]),
        ),
        (
            "mime_type",
            object([
                ("type", Value::from("string")),
                (
                    "enum",
                    Value::Array(vec![Value::from("image/png"), Value::from("image/jpeg")]),
                ),
            ]),
        ),
        ("size_bytes", positive_integer(8_388_608)),
        (
            "image",
            object_schema([
                ("width", positive_integer(8192)),
                ("height", positive_integer(8192)),
            ]),
        ),
    ])
}

pub(super) fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(Map::from_iter(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    ))
}

pub(super) fn object_schema<const N: usize>(fields: [(&str, Value); N]) -> Value {
    let required = Value::Array(fields.iter().map(|(key, _)| Value::from(*key)).collect());
    object([
        ("type", Value::from("object")),
        ("properties", object(fields)),
        ("required", required),
        ("additionalProperties", Value::from(false)),
    ])
}

pub(super) fn positive_integer(maximum: u64) -> Value {
    object([
        ("type", Value::from("integer")),
        ("minimum", Value::from(1)),
        ("maximum", Value::from(maximum)),
    ])
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    path: String,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: Input = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        if input.path.is_empty() || input.path.len() > 4096 {
            return Err(ToolError::InvalidInput(
                "path must contain 1 to 4096 UTF-8 bytes".into(),
            ));
        }
        let path = self.root.join(input.path);
        let store = self.store.clone();
        let cancellation = context.cancellation.clone();
        let session = request.session_id.0;
        let reference = tokio::task::spawn_blocking(move || {
            store.import(&session, &path, cancellation.as_ref())
        })
        .await
        .map_err(|error| ToolError::Internal(error.to_string()))?
        .map_err(ToolError::ExecutionFailed)?;
        let value = serde_json::to_value(reference)
            .map_err(|error| ToolError::Internal(error.to_string()))?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(value),
        )))
    }
}
