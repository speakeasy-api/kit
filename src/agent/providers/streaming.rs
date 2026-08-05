use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use agentkit_core::{
    Delta, ItemKind, MetadataMap, Part, PartId, PartKind, ReasoningPart, TurnCancellation, Usage,
};
use agentkit_loop::{LoopError, ModelTurn, ModelTurnEvent, ModelTurnResult};

use crate::domain::secret::{DataClass, SecretLease, classify_field, classify_header};

const REDACTED: &str = "[REDACTED]";

#[derive(Clone, Copy, Debug)]
pub struct StreamLimits {
    pub max_bytes: usize,
    pub max_items: usize,
    pub max_elapsed: Duration,
    pub max_delta_bytes: usize,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            max_bytes: 8 * 1024 * 1024,
            max_items: 16_384,
            max_elapsed: Duration::from_secs(300),
            max_delta_bytes: 16 * 1024,
        }
    }
}

impl StreamLimits {
    fn validate(self) -> Result<Self, LoopError> {
        if self.max_bytes == 0
            || self.max_items == 0
            || self.max_elapsed.is_zero()
            || self.max_delta_bytes == 0
        {
            Err(provider_error("invalid stream limits"))
        } else {
            Ok(self)
        }
    }
}

#[derive(Clone, Default)]
pub struct CanaryRedactor {
    patterns: Vec<Vec<u8>>,
}

impl CanaryRedactor {
    pub fn new(canaries: impl IntoIterator<Item = String>) -> Self {
        let mut redactor = Self::default();
        for canary in canaries {
            redactor.add_pattern(canary.as_bytes());
        }
        redactor.finish()
    }

    pub fn with_secrets(mut self, secrets: &[Arc<SecretLease>]) -> Self {
        for secret in secrets {
            self.add_pattern(secret.expose());
        }
        self.finish()
    }

    pub(crate) fn with_secret(mut self, secret: &SecretLease) -> Self {
        self.add_pattern(secret.expose());
        self.finish()
    }

    pub fn redact_text(&self, value: &str) -> String {
        let redacted = self
            .patterns
            .iter()
            .filter(|pattern| std::str::from_utf8(pattern).is_ok())
            .fold(value.as_bytes().to_vec(), |value, pattern| {
                replace_bytes(&value, pattern, REDACTED.as_bytes())
            });
        String::from_utf8(redacted).expect("redacting UTF-8 preserves UTF-8")
    }

    fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
        self.patterns.iter().fold(value.to_vec(), |value, pattern| {
            replace_bytes(&value, pattern, REDACTED.as_bytes())
        })
    }

    fn redact_event(&self, event: ModelTurnEvent) -> Result<ModelTurnEvent, LoopError> {
        let mut value = serde_json::to_value(event).map_err(provider_error)?;
        redact_json(&mut value, self, false);
        serde_json::from_value(value).map_err(provider_error)
    }

    fn add_pattern(&mut self, source: &[u8]) {
        if source.is_empty() {
            return;
        }
        self.patterns.push(source.to_vec());
        for (all, uppercase) in [(false, true), (false, false), (true, true), (true, false)] {
            self.patterns.push(percent_encode(source, all, uppercase));
        }
        for (url_safe, padded) in [(false, true), (false, false), (true, true), (true, false)] {
            self.patterns.push(base64(source, url_safe, padded));
        }
        if let Ok(text) = std::str::from_utf8(source)
            && let Ok(json) = serde_json::to_vec(text)
        {
            self.patterns.push(json);
        }
    }

    fn finish(mut self) -> Self {
        self.patterns.retain(|pattern| !pattern.is_empty());
        self.patterns
            .sort_by_key(|pattern| std::cmp::Reverse(pattern.len()));
        self.patterns.dedup();
        self
    }
}

impl Drop for CanaryRedactor {
    fn drop(&mut self) {
        for pattern in &mut self.patterns {
            pattern.fill(0);
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        std::hint::black_box(&mut self.patterns);
    }
}

impl std::fmt::Debug for CanaryRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanaryRedactor")
            .field("pattern_count", &self.patterns.len())
            .finish()
    }
}

pub trait StreamCommit: Send {
    fn commit_chunk(&mut self, sequence: u64, event: &ModelTurnEvent) -> Result<(), LoopError>;

    fn commit_outcome(&mut self, result: &ModelTurnResult) -> Result<(), LoopError>;
}

