use std::{
    io::{Read as _, Seek as _, SeekFrom},
    path::{Component, PathBuf},
};

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// Reads Kit-owned output through the internal filesystem, not a shell path.
#[derive(Clone)]
pub struct ArtifactTool {
    root: PathBuf,
    spec: ToolSpec,
}

impl ArtifactTool {
    pub fn new(artifact_root: PathBuf) -> Self {
        Self {
            root: artifact_root,
            spec: ToolSpec::new(
                ToolName::new("artifact"),
                "Read a UTF-8 Kit output artifact from this session, including artifacts temporarily retained in memory when disk storage fails. Use the artifact path returned by compose. Continue from next_offset to read another bounded chunk. Shell commands cannot read memory-only artifacts.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1, "maxLength": 4096},
                        "offset": {"type": "integer", "minimum": 0, "default": 0},
                        "limit": {"type": "integer", "minimum": 4, "maximum": 1024, "default": 1024}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            )
            .with_output_schema(json!({
                "type": "object",
                "properties": {
                    "content": {"type": "string"},
                    "next_offset": {"type": "integer"},
                    "total_bytes": {"type": "integer"},
                    "eof": {"type": "boolean"}
                },
                "required": ["content", "next_offset", "total_bytes", "eof"],
                "additionalProperties": false
            }))
            .with_annotations(ToolAnnotations::read_only()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    path: PathBuf,
    #[serde(default)]
    offset: u64,
    #[serde(default = "default_limit")]
    limit: usize,
}

const fn default_limit() -> usize {
    1024
}

#[async_trait]
impl Tool for ArtifactTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: Input = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        if !(4..=1024).contains(&input.limit) {
            return Err(ToolError::InvalidInput(
                "limit must be between 4 and 1024 bytes".into(),
            ));
        }
        let root = crate::artifacts::session_directory(&self.root, &request.session_id.0);
        let relative = input
            .path
            .strip_prefix(&root)
            .map_err(|_| ToolError::InvalidInput("artifact must belong to this session".into()))?
            .to_path_buf();
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(ToolError::InvalidInput("invalid artifact path".into()));
        }
        let output = tokio::task::spawn_blocking(move || {
            // Descriptor-relative no-follow access prevents a path or symlink
            // from crossing the session's artifact namespace.
            let mut file = crate::resilient_fs::open_beneath(&root, &relative)
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
            let metadata = file
                .metadata()
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
            if !metadata.is_file() || input.offset > metadata.len() {
                return Err(ToolError::InvalidInput(
                    "artifact or offset is invalid".into(),
                ));
            }
            file.seek(SeekFrom::Start(input.offset))
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
            let mut bytes = Vec::with_capacity(input.limit);
            file.take(input.limit as u64)
                .read_to_end(&mut bytes)
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
            let end = match std::str::from_utf8(&bytes) {
                Ok(_) => bytes.len(),
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(_) => {
                    return Err(ToolError::InvalidInput(
                        "artifact is not UTF-8, or offset is not a character boundary".into(),
                    ));
                }
            };
            if end < bytes.len() && input.offset + bytes.len() as u64 == metadata.len() {
                return Err(ToolError::InvalidInput(
                    "artifact ends with incomplete UTF-8".into(),
                ));
            }
            bytes.truncate(end);
            let content =
                String::from_utf8(bytes).map_err(|error| ToolError::Internal(error.to_string()))?;
            let next_offset = input.offset + end as u64;
            Ok(json!({
                "content": content,
                "next_offset": next_offset,
                "total_bytes": metadata.len(),
                "eof": next_offset == metadata.len()
            }))
        })
        .await
        .map_err(|error| ToolError::Internal(error.to_string()))??;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(output),
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agentkit_core::{MetadataMap, SessionId, ToolCallId, TurnId};
    use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext};

    use super::*;

    #[tokio::test]
    async fn reads_bounded_utf8_chunks_and_rejects_other_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let session = SessionId::new("artifact-session");
        let turn = TurnId::new("turn");
        let root = crate::artifacts::session_directory(directory.path(), &session.0);
        let path = crate::artifacts::write(&root.join("call/output.json"), "abcé😀tail".as_bytes())
            .unwrap();
        let tool = ArtifactTool::new(directory.path().to_path_buf());
        let context = OwnedToolContext {
            session_id: session.clone(),
            turn_id: turn.clone(),
            metadata: MetadataMap::new(),
            permissions: Arc::new(AllowAllPermissions),
            resources: Arc::new(()),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };
        let request = |session, input| {
            ToolRequest::new(
                ToolCallId::new("read"),
                ToolName::new("artifact"),
                input,
                session,
                turn.clone(),
            )
        };
        let result = tool
            .invoke(
                request(session.clone(), json!({"path":path,"limit":7})),
                &mut context.borrowed(),
            )
            .await
            .unwrap();
        let ToolOutput::Structured(first) = result.result.output else {
            panic!("structured output expected")
        };
        assert_eq!(first["content"], "abcé");
        assert_eq!(first["next_offset"], 5);
        let result = tool
            .invoke(
                request(session, json!({"path":path,"offset":5})),
                &mut context.borrowed(),
            )
            .await
            .unwrap();
        let ToolOutput::Structured(last) = result.result.output else {
            panic!("structured output expected")
        };
        assert_eq!(last["content"], "😀tail");
        assert_eq!(last["eof"], true);
        assert!(
            tool.invoke(
                request(SessionId::new("other-session"), json!({"path":path})),
                &mut context.borrowed(),
            )
            .await
            .is_err()
        );
        assert!(
            tool.invoke(
                request(
                    SessionId::new("artifact-session"),
                    json!({"path":root.join("../outside")})
                ),
                &mut context.borrowed(),
            )
            .await
            .is_err()
        );
    }
}
