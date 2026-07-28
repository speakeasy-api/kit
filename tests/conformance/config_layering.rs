use std::collections::BTreeSet;

use kit::capabilities::kernel::identity::{Digest as CapabilityDigest, DigestAlgorithm};
use kit::domain::config::{
    BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigError, ConfigField, ConfigLayer,
    Executor, GRAMMAR_EDIT_EXPERIMENT_VERSION, GrammarEditExperiment, Grant, LayerKind, LayerStack,
    Provider, RetentionLayer, RunConfigContext, RunConfigMaterializer, RunConfigSnapshot,
    StaticRunConfigMaterializer, UnsupportedGrammarEditPolicy,
};
use kit::domain::ids::{PrincipalId, ProjectId, RunId};
use serde_json::json;
use sha2::{Digest, Sha256};

fn grants(values: &[Grant]) -> BTreeSet<Grant> {
    values.iter().copied().collect()
}

fn complete(seed: u32, authority: BTreeSet<Grant>) -> ConfigLayer {
    ConfigLayer {
        schema_version: CONFIG_SCHEMA_VERSION,
        budgets: BudgetLayer {
            max_tokens: Some(u64::from(seed) * 100),
            max_cost_microusd: Some(u64::from(seed) * 1_000),
            max_turns: Some(seed),
        },
        concurrency: ConcurrencyLayer {
            max_runs: Some(seed),
            max_tools: Some(seed + 1),
        },
        retention: RetentionLayer {
            event_days: Some(seed + 2),
            artifact_days: Some(seed + 3),
        },
        provider: Some(if seed.is_multiple_of(2) {
            Provider::OpenAi
        } else {
            Provider::Anthropic
        }),
        executor: Some(if seed.is_multiple_of(2) {
            Executor::RestrictedContainer
        } else {
            Executor::Local
        }),
        grammar_edit: None,
        grants: Some(authority),
    }
}

fn context() -> RunConfigContext {
    RunConfigContext {
        principal_id: PrincipalId::generate().unwrap(),
        project_id: ProjectId::generate().unwrap(),
        run_id: RunId::generate().unwrap(),
    }
}

#[test]
fn production_materializer_uses_the_selected_provider_without_expanding_grants() {
    let authority = grants(&[Grant::ModelCall, Grant::WorkspaceRead]);
    for provider in [Provider::Anthropic, Provider::OpenRouter, Provider::Ollama] {
        let snapshot = StaticRunConfigMaterializer::for_provider(provider)
            .materialize(context(), &authority, None, None)
            .unwrap();
        assert_eq!(snapshot.effective().provider, provider);
        assert_eq!(snapshot.effective_authority(), &authority);
    }
}

fn stack() -> (LayerStack, BTreeSet<Grant>) {
    let authority = grants(&[
        Grant::ModelCall,
        Grant::WorkspaceRead,
        Grant::WorkspaceWrite,
        Grant::ProcessSpawn,
        Grant::NetworkEgress,
    ]);
    let mut built_in = complete(10, authority.clone());
    built_in.grammar_edit = Some(GrammarEditExperiment::default());
    (
        LayerStack {
            built_in,
            user: None,
            project: None,
            run: None,
            experiment: None,
        },
        authority,
    )
}

fn assert_seed(snapshot: &RunConfigSnapshot, seed: u32, source: LayerKind) {
    let config = snapshot.effective();
    assert_eq!(config.max_tokens, u64::from(seed) * 100);
    assert_eq!(config.max_cost_microusd, u64::from(seed) * 1_000);
    assert_eq!(config.max_turns, seed);
    assert_eq!(config.max_concurrent_runs, seed);
    assert_eq!(config.max_concurrent_tools, seed + 1);
    assert_eq!(config.event_retention_days, seed + 2);
    assert_eq!(config.artifact_retention_days, seed + 3);
    assert_eq!(
        config.provider,
        if seed.is_multiple_of(2) {
            Provider::OpenAi
        } else {
            Provider::Anthropic
        }
    );
    assert_eq!(
        config.executor,
        if seed.is_multiple_of(2) {
            Executor::RestrictedContainer
        } else {
            Executor::Local
        }
    );
    for field in ConfigField::ALL
        .into_iter()
        .filter(|field| *field != ConfigField::GrammarEditExperiment)
    {
        assert_eq!(snapshot.provenance()[&field], source, "{}", field.name());
    }
    assert_eq!(
        snapshot.provenance()[&ConfigField::GrammarEditExperiment],
        LayerKind::BuiltIn
    );
}

