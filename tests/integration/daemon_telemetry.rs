use std::{
    fs,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use kit::{
    api::service::{CommandObservation, Scheduler},
    domain::events::TraceId,
    runtime::daemon::{Daemon, DaemonConfig, DaemonSignal, TELEMETRY_FILE},
    telemetry::otel::{
        DropPolicy, DurableLocalExporter, ExportBatch, ExportError, Exporter, InstrumentedRuntime,
        Resource, RunEnvelope, SpanName, TelemetryItem, TelemetryReadinessPolicy, TelemetryRuntime,
    },
};

struct Capture(Arc<Mutex<Vec<ExportBatch>>>);

impl Exporter for Capture {
    fn export(&mut self, batch: &ExportBatch) -> Result<(), ExportError> {
        self.0.lock().unwrap().push(batch.clone());
        Ok(())
    }
}

struct FailingExporter;

impl Exporter for FailingExporter {
    fn export(&mut self, _: &ExportBatch) -> Result<(), ExportError> {
        Err(ExportError("offline".to_owned()))
    }
}

#[test]
fn exporter_failure_preserves_records_and_fails_required_readiness() {
    let telemetry = TelemetryRuntime::local(
        Resource::default(),
        &[],
        8,
        DropPolicy::DropNewest,
        FailingExporter,
        TelemetryReadinessPolicy::Required,
    )
    .unwrap();
    telemetry
        .emit(TelemetryItem::RunEnvelope(RunEnvelope::default()))
        .unwrap();
    assert!(telemetry.flush().is_err());
    let health = telemetry.health();
    assert_eq!(health.queued, 1);
    assert!(!health.exporter_healthy);
    assert!(!health.ready);
}

#[test]
fn encrypted_local_sink_discards_oldest_complete_batches_at_its_byte_bound() {
    let root = std::env::temp_dir().join(format!("kit-bounded-telemetry-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("bounded.enc");
    let mut exporter = DurableLocalExporter::open(&path, &[7; 32], 1_024).unwrap();
    let mut batch = ExportBatch::empty(Resource::default());
    batch.run_envelopes.push(RunEnvelope::default());
    for _ in 0..20 {
        exporter.export(&batch).unwrap();
    }
    assert!(fs::metadata(&path).unwrap().len() <= 1_024);
    let retained = exporter.read_batches().unwrap();
    assert!(!retained.is_empty());
    assert!(retained.len() < 20);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn daemon_command_hook_flushes_on_shutdown() {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let telemetry = Arc::new(
        TelemetryRuntime::local(
            Resource::default(),
            &[],
            8,
            DropPolicy::DropNewest,
            Capture(batches.clone()),
            TelemetryReadinessPolicy::Required,
        )
        .unwrap(),
    );
    let runtime = InstrumentedRuntime::new(kit::test_support::noop_runtime(), telemetry.clone());
    let trace_id = TraceId::parse("daemon-run-1").unwrap();
    runtime.command_completed(CommandObservation {
        trace_id: &trace_id,
        operation: "run.start",
        start_unix_nanos: 1,
        end_unix_nanos: 2,
        succeeded: true,
    });

    assert_eq!(telemetry.shutdown().unwrap(), 5);
    assert_eq!(
        batches.lock().unwrap()[0].spans[0].span_name,
        SpanName::ApiCommand
    );
    assert_eq!(
        batches.lock().unwrap()[0].spans[1].span_name,
        SpanName::RunAttempt
    );
    assert!(!telemetry.health().ready);
}

#[tokio::test]
async fn daemon_telemetry_is_encrypted_durable_and_restart_readable() {
    let sequence = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "kit-daemon-telemetry-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("project")).unwrap();
    fs::write(
        root.join("project/README.md"),
        "deterministic native workspace\n",
    )
    .unwrap();
    init_git(&root.join("project"));
    let mut config = DaemonConfig::new(&root)
        .with_development_provider(
            kit::domain::config::Provider::OpenAi,
            kit::agent::executor::FakeResponse::completed("telemetry test provider"),
            kit::agent::executor::FakeScenario::Complete,
        )
        .with_project_root(root.join("project"));
    config.telemetry_readiness = TelemetryReadinessPolicy::Required;
    Daemon::start(config.clone(), DaemonSignal::install().unwrap())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    Daemon::start(config, DaemonSignal::install().unwrap())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();

    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("daemon-identity.json")).unwrap()).unwrap();
    let key = identity["cursor_key"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_u64().unwrap() as u8)
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let path = root.join(TELEMETRY_FILE);
    let raw = fs::read(&path).unwrap();
    assert!(
        !raw.windows(b"daemon lifecycle".len())
            .any(|window| window == b"daemon lifecycle")
    );
    let batches = DurableLocalExporter::open(path, &key, 16 * 1024 * 1024)
        .unwrap()
        .read_batches()
        .unwrap();
    assert_eq!(batches.len(), 2);
    assert!(batches.iter().all(|batch| batch.logs.len() == 1));

    let _ = fs::remove_dir_all(root.with_extension("backups"));
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn daemon_readiness_fails_when_finite_loopback_session_expires() {
    let sequence = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "kit-daemon-auth-expiry-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("project")).unwrap();
    fs::write(
        root.join("project/README.md"),
        "deterministic native workspace\n",
    )
    .unwrap();
    init_git(&root.join("project"));
    let mut config = DaemonConfig::new(&root)
        .with_development_provider(
            kit::domain::config::Provider::OpenAi,
            kit::agent::executor::FakeResponse::completed("telemetry test provider"),
            kit::agent::executor::FakeScenario::Complete,
        )
        .with_project_root(root.join("project"));
    let session_lifetime = Duration::from_secs(1);
    config.auth_session_lifetime = session_lifetime;
    let daemon = Daemon::start(config, DaemonSignal::install().unwrap())
        .await
        .unwrap();
    assert!(daemon.health().is_ready());
    std::thread::sleep(session_lifetime);
    tokio::time::timeout(Duration::from_millis(250), async {
        while daemon.health().is_ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("auth readiness did not expire after its deadline");
    daemon.shutdown().await.unwrap();

    let _ = fs::remove_dir_all(root.with_extension("backups"));
    fs::remove_dir_all(root).unwrap();
}

fn init_git(root: &std::path::Path) {
    if root.join(".git").exists() {
        return;
    }
    for arguments in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Kit Test",
            "-c",
            "user.email=kit@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }
}
