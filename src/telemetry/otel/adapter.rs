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
    redactor: CaptureRedactor<'a>,
    queue: VecDeque<TelemetryItem>,
    capacity: usize,
    drop_policy: DropPolicy,
    dropped: u64,
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
            redactor,
            queue: VecDeque::with_capacity(capacity),
            capacity,
            drop_policy,
            dropped: 0,
        })
    }

    pub fn enqueue(&mut self, mut item: TelemetryItem) -> Result<EnqueueOutcome, AdapterError> {
        item.validate().map_err(AdapterError::InvalidMetric)?;
        item.redact(&self.redactor);
        if self.queue.len() < self.capacity {
            self.queue.push_back(item);
            return Ok(EnqueueOutcome::Accepted);
        }
        self.dropped = self.dropped.saturating_add(1);
        match self.drop_policy {
            DropPolicy::DropNewest => Ok(EnqueueOutcome::DroppedNewest),
            DropPolicy::DropOldest => {
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

    pub(crate) fn pending_batch(&self) -> Option<ExportBatch> {
        (!self.queue.is_empty())
            .then(|| ExportBatch::from_items(self.resource.clone(), self.queue.iter().cloned()))
    }

    pub(crate) fn acknowledge(&mut self, count: usize) {
        self.queue.drain(..count.min(self.queue.len()));
    }

    pub fn flush(&mut self, exporter: &mut dyn Exporter) -> Result<usize, AdapterError> {
        let Some(batch) = self.pending_batch() else {
            return Ok(0);
        };
        let count = self.queue.len();
        batch.validate().map_err(AdapterError::InvalidBatch)?;
        exporter.export(&batch).map_err(AdapterError::Export)?;
        self.acknowledge(count);
        Ok(count)
    }
}
