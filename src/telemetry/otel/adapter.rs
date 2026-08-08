use std::{collections::VecDeque, fmt};

use crate::telemetry::redact::CaptureRedactor;

use super::{ExportBatch, MetricError, Resource, TelemetryItem};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportError(pub String);

impl fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ExportError {}

pub trait Exporter {
    fn export(&mut self, batch: &ExportBatch) -> Result<(), ExportError>;

    fn export_encrypted_learning(
        &mut self,
        _frame: &EncryptedLearningFrame,
    ) -> Result<(), ExportError> {
        Err(ExportError(
            "exporter is not an authenticated learning sink".to_owned(),
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncryptedLearningFrame {
    pub frame_id: String,
    pub ciphertext: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropPolicy {
    DropNewest,
    DropOldest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    Accepted,
    DroppedNewest,
    DroppedOldest,
}

#[derive(Debug)]
pub enum AdapterError {
    ZeroCapacity,
    InvalidMetric(MetricError),
    InvalidBatch(String),
    Export(ExportError),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => formatter.write_str("telemetry queue capacity must be positive"),
            Self::InvalidMetric(error) => write!(formatter, "invalid metric: {error}"),
            Self::InvalidBatch(error) => write!(formatter, "invalid telemetry batch: {error}"),
            Self::Export(error) => write!(formatter, "telemetry export failed: {error}"),
        }
    }
}

impl std::error::Error for AdapterError {}

pub struct Adapter<'a> {
    resource: Resource,
    redactor: AdapterRedactor<'a>,
    queue: VecDeque<TelemetryItem>,
    capacity: usize,
    drop_policy: DropPolicy,
    dropped: u64,
}

enum AdapterRedactor<'a> {
    Static(Box<CaptureRedactor<'a>>),
    Project(crate::domain::secret::SecretCustody),
}

impl<'a> Adapter<'a> {
    pub fn new(
        mut resource: Resource,
        secrets: &'a [crate::domain::secret::SecretLease],
        capacity: usize,
        drop_policy: DropPolicy,
    ) -> Result<Self, AdapterError> {
        if capacity == 0 {
            return Err(AdapterError::ZeroCapacity);
        }
        let redactor = CaptureRedactor::new(secrets);
        super::model::redact_attributes(
            &mut resource.attributes,
            crate::telemetry::redact::CaptureBoundary::Trace,
            &redactor,
        );
        Ok(Self {
            resource,
            redactor: AdapterRedactor::Static(Box::new(redactor)),
            queue: VecDeque::with_capacity(capacity),
            capacity,
            drop_policy,
            dropped: 0,
        })
    }

    pub fn with_custody(
        mut resource: Resource,
        custody: &crate::domain::secret::SecretCustody,
        capacity: usize,
        drop_policy: DropPolicy,
    ) -> Result<Self, AdapterError> {
        if capacity == 0 {
            return Err(AdapterError::ZeroCapacity);
        }
        let redactor = custody.redactor();
        super::model::redact_attributes(
            &mut resource.attributes,
            crate::telemetry::redact::CaptureBoundary::Trace,
            &redactor.capture(),
        );
        Ok(Self {
            resource,
            redactor: AdapterRedactor::Project(custody.clone()),
            queue: VecDeque::with_capacity(capacity),
            capacity,
            drop_policy,
            dropped: 0,
        })
    }

    pub fn enqueue(&mut self, mut item: TelemetryItem) -> Result<EnqueueOutcome, AdapterError> {
        item.validate().map_err(AdapterError::InvalidMetric)?;
        self.redact_item(&mut item);
        if self.queue.len() < self.capacity {
            self.project_batch(self.queue.iter().cloned().chain([item.clone()]))?;
            self.queue.push_back(item);
            return Ok(EnqueueOutcome::Accepted);
        }
        self.dropped = self.dropped.saturating_add(1);
        match self.drop_policy {
            DropPolicy::DropNewest => Ok(EnqueueOutcome::DroppedNewest),
            DropPolicy::DropOldest => {
                self.project_batch(self.queue.iter().skip(1).cloned().chain([item.clone()]))?;
                self.queue.pop_front();
                self.queue.push_back(item);
                Ok(EnqueueOutcome::DroppedOldest)
            }
        }
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub(crate) fn remaining_capacity(&self) -> usize {
        self.capacity.saturating_sub(self.queue.len())
    }

    pub(crate) fn pending_batch(&self) -> Result<Option<ExportBatch>, AdapterError> {
        if self.queue.is_empty() {
            Ok(None)
        } else {
            self.project_batch(self.queue.iter().cloned()).map(Some)
        }
    }

    pub(crate) fn acknowledge(&mut self, count: usize) {
        self.queue.drain(..count.min(self.queue.len()));
    }

    pub fn flush(&mut self, exporter: &mut dyn Exporter) -> Result<usize, AdapterError> {
        let Some(batch) = self.pending_batch()? else {
            return Ok(0);
        };
        let count = self.queue.len();
        batch.validate().map_err(AdapterError::InvalidBatch)?;
        exporter.export(&batch).map_err(AdapterError::Export)?;
        self.acknowledge(count);
        Ok(count)
    }

    fn redact_item(&self, item: &mut TelemetryItem) {
        match &self.redactor {
            AdapterRedactor::Static(redactor) => item.redact(redactor),
            AdapterRedactor::Project(custody) => item.redact(&custody.redactor().capture()),
        }
    }

    fn project_batch(
        &self,
        items: impl IntoIterator<Item = TelemetryItem>,
    ) -> Result<ExportBatch, AdapterError> {
        let mut resource = self.resource.clone();
        let mut items = items.into_iter().collect::<Vec<_>>();
        match &self.redactor {
            AdapterRedactor::Static(redactor) => {
                super::model::redact_attributes(
                    &mut resource.attributes,
                    crate::telemetry::redact::CaptureBoundary::Trace,
                    redactor,
                );
                for item in &mut items {
                    item.redact(redactor);
                }
                scan_projection(redactor, &resource, &items)?;
            }
            AdapterRedactor::Project(custody) => {
                let redactor = custody.redactor();
                let capture = redactor.capture();
                super::model::redact_attributes(
                    &mut resource.attributes,
                    crate::telemetry::redact::CaptureBoundary::Trace,
                    &capture,
                );
                for item in &mut items {
                    item.redact(&capture);
                }
                scan_projection(&capture, &resource, &items)?;
            }
        }
        let batch = ExportBatch::from_items(resource, items);
        let canonical = batch
            .to_canonical_json()
            .map_err(|error| AdapterError::InvalidBatch(error.to_string()))?;
        let mut scanner = match &self.redactor {
            AdapterRedactor::Static(redactor) => redactor.scanner(),
            AdapterRedactor::Project(custody) => custody.redactor().scanner(),
        };
        scanner.push(&canonical);
        if scanner.found() {
            return Err(AdapterError::InvalidBatch(
                "projected telemetry batch reconstructs active secret".to_owned(),
            ));
        }
        Ok(batch)
    }
}

fn scan_projection(
    redactor: &CaptureRedactor<'_>,
    resource: &Resource,
    items: &[TelemetryItem],
) -> Result<(), AdapterError> {
    let mut aggregate = redactor.scanner();
    scan_attributes(&resource.attributes, &mut aggregate);
    for item in items {
        scan_item(item, &mut aggregate)?;
    }
    if aggregate.found() {
        Err(AdapterError::InvalidBatch(
            "projected telemetry batch reconstructs active secret".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn scan_item(
    item: &TelemetryItem,
    aggregate: &mut crate::telemetry::redact::SensitiveDataScanner,
) -> Result<(), AdapterError> {
    match item {
        TelemetryItem::Span(span) => {
            scan_attributes(&span.attributes, aggregate);
            for event in &span.events {
                aggregate.push(event.name.as_bytes());
                scan_attributes(&event.attributes, aggregate);
            }
            if let super::SpanStatus::Error(Some(description)) = &span.status {
                aggregate.push(description.as_bytes());
            }
        }
        TelemetryItem::Metric(metric) => aggregate.push(metric.unit.as_bytes()),
        TelemetryItem::Log(log) => {
            scan_attribute_value(&log.body, aggregate);
            scan_attributes(&log.attributes, aggregate);
        }
        TelemetryItem::RunEnvelope(envelope) => {
            if let Some(usage) = &envelope.model_usage {
                for value in [&usage.provider, &usage.model].into_iter().flatten() {
                    aggregate.push(value.as_bytes());
                }
            }
            if let Some(checks) = &envelope.checks {
                for check in checks {
                    aggregate.push(check.name.as_bytes());
                    if let Some(outcome) = &check.outcome {
                        aggregate.push(outcome.as_bytes());
                    }
                }
            }
            if let Some(errors) = &envelope.errors {
                for error in errors {
                    for value in [&error.class, &error.code, &error.message]
                        .into_iter()
                        .flatten()
                    {
                        aggregate.push(value.as_bytes());
                    }
                }
            }
            if let Some(canonical) = &envelope.canonical {
                scan_value(
                    &serde_json::to_value(canonical)
                        .map_err(|error| AdapterError::InvalidBatch(error.to_string()))?,
                    aggregate,
                );
            }
        }
    }
    Ok(())
}

fn scan_attributes(
    attributes: &std::collections::BTreeMap<String, super::AttributeValue>,
    aggregate: &mut crate::telemetry::redact::SensitiveDataScanner,
) {
    for (name, value) in attributes {
        aggregate.push(name.as_bytes());
        scan_attribute_value(value, aggregate);
    }
}

fn scan_attribute_value(
    value: &super::AttributeValue,
    aggregate: &mut crate::telemetry::redact::SensitiveDataScanner,
) {
    match value {
        super::AttributeValue::String(value) => aggregate.push(value.as_bytes()),
        super::AttributeValue::Array(items) => {
            for item in items {
                scan_attribute_value(item, aggregate);
            }
        }
        super::AttributeValue::Object(fields) => scan_attributes(fields, aggregate),
        super::AttributeValue::Null => aggregate.push(b"null"),
        super::AttributeValue::Bool(value) => aggregate.push(value.to_string().as_bytes()),
        super::AttributeValue::I64(value) => aggregate.push(value.to_string().as_bytes()),
        super::AttributeValue::U64(value) => aggregate.push(value.to_string().as_bytes()),
        super::AttributeValue::F64(value) => aggregate.push(value.to_string().as_bytes()),
    }
}

fn scan_value(
    value: &serde_json::Value,
    aggregate: &mut crate::telemetry::redact::SensitiveDataScanner,
) {
    match value {
        serde_json::Value::String(value) => aggregate.push(value.as_bytes()),
        serde_json::Value::Array(items) => {
            for item in items {
                scan_value(item, aggregate);
            }
        }
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                aggregate.push(name.as_bytes());
                scan_value(value, aggregate);
            }
        }
        value => aggregate.push(value.to_string().as_bytes()),
    }
}