impl StreamCommit for Box<dyn StreamCommit> {
    fn commit_chunk(&mut self, sequence: u64, event: &ModelTurnEvent) -> Result<(), LoopError> {
        (**self).commit_chunk(sequence, event)
    }

    fn commit_outcome(&mut self, result: &ModelTurnResult) -> Result<(), LoopError> {
        (**self).commit_outcome(result)
    }
}

pub struct BoundedTurn<T, C> {
    inner: Option<T>,
    commit: C,
    limits: StreamLimits,
    redactor: CanaryRedactor,
    retain_reasoning_summaries: bool,
    ready: VecDeque<ModelTurnEvent>,
    prepared: bool,
    failed: bool,
}

impl<T, C> BoundedTurn<T, C> {
    pub fn new(inner: T, commit: C, limits: StreamLimits, redactor: CanaryRedactor) -> Self {
        Self {
            inner: Some(inner),
            commit,
            limits,
            redactor,
            retain_reasoning_summaries: false,
            ready: VecDeque::new(),
            prepared: false,
            failed: false,
        }
    }

    pub fn with_reasoning_summaries(mut self, retain: bool) -> Self {
        self.retain_reasoning_summaries = retain;
        self
    }
}

impl<T, C> ModelTurn for BoundedTurn<T, C>
where
    T: ModelTurn,
    C: StreamCommit,
{
    fn next_event<'life0, 'async_trait>(
        &'life0 mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<ModelTurnEvent>, LoopError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if self.failed {
                return Err(provider_error("provider stream is terminal after failure"));
            }
            if !self.prepared
                && let Err(error) = self.prepare(cancellation).await
            {
                self.ready.clear();
                self.inner = None;
                self.failed = true;
                return Err(error);
            }
            Ok(self.ready.pop_front())
        })
    }
}

impl<T, C> BoundedTurn<T, C>
where
    T: ModelTurn,
    C: StreamCommit,
{
    async fn prepare(&mut self, cancellation: Option<TurnCancellation>) -> Result<(), LoopError> {
        let limits = self.limits.validate()?;
        let started = Instant::now();
        let mut bytes = 0usize;
        let mut events = Vec::new();
        let mut open = HashSet::<PartId>::new();
        let mut finished = false;
        let turn = self
            .inner
            .as_mut()
            .ok_or_else(|| provider_error("provider stream is unavailable"))?;

        loop {
            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                return Err(LoopError::Cancelled);
            }
            let remaining = limits
                .max_elapsed
                .checked_sub(started.elapsed())
                .ok_or_else(|| provider_error("provider stream exceeded time limit"))?;
            let event = tokio::time::timeout(remaining, turn.next_event(cancellation.clone()))
                .await
                .map_err(|_| provider_error("provider stream exceeded time limit"))?
                .map_err(|error| match error {
                    LoopError::Cancelled => LoopError::Cancelled,
                    error => provider_error(self.redactor.redact_text(&error.to_string())),
                })?;
            let Some(event) = event else {
                break;
            };
            if finished {
                return Err(provider_error(
                    "provider emitted data after terminal outcome",
                ));
            }
            validate_order(&event, &mut open)?;
            let size = serde_json::to_vec(&event).map_err(provider_error)?.len();
            bytes = bytes
                .checked_add(size)
                .ok_or_else(|| provider_error("provider stream byte count overflow"))?;
            if bytes > limits.max_bytes || events.len() >= limits.max_items {
                return Err(provider_error("provider stream exceeded buffer limits"));
            }
            finished = matches!(event, ModelTurnEvent::Finished(_));
            events.push(event);
        }

        if !finished {
            return Err(provider_error(
                "provider stream ended without terminal outcome",
            ));
        }
        if !open.is_empty() {
            return Err(provider_error(
                "provider stream ended with uncommitted parts",
            ));
        }

        let events = sanitize_events(events, &self.redactor, self.retain_reasoning_summaries)?;
        let events = redact_and_split(events, &self.redactor, limits.max_delta_bytes)?;
        if events.len() > limits.max_items {
            return Err(provider_error("provider stream exceeded item limit"));
        }
        let visible_bytes = events.iter().try_fold(0_usize, |bytes, event| {
            bytes
                .checked_add(serde_json::to_vec(event).map_err(provider_error)?.len())
                .ok_or_else(|| provider_error("provider stream byte count overflow"))
        })?;
        if visible_bytes > limits.max_bytes || started.elapsed() > limits.max_elapsed {
            return Err(provider_error("provider stream exceeded visible limits"));
        }
        let mut committed = VecDeque::with_capacity(events.len());
        let mut sequence = 0_u64;
        for event in events {
            if started.elapsed() > limits.max_elapsed {
                return Err(provider_error("provider stream exceeded time limit"));
            }
            match &event {
                ModelTurnEvent::Finished(result) => self
                    .commit
                    .commit_outcome(result)
                    .map_err(|_| commit_error())?,
                _ => {
                    sequence = sequence
                        .checked_add(1)
                        .ok_or_else(|| provider_error("provider stream sequence overflow"))?;
                    self.commit
                        .commit_chunk(sequence, &event)
                        .map_err(|_| commit_error())?;
                }
            }
            committed.push_back(event);
        }
        self.ready = committed;
        self.inner = None;
        self.prepared = true;
        Ok(())
    }
}

