use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use agentkit_core::{
    DataRef, Delta, ItemKind, MetadataMap, Part, PartId, PartKind, ReasoningPart, TurnCancellation,
    Usage,
};
use agentkit_loop::{LoopError, ModelTurn, ModelTurnEvent, ModelTurnResult};

use crate::{
    domain::secret::{DataClass, SecretCustody, SecretLease, classify_field, classify_header},
    telemetry::redact::{CaptureBoundary, SanitizedCapture, SensitiveDataScanner},
};

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
    custody: SecretCustody,
}

impl CanaryRedactor {
    pub fn new(canaries: impl IntoIterator<Item = String>) -> Self {
        Self {
            custody: SecretCustody::new(
                canaries
                    .into_iter()
                    .map(|canary| Arc::new(SecretLease::new(canary.into_bytes()))),
            ),
        }
    }

    pub fn with_secrets(self, secrets: &[Arc<SecretLease>]) -> Self {
        Self {
            custody: SecretCustody::new(
                self.custody
                    .leases()
                    .into_iter()
                    .chain(secrets.iter().cloned()),
            ),
        }
    }

    pub(crate) fn with_secret(self, secret: &SecretLease) -> Self {
        Self {
            custody: SecretCustody::new(
                self.custody
                    .leases()
                    .into_iter()
                    .chain([Arc::new(SecretLease::new(secret.expose().to_vec()))]),
            ),
        }
    }

    pub fn redact_text(&self, value: &str) -> String {
        self.custody
            .redactor()
            .redact_text(CaptureBoundary::Artifact, value)
    }

    fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
        self.custody
            .project(CaptureBoundary::Artifact, value)
            .bytes()
            .expect("finished provider projection")
            .to_vec()
    }

    fn redact_event(&self, event: ModelTurnEvent) -> Result<ModelTurnEvent, LoopError> {
        if !matches!(&event, ModelTurnEvent::Delta(Delta::SetMetadata { .. })) {
            let canonical = serde_json::to_vec(&event).map_err(provider_error)?;
            let projected = self.custody.project(CaptureBoundary::Event, &canonical);
            return serde_json::from_slice(projected.bytes().map_err(provider_error)?)
                .map_err(provider_error);
        }
        let mut projected = serde_json::to_value(&event).map_err(provider_error)?;
        let canonical = projected.clone();
        redact_json(&mut projected, self, false, false);
        if projected == canonical {
            Ok(event)
        } else {
            serde_json::from_value(projected).map_err(provider_error)
        }
    }

    fn redact_event_text(&self, value: &str) -> String {
        self.custody
            .redactor()
            .redact_text(CaptureBoundary::Event, value)
    }

    fn capture(&self) -> SanitizedCapture {
        self.custody.redactor().start(CaptureBoundary::Artifact)
    }
}

impl std::fmt::Debug for CanaryRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanaryRedactor")
            .field("active_leases", &self.custody.leases().len())
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
    pending: VecDeque<ModelTurnEvent>,
    captures: HashMap<PartId, SanitizedCapture>,
    accumulated: HashMap<PartId, Vec<u8>>,
    encodings: HashMap<PartId, PartEncoding>,
    canonical_stream: SensitiveDataScanner,
    content_stream: SensitiveDataScanner,
    name_stream: SensitiveDataScanner,
    projection_state: crate::domain::secret::JsonProjectionState,
    stream_redacted_parts: HashSet<PartId>,
    reasoning_parts: HashSet<PartId>,
    open: HashSet<PartId>,
    open_order: VecDeque<PartId>,
    started: Option<Instant>,
    bytes: usize,
    items: usize,
    pending_bytes: usize,
    visible_bytes: usize,
    visible_items: usize,
    sequence: u64,
    prepared: bool,
    failed: bool,
}

