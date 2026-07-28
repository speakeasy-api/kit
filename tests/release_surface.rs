use std::{fs, process::Command};

use kit::domain::config::{
    ConfigError, ConfigLayer, GRAMMAR_EDIT_EXPERIMENT_VERSION, GrammarEditExperiment, Grant,
    LayerStack, RunConfigContext, UnsupportedGrammarEditPolicy,
};

#[test]
fn grammar_edit_experiment_is_off_without_a_release_feature_or_environment_switch() {
    assert!(!kit::domain::config::GrammarEditExperiment::default().enabled);
    let manifest =
        fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .unwrap();
    assert!(!manifest.contains("grammar-edit"));
    let daemon = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime/daemon.rs"),
    )
    .unwrap();
    assert!(!daemon.contains("KIT_GRAMMAR_EDIT"));
}

#[test]
fn release_profile_rejects_explicit_grammar_edit_activation() {
    let mut layers = LayerStack::safe_defaults();
    let mut run = ConfigLayer::empty();
    run.grammar_edit = Some(GrammarEditExperiment {
        version: GRAMMAR_EDIT_EXPERIMENT_VERSION,
        enabled: true,
        unsupported_provider: UnsupportedGrammarEditPolicy::Fail,
    });
    layers.run = Some(run);
    let result = layers.materialize(
        RunConfigContext {
            principal_id: kit::domain::ids::PrincipalId::generate().unwrap(),
            project_id: kit::domain::ids::ProjectId::generate().unwrap(),
            run_id: kit::domain::ids::RunId::generate().unwrap(),
        },
        &std::collections::BTreeSet::from([Grant::ModelCall, Grant::WorkspaceWrite]),
    );
    if cfg!(debug_assertions) {
        assert!(result.is_ok());
    } else {
        assert_eq!(result, Err(ConfigError::GrammarEditReleaseDisabled));
    }
}

#[test]
fn release_api_excludes_control_plane_bypasses_and_test_support() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = std::env::temp_dir().join(format!(
        "kit-release-surface-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(fixture.join("src")).unwrap();
    fs::write(
        fixture.join("Cargo.toml"),
        format!(
            "[package]\nname = \"kit-release-surface\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nkit = {{ path = {:?} }}\n",
            root
        ),
    )
    .unwrap();
    fs::write(
        fixture.join("src/main.rs"),
        r#"
use kit::{
    agent::executor::SelectedModelAdapter,
    api::service::{NoopRuntime, Service, SqliteServiceStore},
    store::{
        backup::{BackupConfig, BackupManager},
        sqlite::{
            append::{AppendCommand, SqliteStore, StoredEvent},
            projection::{ProjectionError, ProjectionStore},
        },
    },
};
fn service<S, A>(store: S, authorizer: A) {
    let _ = Service::new(store, authorizer);
}

fn service_runtime<S, A, R>(store: S, authorizer: A, runtime: R) {
    let _ = Service::with_runtime(store, authorizer, runtime);
}

fn service_store_open() {
    let _ = SqliteServiceStore::open("state.sqlite3");
}

fn raw_open() {
    let _ = SqliteStore::open("state.sqlite3");
}

fn raw_append(store: &mut SqliteStore, command: AppendCommand) {
    let _ = store.append(command);
}

fn projection_open() {
    let _ = ProjectionStore::open("state.sqlite3");
    let _ = kit::store::sqlite::projection::apply_migrations("state.sqlite3");
    let _ = kit::store::sqlite::projection::rollback_latest_migration("state.sqlite3");
    let _ = kit::store::sqlite::projection::MIGRATIONS;
}

fn reduce(_: &mut Vec<u8>, _: &StoredEvent) -> Result<(), ProjectionError> {
    Ok(())
}

fn projection_mutations(store: &mut ProjectionStore) {
    let _ = store.update("name", b"", reduce);
    let _ = store.update_domain();
    let _ = store.rebuild_domain();
    let _ = store.ensure_event_index();
    let _ = store.purge_erased_bytes();
    let _ = store.rebuild("name", b"", reduce);
    let _ = store.update_with_hook("name", b"", false, reduce, |_| false);
    let _ = store.with_store_time(|_, _| Ok(()));
    let _ = store.store_time();
}

fn backup_open(config: BackupConfig) {
    let _ = BackupManager::open(config);
}

fn backup_mutations(manager: &mut BackupManager) {
    let _ = manager.reconcile_generations(1);
    let _ = manager.expire_generations(1);
    let _ = manager.prune_generations();
    manager.record_failure(1, "failure".to_owned());
}

fn provider_bypass() {
    let _ = kit::runtime::daemon::DaemonConfig::new("state")
        .with_development_provider(todo!(), todo!());
}

fn selected_adapter_bypass() {
    let _ = SelectedModelAdapter::for_test;
}

fn grammar_edit_workspace_bypass() {
    let _ = kit::agent::executor::RunExecutorConfig::with_development_edit_workspace;
}

fn deterministic_adapter_bypass() {
    let _ = kit::agent::executor::FakeProvider::new;
}

fn trusted_extension_bypass() {
    let _ = kit::agent::extensions::TrustedExtensionToken::daemon_bootstrap;
}

fn native_dispatch_bypass() {
    let _ = kit::capabilities::native::dispatch::NativeDispatcher::open;
}

fn main() {
    let _ = NoopRuntime;
    let _ = kit::test_support::noop_runtime();
}
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO"))
        .env_remove("CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS")
        .args(["check", "--release", "--offline"])
        .arg("--manifest-path")
        .arg(fixture.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(fixture.join("target"))
        .output()
        .unwrap();
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "release bypass fixture compiled");
    for symbol in [
        "NoopRuntime",
        "Service::new",
        "Service::with_runtime",
        "SqliteServiceStore::open",
        "SqliteStore::open",
        "append",
        "ProjectionStore::open",
        "apply_migrations",
        "rollback_latest_migration",
        "MIGRATIONS",
        "update",
        "update_domain",
        "rebuild_domain",
        "ensure_event_index",
        "purge_erased_bytes",
        "rebuild",
        "update_with_hook",
        "with_store_time",
        "store_time",
        "BackupManager::open",
        "reconcile_generations",
        "expire_generations",
        "prune_generations",
        "record_failure",
        "test_support",
        "with_development_provider",
        "SelectedModelAdapter::for_test",
        "with_development_edit_workspace",
        "FakeProvider",
        "TrustedExtensionToken",
        "NativeDispatcher",
    ] {
        assert!(
            diagnostic.contains(symbol),
            "release diagnostic did not reject {symbol}:\n{diagnostic}"
        );
    }

    fs::remove_dir_all(fixture).unwrap();
}