fn sanitize_events(
    events: Vec<ModelTurnEvent>,
    redactor: &CanaryRedactor,
    retain_reasoning_summaries: bool,
) -> Result<Vec<ModelTurnEvent>, LoopError> {
    let mut reasoning_parts = HashSet::new();
    let mut sanitized = Vec::with_capacity(events.len());
    for event in events {
        match event {
            ModelTurnEvent::Delta(Delta::BeginPart {
                part_id,
                kind: PartKind::Reasoning,
            }) => {
                reasoning_parts.insert(part_id);
            }
            ModelTurnEvent::Delta(
                Delta::AppendText { ref part_id, .. }
                | Delta::AppendBytes { ref part_id, .. }
                | Delta::ReplaceStructured { ref part_id, .. }
                | Delta::SetMetadata { ref part_id, .. },
            ) if reasoning_parts.contains(part_id) => {}
            ModelTurnEvent::Delta(Delta::BeginPart { kind, .. }) if !allowed_part_kind(kind) => {
                return Err(provider_error("provider emitted a private part kind"));
            }
            ModelTurnEvent::Delta(Delta::CommitPart {
                part: Part::Reasoning(_),
            }) => {}
            ModelTurnEvent::Delta(Delta::CommitPart { mut part }) => {
                sanitize_part(&mut part, redactor, false)?;
                sanitized.push(ModelTurnEvent::Delta(Delta::CommitPart { part }));
            }
            ModelTurnEvent::Delta(Delta::SetMetadata {
                part_id,
                mut metadata,
            }) => {
                sanitize_metadata(&mut metadata);
                sanitized.push(ModelTurnEvent::Delta(Delta::SetMetadata {
                    part_id,
                    metadata,
                }));
            }
            ModelTurnEvent::Delta(Delta::ReplaceStructured { part_id, mut value }) => {
                sanitize_provider_value(&mut value, false);
                sanitized.push(ModelTurnEvent::Delta(Delta::ReplaceStructured {
                    part_id,
                    value,
                }));
            }
            ModelTurnEvent::ToolCall(mut call) => {
                sanitize_metadata(&mut call.metadata);
                sanitize_provider_value(&mut call.input, false);
                sanitized.push(ModelTurnEvent::ToolCall(call));
            }
            ModelTurnEvent::Usage(mut usage) => {
                sanitize_usage(&mut usage);
                sanitized.push(ModelTurnEvent::Usage(usage));
            }
            ModelTurnEvent::Finished(mut result) => {
                sanitize_result(&mut result, redactor, retain_reasoning_summaries)?;
                sanitized.push(ModelTurnEvent::Finished(result));
            }
            event => sanitized.push(event),
        }
    }
    Ok(sanitized)
}

fn sanitize_result(
    result: &mut ModelTurnResult,
    redactor: &CanaryRedactor,
    retain_reasoning_summaries: bool,
) -> Result<(), LoopError> {
    sanitize_metadata(&mut result.metadata);
    if let Some(usage) = &mut result.usage {
        sanitize_usage(usage);
    }
    for item in &mut result.output_items {
        if item.kind != ItemKind::Assistant {
            return Err(provider_error("provider emitted a private item kind"));
        }
        sanitize_metadata(&mut item.metadata);
        if let Some(usage) = &mut item.usage {
            sanitize_usage(usage);
        }
        let mut parts = Vec::with_capacity(item.parts.len());
        for mut part in std::mem::take(&mut item.parts) {
            if let Part::Reasoning(reasoning) = part {
                if retain_reasoning_summaries && let Some(summary) = reasoning.summary {
                    parts.push(Part::Reasoning(ReasoningPart::redacted_summary(
                        redactor.redact_text(&summary),
                    )));
                }
                continue;
            }
            sanitize_part(&mut part, redactor, false)?;
            parts.push(part);
        }
        item.parts = parts;
    }
    Ok(())
}

