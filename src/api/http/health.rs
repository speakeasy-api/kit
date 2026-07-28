use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU8, Ordering},
};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;

use crate::{
    agent::executor::ExecutorHealth,
    runtime::telemetry::{TelemetryHealth, TelemetryRetentionStatus},
    store::backup::BackupHealth,
};

pub const LIVENESS_PATH: &str = "/health/live";
pub const READINESS_PATH: &str = "/health/ready";

const PROCESS_LOOP: u8 = 1 << 0;
const AUTH: u8 = 1 << 1;
const STORE: u8 = 1 << 2;
const LEASE: u8 = 1 << 3;
const STARTUP_RECONCILIATION: u8 = 1 << 4;
const ADMISSION: u8 = 1 << 5;
const SHUTTING_DOWN: u8 = 1 << 6;
const REQUIRED: u8 = PROCESS_LOOP | AUTH | STORE | LEASE | STARTUP_RECONCILIATION | ADMISSION;

type BackupProbe = Arc<dyn Fn() -> BackupHealth + Send + Sync>;
type TelemetryProbe = Arc<dyn Fn() -> TelemetryHealth + Send + Sync>;
type ExecutorProbe = Arc<dyn Fn() -> ExecutorHealth + Send + Sync>;

#[derive(Clone)]
pub struct HealthState {
    flags: Arc<AtomicU8>,
    backup: Arc<RwLock<Option<BackupProbe>>>,
    telemetry: Arc<RwLock<Option<TelemetryProbe>>>,
    executor: Arc<RwLock<Option<ExecutorProbe>>>,
}

impl HealthState {
    pub fn new() -> Self {
        Self {
            flags: Arc::new(AtomicU8::new(PROCESS_LOOP)),
            backup: Arc::new(RwLock::new(None)),
            telemetry: Arc::new(RwLock::new(None)),
            executor: Arc::new(RwLock::new(None)),
        }
    }

    pub fn is_live(&self) -> bool {
        self.flags.load(Ordering::Acquire) & PROCESS_LOOP != 0
    }

    pub fn is_ready(&self) -> bool {
        let flags = self.flags.load(Ordering::Acquire);
        flags & REQUIRED == REQUIRED
            && flags & SHUTTING_DOWN == 0
            && self
                .backup_health()
                .is_none_or(|health| backup_ready(&health))
            && self.telemetry_health().is_none_or(|health| health.ready)
            && self
                .executor_health()
                .is_none_or(|health| health.running && health.accepting)
    }

    pub fn is_shutting_down(&self) -> bool {
        self.flags.load(Ordering::Acquire) & SHUTTING_DOWN != 0
    }

    pub fn set_process_loop_healthy(&self, healthy: bool) {
        self.set(PROCESS_LOOP, healthy);
    }

    pub fn set_auth_ready(&self, ready: bool) {
        self.set(AUTH, ready);
    }

    pub fn set_store_ready(&self, ready: bool) {
        self.set(STORE, ready);
    }

    pub fn set_lease_ready(&self, ready: bool) {
        self.set(LEASE, ready);
    }

    pub fn set_startup_reconciliation_ready(&self, ready: bool) {
        self.set(STARTUP_RECONCILIATION, ready);
    }

    pub fn set_admission_ready(&self, ready: bool) {
        self.set(ADMISSION, ready);
    }

    pub fn install_backup_probe(&self, probe: impl Fn() -> BackupHealth + Send + Sync + 'static) {
        *self
            .backup
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(probe));
    }

    pub fn install_telemetry_probe(
        &self,
        probe: impl Fn() -> TelemetryHealth + Send + Sync + 'static,
    ) {
        *self
            .telemetry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(probe));
    }

    pub fn install_executor_probe(
        &self,
        probe: impl Fn() -> ExecutorHealth + Send + Sync + 'static,
    ) {
        *self
            .executor
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(probe));
    }

    pub fn begin_shutdown(&self) {
        self.flags.fetch_or(SHUTTING_DOWN, Ordering::AcqRel);
    }

    fn set(&self, flag: u8, enabled: bool) {
        if enabled {
            self.flags.fetch_or(flag, Ordering::AcqRel);
        } else {
            self.flags.fetch_and(!flag, Ordering::AcqRel);
        }
    }

    fn backup_health(&self) -> Option<BackupHealth> {
        let probe = self
            .backup
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        probe.map(|probe| probe())
    }

    fn telemetry_health(&self) -> Option<TelemetryHealth> {
        let probe = self
            .telemetry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        probe.map(|probe| probe())
    }

    fn executor_health(&self) -> Option<ExecutorHealth> {
        let probe = self
            .executor
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        probe.map(|probe| probe())
    }
}

