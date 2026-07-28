use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::service::SqliteServiceStore;
use crate::store::backup::{BackupGeneration, BackupHealth, BackupManager};

pub trait BackupClock: Send + Sync + 'static {
    fn now_unix_micros(&self) -> i64;
}

#[derive(Default)]
pub struct SystemBackupClock;

impl BackupClock for SystemBackupClock {
    fn now_unix_micros(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_micros()).ok())
            .unwrap_or(i64::MAX)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BackupRuntimeConfig {
    pub interval: Duration,
    pub generation_retention: Duration,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for BackupRuntimeConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60 * 60),
            generation_retention: Duration::from_secs(7 * 24 * 60 * 60),
            request_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub enum BackupRuntimeError {
    InvalidConfiguration(&'static str),
    Startup(String),
    Backup(String),
    Busy,
    Timeout,
    Stopped,
    WorkerPanicked,
}

impl fmt::Display for BackupRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => {
                write!(f, "invalid backup runtime configuration: {message}")
            }
            Self::Startup(message) => write!(f, "backup startup reconciliation failed: {message}"),
            Self::Backup(message) => write!(f, "backup failed: {message}"),
            Self::Busy => f.write_str("backup trigger queue is busy"),
            Self::Timeout => f.write_str("backup operation exceeded its bounded wait"),
            Self::Stopped => f.write_str("backup runtime is stopped"),
            Self::WorkerPanicked => f.write_str("backup worker panicked"),
        }
    }
}

impl std::error::Error for BackupRuntimeError {}

enum WorkerCommand {
    Trigger {
        deadline: Instant,
        reply: mpsc::Sender<Result<BackupGeneration, String>>,
    },
    Shutdown(mpsc::Sender<()>),
}

pub struct BackupRuntime {
    commands: SyncSender<WorkerCommand>,
    worker: Option<JoinHandle<()>>,
    health: Arc<Mutex<BackupHealth>>,
    clock: Arc<dyn BackupClock>,
    config: BackupRuntimeConfig,
    stopped: Arc<AtomicBool>,
}