fn sanitize_part(
    part: &mut Part,
    _redactor: &CanaryRedactor,
    allow_summary: bool,
) -> Result<(), LoopError> {
    let metadata = match part {
        Part::Text(part) => &mut part.metadata,
        Part::Media(part) => &mut part.metadata,
        Part::File(part) => &mut part.metadata,
        Part::Structured(part) => {
            sanitize_provider_value(&mut part.value, false);
            if let Some(schema) = &mut part.schema {
                sanitize_provider_value(schema, false);
            }
            &mut part.metadata
        }
        Part::ToolCall(part) => {
            sanitize_provider_value(&mut part.input, false);
            &mut part.metadata
        }
        Part::Reasoning(part)
            if allow_summary
                && part.redacted
                && part.summary.is_some()
                && part.data.is_none()
                && part.metadata.is_empty() =>
        {
            return Ok(());
        }
        Part::Reasoning(_) | Part::ToolResult(_) | Part::Custom(_) => {
            return Err(provider_error("provider emitted a private content part"));
        }
    };
    sanitize_metadata(metadata);
    Ok(())
}

fn sanitize_usage(usage: &mut Usage) {
    sanitize_metadata(&mut usage.metadata);
}

fn sanitize_metadata(metadata: &mut MetadataMap) {
    metadata.retain(|name, value| {
        if private_field(name) {
            return false;
        }
        if classify_field(name) == DataClass::Secret {
            *value = serde_json::Value::String(REDACTED.to_owned());
        } else {
            sanitize_provider_value(value, name.eq_ignore_ascii_case("headers"));
        }
        true
    });
}

fn sanitize_provider_value(value: &mut serde_json::Value, headers: bool) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                sanitize_provider_value(value, headers);
            }
        }
        serde_json::Value::Object(fields) => {
            fields.retain(|name, value| {
                if private_field(name) {
                    return false;
                }
                let class = if headers {
                    classify_header(name)
                } else {
                    classify_field(name)
                };
                if class == DataClass::Secret {
                    *value = serde_json::Value::String(REDACTED.to_owned());
                } else {
                    sanitize_provider_value(value, name.eq_ignore_ascii_case("headers"));
                }
                true
            });
        }
        _ => {}
    }
}

fn private_field(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "chain_of_thought"
            | "cot"
            | "hidden_reasoning"
            | "reasoning"
            | "reasoning_content"
            | "thinking"
            | "thinking_content"
    )
}

fn allowed_part_kind(kind: PartKind) -> bool {
    matches!(
        kind,
        PartKind::Text
            | PartKind::Media
            | PartKind::File
            | PartKind::Structured
            | PartKind::ToolCall
    )
}

fn validate_order(event: &ModelTurnEvent, open: &mut HashSet<PartId>) -> Result<(), LoopError> {
    match event {
        ModelTurnEvent::Delta(Delta::BeginPart { part_id, .. })
            if !open.insert(part_id.clone()) =>
        {
            return Err(provider_error("provider began the same part twice"));
        }
        ModelTurnEvent::Delta(
            Delta::AppendText { part_id, .. }
            | Delta::AppendBytes { part_id, .. }
            | Delta::ReplaceStructured { part_id, .. }
            | Delta::SetMetadata { part_id, .. },
        ) if !open.contains(part_id) => {
            return Err(provider_error("provider delta references an unopened part"));
        }
        ModelTurnEvent::Delta(Delta::CommitPart { .. }) => {
            let Some(part_id) = open.iter().next().cloned() else {
                return Err(provider_error(
                    "provider committed a part before beginning it",
                ));
            };
            open.remove(&part_id);
        }
        ModelTurnEvent::Finished(_) if !open.is_empty() => {
            return Err(provider_error(
                "provider finished before committing every part",
            ));
        }
        _ => {}
    }
    Ok(())
}

