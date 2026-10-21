//! Connection-bound, foreground-only structured user questions.
use std::{collections::BTreeSet, sync::Arc};

use agent_client_protocol::{Client, V2ConnectionTo, schema::v2 as wire};
use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use futures_util::future::{Either, select};
use serde::Deserialize;
use serde_json::Value;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
#[path = "ask_user_tests.rs"]
mod tests;

const MAX_FORM_REQUEST_BYTES: usize = 128 * 1024;

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn strings(values: Vec<String>) -> Value {
    Value::Array(values.into_iter().map(Value::from).collect())
}

fn text_schema(min: usize, max: usize) -> Value {
    object([
        ("type", Value::from("string")),
        ("minLength", Value::from(min)),
        ("maxLength", Value::from(max)),
    ])
}

#[derive(Clone)]
pub(crate) struct AskUserTool {
    spec: ToolSpec,
    connection: V2ConnectionTo<Client>,
    session: wire::SessionId,
    // This lock owns no mutable data. The permit serializes dialogs and their
    // requires_action/running pair. Cancellation drops the permit; no repair or
    // poison recovery is needed. No other lock is acquired by this owner.
    admission: Arc<tokio::sync::Mutex<()>>,
}

impl AskUserTool {
    pub(crate) fn new(connection: V2ConnectionTo<Client>, session: wire::SessionId) -> Self {
        let option = object([
            ("type", Value::from("object")),
            (
                "properties",
                object([
                    ("id", text_schema(1, 64)),
                    ("label", text_schema(1, 200)),
                    ("description", text_schema(0, 1000)),
                ]),
            ),
            ("required", strings(vec!["id".into(), "label".into()])),
            ("additionalProperties", Value::from(false)),
        ]);
        let question = object([
            ("type", Value::from("object")),
            (
                "properties",
                object([
                    (
                        "id",
                        object([
                            ("type", Value::from("string")),
                            ("pattern", Value::from("^[A-Za-z][A-Za-z0-9_-]{0,63}$")),
                        ]),
                    ),
                    ("title", text_schema(1, 120)),
                    ("prompt", text_schema(1, 2000)),
                    (
                        "options",
                        object([
                            ("type", Value::from("array")),
                            ("minItems", Value::from(1)),
                            ("maxItems", Value::from(32)),
                            ("items", option),
                        ]),
                    ),
                    ("multiple", object([("type", Value::from("boolean"))])),
                    ("allow_custom", object([("type", Value::from("boolean"))])),
                ]),
            ),
            (
                "required",
                strings(vec![
                    "id".into(),
                    "title".into(),
                    "prompt".into(),
                    "options".into(),
                ]),
            ),
            ("additionalProperties", Value::from(false)),
        ]);
        let input_schema = object([
            ("type", Value::from("object")),
            (
                "properties",
                object([
                    ("message", text_schema(1, 2000)),
                    (
                        "questions",
                        object([
                            ("type", Value::from("array")),
                            ("minItems", Value::from(1)),
                            ("maxItems", Value::from(8)),
                            ("items", question),
                        ]),
                    ),
                ]),
            ),
            (
                "required",
                strings(vec!["message".into(), "questions".into()]),
            ),
            ("additionalProperties", Value::from(false)),
        ]);
        let answer = object([
            ("type", Value::from("object")),
            (
                "properties",
                object([
                    (
                        "selected",
                        object([
                            ("type", Value::from("array")),
                            ("items", object([("type", Value::from("string"))])),
                        ]),
                    ),
                    ("custom", object([("type", Value::from("string"))])),
                ]),
            ),
            ("required", strings(vec!["selected".into()])),
            ("additionalProperties", Value::from(false)),
        ]);
        let output_schema = object([
            ("type", Value::from("object")),
            (
                "properties",
                object([
                    (
                        "action",
                        object([
                            ("type", Value::from("string")),
                            (
                                "enum",
                                strings(vec!["accept".into(), "decline".into(), "cancel".into()]),
                            ),
                        ]),
                    ),
                    (
                        "answers",
                        object([
                            ("type", Value::from("object")),
                            ("additionalProperties", answer),
                        ]),
                    ),
                ]),
            ),
            ("required", strings(vec!["action".into()])),
            ("additionalProperties", Value::from(false)),
        ]);
        Self {
            spec: ToolSpec::new(ToolName::new("ask_user"),
                "Ask the user to choose options or provide a custom answer. Use for decisions that need user input; do not request passwords, API keys, credentials, or other secrets. Questions are presented together and submitted after review. The complete serialized form request must fit within 128 KiB (UTF-8); shorten descriptions or split large batches. Decline and cancel are not answers; do not invent selections. This foreground tool is separate from compose and cannot run in the background.",
                input_schema)
                .with_output_schema(output_schema)
                .with_annotations(ToolAnnotations::new()),
            connection, session, admission: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    message: String,
    questions: Vec<Question>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Question {
    id: String,
    title: String,
    prompt: String,
    options: Vec<OptionValue>,
    #[serde(default)]
    multiple: bool,
    #[serde(default)]
    allow_custom: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionValue {
    id: String,
    label: String,
    #[serde(default)]
    description: String,
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidInput(message.into())
}
fn bounded(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.chars().count()) && (min == 0 || !value.trim().is_empty())
}

impl Input {
    fn validate(&self) -> Result<(), ToolError> {
        if !bounded(&self.message, 1, 2000) || !(1..=8).contains(&self.questions.len()) {
            return Err(invalid(
                "message must be 1–2000 characters and questions must contain 1–8 entries",
            ));
        }
        let mut ids = BTreeSet::new();
        for q in &self.questions {
            let valid_id = bounded(&q.id, 1, 64)
                && q.id.starts_with(|c: char| c.is_ascii_alphabetic())
                && q.id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !valid_id
                || !ids.insert(&q.id)
                || !bounded(&q.title, 1, 120)
                || !bounded(&q.prompt, 1, 2000)
                || !(1..=32).contains(&q.options.len())
            {
                return Err(invalid(
                    "questions require unique identifier IDs, bounded title/prompt, and 1–32 options",
                ));
            }
            let mut options = BTreeSet::new();
            for option in &q.options {
                if !bounded(&option.id, 1, 64)
                    || !options.insert(&option.id)
                    || !bounded(&option.label, 1, 200)
                    || !bounded(&option.description, 0, 1000)
                {
                    return Err(invalid(
                        "options require unique IDs and bounded labels/descriptions",
                    ));
                }
            }
        }
        Ok(())
    }

    fn schema(&self) -> Result<wire::ElicitationSchema, ToolError> {
        self.validate()?;
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        let mut order = Vec::new();
        let mut require_answer = Vec::new();
        for (index, q) in self.questions.iter().enumerate() {
            // Generated wire names cannot collide with model-provided IDs.
            let key = format!("question_{index}");
            let choices = Value::Array(
                q.options
                    .iter()
                    .map(|o| {
                        object([
                            ("const", Value::from(o.id.clone())),
                            ("title", Value::from(o.label.clone())),
                            ("description", Value::from(o.description.clone())),
                        ])
                    })
                    .collect(),
            );
            let field = if q.multiple {
                object([
                    ("type", Value::from("array")),
                    ("title", Value::from(q.title.clone())),
                    ("description", Value::from(q.prompt.clone())),
                    ("items", object([("anyOf", choices)])),
                    ("minItems", Value::from(if q.allow_custom { 0 } else { 1 })),
                    ("maxItems", Value::from(q.options.len())),
                ])
            } else {
                object([
                    ("type", Value::from("string")),
                    ("title", Value::from(q.title.clone())),
                    ("description", Value::from(q.prompt.clone())),
                    ("oneOf", choices),
                ])
            };
            properties.insert(key.clone(), field);
            if !q.allow_custom {
                required.push(key.clone());
            }
            order.push(key.clone());
            if q.allow_custom {
                let custom = format!("question_{index}_custom");
                properties.insert(
                    custom.clone(),
                    object([
                        ("type", Value::from("string")),
                        ("title", Value::from(format!("{} — custom answer", q.title))),
                        (
                            "description",
                            Value::from(
                                "Optional custom answer. Provide a selection or a custom answer.",
                            ),
                        ),
                        ("maxLength", Value::from(4000)),
                    ]),
                );
                require_answer.push(object([
                    ("selection", Value::from(key)),
                    ("custom", Value::from(custom.clone())),
                ]));
                order.push(custom);
            }
        }
        serde_json::from_value(object([
            ("type", Value::from("object")),
            ("properties", Value::Object(properties)),
            ("required", strings(required)),
            (
                "_meta",
                object([(
                    "kit/questions",
                    object([
                        ("order", strings(order)),
                        ("require_answer", Value::Array(require_answer)),
                    ]),
                )]),
            ),
        ]))
        .map_err(|e| invalid(e.to_string()))
    }

    fn request(
        &self,
        session: wire::SessionId,
        call: wire::ToolCallId,
    ) -> Result<wire::CreateElicitationRequest, ToolError> {
        let scope = wire::ElicitationSessionScope::new(session).tool_call_id(call);
        let request = wire::CreateElicitationRequest::new(
            wire::ElicitationFormMode::new(scope, self.schema()?),
            self.message.clone(),
        );
        let bytes = serde_json::to_vec(&request).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_FORM_REQUEST_BYTES {
            return Err(invalid(
                "The complete question form exceeds 128 KiB of serialized UTF-8. Shorten question/option descriptions or split the questions into smaller batches.",
            ));
        }
        Ok(request)
    }

    fn answer(&self, response: wire::CreateElicitationResponse) -> Result<Value, ToolError> {
        let failure =
            || ToolError::ExecutionFailed("client returned an invalid question response".into());
        let accepted = match response.action {
            wire::ElicitationAction::Accept(value) => value,
            wire::ElicitationAction::Decline => {
                return Ok(object([("action", Value::from("decline"))]));
            }
            wire::ElicitationAction::Cancel => {
                return Ok(object([("action", Value::from("cancel"))]));
            }
            _ => return Err(failure()),
        };
        let content = accepted.content.ok_or_else(failure)?;
        let mut allowed = BTreeSet::new();
        let mut answers = serde_json::Map::new();
        for (index, q) in self.questions.iter().enumerate() {
            let key = format!("question_{index}");
            allowed.insert(key.clone());
            let selected = match content.get(&key) {
                Some(wire::ElicitationContentValue::String(value)) if !q.multiple => {
                    vec![value.clone()]
                }
                Some(wire::ElicitationContentValue::StringArray(values))
                    if q.multiple && values.len() <= q.options.len() =>
                {
                    values.clone()
                }
                None if q.allow_custom => Vec::new(),
                _ => return Err(failure()),
            };
            let unique: BTreeSet<_> = selected.iter().collect();
            if unique.len() != selected.len()
                || selected
                    .iter()
                    .any(|id| !q.options.iter().any(|option| &option.id == id))
            {
                return Err(failure());
            }
            let custom_key = format!("question_{index}_custom");
            let custom = if q.allow_custom {
                allowed.insert(custom_key.clone());
                match content.get(&custom_key) {
                    Some(wire::ElicitationContentValue::String(value))
                        if bounded(value, 0, 4000) =>
                    {
                        Some(value)
                    }
                    None => None,
                    _ => return Err(failure()),
                }
            } else {
                None
            };
            if selected.is_empty() && custom.is_none_or(|value| value.trim().is_empty()) {
                return Err(failure());
            }
            let mut answer = serde_json::Map::from_iter([("selected".into(), strings(selected))]);
            if let Some(custom) = custom {
                answer.insert("custom".into(), Value::from(custom.clone()));
            }
            answers.insert(q.id.clone(), Value::Object(answer));
        }
        if content.keys().any(|key| !allowed.contains(key)) {
            return Err(failure());
        }
        Ok(object([
            ("action", Value::from("accept")),
            ("answers", Value::Object(answers)),
        ]))
    }
}

// Created only after the request is admitted. On ordinary completion, transport
// failure, unwind, or future cancellation the state is restored before releasing
// admission. The actor owns the eventual idle transition. This tool is not in the
// backgroundable compose registry, so it cannot outlive foreground settlement.
struct Waiting<'a>(&'a AskUserTool);
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let _ = self
            .0
            .state(wire::StateUpdate::Running(wire::RunningStateUpdate::new()));
    }
}
impl AskUserTool {
    fn state(&self, state: wire::StateUpdate) -> Result<(), ToolError> {
        self.connection
            .send_notification(wire::UpdateSessionNotification::new(
                self.session.clone(),
                wire::SessionUpdate::StateUpdate(state),
            ))
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        if request.session_id.0.as_str() != self.session.0.as_ref() {
            return Err(ToolError::ExecutionFailed(
                "question belongs to a different session".into(),
            ));
        }
        let input: Input =
            serde_json::from_value(request.input).map_err(|e| invalid(e.to_string()))?;
        let elicitation = input.request(
            self.session.clone(),
            wire::ToolCallId::new(request.call_id.to_string()),
        )?;
        let cancellation = context.cancellation.clone();
        let interrupted = async {
            match &cancellation {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending().await,
            }
        };
        let operation = async {
            let _admission = self.admission.lock().await;
            if cancellation
                .as_ref()
                .is_some_and(|cancel| cancel.is_cancelled())
            {
                return Err(ToolError::Cancelled);
            }
            self.state(wire::StateUpdate::RequiresAction(
                wire::RequiresActionStateUpdate::new(),
            ))?;
            let _waiting = Waiting(self);
            // Dropping block_task drops SentRequest: the SDK sends the targeted
            // $/cancel_request, never a blanket cancellation of session dialogs.
            let response = self
                .connection
                .send_request(elicitation)
                .block_task()
                .await
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            if cancellation
                .as_ref()
                .is_some_and(|cancel| cancel.is_cancelled())
            {
                return Err(ToolError::Cancelled);
            }
            input.answer(response)
        };
        // Poll cancellation first; drop the losing operation before returning.
        match select(std::pin::pin!(interrupted), std::pin::pin!(operation)).await {
            Either::Left(((), _)) => Err(ToolError::Cancelled),
            Either::Right((value, _)) => Ok(ToolResult::new(ToolResultPart::success(
                request.call_id,
                ToolOutput::structured(value?),
            ))),
        }
    }
}
