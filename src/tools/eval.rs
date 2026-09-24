//! Explicit, opt-in evaluation. One invocation is one paid submission.
use crate::{credentials::CredentialStorage, provider::typesafe_auth};
use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
const MAX_BYTES: usize = 256 * 1024;
const MAX_RESPONSE: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const URL: &str = "https://api.typesafe.ai/v1/systemone";
#[derive(Clone)]
pub struct EvalTool {
    storage: CredentialStorage,
    // Shared by every clone/Compose registry in this runtime. The only writer
    // is invoke: an RAII permit spans submission and response validation. Error,
    // timeout and cancellation drop it; no secondary locks or counters exist.
    permits: Arc<Semaphore>,
    spec: ToolSpec,
}
impl EvalTool {
    pub(crate) fn available(enabled: bool, storage: &CredentialStorage) -> bool {
        enabled && typesafe_auth::resolve_api_key(storage).is_ok_and(|key| key.is_some())
    }
    pub(crate) fn new(storage: CredentialStorage) -> Self {
        Self {
            storage,
            permits: Arc::new(Semaphore::new(4)),
            spec: ToolSpec::new(ToolName::new("eval"),
                "Evaluate supplied state with named independent questions in one TypeSafe Jev request. Noul answers yes/no with a probability; Choice selects a named option; Score rates ordered levels. Sends state and questions to TypeSafe and consumes its quota. Maximum 256 KiB input, 64 questions, 30 seconds, four concurrent evaluations. Never automatically retries or batches; a cancelled or failed evaluation may still consume quota. Results are assessments, not facts. No automatic compaction or filtering.",
                input_schema())
                .with_output_schema(output_schema()),
        }
    }
}
fn failure(message: &str) -> ToolError {
    ToolError::ExecutionFailed(message.into())
}
fn content(value: &Value) -> bool {
    value.is_string() || value.is_object() || value.is_array()
}
fn payload(input: Value) -> Result<Value, ToolError> {
    let invalid = || {
        ToolError::InvalidInput("Use state and 1–64 named Noul, Choice, or Score questions; input must not exceed 256 KiB.".into())
    };
    if serde_json::to_vec(&input).map_err(|_| invalid())?.len() > MAX_BYTES {
        return Err(invalid());
    }
    let fields = input.as_object().ok_or_else(invalid)?;
    if fields.len() != 2 || !content(&input["state"]) {
        return Err(invalid());
    }
    let questions = input["questions"].as_object().ok_or_else(invalid)?;
    if questions.is_empty() || questions.len() > 64 {
        return Err(invalid());
    }
    for (name, question) in questions {
        let q = question.as_object().ok_or_else(invalid)?;
        if name.is_empty()
            || name.chars().count() > 128
            || !content(&question["instructions"])
            || q.keys()
                .any(|key| !["type", "instructions", "criteria"].contains(&key.as_str()))
        {
            return Err(invalid());
        }
        let valid = match question["type"].as_str() {
            Some("noul") => q.get("criteria").is_none_or(|v| {
                v.as_object().is_some_and(|c| {
                    c.iter()
                        .all(|(k, v)| ["true", "false"].contains(&k.as_str()) && content(v))
                })
            }),
            Some("choice") => question["criteria"].as_object().is_some_and(|c| {
                !c.is_empty() && c.len() <= 255 && c.values().all(|v| v.is_null() || content(v))
            }),
            Some("score") => question["criteria"]
                .as_array()
                .is_some_and(|c| (2..=10).contains(&c.len()) && c.iter().all(content)),
            _ => false,
        };
        if !valid {
            return Err(invalid());
        }
    }
    Ok(object([
        ("model", Value::from("jev-latest")),
        ("state", input["state"].clone()),
        ("questions", input["questions"].clone()),
    ]))
}
fn probability(value: &Value) -> bool {
    value.as_f64().is_some_and(|n| (0.0..=1.0).contains(&n))
}
fn validate_response(value: Value, body: &Value) -> Result<Value, ToolError> {
    let invalid = || failure("The evaluation returned an invalid result. No retry was made.");
    if !value["model"]
        .as_str()
        .is_some_and(|s| !s.is_empty() && s.len() <= 128 && s != "jev-latest")
        || value["usage"]["input_tokens"].as_u64().is_none()
        || value["usage"]["output_tokens"].as_u64().is_none()
    {
        return Err(invalid());
    }
    let answers = value["answers"].as_object().ok_or_else(invalid)?;
    let questions = body["questions"].as_object().ok_or_else(invalid)?;
    if answers.len() != questions.len() {
        return Err(invalid());
    }
    for (name, q) in questions {
        let a = answers.get(name).ok_or_else(invalid)?;
        if a["type"] != q["type"] {
            return Err(invalid());
        }
        if q["type"] == "noul" {
            if !probability(&a["noul"]) {
                return Err(invalid());
            }
            continue;
        }
        if !probability(&a["confidence"]) {
            return Err(invalid());
        }
        let probabilities = a["probabilities"].as_object().ok_or_else(invalid)?;
        if !probabilities.values().all(probability)
            || (probabilities
                .values()
                .filter_map(Value::as_f64)
                .sum::<f64>()
                - 1.0)
                .abs()
                > 0.001
        {
            return Err(invalid());
        }
        if q["type"] == "choice" {
            let criteria = q["criteria"].as_object().ok_or_else(invalid)?;
            if probabilities.len() != criteria.len()
                || !criteria.keys().all(|k| probabilities.contains_key(k))
                || !a["choice"]
                    .as_str()
                    .is_some_and(|s| criteria.contains_key(s))
            {
                return Err(invalid());
            }
        } else {
            let levels = q["criteria"].as_array().ok_or_else(invalid)?.len();
            let legend = a["legend"].as_object().ok_or_else(invalid)?;
            if probabilities.len() != levels
                || legend.len() != levels
                || !(0..levels).all(|i| {
                    probabilities.contains_key(&i.to_string())
                        && legend.get(&i.to_string()).is_some_and(Value::is_string)
                })
                || !a["score"]
                    .as_f64()
                    .is_some_and(|s| (0.0..=(levels - 1) as f64).contains(&s))
            {
                return Err(invalid());
            }
        }
    }
    Ok(value)
}
fn client() -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| failure("Evaluation is temporarily unavailable."))
}
async fn submit(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    body: &Value,
) -> Result<Value, ToolError> {
    let mut response = client
        .post(url)
        .bearer_auth(key)
        .json(body)
        .send()
        .await
        .map_err(|_| {
            failure("Evaluation did not complete. No retry was made; quota may have been used.")
        })?;
    if !response.status().is_success() {
        return Err(failure(match response.status().as_u16() {
            401 | 403 => {
                "TypeSafe could not accept your key. Use kit auth login typesafe to update it."
            }
            429 => "Your TypeSafe evaluation limit was reached. Try again later.",
            _ => "Evaluation did not complete. No retry was made; quota may have been used.",
        }));
    }
    let mut bytes = Vec::new();
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE as u64)
    {
        return Err(failure("Evaluation result exceeds 1 MiB."));
    }
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure("Evaluation result was interrupted. No retry was made."))?
    {
        if bytes.len() + chunk.len() > MAX_RESPONSE {
            return Err(failure("Evaluation result exceeds 1 MiB."));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| failure("The evaluation returned an invalid result. No retry was made."))?;
    validate_response(value, body)
}
#[async_trait]
impl Tool for EvalTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let work = async {
            let body = payload(request.input)?;
            let _permit = self
                .permits
                .acquire()
                .await
                .map_err(|_| failure("Evaluation is unavailable."))?;
            let storage = self.storage.clone();
            let credentials =
                tokio::task::spawn_blocking(move || typesafe_auth::resolve_api_key(&storage))
                    .await
                    .map_err(|_| failure("TypeSafe key could not be loaded."))?
                    .map_err(|_| failure("TypeSafe key could not be loaded."))?
                    .ok_or_else(|| failure("Use kit auth login typesafe before evaluating."))?;
            let value = submit(&client()?, URL, credentials.api_key(), &body).await?;
            Ok(ToolResult::new(ToolResultPart::success(
                request.call_id,
                ToolOutput::structured(value),
            )))
        };
        let cancelled = std::pin::pin!(async {
            match &context.cancellation {
                Some(c) => c.cancelled().await,
                None => std::future::pending().await,
            }
        });
        let work = std::pin::pin!(tokio::time::timeout(TIMEOUT, work));
        match futures_util::future::select(cancelled, work).await {
            futures_util::future::Either::Left(((), _)) => Err(failure(
                "Evaluation cancelled. Quota may have been used; no retry was made.",
            )),
            futures_util::future::Either::Right((result, _)) => result.unwrap_or_else(|_| {
                Err(failure(
                    "Evaluation timed out. Quota may have been used; no retry was made.",
                ))
            }),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests;

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(Map::from_iter(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    ))
}
fn strings(values: &[&str]) -> Value {
    Value::Array(values.iter().map(|s| Value::from(*s)).collect())
}
fn typed(kind: &str) -> Value {
    object([("type", Value::from(kind))])
}
fn record(properties: Value, required: &[&str]) -> Value {
    object([
        ("type", Value::from("object")),
        ("properties", properties),
        ("required", strings(required)),
        ("additionalProperties", Value::Bool(false)),
    ])
}
fn content_schema() -> Value {
    object([("type", strings(&["string", "object", "array"]))])
}
fn question_schema(kind: &str, criteria: Value, required: bool) -> Value {
    record(
        object([
            ("type", object([("const", Value::from(kind))])),
            ("instructions", content_schema()),
            ("criteria", criteria),
        ]),
        if required {
            &["type", "instructions", "criteria"]
        } else {
            &["type", "instructions"]
        },
    )
}
fn input_schema() -> Value {
    let questions = object([(
        "oneOf",
        Value::Array(vec![
            question_schema(
                "noul",
                record(
                    object([("true", content_schema()), ("false", content_schema())]),
                    &[],
                ),
                false,
            ),
            question_schema(
                "choice",
                object([
                    ("type", Value::from("object")),
                    ("minProperties", Value::from(1)),
                    ("maxProperties", Value::from(255)),
                    (
                        "additionalProperties",
                        object([("type", strings(&["string", "object", "array", "null"]))]),
                    ),
                ]),
                true,
            ),
            question_schema(
                "score",
                object([
                    ("type", Value::from("array")),
                    ("minItems", Value::from(2)),
                    ("maxItems", Value::from(10)),
                    ("items", content_schema()),
                ]),
                true,
            ),
        ]),
    )]);
    record(
        object([
            ("state", content_schema()),
            (
                "questions",
                object([
                    ("type", Value::from("object")),
                    ("minProperties", Value::from(1)),
                    ("maxProperties", Value::from(64)),
                    (
                        "propertyNames",
                        object([
                            ("minLength", Value::from(1)),
                            ("maxLength", Value::from(128)),
                        ]),
                    ),
                    ("additionalProperties", questions),
                ]),
            ),
        ]),
        &["state", "questions"],
    )
}
fn output_schema() -> Value {
    let probability = object([
        ("type", Value::from("number")),
        ("minimum", Value::from(0)),
        ("maximum", Value::from(1)),
    ]);
    let probabilities = object([
        ("type", Value::from("object")),
        ("additionalProperties", probability.clone()),
    ]);
    let answers = object([(
        "oneOf",
        Value::Array(vec![
            record(
                object([
                    ("type", object([("const", Value::from("noul"))])),
                    ("noul", probability.clone()),
                ]),
                &["type", "noul"],
            ),
            record(
                object([
                    ("type", object([("const", Value::from("choice"))])),
                    ("choice", typed("string")),
                    ("probabilities", probabilities.clone()),
                    ("confidence", probability.clone()),
                ]),
                &["type", "choice", "probabilities", "confidence"],
            ),
            record(
                object([
                    ("type", object([("const", Value::from("score"))])),
                    ("score", typed("number")),
                    ("probabilities", probabilities),
                    ("confidence", probability),
                    (
                        "legend",
                        object([
                            ("type", Value::from("object")),
                            ("additionalProperties", typed("string")),
                        ]),
                    ),
                ]),
                &["type", "score", "probabilities", "confidence", "legend"],
            ),
        ]),
    )]);
    // Preserve forward-compatible response metadata rather than filtering it.
    object([
        ("type", Value::from("object")),
        (
            "properties",
            object([
                ("model", typed("string")),
                (
                    "answers",
                    object([
                        ("type", Value::from("object")),
                        ("additionalProperties", answers),
                    ]),
                ),
                (
                    "usage",
                    object([
                        ("type", Value::from("object")),
                        (
                            "properties",
                            object([
                                ("input_tokens", typed("integer")),
                                ("output_tokens", typed("integer")),
                            ]),
                        ),
                        ("required", strings(&["input_tokens", "output_tokens"])),
                    ]),
                ),
            ]),
        ),
        ("required", strings(&["model", "answers", "usage"])),
    ])
}
