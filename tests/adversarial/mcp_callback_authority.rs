use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use agentkit_core::{
    CostUsage, FinishReason, Item, ItemKind, MetadataMap, Part, TokenUsage, Usage,
};
use agentkit_loop::ModelTurnResult;
use agentkit_mcp::{McpSamplingResponder, McpServerId};
use kit::{
    agent::{
        adapters::model::ModelSecurity,
        executor::{
            FakeProvider as ExecutorFakeProvider, FakeResponse, durable_sampling_outcomes_for_test,
        },
    },
    api::{
        auth::{
            contract::{Authenticator, GrantSnapshot},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        service::AttemptDriverClaim,
    },
    capabilities::kernel::{
        grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass},
        identity::{
            CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
            CapabilityVersion, Digest, DigestAlgorithm,
        },
    },
    domain::{
        config::{Grant, LayerStack, RunConfigContext},
        ids::{AttemptId, PrincipalId, ProjectId, RunId, WorkspaceId},
        lifecycle::{AttemptOwnership, FencingToken},
        secret::SecretLease,
    },
    protocols::mcp::config::{
        McpOwnerConfig, McpResponderConfig, McpSamplingApprovalMode, McpSamplingResponderConfig,
        McpServerConfig, McpTransportConfig,
    },
    runtime::scheduler::{
        DurableScheduler,
        budget::RunBudget,
        reserve::{BudgetLedger, ReservationStatus},
    },
    test_support,
};
use rmcp::model::{NumberOrString, SamplingMessage};
use tokio_util::sync::CancellationToken;

struct Database {
    root: PathBuf,
    path: PathBuf,
}