#[test]
fn every_field_obeys_each_layer_in_exact_precedence_order() {
    let (mut layers, authenticated) = stack();
    let context = context();
    assert_seed(
        &layers.materialize(context, &authenticated).unwrap(),
        10,
        LayerKind::BuiltIn,
    );

    let cases = [
        (LayerKind::User, 9),
        (LayerKind::Project, 8),
        (LayerKind::Run, 7),
        (LayerKind::Experiment, 6),
    ];
    for (kind, seed) in cases {
        let layer = complete(seed, authenticated.clone());
        match kind {
            LayerKind::User => layers.user = Some(layer),
            LayerKind::Project => layers.project = Some(layer),
            LayerKind::Run => layers.run = Some(layer),
            LayerKind::Experiment => layers.experiment = Some(layer),
            LayerKind::BuiltIn => unreachable!(),
        }
        assert_seed(
            &layers.materialize(context, &authenticated).unwrap(),
            seed,
            kind,
        );
    }
}

#[test]
fn partial_layers_preserve_field_level_provenance() {
    let (mut layers, authenticated) = stack();
    let mut user = ConfigLayer::empty();
    user.budgets.max_tokens = Some(900);
    let mut project = ConfigLayer::empty();
    project.concurrency.max_runs = Some(8);
    let mut run = ConfigLayer::empty();
    run.retention.event_days = Some(7);
    let mut experiment = ConfigLayer::empty();
    experiment.provider = Some(Provider::Ollama);
    layers.user = Some(user);
    layers.project = Some(project);
    layers.run = Some(run);
    layers.experiment = Some(experiment);

    let snapshot = layers.materialize(context(), &authenticated).unwrap();
    assert_eq!(
        snapshot.provenance()[&ConfigField::MaxTokens],
        LayerKind::User
    );
    assert_eq!(
        snapshot.provenance()[&ConfigField::MaxConcurrentRuns],
        LayerKind::Project
    );
    assert_eq!(
        snapshot.provenance()[&ConfigField::EventRetentionDays],
        LayerKind::Run
    );
    assert_eq!(
        snapshot.provenance()[&ConfigField::Provider],
        LayerKind::Experiment
    );
    assert_eq!(
        snapshot.provenance()[&ConfigField::Executor],
        LayerKind::BuiltIn
    );
}

#[test]
fn authority_is_authenticated_intersection_and_only_narrows() {
    let (mut layers, _) = stack();
    let authenticated = grants(&[
        Grant::ModelCall,
        Grant::WorkspaceRead,
        Grant::WorkspaceWrite,
    ]);
    let mut user = ConfigLayer::empty();
    user.grants = Some(grants(&[Grant::ModelCall, Grant::WorkspaceRead]));
    let mut project = ConfigLayer::empty();
    project.grants = Some(grants(&[Grant::WorkspaceRead]));
    layers.user = Some(user);
    layers.project = Some(project);

    let snapshot = layers.materialize(context(), &authenticated).unwrap();
    assert_eq!(
        snapshot.effective_authority(),
        &grants(&[Grant::WorkspaceRead])
    );
    assert_eq!(
        snapshot.provenance()[&ConfigField::Grants],
        LayerKind::Project
    );
}

