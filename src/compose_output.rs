use std::{path::Path, sync::Arc};

use agentkit_core::{Part, ToolOutput, TurnCancellation};
use agentkit_tools_core::ToolError;
use serde_json::{Map, Value};

const MAX_MODEL_OUTPUT_BYTES: usize = 8 * 1024;

/// Resolve only the final Runlet JSON value. Both foreground and detached
/// completions use this boundary; resolving a reference never reimports its path.
pub(crate) async fn finalize(
    root: &Path,
    session: &str,
    artifact_directory: &Path,
    output: ToolOutput,
    cancellation: Option<TurnCancellation>,
) -> Result<ToolOutput, ToolError> {
    let output = if let ToolOutput::Structured(value) = output {
        let store = crate::managed_files::FileStore::new(root);
        let session = session.to_owned();
        let (value, selected) = tokio::task::spawn_blocking(move || {
            let selected = store.selected_parts(&session, &value, cancellation.as_ref());
            (value, selected)
        })
        .await
        .map_err(|error| delivery_failed(error.to_string()))?;
        let selected = selected.map_err(delivery_failed)?;
        if selected.is_empty() {
            ToolOutput::Structured(value)
        } else {
            // Keep each position label adjacent to its image, even when the
            // ordinary JSON spills. Reserve label bytes from the text budget.
            let label_bytes = selected
                .iter()
                .filter_map(|part| match part {
                    Part::Text(text) => Some(text.text.len()),
                    _ => None,
                })
                .sum::<usize>();
            if label_bytes > MAX_MODEL_OUTPUT_BYTES / 2 {
                return Err(delivery_failed(
                    "selected image labels exceed the 4 KiB label budget".into(),
                ));
            }
            let text = guard_text(
                artifact_directory,
                ToolOutput::Structured(value),
                MAX_MODEL_OUTPUT_BYTES - label_bytes,
            )
            .await?;
            let mut parts = match text {
                ToolOutput::Structured(value) => vec![Part::structured(value)],
                ToolOutput::Text(text) => vec![Part::text(text)],
                ToolOutput::Parts(parts) => parts,
                ToolOutput::Files(files) => files.into_iter().map(Part::File).collect(),
            };
            parts.extend(selected);
            return Ok(ToolOutput::Parts(parts));
        }
    } else {
        output
    };
    guard(artifact_directory, output).await
}

fn delivery_failed(detail: String) -> ToolError {
    ToolError::ExecutionFailed(format!(
        "compose program completed; selected File delivery failed: {detail}. Side effects may have occurred; do not rerun blindly."
    ))
}

pub(crate) async fn guard(
    artifact_directory: &Path,
    output: ToolOutput,
) -> Result<ToolOutput, ToolError> {
    // Never serialize typed media into a spill artifact or count it as text.
    let (output, media) = match output {
        ToolOutput::Parts(parts) => {
            let (media, text): (Vec<_>, Vec<_>) = parts
                .into_iter()
                .partition(|part| matches!(part, Part::Media(_)));
            (ToolOutput::Parts(text), media)
        }
        output => (output, Vec::new()),
    };
    let text = guard_text(artifact_directory, output, MAX_MODEL_OUTPUT_BYTES).await?;
    if media.is_empty() {
        return Ok(text);
    }
    let mut parts = match text {
        ToolOutput::Text(text) => vec![Part::text(text)],
        ToolOutput::Structured(value) => vec![Part::structured(value)],
        ToolOutput::Parts(parts) => parts,
        ToolOutput::Files(files) => files.into_iter().map(Part::File).collect(),
    };
    parts.extend(media);
    Ok(ToolOutput::Parts(parts))
}