fn redact_and_split(
    events: Vec<ModelTurnEvent>,
    redactor: &CanaryRedactor,
    maximum: usize,
) -> Result<Vec<ModelTurnEvent>, LoopError> {
    let mut text = HashMap::<PartId, String>::new();
    let mut bytes = HashMap::<PartId, Vec<u8>>::new();
    for event in &events {
        match event {
            ModelTurnEvent::Delta(Delta::AppendText { part_id, chunk }) => {
                text.entry(part_id.clone()).or_default().push_str(chunk);
            }
            ModelTurnEvent::Delta(Delta::AppendBytes { part_id, chunk }) => {
                bytes
                    .entry(part_id.clone())
                    .or_default()
                    .extend_from_slice(chunk);
            }
            _ => {}
        }
    }
    let mut emitted_text = HashSet::new();
    let mut emitted_bytes = HashSet::new();
    let mut output = Vec::new();
    for event in events {
        match event {
            ModelTurnEvent::Delta(Delta::AppendText { part_id, .. }) => {
                if emitted_text.insert(part_id.clone()) {
                    let value = redactor.redact_text(text.get(&part_id).map_or("", String::as_str));
                    for chunk in split_text(value, maximum) {
                        output.push(ModelTurnEvent::Delta(Delta::AppendText {
                            part_id: part_id.clone(),
                            chunk,
                        }));
                    }
                }
            }
            ModelTurnEvent::Delta(Delta::AppendBytes { part_id, .. }) => {
                if emitted_bytes.insert(part_id.clone()) {
                    let value =
                        redactor.redact_bytes(bytes.get(&part_id).map_or(&[][..], Vec::as_slice));
                    for chunk in value.chunks(maximum) {
                        output.push(ModelTurnEvent::Delta(Delta::AppendBytes {
                            part_id: part_id.clone(),
                            chunk: chunk.to_vec(),
                        }));
                    }
                }
            }
            event => output.push(redactor.redact_event(event)?),
        }
    }
    Ok(output)
}

fn split_text(value: String, maximum: usize) -> Vec<String> {
    if value.is_empty() {
        return vec![value];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < value.len() {
        let mut end = (start + maximum).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = value[start..]
                .char_indices()
                .nth(1)
                .map_or(value.len(), |(offset, _)| start + offset);
        }
        chunks.push(value[start..end].to_owned());
        start = end;
    }
    chunks
}

fn redact_json(value: &mut serde_json::Value, redactor: &CanaryRedactor, headers: bool) {
    match value {
        serde_json::Value::String(value) => *value = redactor.redact_text(value),
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json(value, redactor, headers);
            }
        }
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                let class = if headers {
                    classify_header(name)
                } else {
                    classify_field(name)
                };
                if class == DataClass::Secret {
                    *value = serde_json::Value::String(REDACTED.to_owned());
                } else {
                    redact_json(value, redactor, name.eq_ignore_ascii_case("headers"));
                }
            }
        }
        _ => {}
    }
}

fn percent_encode(source: &[u8], all: bool, uppercase: bool) -> Vec<u8> {
    let hex = if uppercase {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut output = Vec::with_capacity(source.len() * 3);
    for &byte in source {
        if !all && (byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)) {
            output.push(byte);
        } else {
            output.extend_from_slice(&[b'%', hex[(byte >> 4) as usize], hex[(byte & 15) as usize]]);
        }
    }
    output
}

fn base64(source: &[u8], url_safe: bool, padded: bool) -> Vec<u8> {
    let alphabet = if url_safe {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut output = Vec::with_capacity(source.len().div_ceil(3) * 4);
    for chunk in source.chunks(3) {
        let bits = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(alphabet[((bits >> 18) & 63) as usize]);
        output.push(alphabet[((bits >> 12) & 63) as usize]);
        if chunk.len() > 1 {
            output.push(alphabet[((bits >> 6) & 63) as usize]);
        } else if padded {
            output.push(b'=');
        }
        if chunk.len() > 2 {
            output.push(alphabet[(bits & 63) as usize]);
        } else if padded {
            output.push(b'=');
        }
    }
    output
}

fn replace_bytes(value: &[u8], pattern: &[u8], replacement: &[u8]) -> Vec<u8> {
    if pattern.is_empty() {
        return value.to_vec();
    }
    let mut output = Vec::with_capacity(value.len());
    let mut offset = 0;
    while offset < value.len() {
        if value[offset..].starts_with(pattern) {
            output.extend_from_slice(replacement);
            offset += pattern.len();
        } else {
            output.push(value[offset]);
            offset += 1;
        }
    }
    output
}

fn provider_error(error: impl std::fmt::Display) -> LoopError {
    LoopError::Provider(format!("bounded provider stream: {error}"))
}

fn commit_error() -> LoopError {
    provider_error("stream commit failed; outcome_unknown")
}