#[test]
fn every_later_layer_grant_expansion_is_rejected() {
    for kind in [
        LayerKind::User,
        LayerKind::Project,
        LayerKind::Run,
        LayerKind::Experiment,
    ] {
        let (mut layers, authenticated) = stack();
        let mut narrowing = ConfigLayer::empty();
        narrowing.grants = Some(grants(&[Grant::WorkspaceRead]));
        let mut expansion = ConfigLayer::empty();
        expansion.grants = Some(grants(&[Grant::WorkspaceRead, Grant::WorkspaceWrite]));
        match kind {
            LayerKind::User => {
                layers.built_in.grants = Some(grants(&[Grant::WorkspaceRead]));
                layers.user = Some(expansion);
            }
            LayerKind::Project => {
                layers.user = Some(narrowing);
                layers.project = Some(expansion);
            }
            LayerKind::Run => {
                layers.user = Some(narrowing);
                layers.run = Some(expansion);
            }
            LayerKind::Experiment => {
                layers.user = Some(narrowing);
                layers.experiment = Some(expansion);
            }
            LayerKind::BuiltIn => unreachable!(),
        }
        assert!(matches!(
            layers.materialize(context(), &authenticated),
            Err(ConfigError::GrantExpansion { layer, .. }) if layer == kind
        ));
    }
}

#[test]
fn canonical_snapshot_reconstructs_identically_three_times() {
    let (mut layers, authenticated) = stack();
    let mut experiment = ConfigLayer::empty();
    experiment.grants = Some(grants(&[Grant::WorkspaceRead, Grant::ModelCall]));
    experiment.provider = Some(Provider::OpenRouter);
    layers.experiment = Some(experiment);
    let original = layers.materialize(context(), &authenticated).unwrap();
    let expected_bytes = original.canonical_bytes();
    let expected_digest = original.digest();

    assert_eq!(Sha256::digest(&expected_bytes).as_slice(), expected_digest);
    assert_eq!(
        CapabilityDigest::of(DigestAlgorithm::Sha256, &expected_bytes).as_bytes(),
        expected_digest
    );
    for _ in 0..3 {
        let reconstructed = RunConfigSnapshot::from_canonical_bytes(&expected_bytes).unwrap();
        assert_eq!(reconstructed.canonical_bytes(), expected_bytes);
        assert_eq!(reconstructed.digest(), expected_digest);
        assert_eq!(reconstructed, original);
    }
}

#[test]
fn canonical_authority_ignores_input_set_order() {
    let (layers, _) = stack();
    let first = [
        Grant::WorkspaceWrite,
        Grant::ModelCall,
        Grant::WorkspaceRead,
    ]
    .into_iter()
    .collect();
    let second = [
        Grant::WorkspaceRead,
        Grant::WorkspaceWrite,
        Grant::ModelCall,
    ]
    .into_iter()
    .collect();
    let context = context();
    let a = layers.materialize(context, &first).unwrap();
    let b = layers.materialize(context, &second).unwrap();
    assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(a.digest(), b.digest());
}

#[test]
fn strict_schema_rejects_unknown_version_type_and_range() {
    let valid = json!({
        "schema_version": CONFIG_SCHEMA_VERSION,
        "budgets": {"max_tokens": 100}
    });
    assert!(serde_json::from_value::<ConfigLayer>(valid.clone()).is_ok());

    let mut unknown = valid.clone();
    unknown["mystery"] = json!(true);
    assert!(serde_json::from_value::<ConfigLayer>(unknown).is_err());
    let mut nested_unknown = valid.clone();
    nested_unknown["budgets"]["mystery"] = json!(1);
    assert!(serde_json::from_value::<ConfigLayer>(nested_unknown).is_err());
    let mut wrong_type = valid;
    wrong_type["budgets"]["max_tokens"] = json!("100");
    assert!(serde_json::from_value::<ConfigLayer>(wrong_type).is_err());

    let (mut layers, authenticated) = stack();
    layers.user = Some(ConfigLayer {
        schema_version: CONFIG_SCHEMA_VERSION + 1,
        ..ConfigLayer::empty()
    });
    assert!(matches!(
        layers.materialize(context(), &authenticated),
        Err(ConfigError::UnsupportedSchemaVersion {
            layer: LayerKind::User,
            found
        }) if found == CONFIG_SCHEMA_VERSION + 1
    ));
    layers.user = Some(ConfigLayer::empty());
    layers.user.as_mut().unwrap().budgets.max_tokens = Some(0);
    assert!(matches!(
        layers.materialize(context(), &authenticated),
        Err(ConfigError::InvalidRange {
            field: ConfigField::MaxTokens,
            value: 0
        })
    ));
}

