use std::{path::Path, sync::Arc};

use agentkit_core::ToolOutput;
use agentkit_tools_core::ToolError;
use serde_json::json;

const MAX_MODEL_OUTPUT_BYTES: usize = 8 * 1024;

pub(crate) async fn guard(
    artifact_directory: &Path,
    output: ToolOutput,
) -> Result<ToolOutput, ToolError> {
    let body =
        match &output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Structured(value) => serde_json::to_string(value)
                .map_err(|error| ToolError::Internal(error.to_string()))?,
            other => serde_json::to_string(other)
                .map_err(|error| ToolError::Internal(error.to_string()))?,
        };
    let original_bytes = body.len();
    if original_bytes <= MAX_MODEL_OUTPUT_BYTES {
        return Ok(output);
    }

    let body = Arc::new(body);
    let artifact_body = Arc::clone(&body);
    let path = artifact_directory.join("compose-output.json");
    let stored = tokio::task::spawn_blocking(move || {
        crate::artifacts::write(&path, artifact_body.as_bytes())
    })
    .await
    .map_err(|error| ToolError::Internal(error.to_string()))?;
    let (artifact, artifact_error) = match stored {
        Ok(path) => (Some(path.display().to_string()), None),
        Err(error) => (None, Some(prefix(&error.to_string(), 256).to_owned())),
    };
    // Artifact storage must not turn an already-executed tool into a failed
    // tool call: retrying that call could duplicate its side effects.
    let marker = if artifact.is_some() {
        format!(
            "\n...[compose output spilled: {original_bytes} bytes; read with artifact(path)]...\n"
        )
    } else {
        format!(
            "\n...[tool completed; output truncated: {original_bytes} bytes; artifact storage failed]...\n"
        )
    };
    let mut preview_budget = MAX_MODEL_OUTPUT_BYTES;
    loop {
        let preview = preview(&body, &marker, preview_budget);
        let replacement = json!({
            "preview": preview,
            "artifact": artifact,
            "original_bytes": original_bytes,
            "artifact_error": artifact_error,
        });
        let replacement_bytes = serde_json::to_vec(&replacement)
            .map_err(|error| ToolError::Internal(error.to_string()))?
            .len();
        if replacement_bytes <= MAX_MODEL_OUTPUT_BYTES {
            return Ok(ToolOutput::structured(replacement));
        }
        let next_budget = preview_budget
            .saturating_mul(MAX_MODEL_OUTPUT_BYTES)
            .checked_div(replacement_bytes)
            .unwrap_or(0)
            .min(preview_budget.saturating_sub(1))
            .max(marker.len());
        if next_budget >= preview_budget {
            return Err(ToolError::Internal(
                "compose spill metadata exceeds the model output budget".into(),
            ));
        }
        preview_budget = next_budget;
    }
}

fn preview(value: &str, marker: &str, budget: usize) -> String {
    let remaining = budget.saturating_sub(marker.len());
    let head_budget = remaining / 2;
    let tail_budget = remaining - head_budget;
    format!(
        "{}{}{}",
        prefix(value, head_budget),
        marker,
        suffix(value, tail_budget)
    )
}

fn prefix(value: &str, budget: usize) -> &str {
    let mut end = value.len().min(budget);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn suffix(value: &str, budget: usize) -> &str {
    if value.len() <= budget {
        return value;
    }
    let mut start = value.len() - budget;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

#[cfg(test)]
mod tests {
    use agentkit_core::ToolOutput;
    use serde_json::json;

    use super::{MAX_MODEL_OUTPUT_BYTES, guard};

    #[tokio::test]
    async fn oversized_compose_output_spills_at_the_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let value = json!({ "document": "\\".repeat(MAX_MODEL_OUTPUT_BYTES * 2) });
        let expected = serde_json::to_string(&value).unwrap();

        let output = guard(directory.path(), ToolOutput::structured(value))
            .await
            .unwrap();
        let ToolOutput::Structured(output) = output else {
            panic!("guard returned non-structured output");
        };
        let artifact = output["artifact"].as_str().unwrap();

        assert_eq!(output["original_bytes"], expected.len());
        assert!(
            output["preview"]
                .as_str()
                .unwrap()
                .contains("compose output spilled")
        );
        assert!(serde_json::to_vec(&output).unwrap().len() <= MAX_MODEL_OUTPUT_BYTES);
        assert_eq!(std::fs::read_to_string(artifact).unwrap(), expected);
    }
}