impl<T, C> BoundedTurn<T, C> {
    pub fn new(inner: T, commit: C, limits: StreamLimits, redactor: CanaryRedactor) -> Self {
        let canonical_stream = redactor.custody.redactor().scanner();
        let content_stream = redactor.custody.redactor().scanner();
        let name_stream = redactor.custody.redactor().scanner();
        Self {
            inner: Some(inner),
            commit,
            limits,
            redactor,
            retain_reasoning_summaries: false,
            ready: VecDeque::new(),
            pending: VecDeque::new(),
            captures: HashMap::new(),
            accumulated: HashMap::new(),
            encodings: HashMap::new(),
            canonical_stream,
            content_stream,
            name_stream,
            projection_state: crate::domain::secret::JsonProjectionState::default(),
            stream_redacted_parts: HashSet::new(),
            reasoning_parts: HashSet::new(),
            open: HashSet::new(),
            open_order: VecDeque::new(),
            started: None,
            bytes: 0,
            items: 0,
            pending_bytes: 0,
            visible_bytes: 0,
            visible_items: 0,
            sequence: 0,
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
            // Chunks are committed incrementally for transport continuity, but remain private
            // until the terminal outcome establishes the durable turn boundary.
            while !self.prepared {
                if let Err(error) = self.pump(cancellation.clone()).await {
                    self.ready.clear();
                    self.pending.clear();
                    self.inner = None;
                    self.failed = true;
                    return Err(error);
                }
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
    async fn pump(&mut self, cancellation: Option<TurnCancellation>) -> Result<(), LoopError> {
        let limits = self.limits.validate()?;
        let started = *self.started.get_or_insert_with(Instant::now);
        if cancellation
            .as_ref()
            .is_some_and(TurnCancellation::is_cancelled)
        {
            return Err(LoopError::Cancelled);
        }
        let turn = self
            .inner
            .as_mut()
            .ok_or_else(|| provider_error("provider stream is unavailable"))?;
        let remaining = limits
            .max_elapsed
            .checked_sub(started.elapsed())
            .ok_or_else(|| provider_error("provider stream exceeded time limit"))?;
        let event = tokio::time::timeout(remaining, turn.next_event(cancellation))
            .await
            .map_err(|_| provider_error("provider stream exceeded time limit"))?
            .map_err(|error| match error {
                LoopError::Cancelled => LoopError::Cancelled,
                error => provider_error(self.redactor.redact_text(&error.to_string())),
            })?
            .ok_or_else(|| provider_error("provider stream ended without terminal outcome"))?;
        self.items = self
            .items
            .checked_add(1)
            .ok_or_else(|| provider_error("provider stream item count overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(serde_json::to_vec(&event).map_err(provider_error)?.len())
            .ok_or_else(|| provider_error("provider stream byte count overflow"))?;
        if self.items > limits.max_items || self.bytes > limits.max_bytes {
            return Err(provider_error("provider stream exceeded buffer limits"));
        }
        let committed = validate_order(&event, &mut self.open, &mut self.open_order)?;
        self.process_event(event, committed, limits.max_delta_bytes)?;
        if started.elapsed() > limits.max_elapsed {
            return Err(provider_error("provider stream exceeded time limit"));
        }
        Ok(())
    }

    fn process_event(
        &mut self,
        event: ModelTurnEvent,
        committed: Option<PartId>,
        maximum: usize,
    ) -> Result<(), LoopError> {
        match event {
            ModelTurnEvent::Delta(Delta::BeginPart {
                part_id,
                kind: PartKind::Reasoning,
            }) => {
                self.reasoning_parts.insert(part_id);
            }
            ModelTurnEvent::Delta(Delta::BeginPart { part_id, kind }) => {
                if !allowed_part_kind(kind) {
                    return Err(provider_error("provider emitted a private part kind"));
                }
                self.captures
                    .insert(part_id.clone(), self.redactor.capture());
                self.accumulated.insert(part_id.clone(), Vec::new());
                self.pending
                    .push_back(ModelTurnEvent::Delta(Delta::BeginPart { part_id, kind }));
                self.release_pending(false, maximum)?;
            }
            ModelTurnEvent::Delta(Delta::AppendText { part_id, chunk }) => {
                if !self.reasoning_parts.contains(&part_id) {
                    self.push_part(part_id, chunk.as_bytes(), true, maximum)?;
                }
            }
            ModelTurnEvent::Delta(Delta::AppendBytes { part_id, chunk }) => {
                if !self.reasoning_parts.contains(&part_id) {
                    self.push_part(part_id, &chunk, false, maximum)?;
                }
            }
            ModelTurnEvent::Delta(Delta::CommitPart { mut part }) => {
                let part_id = committed
                    .ok_or_else(|| provider_error("provider committed an unknown part"))?;
                if self.reasoning_parts.remove(&part_id) {
                    return Ok(());
                }
                let encoding = self.encodings.remove(&part_id);
                if let Some(mut capture) = self.captures.remove(&part_id) {
                    capture.finish().map_err(provider_error)?;
                    if let Some(ready) = capture.take_ready() {
                        let bytes = ready.bytes().map_err(provider_error)?.to_vec();
                        self.accumulated
                            .get_mut(&part_id)
                            .expect("open captures retain accumulated output")
                            .extend_from_slice(&bytes);
                        self.queue_part(
                            part_id.clone(),
                            &bytes,
                            encoding == Some(PartEncoding::Text),
                            maximum,
                        )?;
                    }
                }
                sanitize_part(&mut part, &self.redactor, false)?;
                if encoding.is_some() {
                    let projected = self
                        .accumulated
                        .remove(&part_id)
                        .ok_or_else(|| provider_error("provider committed an unknown part"))?;
                    set_part_content(&mut part, projected)?;
                } else {
                    self.accumulated.remove(&part_id);
                }
                self.pending.push_back(
                    self.redactor
                        .redact_event(ModelTurnEvent::Delta(Delta::CommitPart { part }))?,
                );
                self.release_pending(false, maximum)?;
            }
            ModelTurnEvent::Finished(mut result) => {
                sanitize_result(&mut result, &self.redactor, self.retain_reasoning_summaries)?;
                let ModelTurnEvent::Finished(result) = self
                    .redactor
                    .redact_event(ModelTurnEvent::Finished(result))?
                else {
                    unreachable!()
                };
                self.release_pending(true, maximum)?;
                let result: ModelTurnResult =
                    serde_json::from_value(self.project_stream_value(
                        serde_json::to_value(result).map_err(provider_error)?,
                    ))
                    .map_err(provider_error)?;
                self.scan_visible(&ModelTurnEvent::Finished(result.clone()))?;
                self.commit
                    .commit_outcome(&result)
                    .map_err(|_| commit_error())?;
                self.ready.push_back(ModelTurnEvent::Finished(result));
                self.inner = None;
                self.prepared = true;
            }
            event => {
                let mut events =
                    sanitize_events(vec![event], &self.redactor, self.retain_reasoning_summaries)?;
                for event in events.drain(..) {
                    self.pending.push_back(self.redactor.redact_event(event)?);
                }
                self.release_pending(false, maximum)?;
            }
        }
        Ok(())
    }

    fn push_part(
        &mut self,
        part_id: PartId,
        bytes: &[u8],
        text: bool,
        maximum: usize,
    ) -> Result<(), LoopError> {
        let capture = self
            .captures
            .get_mut(&part_id)
            .ok_or_else(|| provider_error("provider delta references an unopened part"))?;
        let encoding = if text {
            PartEncoding::Text
        } else {
            PartEncoding::Bytes
        };
        if self
            .encodings
            .insert(part_id.clone(), encoding)
            .is_some_and(|current| current != encoding)
        {
            return Err(provider_error(
                "provider mixed text and byte deltas for one part",
            ));
        }
        capture.push(bytes).map_err(provider_error)?;
        if let Some(ready) = capture.take_ready() {
            self.accumulated
                .get_mut(&part_id)
                .expect("open captures retain accumulated output")
                .extend_from_slice(ready.bytes().map_err(provider_error)?);
            self.queue_part(
                part_id,
                ready.bytes().map_err(provider_error)?,
                text,
                maximum,
            )?;
        }
        Ok(())
    }

    fn queue_part(
        &mut self,
        part_id: PartId,
        bytes: &[u8],
        text: bool,
        maximum: usize,
    ) -> Result<(), LoopError> {
        if text {
            let value = String::from_utf8(bytes.to_vec()).map_err(provider_error)?;
            for chunk in split_text(value, maximum) {
                self.pending_bytes += chunk.len();
                self.pending
                    .push_back(ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: part_id.clone(),
                        chunk,
                    }));
            }
        } else {
            for chunk in bytes.chunks(maximum) {
                self.pending_bytes += chunk.len();
                self.pending
                    .push_back(ModelTurnEvent::Delta(Delta::AppendBytes {
                        part_id: part_id.clone(),
                        chunk: chunk.to_vec(),
                    }));
            }
        }
        self.release_pending(false, maximum)
    }

    fn release_pending(&mut self, _finishing: bool, _maximum: usize) -> Result<(), LoopError> {
        let mut scanner = self.redactor.custody.redactor().scanner();
        for event in &self.pending {
            match event {
                ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => {
                    scanner.push(chunk.as_bytes())
                }
                ModelTurnEvent::Delta(Delta::AppendBytes { chunk, .. }) => scanner.push(chunk),
                ModelTurnEvent::Delta(Delta::CommitPart {
                    part: Part::Text(part),
                }) => scanner.push(part.text.as_bytes()),
                _ => {}
            }
        }
        if scanner.found() {
            for event in &mut self.pending {
                match event {
                    ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => {
                        *chunk = REDACTED.to_owned()
                    }
                    ModelTurnEvent::Delta(Delta::AppendBytes { chunk, .. }) => {
                        *chunk = REDACTED.as_bytes().to_vec()
                    }
                    ModelTurnEvent::Delta(Delta::CommitPart {
                        part: Part::Text(part),
                    }) => part.text = REDACTED.to_owned(),
                    _ => {}
                }
            }
            self.pending_bytes = self.pending.iter().map(delta_bytes).sum();
        }
        while let Some(event) = self.pending.pop_front() {
            let size = delta_bytes(&event);
            self.pending_bytes = self.pending_bytes.saturating_sub(size);
            self.commit_ready(event)?;
        }
        Ok(())
    }

    fn commit_ready(&mut self, mut event: ModelTurnEvent) -> Result<(), LoopError> {
        self.project_stream_content(&mut event)?;
        self.scan_visible(&event)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| provider_error("provider stream sequence overflow"))?;
        self.commit
            .commit_chunk(self.sequence, &event)
            .map_err(|_| commit_error())?;
        self.ready.push_back(event);
        Ok(())
    }

    fn project_stream_content(&mut self, event: &mut ModelTurnEvent) -> Result<(), LoopError> {
        match event {
            ModelTurnEvent::Delta(Delta::AppendText { part_id, chunk }) => {
                let projected = self.project_stream_value(serde_json::Value::String(chunk.clone()));
                *chunk = projected.as_str().unwrap_or(REDACTED).to_owned();
                if chunk == REDACTED {
                    self.stream_redacted_parts.insert(part_id.clone());
                }
            }
            ModelTurnEvent::Delta(Delta::AppendBytes { part_id, chunk }) => {
                let Ok(text) = String::from_utf8(chunk.clone()) else {
                    return Ok(());
                };
                let projected = self.project_stream_value(serde_json::Value::String(text));
                *chunk = projected.as_str().unwrap_or(REDACTED).as_bytes().to_vec();
                if chunk == REDACTED.as_bytes() {
                    self.stream_redacted_parts.insert(part_id.clone());
                }
            }
            ModelTurnEvent::Delta(Delta::SetMetadata { metadata, .. }) => {
                *metadata = serde_json::from_value(self.project_stream_value(
                    serde_json::to_value(&*metadata).map_err(provider_error)?,
                ))
                .map_err(provider_error)?;
            }
            ModelTurnEvent::Delta(Delta::ReplaceStructured { value, .. }) => {
                *value = self.project_stream_value(value.clone());
            }
            ModelTurnEvent::Delta(Delta::CommitPart { part }) => {
                if self.stream_redacted_parts.is_empty() {
                    *part = serde_json::from_value(self.project_stream_value(
                        serde_json::to_value(&*part).map_err(provider_error)?,
                    ))
                    .map_err(provider_error)?;
                } else {
                    sanitize_part_content(part);
                    self.stream_redacted_parts.clear();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn project_stream_value(&mut self, value: serde_json::Value) -> serde_json::Value {
        self.redactor.custody.project_json_stream(
            CaptureBoundary::Event,
            &value,
            &mut self.projection_state,
        )
    }

    fn scan_visible(&mut self, event: &ModelTurnEvent) -> Result<(), LoopError> {
        let canonical = serde_json::to_vec(&event).map_err(provider_error)?;
        self.canonical_stream.push(&canonical);
        scan_event_content(event, &mut self.content_stream, &mut self.name_stream)?;
        if self.canonical_stream.found() || self.content_stream.found() || self.name_stream.found()
        {
            return Err(provider_error(
                "provider stream reconstructed active secret across events",
            ));
        }
        self.account_visible_bytes(canonical.len())
    }

    fn account_visible_bytes(&mut self, bytes: usize) -> Result<(), LoopError> {
        self.visible_items = self
            .visible_items
            .checked_add(1)
            .ok_or_else(|| provider_error("provider stream item count overflow"))?;
        self.visible_bytes = self
            .visible_bytes
            .checked_add(bytes)
            .ok_or_else(|| provider_error("provider stream byte count overflow"))?;
        if self.visible_items > self.limits.max_items || self.visible_bytes > self.limits.max_bytes
        {
            Err(provider_error("provider stream exceeded buffer limits"))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PartEncoding {
    Text,
    Bytes,
}

fn set_part_content(part: &mut Part, projected: Vec<u8>) -> Result<(), LoopError> {
    match part {
        Part::Text(text) => text.text = String::from_utf8(projected).map_err(provider_error)?,
        Part::Media(media) => set_data_ref(&mut media.data, projected)?,
        Part::File(file) => set_data_ref(&mut file.data, projected)?,
        _ => {}
    }
    Ok(())
}

fn set_data_ref(data: &mut DataRef, projected: Vec<u8>) -> Result<(), LoopError> {
    match data {
        DataRef::InlineText(text) => {
            *text = String::from_utf8(projected).map_err(provider_error)?;
        }
        DataRef::InlineBytes(bytes) => *bytes = projected,
        DataRef::Uri(_) | DataRef::Handle(_) => {}
    }
    Ok(())
}

fn scan_event_content(
    event: &ModelTurnEvent,
    values: &mut SensitiveDataScanner,
    names: &mut SensitiveDataScanner,
) -> Result<(), LoopError> {
    let value = match event {
        ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => {
            values.push(chunk.as_bytes());
            return Ok(());
        }
        ModelTurnEvent::Delta(Delta::AppendBytes { chunk, .. }) => {
            values.push(chunk);
            return Ok(());
        }
        ModelTurnEvent::Delta(Delta::ReplaceStructured { value, .. }) => value.clone(),
        ModelTurnEvent::Delta(Delta::SetMetadata { metadata, .. }) => {
            serde_json::to_value(metadata).map_err(provider_error)?
        }
        ModelTurnEvent::Delta(Delta::CommitPart { part }) => {
            serde_json::to_value(part).map_err(provider_error)?
        }
        ModelTurnEvent::ToolCall(call) => serde_json::json!({
            "input": call.input,
            "metadata": call.metadata,
        }),
        ModelTurnEvent::Usage(usage) => serde_json::to_value(usage).map_err(provider_error)?,
        ModelTurnEvent::Finished(result) => {
            let value = serde_json::to_value(result).map_err(provider_error)?;
            if scan_json_value_continuations(&value, values)
                || scan_json_name_continuations(&value, names)
            {
                return Err(provider_error(
                    "provider stream reconstructed active secret across events",
                ));
            }
            value
        }
        ModelTurnEvent::Delta(Delta::BeginPart { .. }) => return Ok(()),
    };
    scan_json_values(&value, values);
    scan_json_names(&value, names);
    Ok(())
}

fn scan_json_values(value: &serde_json::Value, scanner: &mut SensitiveDataScanner) {
    match value {
        serde_json::Value::String(value) => scanner.push(value.as_bytes()),
        serde_json::Value::Array(values) => {
            for value in values {
                scan_json_values(value, scanner);
            }
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values() {
                scan_json_values(value, scanner);
            }
        }
        value => scanner.push(value.to_string().as_bytes()),
    }
}

fn scan_json_names(value: &serde_json::Value, scanner: &mut SensitiveDataScanner) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                scan_json_names(value, scanner);
            }
        }
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                scanner.push(name.as_bytes());
                scan_json_names(value, scanner);
            }
        }
        _ => {}
    }
}

fn scan_json_value_continuations(
    value: &serde_json::Value,
    scanner: &SensitiveDataScanner,
) -> bool {
    match value {
        serde_json::Value::String(value) => continuation_found(scanner, value.as_bytes()),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| scan_json_value_continuations(value, scanner)),
        serde_json::Value::Object(fields) => fields
            .values()
            .any(|value| scan_json_value_continuations(value, scanner)),
        value => continuation_found(scanner, value.to_string().as_bytes()),
    }
}

fn scan_json_name_continuations(value: &serde_json::Value, scanner: &SensitiveDataScanner) -> bool {
    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| scan_json_name_continuations(value, scanner)),
        serde_json::Value::Object(fields) => fields.iter().any(|(name, value)| {
            continuation_found(scanner, name.as_bytes())
                || scan_json_name_continuations(value, scanner)
        }),
        _ => false,
    }
}