impl BackupRuntime {
    pub fn start(
        mut manager: BackupManager,
        mut inventory: SqliteServiceStore,
        snapshot_gate: Arc<Mutex<()>>,
        config: BackupRuntimeConfig,
        clock: Arc<dyn BackupClock>,
    ) -> Result<Self, BackupRuntimeError> {
        if config.interval.is_zero() {
            return Err(BackupRuntimeError::InvalidConfiguration(
                "interval must be greater than zero",
            ));
        }
        let generation_retention_micros = i64::try_from(config.generation_retention.as_micros())
            .ok()
            .filter(|retention| *retention > 0)
            .ok_or(BackupRuntimeError::InvalidConfiguration(
                "generation_retention must be a positive i64 microsecond duration",
            ))?;
        if config.request_timeout.is_zero() || config.shutdown_timeout.is_zero() {
            return Err(BackupRuntimeError::InvalidConfiguration(
                "timeouts must be greater than zero",
            ));
        }

        let now = clock.now_unix_micros();
        let generations = manager
            .reconcile_generations(now)
            .map_err(|error| BackupRuntimeError::Startup(error.to_string()))?;
        inventory
            .reconcile_backup_generations(&generations, now)
            .map_err(|error| BackupRuntimeError::Startup(error.to_string()))?;
        manager
            .prune_generations()
            .map_err(|error| BackupRuntimeError::Startup(error.to_string()))?;

        let health = Arc::new(Mutex::new(manager.health(now)));
        let worker_health = Arc::clone(&health);
        let worker_clock = Arc::clone(&clock);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker_snapshot_gate = Arc::clone(&snapshot_gate);
        let (commands, receiver) = mpsc::sync_channel(1);
        let interval = config.interval;
        let worker = thread::Builder::new()
            .name("kit-backup".to_owned())
            .spawn(move || {
                loop {
                    match receiver.recv_timeout(interval) {
                        Ok(WorkerCommand::Trigger { deadline, reply }) => {
                            if Instant::now() >= deadline {
                                let _ = reply.send(Err(BackupRuntimeError::Timeout.to_string()));
                                continue;
                            }
                            let result = run_backup(
                                &mut manager,
                                &mut inventory,
                                worker_clock.as_ref(),
                                &worker_health,
                                generation_retention_micros,
                                &worker_snapshot_gate,
                            );
                            let _ = reply.send(result);
                        }
                        Ok(WorkerCommand::Shutdown(reply)) => {
                            let _ = reply.send(());
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let _ = run_backup(
                                &mut manager,
                                &mut inventory,
                                worker_clock.as_ref(),
                                &worker_health,
                                generation_retention_micros,
                                &worker_snapshot_gate,
                            );
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                worker_stopped.store(true, Ordering::Release);
            })
            .map_err(|error| BackupRuntimeError::Startup(error.to_string()))?;

        Ok(Self {
            commands,
            worker: Some(worker),
            health,
            clock,
            config,
            stopped,
        })
    }

    pub fn trigger(&self) -> Result<BackupGeneration, BackupRuntimeError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(BackupRuntimeError::Stopped);
        }
        let deadline = Instant::now()
            .checked_add(self.config.request_timeout)
            .ok_or(BackupRuntimeError::Timeout)?;
        let (reply, response) = mpsc::channel();
        self.commands
            .try_send(WorkerCommand::Trigger { deadline, reply })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => BackupRuntimeError::Busy,
                mpsc::TrySendError::Disconnected(_) => BackupRuntimeError::Stopped,
            })?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(BackupRuntimeError::Timeout)?;
        response
            .recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => BackupRuntimeError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => BackupRuntimeError::Stopped,
            })?
            .map_err(BackupRuntimeError::Backup)
    }

    pub fn health(&self) -> BackupHealth {
        let mut health = self
            .health
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        health.age_micros = health.last_success.as_ref().map(|success| {
            self.clock
                .now_unix_micros()
                .saturating_sub(success.at_unix_micros)
                .try_into()
                .unwrap_or(0)
        });
        health
    }

    pub fn shutdown(mut self) -> Result<(), BackupRuntimeError> {
        self.stopped.store(true, Ordering::Release);
        let deadline = Instant::now()
            .checked_add(self.config.shutdown_timeout)
            .ok_or(BackupRuntimeError::Timeout)?;
        let (reply, response) = mpsc::channel();
        send_until(&self.commands, WorkerCommand::Shutdown(reply), deadline)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(BackupRuntimeError::Timeout)?;
        response
            .recv_timeout(remaining)
            .map_err(|_| BackupRuntimeError::Timeout)?;
        if self
            .worker
            .take()
            .expect("backup worker exists until shutdown")
            .join()
            .is_err()
        {
            return Err(BackupRuntimeError::WorkerPanicked);
        }
        Ok(())
    }
}

impl Drop for BackupRuntime {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let (reply, _) = mpsc::channel();
        let _ = self.commands.try_send(WorkerCommand::Shutdown(reply));
    }
}

fn run_backup(
    manager: &mut BackupManager,
    inventory: &mut SqliteServiceStore,
    clock: &dyn BackupClock,
    health: &Mutex<BackupHealth>,
    generation_retention_micros: i64,
    snapshot_gate: &Mutex<()>,
) -> Result<BackupGeneration, String> {
    let now = clock.now_unix_micros();
    let result = (|| {
        let _snapshot = snapshot_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let expires_at = now
            .checked_add(generation_retention_micros)
            .ok_or_else(|| "backup generation expiry overflow".to_owned())?;
        manager
            .expire_generations(now)
            .map_err(|error| error.to_string())?;
        inventory
            .expire_backup_generations(now)
            .map_err(|error| error.to_string())?;
        inventory
            .refresh_backup_policy_inventory()
            .map_err(|error| error.to_string())?;
        manager
            .create_backup_until(now, expires_at, inventory)
            .map_err(|error| error.to_string())
    })();
    if let Err(error) = &result {
        manager.record_failure(now, error.clone());
    }
    *health.lock().unwrap_or_else(|error| error.into_inner()) = manager.health(now);
    result.map_err(|error| error.to_string())
}

fn send_until(
    sender: &SyncSender<WorkerCommand>,
    mut command: WorkerCommand,
    deadline: Instant,
) -> Result<(), BackupRuntimeError> {
    loop {
        match sender.try_send(command) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => return Err(BackupRuntimeError::Stopped),
            Err(mpsc::TrySendError::Full(returned)) => command = returned,
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(BackupRuntimeError::Timeout);
        };
        thread::sleep(remaining.min(Duration::from_millis(1)));
    }
}