#[test]
fn built_in_schema_must_be_complete_and_snapshot_is_identity_bound() {
    let (mut layers, authenticated) = stack();
    layers.built_in.executor = None;
    assert!(matches!(
        layers.materialize(context(), &authenticated),
        Err(ConfigError::MissingBuiltInField(ConfigField::Executor))
    ));

    let (layers, authenticated) = stack();
    let context = context();
    let snapshot = layers.materialize(context, &authenticated).unwrap();
    assert_eq!(snapshot.version(), CONFIG_SCHEMA_VERSION);
    assert_eq!(snapshot.principal_id(), context.principal_id);
    assert_eq!(snapshot.project_id(), context.project_id);
    assert_eq!(snapshot.run_id(), context.run_id);
    assert_eq!(snapshot.provenance().len(), ConfigField::ALL.len());
}

#[test]
fn malformed_canonical_snapshots_are_rejected() {
    let (layers, authenticated) = stack();
    let snapshot = layers.materialize(context(), &authenticated).unwrap();
    let bytes = snapshot.canonical_bytes();
    assert!(RunConfigSnapshot::from_canonical_bytes(&bytes[..bytes.len() - 1]).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(RunConfigSnapshot::from_canonical_bytes(&trailing).is_err());
    let mut wrong_version = bytes;
    wrong_version[7] = 3;
    assert!(matches!(
        RunConfigSnapshot::from_canonical_bytes(&wrong_version),
        Err(ConfigError::UnsupportedSnapshotVersion(3))
    ));
}

#[test]
fn grammar_edit_is_versioned_default_off_and_only_run_or_experiment_scoped() {
    let (mut layers, authenticated) = stack();
    let defaults = layers.materialize(context(), &authenticated).unwrap();
    assert!(!defaults.effective().grammar_edit.enabled);
    assert_eq!(
        defaults.effective().grammar_edit.version,
        GRAMMAR_EDIT_EXPERIMENT_VERSION
    );

    let enabled = GrammarEditExperiment {
        version: GRAMMAR_EDIT_EXPERIMENT_VERSION,
        enabled: true,
        unsupported_provider: UnsupportedGrammarEditPolicy::OrdinaryOutput,
    };
    let mut run = ConfigLayer::empty();
    run.grammar_edit = Some(enabled);
    layers.run = Some(run);
    #[cfg(not(debug_assertions))]
    {
        assert_eq!(
            layers.materialize(context(), &authenticated),
            Err(ConfigError::GrammarEditReleaseDisabled)
        );
        return;
    }
    #[cfg(debug_assertions)]
    {
        let snapshot = layers.materialize(context(), &authenticated).unwrap();
        assert_eq!(snapshot.effective().grammar_edit, enabled);
        assert_eq!(
            snapshot.provenance()[&ConfigField::GrammarEditExperiment],
            LayerKind::Run
        );
        assert_ne!(
            snapshot.grammar_edit_experiment_digest(),
            defaults.grammar_edit_experiment_digest()
        );
        assert_eq!(
            snapshot.reference().experiment_identity,
            kit::domain::config::GRAMMAR_EDIT_EXPERIMENT_ID
        );
        assert_eq!(
            snapshot.reference().experiment_digest,
            snapshot.grammar_edit_experiment_digest()
        );

        let mut user = ConfigLayer::empty();
        user.grammar_edit = Some(enabled);
        layers.user = Some(user);
        assert_eq!(
            layers.materialize(context(), &authenticated),
            Err(ConfigError::InvalidExperimentLayer(LayerKind::User))
        );
    }
}