impl Database {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-adversarial-mcp-callback-{}-{}",
            std::process::id(),
            RunId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state.sqlite3");
        drop(test_support::open_service_store(&path).unwrap());
        Self { root, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Fixture {
    _database: Database,
    scheduler: DurableScheduler,
    run_id: RunId,
    fake: Arc<ExecutorFakeProvider>,
    responder: Arc<dyn McpSamplingResponder>,
}

impl Fixture {
    fn new(result: ModelTurnResult, secrets: Vec<Arc<SecretLease>>) -> Self {
        let database = Database::new();
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let workspace = WorkspaceId::generate().unwrap();
        let run_id = RunId::generate().unwrap();
        let authority = BTreeSet::from([Grant::ModelCall]);
        let config = LayerStack::safe_defaults()
            .materialize(
                RunConfigContext {
                    principal_id: principal,
                    project_id: project,
                    run_id,
                },
                &authority,
            )
            .unwrap();
        let authenticated = LocalPeerAuthenticator::new(BTreeMap::from([(
            501,
            GrantSnapshot::new(principal, project, authority.clone()),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(501, 42, 501))
        .unwrap();
        let capability = CapabilityIdentity::new(
            CapabilitySource::new("native").unwrap(),
            CapabilityNamespace::new("kit.model").unwrap(),
            CapabilityName::new("call").unwrap(),
            CapabilityVersion::new("1.0.0").unwrap(),
            Digest::of(DigestAlgorithm::Blake3, b"adversarial sampling"),
        );
        let schema = Digest::of(DigestAlgorithm::Sha256, b"sampling schema");
        let constraints = ArgumentConstraints::default();
        let grants = CapabilityGrantSnapshot::new(
            &config,
            [CapabilityGrant::new(
                principal,
                project,
                workspace,
                capability.clone(),
                schema,
                EffectClass::ModelCall,
                constraints.clone(),
            )],
            DigestAlgorithm::Sha256,
        );
        let attempt = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal,
            FencingToken::new(7),
        );
        let scheduler = DurableScheduler::open(database.path()).unwrap();
        scheduler
            .register_run_with_snapshot(run_id, principal, "adversarial-sampling", &config)
            .unwrap();
        scheduler.admit_run(run_id).unwrap();
        let claim = test_support::open_sqlite_store(database.path())
            .unwrap()
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id,
                attempt_id: attempt.attempt_id,
                principal_id: principal,
                fence: attempt.fencing_token,
                lease_version: 1,
                expires_at_unix_micros: 0,
            })
            .unwrap();
        let security = ModelSecurity {
            authenticated,
            config,
            grants,
            delegation: None,
            capability,
            schema_digest: schema,
            argument_constraints: constraints,
            workspace_id: workspace,
            attempt,
            claim,
        };
        let policy = policy();
        let text = result
            .output_items
            .iter()
            .flat_map(|item| &item.parts)
            .find_map(|part| match part {
                Part::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let fake = Arc::new(ExecutorFakeProvider::new(FakeResponse {
            text,
            hidden_reasoning: "private reasoning".to_owned(),
            include_reasoning: false,
            usage: result.usage.unwrap(),
            metadata: result.metadata,
            delay: Duration::ZERO,
        }));
        let outcomes = durable_sampling_outcomes_for_test(
            database.path(),
            Arc::new(Mutex::new(
                test_support::open_service_store(database.path()).unwrap(),
            )),
            scheduler.clone(),
            Arc::clone(&fake),
            security.clone(),
            "configured-server",
            policy.clone(),
            secrets,
        );
        let server = McpServerConfig {
            id: "configured-server".to_owned(),
            transport: McpTransportConfig::Http {
                endpoint: "https://example.invalid/mcp".to_owned(),
            },
            owner: McpOwnerConfig {
                principal_id: principal,
                project_id: project,
                workspace_id: Some(workspace),
            },
            source: "adversarial".to_owned(),
            trust_domain: "test".to_owned(),
            namespace: "test".to_owned(),
            version: "1".to_owned(),
            credential_handle: None,
            credential_scope: None,
            egress: None,
            descriptors: Vec::new(),
            responders: McpResponderConfig {
                sampling: Some(policy),
                ..Default::default()
            },
        };
        let installation = outcomes
            .install_sampling_for_test(
                &server,
                attempt,
                claim,
                scheduler.clone(),
                Arc::new(BudgetLedger::new(RunBudget::from_effective_config(
                    security.config.effective(),
                ))),
            )
            .unwrap();
        installation.arm_for_test();
        let responder = installation.handler_config().sampling.unwrap();
        Self {
            _database: database,
            scheduler,
            run_id,
            fake,
            responder,
        }
    }

    async fn request(
        &self,
        text: &str,
    ) -> Result<agentkit_mcp::McpCreateMessageResult, agentkit_mcp::McpError> {
        self.request_tracked(text).await.0
    }

    async fn request_messages(
        &self,
        messages: Vec<SamplingMessage>,
    ) -> Result<agentkit_mcp::McpCreateMessageResult, agentkit_mcp::McpError> {
        let cancellation = CancellationToken::new();
        self.responder
            .create_message(
                agentkit_mcp::McpCreateMessageRequestParams::new(messages, 32),
                agentkit_mcp::McpResponderRequestContext::new(
                    McpServerId::new("configured-server"),
                    NumberOrString::Number(1),
                    1,
                    move || cancellation.is_cancelled(),
                ),
            )
            .await
    }

    async fn request_tracked(
        &self,
        text: &str,
    ) -> (
        Result<agentkit_mcp::McpCreateMessageResult, agentkit_mcp::McpError>,
        agentkit_mcp::McpResponderRequestContext,
    ) {
        let cancellation = CancellationToken::new();
        let context = agentkit_mcp::McpResponderRequestContext::new(
            McpServerId::new("configured-server"),
            NumberOrString::Number(1),
            1,
            move || cancellation.is_cancelled(),
        );
        let tracker = context.clone();
        let result = self
            .responder
            .create_message(
                agentkit_mcp::McpCreateMessageRequestParams::new(
                    vec![SamplingMessage::user_text(text)],
                    32,
                ),
                context,
            )
            .await;
        (result, tracker)
    }
}

fn policy() -> McpSamplingResponderConfig {
    McpSamplingResponderConfig {
        model_id: "fake-deterministic-v1".to_owned(),
        approval: McpSamplingApprovalMode::None,
        timeout_millis: 5_000,
        max_cost_microusd: 10,
        max_tokens: 64,
        max_messages: 4,
        max_content_items: 2,
        max_content_bytes: 512,
        max_output_bytes: 1_024,
        max_output_content_items: 2,
        max_system_prompt_bytes: 64,
        max_stop_sequences: 2,
        max_stop_sequence_bytes: 8,
        max_temperature: 1.0,
        pricing: Some(kit::protocols::mcp::config::McpSamplingPricingPolicy {
            version: "test-pricing-v1".to_owned(),
            provider: "deterministic-test".to_owned(),
            model: "fake-deterministic-v1".to_owned(),
            tokenizer_bytes_per_token: 4,
            input: kit::agent::accounting::CostRate::new(0, 1),
            cache_read: kit::agent::accounting::CostRate::new(0, 1),
            cache_write: kit::agent::accounting::CostRate::new(0, 1),
            output: kit::agent::accounting::CostRate::new(0, 1),
            reasoning: kit::agent::accounting::CostRate::new(0, 1),
            local_free: true,
        }),
    }
}

fn result(text: &str, cost_microusd: u64) -> ModelTurnResult {
    ModelTurnResult {
        finish_reason: FinishReason::Completed,
        output_items: vec![Item::text(ItemKind::Assistant, text)],
        usage: Some(
            Usage::new(TokenUsage::new(12, 3))
                .with_cost(CostUsage::new(cost_microusd as f64 / 1_000_000.0, "USD")),
        ),
        metadata: MetadataMap::new(),
        model: Some("fake-deterministic-v1".to_owned()),
        response_id: Some("response".to_owned()),
    }
}

#[tokio::test]
async fn production_pipeline_replays_actual_overage_without_delivery_or_redispatch() {
    let fixture = Fixture::new(result("must not be delivered", 11), Vec::new());
    assert!(fixture.request("same request").await.is_err());
    assert!(fixture.request("same request").await.is_err());
    assert_eq!(fixture.fake.dispatch_count(), 1);

    let snapshots = fixture.scheduler.totals(fixture.run_id).unwrap();
    assert_eq!(snapshots.committed.cost_microusd(), 11);
    let connection = rusqlite::Connection::open(fixture._database.path()).unwrap();
    let state: String = connection
        .query_row(
            "SELECT state FROM scheduler_reservations WHERE kind='model'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "actual_overage");
    assert_eq!(
        ReservationStatus::ActualOverage,
        fixture
            .scheduler
            .snapshot(kit::runtime::scheduler::reserve::ReservationId::new(
                u128::from_str_radix(
                    &connection
                        .query_row(
                            "SELECT reservation_id FROM scheduler_reservations WHERE kind='model'",
                            [],
                            |row| row.get::<_, String>(0),
                        )
                        .unwrap(),
                    16,
                )
                .unwrap(),
            ),)
            .unwrap()
            .status()
    );
}

#[tokio::test]
async fn inbound_sampling_secrets_are_rejected_before_provider_or_callback_storage() {
    let secret = Arc::new(SecretLease::new(b"secret-value".to_vec()));
    for (case, messages) in [
        vec![SamplingMessage::user_text("secret-value")],
        vec![SamplingMessage::user_text("c2VjcmV0LXZhbHVl")],
        vec![
            SamplingMessage::user_text("secret-"),
            SamplingMessage::user_text("value"),
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(result("public", 6), vec![Arc::clone(&secret)]);
        let error = fixture.request_messages(messages).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid MCP responder request: MCP responder request rejected"
        );
        assert_eq!(fixture.fake.dispatch_count(), 0);
        assert_eq!(
            fixture
                .scheduler
                .totals(fixture.run_id)
                .unwrap()
                .committed
                .turns(),
            1,
            "secret case {case} was not durably metered"
        );
        let callbacks: u64 = rusqlite::Connection::open(fixture._database.path())
            .unwrap()
            .query_row("SELECT count(*) FROM mcp_callback_projection", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(callbacks, 0);
    }
}

#[tokio::test]
async fn outbound_sampling_secrets_return_the_same_generic_metered_error() {
    let secret = Arc::new(SecretLease::new(b"secret-value".to_vec()));
    let fixture = Fixture::new(result("secret-value", 6), vec![secret]);
    let error = fixture.request("public request").await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid MCP responder request: MCP responder request rejected"
    );
    assert_eq!(fixture.fake.dispatch_count(), 1);
    assert!(
        fixture
            .scheduler
            .totals(fixture.run_id)
            .unwrap()
            .committed
            .turns()
            >= 2
    );
}

#[tokio::test]
async fn production_pipeline_tracks_response_until_transport_delivery_is_known() {
    let fixture = Fixture::new(result("public response", 6), Vec::new());
    let (response, tracker) = fixture.request_tracked("deliver me").await;
    response
        .as_ref()
        .expect("sampling response should be prepared");
    let connection = rusqlite::Connection::open(fixture._database.path()).unwrap();
    let state = || {
        connection
            .query_row("SELECT state FROM mcp_callback_projection", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
    };
    assert_eq!(state(), "response_prepared");
    drop(tracker);
    assert_eq!(state(), "delivery_unknown");
}
