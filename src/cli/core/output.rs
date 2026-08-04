use serde_json::{Value, json};

use crate::api::{
    http::core::encode_cursor,
    service::{EventProjection, QueryProjection},
};

use super::{ClientError, ClientErrorKind, ClientResponse, OutputFormat};

pub const EXIT_OK: u8 = 0;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_NOT_FOUND: u8 = 4;
pub const EXIT_CONFLICT: u8 = 5;
pub const EXIT_TRANSPORT: u8 = 6;
pub const EXIT_INTERNAL: u8 = 70;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Output {
    pub exit_code: u8,
    pub stdout: String,
    pub stderr: String,
}

pub fn render_response(
    response: ClientResponse,
    format: OutputFormat,
) -> Result<Output, ClientError> {
    let value = response_value(response)?;
    let stdout = match format {
        OutputFormat::Human => human(&value),
        OutputFormat::Json => serde_json::to_string(&value),
        OutputFormat::Jsonl => jsonl(&value),
    }
    .map_err(|error| ClientError::internal(error.to_string()))?;
    Ok(Output {
        exit_code: EXIT_OK,
        stdout: format!("{stdout}\n"),
        stderr: String::new(),
    })
}

pub fn render_exec_response(value: Value, format: OutputFormat) -> Result<Output, ClientError> {
    let stdout = match format {
        OutputFormat::Human => human(&value),
        OutputFormat::Json => serde_json::to_string(&value),
        OutputFormat::Jsonl => jsonl(&value),
    }
    .map_err(|error| ClientError::internal(error.to_string()))?;
    Ok(Output {
        exit_code: EXIT_OK,
        stdout: format!("{stdout}\n"),
        stderr: String::new(),
    })
}

pub fn render_error(error: &ClientError, format: OutputFormat) -> Output {
    let (exit_code, status, code) = match error.kind {
        ClientErrorKind::Authentication => (EXIT_NOT_FOUND, 404, "not_found"),
        ClientErrorKind::NotFound => (EXIT_NOT_FOUND, 404, "not_found"),
        ClientErrorKind::Conflict => (EXIT_CONFLICT, 409, "conflict"),
        ClientErrorKind::Invalid => (EXIT_USAGE, 400, "invalid_request"),
        ClientErrorKind::Unavailable => (EXIT_TRANSPORT, 503, "unavailable"),
        ClientErrorKind::Timeout => (EXIT_TRANSPORT, 504, "request_timeout"),
        ClientErrorKind::Internal => (EXIT_INTERNAL, 500, "internal_error"),
    };
    let problem = json!({
        "type": format!("https://kit.dev/problems/{code}"),
        "title": error_title(error.kind),
        "status": status,
        "detail": error.message,
        "instance": "cli",
        "code": code,
    });
    let stderr = match format {
        OutputFormat::Human => format!("error: {}\n", error.message),
        OutputFormat::Json | OutputFormat::Jsonl => {
            format!(
                "{}\n",
                serde_json::to_string(&problem).expect("JSON value serializes")
            )
        }
    };
    Output {
        exit_code,
        stdout: String::new(),
        stderr,
    }
}

fn response_value(response: ClientResponse) -> Result<Value, ClientError> {
    match response {
        ClientResponse::Mutation {
            resource_id,
            receipt,
        } => Ok(json!({ "resource": { "id": resource_id }, "receipt": receipt })),
        ClientResponse::Query(projection) => projection_value(*projection),
    }
}

fn projection_value(projection: QueryProjection) -> Result<Value, ClientError> {
    Ok(match projection {
        QueryProjection::Project(value) => json!(value),
        QueryProjection::Retention(value) => json!({ "retention": value }),
        QueryProjection::Threads(value) => json!({ "items": value }),
        QueryProjection::Thread(value) => json!(value),
        QueryProjection::DeletionJob(value) => value,
        QueryProjection::Events(page) => json!({
            "items": page.events.into_iter().map(event_value).collect::<Result<Vec<_>, _>>()?,
            "next_cursor": encode_cursor(page.next_cursor.position()),
        }),
        QueryProjection::Runs(value) => json!({ "items": value }),
        QueryProjection::Run(value) => json!(value),
        QueryProjection::RunCost(value) => json!(value),
        QueryProjection::RunPrompts(value) => json!(value),
        QueryProjection::RunTranscript(value) => json!(value),
        QueryProjection::Attempt(value) => json!(value),
        QueryProjection::Approvals(value) => json!({ "items": value }),
        QueryProjection::AuthRequests(value) => json!({ "items": value }),
        QueryProjection::McpCallbacks(value) => json!({ "items": value }),
        QueryProjection::McpCallback(value) => json!(value),
        QueryProjection::ArtifactMetadata(value) => json!(value),
        QueryProjection::Capabilities(value) => json!({ "items": value }),
        QueryProjection::CursorStatus(value) => json!({
            "requested": encode_cursor(value.requested.position()),
            "committed": encode_cursor(value.committed.position()),
            "caught_up": value.caught_up,
        }),
        QueryProjection::Status(value) => json!({
            "committed": encode_cursor(value.committed.position()),
            "ready": value.ready,
        }),
    })
}

fn event_value(event: EventProjection) -> Result<Value, ClientError> {
    let payload = serde_json::from_slice::<Value>(&event.payload)
        .map_err(|error| ClientError::internal(error.to_string()))?;
    Ok(json!({
        "cursor": encode_cursor(event.cursor.position()),
        "project_id": event.project_id,
        "operation": event.operation,
        "stream": event.stream,
        "payload": payload,
    }))
}

fn human(value: &Value) -> serde_json::Result<String> {
    match value {
        Value::Object(object) if object.len() == 1 && object.contains_key("items") => {
            let items = object["items"].as_array().cloned().unwrap_or_default();
            items
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()
                .map(|lines| lines.join("\n"))
        }
        _ => serde_json::to_string_pretty(value),
    }
}

fn jsonl(value: &Value) -> serde_json::Result<String> {
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![value.clone()]);
    items
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map(|lines| lines.join("\n"))
}

fn error_title(kind: ClientErrorKind) -> &'static str {
    match kind {
        ClientErrorKind::Authentication | ClientErrorKind::NotFound => "Resource not found",
        ClientErrorKind::Conflict => "Request conflict",
        ClientErrorKind::Invalid => "Invalid request",
        ClientErrorKind::Unavailable => "Service unavailable",
        ClientErrorKind::Timeout => "Request timed out",
        ClientErrorKind::Internal => "Internal server error",
    }
}