fn continuation_found(scanner: &SensitiveDataScanner, bytes: &[u8]) -> bool {
    let mut continuation = scanner.fork();
    continuation.push(bytes);
    continuation.found()
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
                if super::openai_subscription::durable_reasoning(&reasoning) {
                    parts.push(Part::Reasoning(reasoning));
                    continue;
                }
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
    redactor: &CanaryRedactor,
    allow_summary: bool,
) -> Result<(), LoopError> {
    let metadata = match part {
        Part::Text(part) => {
            part.text = redactor.redact_text(&part.text);
            &mut part.metadata
        }
        Part::Media(part) => {
            part.mime_type = redactor.redact_text(&part.mime_type);
            sanitize_data_ref(&mut part.data, redactor);
            &mut part.metadata
        }
        Part::File(part) => {
            if let Some(name) = &mut part.name {
                *name = redactor.redact_text(name);
            }
            if let Some(mime_type) = &mut part.mime_type {
                *mime_type = redactor.redact_text(mime_type);
            }
            sanitize_data_ref(&mut part.data, redactor);
            &mut part.metadata
        }
        Part::Structured(part) => {
            sanitize_provider_value(&mut part.value, false);
            if let Some(schema) = &mut part.schema {
                sanitize_provider_value(schema, false);
            }
            &mut part.metadata
        }
        Part::ToolCall(part) => {
            sanitize_provider_value(&mut part.input, false);
            if super::openai_subscription::durable_tool_call_metadata(&part.metadata) {
                return Ok(());
            }
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

fn sanitize_part_content(part: &mut Part) {
    match part {
        Part::Text(part) => part.text = REDACTED.to_owned(),
        Part::Media(part) => redact_data_ref(&mut part.data),
        Part::File(part) => redact_data_ref(&mut part.data),
        Part::Structured(part) => part.value = serde_json::Value::String(REDACTED.to_owned()),
        Part::ToolCall(part) => part.input = serde_json::Value::String(REDACTED.to_owned()),
        Part::Reasoning(_) | Part::ToolResult(_) | Part::Custom(_) => {}
    }
}

fn redact_data_ref(data: &mut DataRef) {
    match data {
        DataRef::InlineText(value) | DataRef::Uri(value) => *value = REDACTED.to_owned(),
        DataRef::InlineBytes(value) => *value = REDACTED.as_bytes().to_vec(),
        DataRef::Handle(_) => {}
    }
}

fn sanitize_data_ref(data: &mut DataRef, redactor: &CanaryRedactor) {
    match data {
        DataRef::InlineText(value) | DataRef::Uri(value) => {
            *value = redactor.redact_text(value);
        }
        DataRef::InlineBytes(value) => *value = redactor.redact_bytes(value),
        DataRef::Handle(_) => {}
    }
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

fn validate_order(
    event: &ModelTurnEvent,
    open: &mut HashSet<PartId>,
    order: &mut VecDeque<PartId>,
) -> Result<Option<PartId>, LoopError> {
    match event {
        ModelTurnEvent::Delta(Delta::BeginPart { part_id, .. }) => {
            if !open.insert(part_id.clone()) {
                return Err(provider_error("provider began the same part twice"));
            }
            order.push_back(part_id.clone());
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
            let Some(part_id) = order.pop_front() else {
                return Err(provider_error(
                    "provider committed a part before beginning it",
                ));
            };
            if !open.remove(&part_id) {
                return Err(provider_error("provider committed an unknown part"));
            }
            return Ok(Some(part_id));
        }
        ModelTurnEvent::Finished(_) if !open.is_empty() => {
            return Err(provider_error(
                "provider finished before committing every part",
            ));
        }
        _ => {}
    }
    Ok(None)
}

fn delta_bytes(event: &ModelTurnEvent) -> usize {
    match event {
        ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => chunk.len(),
        ModelTurnEvent::Delta(Delta::AppendBytes { chunk, .. }) => chunk.len(),
        ModelTurnEvent::Delta(Delta::CommitPart {
            part: Part::Text(part),
        }) => part.text.len(),
        _ => 0,
    }
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

fn redact_json(
    value: &mut serde_json::Value,
    redactor: &CanaryRedactor,
    headers: bool,
    structural: bool,
) {
    match value {
        serde_json::Value::String(value) => {
            *value = if structural || protocol_literal(value) {
                redactor.redact_text(value)
            } else {
                redactor.redact_event_text(value)
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json(value, redactor, headers, structural);
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
                    let structural =
                        structural || matches!(name.as_str(), "currency" | "cost_currency");
                    redact_json(
                        value,
                        redactor,
                        name.eq_ignore_ascii_case("headers"),
                        structural,
                    );
                }
            }
        }
        _ => {}
    }
}

fn protocol_literal(value: &str) -> bool {
    crate::telemetry::redact::trusted_event_literal(value)
        || (!value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.'))
        || matches!(
            value,
            "Text"
                | "Media"
                | "File"
                | "Structured"
                | "Reasoning"
                | "ToolCall"
                | "ToolResult"
                | "Custom"
                | "InlineText"
                | "InlineBytes"
                | "Uri"
                | "Handle"
                | "Assistant"
                | "Stop"
                | "Length"
                | "ToolUse"
                | "ContentFilter"
                | "Completed"
                | "MaxTokens"
                | "Blocked"
                | "Error"
                | "Cancelled"
                | "Other"
        )
}

fn provider_error(error: impl std::fmt::Display) -> LoopError {
    LoopError::Provider(format!("bounded provider stream: {error}"))
}

fn commit_error() -> LoopError {
    provider_error("stream commit failed; outcome_unknown")
}