async fn guard_text(
    artifact_directory: &Path,
    output: ToolOutput,
    budget: usize,
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
    if original_bytes <= budget {
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
    let stored = artifact.is_some();
    let marker = |start: usize, end: usize| {
        if stored {
            format!(
                "\n...[compose output spilled: bytes {start}..{end} of {original_bytes} omitted here; read them with artifact({{path, offset: {start}}})]...\n"
            )
        } else {
            format!(
                "\n...[tool completed; output truncated: bytes {start}..{end} of {original_bytes} omitted here; artifact storage failed]...\n"
            )
        }
    };
    let marker_bytes = marker(original_bytes, original_bytes).len();
    let mut preview_budget = budget;
    loop {
        let preview = preview(&body, marker, marker_bytes, preview_budget);
        let replacement = Value::Object(Map::from_iter([
            ("preview".into(), Value::from(preview)),
            ("artifact".into(), Value::from(artifact.as_deref())),
            ("original_bytes".into(), Value::from(original_bytes)),
            (
                "artifact_error".into(),
                Value::from(artifact_error.as_deref()),
            ),
        ]));
        let replacement_bytes = serde_json::to_vec(&replacement)
            .map_err(|error| ToolError::Internal(error.to_string()))?
            .len();
        if replacement_bytes <= budget {
            return Ok(ToolOutput::structured(replacement));
        }
        let next_budget = preview_budget
            .saturating_mul(budget)
            .checked_div(replacement_bytes)
            .unwrap_or(0)
            .min(preview_budget.saturating_sub(1))
            .max(marker_bytes);
        if next_budget >= preview_budget {
            return Err(ToolError::Internal(
                "compose spill metadata exceeds the model output budget".into(),
            ));
        }
        preview_budget = next_budget;
    }
}

/// `marker_bytes` bounds the marker for any omitted range of `value`.
fn preview(
    value: &str,
    marker: impl Fn(usize, usize) -> String,
    marker_bytes: usize,
    budget: usize,
) -> String {
    let remaining = budget.saturating_sub(marker_bytes);
    let head_budget = remaining / 2;
    let tail_budget = remaining - head_budget;
    let head = prefix(value, head_budget);
    let tail = suffix(value, tail_budget);
    format!(
        "{head}{}{tail}",
        marker(head.len(), value.len() - tail.len())
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use agentkit_core::ToolOutput;
    use serde_json::json;

    use super::{MAX_MODEL_OUTPUT_BYTES, guard};

    #[tokio::test]
    async fn inline_structured_output_preserves_numeric_values() {
        let directory = tempfile::tempdir().unwrap();
        let value = json!({
            "unsigned": u64::MAX,
            "signed": i64::MIN,
            "fraction": -1.25,
        });
        let output = guard(directory.path(), ToolOutput::structured(value.clone()))
            .await
            .unwrap();
        let ToolOutput::Structured(output) = output else {
            panic!("guard returned non-structured output");
        };
        assert_eq!(output, value);
        assert_eq!(output["unsigned"].as_u64(), Some(u64::MAX));
        assert_eq!(output["signed"].as_i64(), Some(i64::MIN));
        assert_eq!(output["fraction"].as_f64(), Some(-1.25));
    }

    #[tokio::test]
    async fn failed_artifact_storage_keeps_output_and_reports_error() {
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("not-a-directory");
        std::fs::write(&blocked, "occupied").unwrap();
        let body = "é".repeat(MAX_MODEL_OUTPUT_BYTES);
        let output = guard(&blocked, ToolOutput::Text(body.clone()))
            .await
            .unwrap();
        let ToolOutput::Structured(output) = output else {
            panic!("guard returned non-structured output");
        };
        assert!(output["artifact"].is_null());
        assert!(!output["artifact_error"].as_str().unwrap().is_empty());
        assert_eq!(output["original_bytes"], body.len());
        assert!(
            output["preview"]
                .as_str()
                .unwrap()
                .contains("artifact storage failed")
        );
        assert!(serde_json::to_vec(&output).unwrap().len() <= MAX_MODEL_OUTPUT_BYTES);
        assert_eq!(std::fs::read_to_string(blocked).unwrap(), "occupied");
    }

    #[tokio::test]
    async fn oversized_compose_output_spills_at_the_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let value = json!({
            "document": "\\".repeat(MAX_MODEL_OUTPUT_BYTES * 2),
            "unsigned": u64::MAX,
            "signed": i64::MIN,
            "fraction": 1.25,
        });
        let expected = serde_json::to_string(&value).unwrap();

        let output = guard(directory.path(), ToolOutput::structured(value))
            .await
            .unwrap();
        let ToolOutput::Structured(output) = output else {
            panic!("guard returned non-structured output");
        };
        let artifact = output["artifact"].as_str().unwrap();

        assert_eq!(output["original_bytes"], expected.len());
        assert!(output["original_bytes"].is_u64());
        assert!(output["artifact_error"].is_null());
        assert!(
            output["preview"]
                .as_str()
                .unwrap()
                .contains("compose output spilled")
        );
        assert!(serde_json::to_vec(&output).unwrap().len() <= MAX_MODEL_OUTPUT_BYTES);
        assert_eq!(std::fs::read_to_string(artifact).unwrap(), expected);
    }

    #[tokio::test]
    async fn spill_marker_names_the_omitted_artifact_range() {
        let directory = tempfile::tempdir().unwrap();
        let text = (0..4000)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let expected = serde_json::to_string(&json!({ "stdout": text })).unwrap();

        let output = guard(
            directory.path(),
            ToolOutput::structured(json!({ "stdout": text })),
        )
        .await
        .unwrap();
        let ToolOutput::Structured(output) = output else {
            panic!("guard returned non-structured output");
        };
        let preview = output["preview"].as_str().unwrap();
        let (head, rest) = preview
            .split_once("\n...[compose output spilled: bytes ")
            .unwrap();
        let (range, rest) = rest.split_once(" of ").unwrap();
        let (start, end) = range.split_once("..").unwrap();
        let (start, end) = (
            start.parse::<usize>().unwrap(),
            end.parse::<usize>().unwrap(),
        );
        let tail = rest.split_once("]...\n").unwrap().1;
        assert!(rest.contains(&format!("offset: {start}")));
        assert_eq!(format!("{head}{}{tail}", &expected[start..end]), expected);
    }
}