impl std::fmt::Debug for HealthState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HealthState")
            .field("live", &self.is_live())
            .field("ready", &self.is_ready())
            .field("shutting_down", &self.is_shutting_down())
            .finish()
    }
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn routes(state: HealthState) -> Router {
    Router::new()
        .route(LIVENESS_PATH, get(liveness))
        .route(READINESS_PATH, get(readiness))
        .with_state(state)
}

#[derive(Serialize)]
struct LivenessResponse {
    live: bool,
    version: &'static str,
}

#[derive(Serialize)]
struct ReadinessResponse {
    ready: bool,
    version: &'static str,
    backup: Option<BackupResponse>,
    telemetry: Option<TelemetryResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    executor: Option<ExecutorResponse>,
}

#[derive(Serialize)]
struct ExecutorResponse {
    running: bool,
    accepting: bool,
    active: usize,
    completed: u64,
    failed: u64,
}

#[derive(Serialize)]
struct BackupResponse {
    ready: bool,
    current_generation: Option<String>,
    last_successful_generation: Option<String>,
    last_successful_at_unix_micros: Option<i64>,
    age_micros: Option<u64>,
    last_error: Option<String>,
}

#[derive(Serialize)]
struct TelemetryResponse {
    ready: bool,
    exporter_healthy: bool,
    retention_healthy: bool,
    queue_healthy: bool,
    retention: &'static str,
    queued: usize,
    dropped: u64,
    last_error: Option<String>,
}

async fn liveness(State(state): State<HealthState>) -> Response {
    let live = state.is_live();
    (
        status(live),
        Json(LivenessResponse {
            live,
            version: env!("CARGO_PKG_VERSION"),
        }),
    )
        .into_response()
}

async fn readiness(State(state): State<HealthState>) -> Response {
    let ready = state.is_ready();
    let backup = state.backup_health().map(|health| BackupResponse {
        ready: backup_ready(&health),
        current_generation: health.current_generation,
        last_successful_generation: health
            .last_success
            .as_ref()
            .map(|success| success.generation.clone()),
        last_successful_at_unix_micros: health
            .last_success
            .as_ref()
            .map(|success| success.at_unix_micros),
        age_micros: health.age_micros,
        last_error: health.last_failure.map(|failure| failure.message),
    });
    let telemetry = state.telemetry_health().map(|health| TelemetryResponse {
        ready: health.ready,
        exporter_healthy: health.exporter_healthy,
        retention_healthy: health.retention_healthy,
        queue_healthy: health.queue_healthy,
        retention: match health.retention_status {
            TelemetryRetentionStatus::DisabledByPolicy => "disabled_by_policy",
            TelemetryRetentionStatus::Encrypted => "encrypted",
        },
        queued: health.queued,
        dropped: health.dropped,
        last_error: health.last_error,
    });
    let executor = state.executor_health().map(|health| ExecutorResponse {
        running: health.running,
        accepting: health.accepting,
        active: health.active,
        completed: health.completed,
        failed: health.failed,
    });
    (
        status(ready),
        Json(ReadinessResponse {
            ready,
            version: env!("CARGO_PKG_VERSION"),
            backup,
            telemetry,
            executor,
        }),
    )
        .into_response()
}

fn backup_ready(health: &BackupHealth) -> bool {
    match (&health.last_success, &health.last_failure) {
        (_, None) => true,
        (Some(success), Some(failure)) => success.at_unix_micros >= failure.at_unix_micros,
        (None, Some(_)) => false,
    }
}

fn status(healthy: bool) -> StatusCode {
    if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
