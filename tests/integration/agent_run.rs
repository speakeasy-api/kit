mod agent_run_tests {
    use std::{
        collections::{BTreeMap, BTreeSet, VecDeque},
        fs, io,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use kit::agent::{
        accounting::UsageEnvelope,
        driver::restart::{RecoveryState, RestartProjection, SafeBoundary},
    };
    use kit::{
        agent::executor::{
            FakeBarrierCheckpoint, FakeProvider, FakeProviderBarrier, FakeResponse, FakeScenario,
            NativeApprovalPolicy, RunExecutor, RunExecutorConfig, SelectedModelAdapter,
        },
        api::{
            auth::{
                contract::{Authenticator, GrantSnapshot, ScopedAuthorizer},
                local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
            },
            service::{
                Command, Query, QueryProjection, RequestContext, RunCompletionRecord,
                RunFailureCode, RunFailureProjection, RunOutputProjection, ServiceStore,
                WorkerStore,
            },
            stream::{
                CursorKey, EventFilter, PumpOutcome, SqliteStreamAdapter, SseFrame, StreamConfig,
            },
        },
        domain::{
            config::{Grant, Provider as ConfigProvider, StaticRunConfigMaterializer},
            events::{ApprovalDecision, AttemptState, RunState, SchemaVersion, TraceId},
            ids::{
                CommandId, EventId, PrincipalId, ProcessId, ProjectId, RunId, ThreadId, WorkspaceId,
            },
            lifecycle::{AttemptOwnership, ProcessClaim, ProcessOwnership},
            secret::SecretLease,
        },
        executor::cancel::{
            CancellationError, CancellationIntent, ExecutorCancellationCoordinator,
            ExecutorCancellationOutcome, SqliteCancellationCoordinator, WorkspaceIdentity,
        },
        executor::process::tree::{BoundaryIdentity, BoundaryKind, Ownership, PersistedBoundary},
        runtime::scheduler::{DurableScheduler, limits::Spend},
        store::{
            artifacts::{
                ArtifactClass, ArtifactMetadata, ArtifactRetention, ArtifactStore, now_unix_micros,
            },
            sqlite::idempotency::IdempotencyKey,
        },
        test_support,
    };

    struct Fixture {
        root: PathBuf,
        database: PathBuf,
        artifacts: Arc<ArtifactStore>,
        store: Arc<std::sync::Mutex<kit::api::service::SqliteServiceStore>>,
        scheduler: DurableScheduler,
        principal_id: PrincipalId,
        project_id: ProjectId,
        run_id: RunId,
    }

    struct TestCancellationCoordinator {
        entered: AtomicBool,
        release: AtomicBool,
        outcome: ExecutorCancellationOutcome,
        calls: AtomicUsize,
    }

    struct MemoryMcpProcess {
        responses: tokio::sync::Mutex<VecDeque<Vec<u8>>>,
        ready: tokio::sync::Notify,
        closed: AtomicBool,
        methods: std::sync::Mutex<Vec<String>>,
    }

    impl MemoryMcpProcess {
        fn new() -> Self {
            Self {
                responses: tokio::sync::Mutex::new(VecDeque::new()),
                ready: tokio::sync::Notify::new(),
                closed: AtomicBool::new(false),
                methods: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn methods(&self) -> Vec<String> {
            self.methods.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl kit::protocols::mcp::transport::OwnedStdioProcess for MemoryMcpProcess {
        async fn send_frame(&self, frame: &[u8]) -> io::Result<()> {
            let request: serde_json::Value =
                serde_json::from_slice(frame).map_err(io::Error::other)?;
            let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
                return Ok(());
            };
            self.methods.lock().unwrap().push(method.to_owned());
            let Some(id) = request.get("id").cloned() else {
                return Ok(());
            };
            let result = match method {
                "initialize" => serde_json::json!({
                    "protocolVersion":"2025-11-25",
                    "capabilities":{"tools":{}},
                    "serverInfo":{"name":"learning-fixture","version":"1"}
                }),
                "tools/list" => serde_json::json!({"tools":[mcp_tool_descriptor()]}),
                "resources/list" => serde_json::json!({"resources":[]}),
                "resources/templates/list" => serde_json::json!({"resourceTemplates":[]}),
                "prompts/list" => serde_json::json!({"prompts":[]}),
                "tools/call" => serde_json::json!({
                    "content":[{"type":"text","text":"fixture result"}],
                    "isError":false
                }),
                _ => serde_json::json!({}),
            };
            self.responses.lock().await.push_back(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc":"2.0", "id":id, "result":result
                }))
                .map_err(io::Error::other)?,
            );
            self.ready.notify_waiters();
            Ok(())
        }

        async fn receive_frame(&self) -> io::Result<Option<Vec<u8>>> {
            loop {
                let notified = self.ready.notified();
                if let Some(response) = self.responses.lock().await.pop_front() {
                    return Ok(Some(response));
                }
                if self.closed.load(Ordering::Acquire) {
                    return Ok(None);
                }
                notified.await;
            }
        }

        async fn close_and_reap(&self) -> io::Result<()> {
            self.closed.store(true, Ordering::Release);
            self.ready.notify_waiters();
            Ok(())
        }
    }

    struct MemoryMcpProfiles(Arc<MemoryMcpProcess>);

    impl kit::protocols::mcp::transport::OwnedStdioProfileProvider for MemoryMcpProfiles {
        fn prepare(
            &self,
            _profile: &str,
            _owner: AttemptOwnership,
            _authorized_credentials: &Arc<
                BTreeMap<kit::domain::secret::SecretHandle, Arc<SecretLease>>,
            >,
            _executable: &kit::protocols::mcp::config::StdioExecutableIdentity,
        ) -> Result<
            kit::protocols::mcp::transport::SandboxedStdioLauncher,
            kit::protocols::mcp::transport::OwnedStdioProfileError,
        > {
            Ok(kit::protocols::mcp::transport::SandboxedStdioLauncher::for_test(self.0.clone()))
        }
    }

    fn mcp_tool_descriptor() -> serde_json::Value {
        serde_json::json!({
            "name":"fixture_echo",
            "description":"Echo fixture text.",
            "inputSchema":{
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "additionalProperties":false,
                "properties":{"text":{"type":"string"}},
                "required":["text"],
                "type":"object"
            }
        })
    }

    impl TestCancellationCoordinator {
        fn new(outcome: ExecutorCancellationOutcome, released: bool) -> Self {
            Self {
                entered: AtomicBool::new(false),
                release: AtomicBool::new(released),
                outcome,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl ExecutorCancellationCoordinator for TestCancellationCoordinator {
        fn cancel_attempt(
            &self,
            _authority: AttemptOwnership,
        ) -> Result<ExecutorCancellationOutcome, CancellationError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(self.outcome)
        }
    }

    impl Fixture {
        fn new() -> Self {
            Self::new_for_provider(ConfigProvider::OpenAi)
        }

        fn new_for_provider(provider: ConfigProvider) -> Self {
            Self::new_for_provider_with_grants(provider, &[])
        }

        fn new_for_provider_with_grants(provider: ConfigProvider, extra_grants: &[Grant]) -> Self {
            let root = std::env::temp_dir().join(format!(
                "kit-agent-run-{}-{}",
                std::process::id(),
                EventId::generate().unwrap()
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join("project")).unwrap();
            fs::write(
                root.join("project/README.md"),
                "deterministic native workspace\n",
            )
            .unwrap();
            init_git(&root.join("project"));
            let database = root.join("state.sqlite3");
            let artifact_root = root.join("artifacts");
            let artifacts = Arc::new(ArtifactStore::open(&artifact_root).unwrap());
            let principal_id = PrincipalId::generate().unwrap();
            let project_id = ProjectId::generate().unwrap();
            let thread_id = ThreadId::generate().unwrap();
            let run_id = RunId::generate().unwrap();
            let grants = [
                Grant::ModelCall,
                Grant::WorkspaceRead,
                Grant::WorkspaceWrite,
                Grant::ProcessSpawn,
                Grant::NetworkEgress,
            ]
            .into_iter()
            .chain(extra_grants.iter().copied())
            .collect::<Vec<_>>();
            let principal = LocalPeerAuthenticator::new(BTreeMap::from([(
                1000,
                GrantSnapshot::new(principal_id, project_id, grants),
            )]))
            .authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
            .unwrap();
            let input = artifacts
                .put(
                    b"return a deterministic answer",
                    ArtifactMetadata::new(
                        "text/plain",
                        ArtifactClass::File,
                        principal_id.to_string(),
                        project_id.to_string(),
                        ArtifactRetention::Forever,
                        now_unix_micros().unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
            let mut service = test_support::service_with_runtime_and_config(
                test_support::open_service_store(&database).unwrap(),
                ScopedAuthorizer,
                ArtifactStore::open(&artifact_root).unwrap(),
                StaticRunConfigMaterializer::for_provider(provider),
            );
            for (key, command) in [
                (
                    "project",
                    Command::CreateProject {
                        schema_version: SchemaVersion::CURRENT,
                        project_id,
                    },
                ),
                (
                    "thread",
                    Command::CreateThread {
                        schema_version: SchemaVersion::CURRENT,
                        thread_id,
                        project_id,
                    },
                ),
                (
                    "run",
                    Command::StartRun {
                        schema_version: SchemaVersion::CURRENT,
                        run_id,
                        thread_id,
                        input: input.digest().to_string().parse().unwrap(),
                        run_config: None,
                        experiment_config: None,
                        effective_config: None,
                    },
                ),
            ] {
                let context = RequestContext::authenticated(
                    Ok(principal.clone()),
                    Some(IdempotencyKey::parse(key).unwrap()),
                    TraceId::parse(key).unwrap(),
                )
                .unwrap();
                service.execute(&context, command).unwrap();
            }
            let store = Arc::new(std::sync::Mutex::new(service.into_store()));
            let scheduler = DurableScheduler::open(&database).unwrap();
            Self {
                root,
                database,
                artifacts,
                store,
                scheduler,
                principal_id,
                project_id,
                run_id,
            }
        }

        fn config(&self, provider: Arc<FakeProvider>) -> RunExecutorConfig {
            let mut config = RunExecutorConfig::new(
                &self.database,
                Arc::clone(&self.artifacts),
                Arc::clone(&self.store),
                self.scheduler.clone(),
                SelectedModelAdapter::for_test(ConfigProvider::OpenAi, provider),
            );
            config.poll_interval = Duration::from_millis(5);
            config = config.with_project_root(self.root.join("project"));
            config
        }

        fn context(&self, key: &str) -> RequestContext {
            let principal = LocalPeerAuthenticator::new(BTreeMap::from([(
                1000,
                GrantSnapshot::new(
                    self.principal_id,
                    self.project_id,
                    [
                        Grant::WorkspaceRead,
                        Grant::WorkspaceWrite,
                        Grant::ModelCall,
                    ],
                ),
            )]))
            .authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000));
            RequestContext::authenticated(
                principal,
                Some(IdempotencyKey::parse(key).unwrap()),
                TraceId::parse(key).unwrap(),
            )
            .unwrap()
        }

        async fn wait_for(&self, expected: RunState) {
            let mut actual = RunState::Queued;
            for _ in 0..10_000 {
                let projection = self
                    .store
                    .lock()
                    .unwrap()
                    .query(&Query::GetRun {
                        run_id: self.run_id,
                    })
                    .unwrap();
                let QueryProjection::Run(run) = projection else {
                    panic!("unexpected projection")
                };
                actual = run.state;
                if actual == expected {
                    return;
                }
                if actual.is_terminal() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!(
                "run reached {actual:?}, not {expected:?}: {}",
                self.event_json()
            );
        }

        fn event_json(&self) -> String {
            let events = self
                .store
                .lock()
                .unwrap()
                .worker_append_store()
                .unwrap()
                .events()
                .unwrap();
            serde_json::to_string(
                &events
                    .iter()
                    .map(|event| {
                        (
                            event.event.event_type.as_str(),
                            String::from_utf8_lossy(&event.event.payload),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        }

        fn durable_bytes(&self) -> Vec<u8> {
            fn append(path: &std::path::Path, output: &mut Vec<u8>) {
                if path.is_dir() {
                    for entry in fs::read_dir(path).unwrap() {
                        append(&entry.unwrap().path(), output);
                    }
                } else if let Ok(bytes) = fs::read(path) {
                    output.extend(bytes);
                }
            }

            let mut bytes = Vec::new();
            append(&self.root, &mut bytes);
            bytes
        }

        fn execute(&self, key: &str, command: Command) {
            let grants = [
                Grant::ModelCall,
                Grant::WorkspaceRead,
                Grant::WorkspaceWrite,
                Grant::ProcessSpawn,
                Grant::NetworkEgress,
            ];
            let principal = LocalPeerAuthenticator::new(BTreeMap::from([(
                1000,
                GrantSnapshot::new(self.principal_id, self.project_id, grants),
            )]))
            .authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
            .unwrap();
            let context = RequestContext::authenticated(
                Ok(principal),
                Some(IdempotencyKey::parse(key).unwrap()),
                TraceId::parse(key).unwrap(),
            )
            .unwrap();
            let mut service = test_support::service_with_runtime(
                test_support::open_service_store(&self.database).unwrap(),
                ScopedAuthorizer,
                ArtifactStore::open(self.root.join("artifacts")).unwrap(),
            );
            service.execute(&context, command).unwrap();
        }

        fn input_artifact(&self, value: &str) -> kit::domain::events::ArtifactRef {
            self.artifacts
                .put(
                    value.as_bytes(),
                    ArtifactMetadata::new(
                        "text/plain; charset=utf-8",
                        ArtifactClass::File,
                        self.principal_id.to_string(),
                        self.project_id.to_string(),
                        ArtifactRetention::Forever,
                        now_unix_micros().unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap()
                .digest()
                .to_string()
                .parse()
                .unwrap()
        }

        fn completion(&self, value: &str) -> RunCompletionRecord {
            RunCompletionRecord {
                output: RunOutputProjection {
                    artifact: self.input_artifact(value),
                    preview: value.to_owned(),
                    status: "complete".to_owned(),
                },
                item_preview: serde_json::json!({"preview": value}),
                usage: UsageEnvelope::default(),
                cost: None,
                telemetry_digest: "test".to_owned(),
            }
        }
    }

    fn init_git(root: &std::path::Path) {
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

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopdriver_commits_completion_progress_usage_and_cost() {
        let fixture = Fixture::new();
        let adapter = SqliteStreamAdapter::new(
            &fixture.database,
            CursorKey::new([0x91; 32]),
            StreamConfig {
                buffer_capacity: 64,
                schema_version: 1,
            },
        )
        .unwrap();
        let follow_context = fixture.context("run-follow");
        let mut follower = adapter
            .open(
                &follow_context,
                fixture.project_id,
                EventFilter::run(fixture.run_id),
                None,
            )
            .unwrap();
        let mut response = FakeResponse::completed("public answer");
        response.metadata = agentkit_core::MetadataMap::from([
            (
                "headers".into(),
                serde_json::json!({"authorization": "HEADER_ONLY_CANARY"}),
            ),
            ("error".into(), serde_json::json!("Q0FOQVJZX1NFQ1JFVA==")),
        ]);
        let provider = Arc::new(
            FakeProvider::new(response)
                .with_secret_leases([Arc::new(SecretLease::new(b"CANARY_SECRET".to_vec()))]),
        );
        let executor = RunExecutor::start(fixture.config(Arc::clone(&provider))).unwrap();
        let mut progress = executor.subscribe();
        executor.notify();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();

        let persisted = fixture.event_json();
        assert!(persisted.contains("model_call.intent"));
        assert!(persisted.contains("model_call.outcome"));
        assert!(persisted.contains("public answer"));
        assert!(persisted.contains("input_tokens"));
        assert!(persisted.contains("0.000006"));
        assert!(persisted.contains("deterministic-test"));
        assert!(!persisted.contains("SECRET_CHAIN_OF_THOUGHT"));
        let outcome: Vec<u8> = rusqlite::Connection::open(&fixture.database)
            .unwrap()
            .query_row(
                "SELECT outcome FROM provider_streams WHERE outcome IS NOT NULL LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let outcome: agentkit_loop::ModelTurnResult = serde_json::from_slice(&outcome).unwrap();
        assert_eq!(
            outcome
                .usage
                .and_then(|usage| usage.tokens)
                .and_then(|tokens| tokens.reasoning_tokens),
            Some(3)
        );
        let durable_bytes = fixture.durable_bytes();
        let durable = String::from_utf8_lossy(&durable_bytes);
        for private in [
            "SECRET_CHAIN_OF_THOUGHT",
            "CANARY_SECRET",
            "Q0FOQVJZX1NFQ1JFVA==",
            "HEADER_ONLY_CANARY",
        ] {
            assert!(!durable.contains(private), "durable state leaked {private}");
        }
        assert_eq!(provider.dispatch_count(), 1);
        assert!(progress.try_recv().is_ok());
        assert_eq!(executor.health().completed, 1);

        let mut streamed = Vec::new();
        loop {
            let outcome = follower.pump().unwrap();
            assert!(!matches!(outcome, PumpOutcome::Disconnected { .. }));
            while let Some(frame) = follower.next_frame() {
                let SseFrame::Semantic {
                    id,
                    operation,
                    data,
                } = frame
                else {
                    panic!("run follower received a non-semantic frame")
                };
                let data: serde_json::Value = serde_json::from_slice(&data).unwrap();
                if matches!(operation.as_str(), "run.progress" | "run.output") {
                    let payload = &data["payload"];
                    for internal in [
                        "run_id",
                        "project_id",
                        "attempt",
                        "stored_at_unix_micros",
                        "record",
                    ] {
                        assert!(payload.get(internal).is_none());
                    }
                }
                streamed.push((id, operation, data));
            }
            if matches!(outcome, PumpOutcome::Ready { queued: 0 }) {
                break;
            }
        }
        assert!(
            streamed
                .iter()
                .any(|(_, operation, _)| operation == "run.progress")
        );
        assert_eq!(
            streamed
                .iter()
                .filter(|(_, operation, _)| operation == "run.output")
                .count(),
            1
        );
        assert_eq!(
            streamed
                .iter()
                .map(|(cursor, _, _)| cursor.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            streamed.len()
        );
        let output = streamed
            .iter()
            .find(|(_, operation, _)| operation == "run.output")
            .unwrap();
        assert_eq!(output.2["payload"]["output"]["preview"], "public answer");
        let cursor = streamed.last().unwrap().0.clone();
        assert_eq!(follower.last_durable_cursor(), cursor);
        let mut resumed = adapter
            .open(
                &follow_context,
                fixture.project_id,
                EventFilter::run(fixture.run_id),
                Some(&cursor),
            )
            .unwrap();
        assert_eq!(resumed.pump().unwrap(), PumpOutcome::Ready { queued: 0 });
        assert_eq!(resumed.next_frame(), None);

        let mut store = fixture.store.lock().unwrap();
        let QueryProjection::Run(run) = store
            .query(&Query::GetRun {
                run_id: fixture.run_id,
            })
            .unwrap()
        else {
            panic!("unexpected run projection")
        };
        assert_eq!(run.output.unwrap().preview, "public answer");
        let QueryProjection::RunCost(cost) = store
            .query(&Query::GetRunCost {
                run_id: fixture.run_id,
            })
            .unwrap()
        else {
            panic!("unexpected cost projection")
        };
        let usage = cost.usage.unwrap();
        assert_eq!(usage.categories.uncached_input.billed_tokens, Some(4));
        assert_eq!(usage.categories.visible_output.billed_tokens, Some(2));
        assert_eq!(usage.provider_cost.unwrap().amount.micros, 6);
        assert_eq!(cost.cost.unwrap().effective.unwrap().micros, 6);
        let QueryProjection::RunPrompts(prompt) = store
            .query(&Query::GetRunPrompts {
                run_id: fixture.run_id,
            })
            .unwrap()
        else {
            panic!("unexpected prompt projection")
        };
        assert!(prompt.first_dynamic_byte.unwrap() < prompt.context_bytes.unwrap());
        assert!(prompt.estimated_tokens.unwrap() <= prompt.token_budget.unwrap());
        let QueryProjection::RunTranscript(transcript) = store
            .query(&Query::RunTranscript {
                run_id: fixture.run_id,
            })
            .unwrap()
        else {
            panic!("unexpected transcript projection")
        };
        let transcript = serde_json::to_string(&transcript).unwrap();
        assert!(transcript.contains("public answer"));
        assert!(!transcript.contains("SECRET_CHAIN_OF_THOUGHT"));
        let QueryProjection::Events(timeline) = store
            .query(&Query::RunTimeline {
                run_id: fixture.run_id,
                after: kit::api::service::EventCursor::START,
                limit: 100,
                opaque_cursor: None,
            })
            .unwrap()
        else {
            panic!("unexpected timeline projection")
        };
        let operations = timeline
            .events
            .iter()
            .map(|event| event.operation.as_str())
            .collect::<Vec<_>>();
        assert!(operations.contains(&"run.progress"));
        assert!(operations.contains(&"run.output"));
        assert!(
            operations.iter().position(|value| *value == "run.progress")
                < operations.iter().position(|value| *value == "run.output")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unavailable_effective_provider_is_a_visible_typed_run_failure() {
        let fixture = Fixture::new_for_provider(ConfigProvider::Anthropic);
        let provider = Arc::new(FakeProvider::new(FakeResponse::completed(
            "must not dispatch",
        )));
        let executor = RunExecutor::start(fixture.config(Arc::clone(&provider))).unwrap();
        executor.notify();
        fixture.wait_for(RunState::Failed).await;
        executor.shutdown().await.unwrap();

        let QueryProjection::Run(run) = fixture
            .store
            .lock()
            .unwrap()
            .query(&Query::GetRun {
                run_id: fixture.run_id,
            })
            .unwrap()
        else {
            panic!("unexpected projection")
        };
        let failure = run.failure.expect("failed run exposes its reason");
        assert_eq!(failure.code, RunFailureCode::ProviderUnavailable);
        assert!(failure.detail.contains("Anthropic"));
        assert!(fixture.event_json().contains("run.failure"));
        assert_eq!(provider.dispatch_count(), 0);
    }

    #[test]
    fn run_projection_rejects_mixed_output_and_failure() {
        let output_first = Fixture::new();
        let mut store = output_first.store.lock().unwrap();
        let job = store
            .claim_queued_run(Duration::from_secs(30))
            .unwrap()
            .unwrap();
        store
            .publish_run_completion(job.run.id, job.claim, output_first.completion("completed"))
            .unwrap();
        assert!(
            store
                .fail_worker_run(
                    job.run.id,
                    job.claim,
                    RunFailureProjection {
                        code: RunFailureCode::ExecutionFailed,
                        detail: "late failure".to_owned(),
                    },
                )
                .is_err()
        );
        let QueryProjection::Run(run) = store.query(&Query::GetRun { run_id: job.run.id }).unwrap()
        else {
            panic!("unexpected projection")
        };
        assert!(run.output.is_some());
        assert!(run.failure.is_none());
        drop(store);

        let failure_first = Fixture::new();
        let mut store = failure_first.store.lock().unwrap();
        let job = store
            .claim_queued_run(Duration::from_secs(30))
            .unwrap()
            .unwrap();
        let first = RunFailureProjection {
            code: RunFailureCode::ExecutionFailed,
            detail: "sanitized first failure".to_owned(),
        };
        store
            .fail_worker_run(job.run.id, job.claim, first.clone())
            .unwrap();
        rusqlite::Connection::open(&failure_first.database)
            .unwrap()
            .execute(
                "UPDATE attempt_driver_claims SET expires_at_unix_micros = 0 WHERE run_id = ?1",
                [job.run.id.to_string()],
            )
            .unwrap();
        store
            .fail_worker_run(
                job.run.id,
                job.claim,
                RunFailureProjection {
                    detail: "different retry detail".to_owned(),
                    ..first.clone()
                },
            )
            .unwrap();
        assert!(
            store
                .publish_run_completion(
                    job.run.id,
                    job.claim,
                    failure_first.completion("late output"),
                )
                .is_err()
        );
        let QueryProjection::Run(run) = store.query(&Query::GetRun { run_id: job.run.id }).unwrap()
        else {
            panic!("unexpected projection")
        };
        assert_eq!(run.failure, Some(first));
        assert!(run.output.is_none());
        assert_eq!(run.state, RunState::Failed);
        assert_eq!(
            store
                .worker_append_store()
                .unwrap()
                .events()
                .unwrap()
                .iter()
                .filter(|event| event.event.event_type.as_str() == "run.failure")
                .count(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_executors_dispatch_one_durable_attempt() {
        for _ in 0..10 {
            let mut cases = Vec::with_capacity(10);
            for _ in 0..10 {
                let fixture = Fixture::new();
                let mut response = FakeResponse::completed("one owner");
                response.delay = Duration::from_millis(20);
                let provider = Arc::new(FakeProvider::new(response));
                let first = RunExecutor::start(fixture.config(Arc::clone(&provider))).unwrap();
                let second = RunExecutor::start(fixture.config(Arc::clone(&provider))).unwrap();
                first.notify();
                second.notify();
                cases.push((fixture, provider, first, second));
            }

            for (fixture, provider, first, second) in cases {
                fixture.wait_for(RunState::Completed).await;
                first.shutdown().await.unwrap();
                second.shutdown().await.unwrap();
                assert_eq!(provider.dispatch_count(), 1);
                assert_eq!(first.health().completed + second.health().completed, 1);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn expired_owner_cannot_publish_after_live_failover() {
        let fixture = Fixture::new();
        let barrier = FakeProviderBarrier::new(
            fixture.root.join("claim-renewal-barrier"),
            FakeBarrierCheckpoint::BeforeClaimRenewal,
        );
        let mut response = FakeResponse::completed("stale result");
        response.delay = Duration::from_secs(6);
        let provider = Arc::new(FakeProvider::with_scenario(
            response,
            FakeScenario::Barrier(barrier.clone()),
        ));
        let mut first_config = fixture.config(Arc::clone(&provider));
        first_config.lease_duration = Duration::from_secs(3);
        first_config.claim_renewal_interval = Duration::from_millis(500);
        let first = RunExecutor::start(first_config).unwrap();
        fixture.wait_for(RunState::Running).await;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if barrier.reached_path().exists() && provider.dispatch_count() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first owner reached its suspended claim renewal");

        let second = RunExecutor::start(fixture.config(Arc::clone(&provider))).unwrap();
        second.notify();
        fixture.wait_for(RunState::Failed).await;
        fs::write(barrier.release_path(), b"release").unwrap();
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();

        let connection = rusqlite::Connection::open(&fixture.database).unwrap();
        let published: i64 = connection
            .query_row("SELECT count(*) FROM provider_stream_chunks", [], |row| {
                row.get(0)
            })
            .unwrap();
        let outcomes: i64 = connection
            .query_row(
                "SELECT count(*) FROM provider_streams WHERE outcome IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(published, 0);
        assert_eq!(outcomes, 0);
        assert_eq!(provider.dispatch_count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claim_heartbeat_renews_expiry_without_changing_identity() {
        let fixture = Fixture::new();
        let mut response = FakeResponse::completed("renewed owner");
        response.delay = Duration::from_secs(6);
        let provider = Arc::new(FakeProvider::new(response));
        let mut config = fixture.config(provider);
        config.lease_duration = Duration::from_secs(3);
        config.claim_renewal_interval = Duration::from_millis(500);
        let executor = RunExecutor::start(config).unwrap();
        fixture.wait_for(RunState::Running).await;
        let initial = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap()
            .claim;
        let renewed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let claim = fixture
                    .store
                    .lock()
                    .unwrap()
                    .worker_run(fixture.run_id)
                    .unwrap()
                    .claim;
                if claim.expires_at_unix_micros > initial.expires_at_unix_micros {
                    break claim;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("claim expiry was renewed");
        assert!(renewed.same_lease(initial));
        assert_eq!(renewed.fence, initial.fence);
        assert_eq!(renewed.lease_version, initial.lease_version);
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();
    }

    #[test]
    fn executor_rejects_unsafe_claim_renewal_intervals() {
        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::new(FakeResponse::completed("unused")));

        for interval in [Duration::from_millis(1), Duration::from_secs(5)] {
            let mut config = fixture.config(Arc::clone(&provider));
            config.claim_renewal_interval = interval;
            assert!(RunExecutor::start(config).is_err());
        }

        let mut config = fixture.config(provider);
        config.claim_renewal_interval = Duration::from_secs(2);
        assert!(RunExecutor::start(config).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_reaches_terminal_state_without_exposing_hidden_reasoning() {
        let fixture = Fixture::new();
        let mut response = FakeResponse::completed("too late");
        response.delay = Duration::from_secs(2);
        let provider = Arc::new(FakeProvider::new(response));
        let executor = RunExecutor::start(fixture.config(provider)).unwrap();
        fixture.wait_for(RunState::Running).await;
        {
            let run = fixture
                .store
                .lock()
                .unwrap()
                .worker_run(fixture.run_id)
                .unwrap();
            fixture
                .store
                .lock()
                .unwrap()
                .transition_worker_run(run.run.id, RunState::Cancelling)
                .unwrap();
        }
        executor.notify();
        fixture.wait_for(RunState::Cancelled).await;
        executor.shutdown().await.unwrap();
        assert!(!fixture.event_json().contains("SECRET_CHAIN_OF_THOUGHT"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn model_only_attempt_registers_explicit_no_process_state() {
        let fixture = Fixture::new();
        let executor = RunExecutor::start(
            fixture.config(Arc::new(FakeProvider::new(FakeResponse::completed("done")))),
        )
        .unwrap();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();
        let attempt = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap()
            .attempt;
        let state: String = rusqlite::Connection::open(&fixture.database)
            .unwrap()
            .query_row(
                "SELECT state FROM executor_attempt_boundaries WHERE attempt_id=?1",
                [attempt.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "no_process");
    }

    #[test]
    fn missing_boundary_claim_is_unknown_and_unknown_recovery_never_requeues() {
        let fixture = Fixture::new();
        let job = fixture
            .store
            .lock()
            .unwrap()
            .claim_queued_run(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let coordinator = SqliteCancellationCoordinator::new(&fixture.database);
        assert_eq!(
            coordinator.cancel_attempt(job.attempt.owner).unwrap(),
            ExecutorCancellationOutcome::OutcomeUnknown
        );

        coordinator.register_no_process(job.attempt.owner).unwrap();
        rusqlite::Connection::open(&fixture.database)
            .unwrap()
            .execute(
                "UPDATE executor_attempt_boundaries SET state='outcome_unknown'
                 WHERE attempt_id=?1",
                [job.attempt.id.to_string()],
            )
            .unwrap();
        rusqlite::Connection::open(&fixture.database)
            .unwrap()
            .execute(
                "UPDATE attempt_driver_claims SET expires_at_unix_micros=0 WHERE run_id=?1",
                [fixture.run_id.to_string()],
            )
            .unwrap();
        let mut store = fixture.store.lock().unwrap();
        assert!(store.recoverable_runs(10).unwrap().is_empty());
        assert!(
            store
                .claim_recoverable_run(fixture.run_id, Duration::from_secs(1))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_gate_precedes_terminal_state_and_scheduler_finish() {
        let fixture = Fixture::new();
        let mut response = FakeResponse::completed("too late");
        response.delay = Duration::from_secs(2);
        let coordinator = Arc::new(TestCancellationCoordinator::new(
            ExecutorCancellationOutcome::Quiescent,
            false,
        ));
        let mut config = fixture.config(Arc::new(FakeProvider::new(response)));
        config.cancellation_coordinator = coordinator.clone();
        let executor = RunExecutor::start(config).unwrap();
        fixture.wait_for(RunState::Running).await;
        let run = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap();
        fixture
            .store
            .lock()
            .unwrap()
            .transition_worker_run(run.run.id, RunState::Cancelling)
            .unwrap();
        executor.notify();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !coordinator.entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let blocked = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap();
        assert_eq!(blocked.run.state, RunState::Cancelling);
        assert_eq!(blocked.attempt.state, AttemptState::Quiescing);
        let scheduler_phase: String = rusqlite::Connection::open(&fixture.database)
            .unwrap()
            .query_row(
                "SELECT phase FROM scheduler_runs WHERE run_id=?1",
                [fixture.run_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!matches!(scheduler_phase.as_str(), "terminal" | "canceled"));

        coordinator.release.store(true, Ordering::Release);
        fixture.wait_for(RunState::Cancelled).await;
        executor.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_cancellation_keeps_attempt_run_and_scheduler_held() {
        let fixture = Fixture::new();
        let mut response = FakeResponse::completed("too late");
        response.delay = Duration::from_secs(2);
        let coordinator = Arc::new(TestCancellationCoordinator::new(
            ExecutorCancellationOutcome::OutcomeUnknown,
            true,
        ));
        let mut config = fixture.config(Arc::new(FakeProvider::new(response)));
        config.cancellation_coordinator = coordinator.clone();
        let executor = RunExecutor::start(config).unwrap();
        fixture.wait_for(RunState::Running).await;
        let run = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap();
        fixture
            .store
            .lock()
            .unwrap()
            .transition_worker_run(run.run.id, RunState::Cancelling)
            .unwrap();
        executor.notify();
        tokio::time::timeout(Duration::from_secs(10), async {
            while coordinator.calls.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;

        let blocked = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap();
        assert_eq!(blocked.run.state, RunState::Cancelling);
        assert_eq!(blocked.attempt.state, AttemptState::Quiescing);
        let scheduler_phase: String = rusqlite::Connection::open(&fixture.database)
            .unwrap()
            .query_row(
                "SELECT phase FROM scheduler_runs WHERE run_id=?1",
                [fixture.run_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!matches!(scheduler_phase.as_str(), "terminal" | "canceled"));
        executor.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_registered_claim_is_durably_unknown_and_held() {
        let fixture = Fixture::new();
        let mut response = FakeResponse::completed("too late");
        response.delay = Duration::from_secs(2);
        let executor =
            RunExecutor::start(fixture.config(Arc::new(FakeProvider::new(response)))).unwrap();
        fixture.wait_for(RunState::Running).await;
        let run = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap();
        let process = ProcessClaim::new(
            ProcessId::generate().unwrap(),
            ProcessOwnership::Attempt(run.attempt.owner),
        );
        let boundary = PersistedBoundary {
            ownership: Ownership::new(
                serde_json::to_string(&process.owner).unwrap(),
                process.process_id.to_string(),
            )
            .unwrap(),
            // Process groups cannot be reconstructed after daemon loss, so the
            // production adapter must fail closed rather than claim quiescence.
            identity: BoundaryIdentity::new(
                BoundaryKind::MacOsProcessGroup,
                "production-unknown",
                "a".repeat(64),
                "start-token",
            )
            .unwrap(),
        };
        let intent = CancellationIntent::new(
            CommandId::generate().unwrap(),
            run.attempt.owner,
            process,
            boundary,
            WorkspaceIdentity::new(
                WorkspaceId::generate().unwrap(),
                "production-acquisition",
                "production-revision",
            )
            .unwrap(),
            Duration::from_millis(1),
        )
        .unwrap();
        SqliteCancellationCoordinator::new(&fixture.database)
            .register_claim(&intent)
            .unwrap();
        fixture
            .store
            .lock()
            .unwrap()
            .transition_worker_run(run.run.id, RunState::Cancelling)
            .unwrap();
        executor.notify();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let phase = rusqlite::Connection::open(&fixture.database)
                    .unwrap()
                    .query_row(
                        "SELECT phase FROM executor_cancellations WHERE request_id=?1",
                        [intent.request_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .ok();
                if phase.as_deref() == Some("unknown") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let blocked = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap();
        assert_eq!(blocked.run.state, RunState::Cancelling);
        assert_eq!(blocked.attempt.state, AttemptState::Quiescing);
        let connection = rusqlite::Connection::open(&fixture.database).unwrap();
        let claims: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM executor_execution_claims WHERE attempt_id=?1",
                [run.attempt.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let operations: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM executor_cancellation_operations WHERE request_id=?1",
                [intent.request_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(claims, 1);
        assert_eq!(operations, 4);
        executor.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generated_journal_projects_every_restart_boundary() {
        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::new(FakeResponse::completed("restartable")));
        let executor = RunExecutor::start(fixture.config(provider)).unwrap();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();

        let job = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap();
        let events = fixture
            .store
            .lock()
            .unwrap()
            .worker_append_store()
            .unwrap()
            .events()
            .unwrap();
        let journal_positions = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.event.event_type.as_str() == "agent.effect_journal")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let states = journal_positions
            .into_iter()
            .map(|index| RestartProjection::reconstruct(&job.attempt, &events[..=index]).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            &states[0],
            RecoveryState::Ready(plan) if plan.snapshot.boundary == SafeBoundary::BeforeModelDispatch
        ));
        let RecoveryState::Ready(initial) = &states[0] else {
            unreachable!()
        };
        assert_eq!(initial.snapshot.transcript.len(), 1);
        assert_eq!(
            initial.snapshot.transcript[0].kind,
            kit::agent::agentkit_bridge::mapping::CanonicalItemKind::User
        );
        let prompt = serde_json::to_string(&initial.snapshot.transcript).unwrap();
        assert_eq!(prompt.matches("return a deterministic answer").count(), 1);
        assert!(
            states
                .iter()
                .any(|state| matches!(state, RecoveryState::OutcomeUnknown(_)))
        );
        assert!(states.iter().any(|state| matches!(
            state,
            RecoveryState::Ready(plan) if plan.snapshot.boundary == SafeBoundary::AfterModelOutcome
        )));
        assert!(matches!(
            states.last().unwrap(),
            RecoveryState::Ready(plan) if plan.snapshot.boundary == SafeBoundary::TurnEnd
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn budget_exhaustion_fails_before_provider_dispatch() {
        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::new(FakeResponse::completed("over budget")));
        let mut config = fixture.config(Arc::clone(&provider));
        config.model_reservation = Spend::new(0, 1_000_001, 1, 0, 0);
        let executor = RunExecutor::start(config).unwrap();
        fixture.wait_for(RunState::Failed).await;
        executor.shutdown().await.unwrap();
        assert_eq!(provider.dispatch_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_mcp_deferred_learning_path_records_search_inspect_bind_call_error_outcome()
    {
        let fixture = Fixture::new();
        let process = Arc::new(MemoryMcpProcess::new());
        let identity =
            kit::protocols::mcp::features::ConfiguredServerIdentity::new("learning-fixture")
                .unwrap();
        let descriptor_digest = kit::protocols::mcp::features::decode_tools_page(
            &identity,
            serde_json::to_vec(&serde_json::json!({
                "jsonrpc":"2.0", "id":1, "result":{"tools":[mcp_tool_descriptor()]}
            }))
            .unwrap(),
            kit::protocols::mcp::features::PayloadLimits::default(),
        )
        .unwrap()
        .items()[0]
            .normalize()
            .unwrap()
            .descriptor_digest();
        let platform = if cfg!(target_os = "windows") {
            kit::executor::profile::Platform::Windows
        } else if cfg!(target_os = "macos") {
            kit::executor::profile::Platform::MacOs
        } else {
            kit::executor::profile::Platform::Linux
        };
        let architecture = if cfg!(target_arch = "aarch64") {
            kit::executor::profile::Architecture::Aarch64
        } else {
            kit::executor::profile::Architecture::X86_64
        };
        let profile = kit::executor::profile::ProfileSpec::isolated(
            kit::executor::profile::TrustTier::Restricted,
            platform,
            architecture,
            kit::executor::profile::ResourceLimits::new(
                10_000,
                256 * 1024 * 1024,
                16,
                16 * 1024 * 1024,
                64 * 1024 * 1024,
                64 * 1024 * 1024,
                16 * 1024 * 1024,
                30_000,
            ),
        );
        let profile_digest = kit::executor::profile::ExecutorProfile::new(profile.clone())
            .unwrap()
            .digest()
            .to_string();
        let server = kit::protocols::mcp::config::McpServerConfig {
            id: "learning-fixture".to_owned(),
            transport: kit::protocols::mcp::config::McpTransportConfig::Stdio {
                owned_process_profile: "memory".to_owned(),
                argv: vec![
                    std::env::current_exe()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                ],
                profile: Box::new(profile),
                profile_digest,
                environment: BTreeMap::new(),
            },
            owner: kit::protocols::mcp::config::McpOwnerConfig {
                principal_id: fixture.principal_id,
                project_id: fixture.project_id,
                workspace_id: None,
            },
            source: "mcp.learning-fixture".to_owned(),
            trust_domain: "local".to_owned(),
            namespace: "fixture".to_owned(),
            version: "1".to_owned(),
            credential_handle: None,
            credential_scope: None,
            egress: None,
            descriptors: vec![kit::protocols::mcp::config::McpDescriptorPolicyConfig {
                kind: kit::capabilities::catalog::CapabilityKind::Tool,
                remote: "fixture_echo".to_owned(),
                descriptor_digest,
                effect: kit::capabilities::kernel::grant::EffectClass::ProcessSpawn,
                retry_safety: kit::capabilities::kernel::invoke::RetrySafety::Idempotent,
                required_grants: BTreeSet::from([Grant::ProcessSpawn]),
                auth_scopes: BTreeSet::new(),
                availability: kit::capabilities::catalog::Availability::Available,
            }],
            responders: Default::default(),
        };
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("tool complete"),
            FakeScenario::DeferredMcp,
        ));
        let executor = RunExecutor::start(
            fixture
                .config(Arc::clone(&provider))
                .with_tool_learning_key([41; 32])
                .with_mcp_servers([server])
                .with_mcp_stdio_profiles(Arc::new(MemoryMcpProfiles(Arc::clone(&process)))),
        )
        .unwrap();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();

        let persisted = fixture.event_json();
        assert!(persisted.contains("tools_search"));
        assert!(persisted.contains("tools_inspect"));
        assert!(persisted.contains("tools_bind"));
        assert!(persisted.contains("tools_invoke"));
        assert!(persisted.contains("capability.invocation_intent"));
        assert!(persisted.contains("capability.invocation_outcome"));
        assert!(persisted.contains("after_tool_outcome"));
        assert_eq!(provider.dispatch_count(), 6);
        let store = test_support::open_sqlite_store(&fixture.database).unwrap();
        let learning = kit::telemetry::tool_learning::records(
            &store,
            fixture.run_id,
            &kit::telemetry::tool_learning::ProjectPointerHasher::new(
                fixture.project_id,
                &[41; 32],
            ),
        )
        .unwrap();
        let opportunities = learning
            .iter()
            .filter_map(|event| match event {
                kit::telemetry::tool_learning::ToolLearningEvent::Opportunity {
                    common,
                    offered,
                    candidates,
                    ..
                } => Some((common, *offered, candidates)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(opportunities.iter().all(|(_, offered, candidates)| {
            *offered <= kit::telemetry::tool_learning::MAX_LEARNING_CANDIDATES
                && candidates.len() == usize::from(*offered)
                && candidates
                    .iter()
                    .all(|candidate| candidate.authorized && candidate.offered)
        }));
        assert!(opportunities.iter().any(|(_, _, candidates)| {
            candidates.iter().any(|candidate| {
                candidate.surface == kit::telemetry::tool_learning::LearningSurface::Eager
            })
        }));
        assert!(opportunities.iter().any(|(_, _, candidates)| {
            candidates
                .iter()
                .filter(|candidate| {
                    candidate.surface == kit::telemetry::tool_learning::LearningSurface::Generic
                })
                .count()
                == 4
        }));
        let learning_hasher =
            kit::telemetry::tool_learning::ProjectPointerHasher::new(fixture.project_id, &[41; 32]);
        let generic_capabilities = opportunities
            .iter()
            .flat_map(|(_, _, candidates)| candidates.iter())
            .filter(|candidate| {
                candidate.surface == kit::telemetry::tool_learning::LearningSurface::Generic
            })
            .map(|candidate| candidate.capability.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            generic_capabilities,
            [
                "tools.search",
                "tools.inspect",
                "tools.bind",
                "tools.invoke",
            ]
            .map(|operation| {
                learning_hasher.pointer(
                    kit::telemetry::tool_learning::PointerDomain::Capability,
                    operation.as_bytes(),
                )
            })
            .into_iter()
            .collect()
        );
        assert_eq!(
            learning
                .iter()
                .map(kit::telemetry::tool_learning::ToolLearningEvent::class_name)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "opportunity",
                "search",
                "inspection",
                "call",
                "error",
                "outcome",
            ])
        );
        assert!(learning.iter().any(|event| matches!(
            event,
            kit::telemetry::tool_learning::ToolLearningEvent::Search {
                status: kit::telemetry::tool_learning::LearningStatus::Succeeded,
                result_count: 1,
                ..
            }
        )));
        assert!(learning.iter().any(|event| matches!(
            event,
            kit::telemetry::tool_learning::ToolLearningEvent::Inspection {
                common,
                status: kit::telemetry::tool_learning::LearningStatus::Succeeded,
                ..
            } if common.operation == kit::telemetry::tool_learning::LearningOperation::Inspect
        )));
        assert!(learning.iter().any(|event| matches!(
            event,
            kit::telemetry::tool_learning::ToolLearningEvent::Inspection {
                common,
                status: kit::telemetry::tool_learning::LearningStatus::Succeeded,
                ..
            } if common.operation == kit::telemetry::tool_learning::LearningOperation::Bind
        )));
        assert!(learning.iter().any(|event| matches!(
            event,
            kit::telemetry::tool_learning::ToolLearningEvent::Call {
                common,
                binding: Some(_),
                source: Some(_),
                kind: Some(_),
                sequence: Some(_),
                kernel_intent: Some(_),
                ..
            } if common.capability.is_some() && common.schema.is_some() && common.request.is_some()
        )));
        assert!(learning.iter().any(|event| matches!(
            event,
            kit::telemetry::tool_learning::ToolLearningEvent::Error {
                stage: kit::telemetry::tool_learning::ErrorStage::SchemaValidation,
                code: kit::telemetry::tool_learning::ErrorCode::InvalidSchema,
                dispatched: false,
                known: true,
                ..
            }
        )));
        assert!(learning.iter().any(|event| matches!(
            event,
            kit::telemetry::tool_learning::ToolLearningEvent::Outcome {
                status: kit::telemetry::tool_learning::LearningStatus::Succeeded,
                dispatched: true,
                known: true,
                ..
            }
        )));
        let run =
            kit::telemetry::tool_learning::ProjectPointerHasher::new(fixture.project_id, &[41; 32])
                .pointer(
                    kit::telemetry::tool_learning::PointerDomain::Run,
                    fixture.run_id.to_string().as_bytes(),
                );
        assert_eq!(store.catalog_stats(run.as_str()).unwrap()[0].succeeded, 1);
        assert_eq!(
            process
                .methods()
                .iter()
                .filter(|method| method.as_str() == "tools/call")
                .count(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn required_learning_failure_preserves_completed_provider_result_and_blocks_admission() {
        struct OrdinaryOnly;

        impl kit::telemetry::otel::Exporter for OrdinaryOnly {
            fn export(
                &mut self,
                _: &kit::telemetry::otel::ExportBatch,
            ) -> Result<(), kit::telemetry::otel::ExportError> {
                Ok(())
            }
        }

        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("provider result survives learning failure"),
            FakeScenario::Tool,
        ));
        let telemetry = Arc::new(
            kit::telemetry::otel::TelemetryRuntime::encrypted_local(
                kit::telemetry::otel::Resource::default(),
                &[],
                32,
                kit::telemetry::otel::DropPolicy::DropNewest,
                OrdinaryOnly,
                kit::telemetry::otel::TelemetryReadinessPolicy::Required,
            )
            .unwrap(),
        );
        let mut config = fixture
            .config(Arc::clone(&provider))
            .with_tool_learning_key([43; 32]);
        config.telemetry = Some(Arc::clone(&telemetry));
        let executor = RunExecutor::start(config).unwrap();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();

        let QueryProjection::Run(run) = fixture
            .store
            .lock()
            .unwrap()
            .query(&Query::GetRun {
                run_id: fixture.run_id,
            })
            .unwrap()
        else {
            panic!("unexpected projection")
        };
        assert_eq!(run.state, RunState::Completed);
        assert_eq!(
            run.output.unwrap().preview,
            "provider result survives learning failure"
        );
        assert!(!telemetry.health().learning_healthy);
        assert!(!telemetry.learning_admission_ready());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn best_effort_learning_failure_preserves_completed_provider_result_and_admission() {
        struct OrdinaryOnly;

        impl kit::telemetry::otel::Exporter for OrdinaryOnly {
            fn export(
                &mut self,
                _: &kit::telemetry::otel::ExportBatch,
            ) -> Result<(), kit::telemetry::otel::ExportError> {
                Ok(())
            }
        }

        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("provider result survives best-effort learning failure"),
            FakeScenario::Tool,
        ));
        let telemetry = Arc::new(
            kit::telemetry::otel::TelemetryRuntime::encrypted_local(
                kit::telemetry::otel::Resource::default(),
                &[],
                32,
                kit::telemetry::otel::DropPolicy::DropNewest,
                OrdinaryOnly,
                kit::telemetry::otel::TelemetryReadinessPolicy::BestEffort,
            )
            .unwrap(),
        );
        let mut config = fixture
            .config(Arc::clone(&provider))
            .with_tool_learning_key([44; 32]);
        config.telemetry = Some(Arc::clone(&telemetry));
        let executor = RunExecutor::start(config).unwrap();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();

        let QueryProjection::Run(run) = fixture
            .store
            .lock()
            .unwrap()
            .query(&Query::GetRun {
                run_id: fixture.run_id,
            })
            .unwrap()
        else {
            panic!("unexpected projection")
        };
        assert_eq!(run.state, RunState::Completed);
        assert_eq!(
            run.output.unwrap().preview,
            "provider result survives best-effort learning failure"
        );
        assert!(!telemetry.health().learning_healthy);
        assert!(telemetry.learning_admission_ready());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_native_schema_failure_commits_learning_triple_before_dispatch() {
        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("invalid tool handled"),
            FakeScenario::ToolInvalid,
        ));
        let executor = RunExecutor::start(
            fixture
                .config(Arc::clone(&provider))
                .with_tool_learning_key([42; 32]),
        )
        .unwrap();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();

        let store = test_support::open_sqlite_store(&fixture.database).unwrap();
        let records = kit::telemetry::tool_learning::records(
            &store,
            fixture.run_id,
            &kit::telemetry::tool_learning::ProjectPointerHasher::new(
                fixture.project_id,
                &[42; 32],
            ),
        )
        .unwrap();
        let call = records.iter().find_map(|event| match event {
            kit::telemetry::tool_learning::ToolLearningEvent::Call { call, .. } => {
                Some(call.clone())
            }
            _ => None,
        });
        assert!(call.is_some());
        let hasher =
            kit::telemetry::tool_learning::ProjectPointerHasher::new(fixture.project_id, &[42; 32]);
        assert!(records.iter().any(|event| match event {
            kit::telemetry::tool_learning::ToolLearningEvent::Error {
                common,
                call: found,
                stage: kit::telemetry::tool_learning::ErrorStage::SchemaValidation,
                code: kit::telemetry::tool_learning::ErrorCode::InvalidSchema,
                field: Some(field),
                dispatched: false,
                known: true,
                ..
            } =>
                Some(found) == call.as_ref()
                    && common.schema.as_ref().is_some_and(|schema| {
                        field
                            == &hasher.pointer(
                                kit::telemetry::tool_learning::PointerDomain::Field,
                                format!("{}:", schema.as_str()).as_bytes(),
                            )
                    }),
            _ => false,
        }));
        assert!(records.iter().any(|event| matches!(
            event,
            kit::telemetry::tool_learning::ToolLearningEvent::Outcome {
                call: found,
                dispatched: false,
                known: true,
                kernel_outcome: None,
                ..
            } if Some(found) == call.as_ref()
        )));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn workspace_acquisition_failure_before_agent_route_has_zero_native_effects() {
        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("must not dispatch"),
            FakeScenario::Tool,
        ));
        let config = fixture
            .config(Arc::clone(&provider))
            .with_project_root(fixture.root.join("missing-project"));
        let executor = RunExecutor::start(config).unwrap();
        fixture.wait_for(RunState::Failed).await;
        executor.shutdown().await.unwrap();

        assert_eq!(provider.dispatch_count(), 0);
        let persisted = fixture.event_json();
        assert!(!persisted.contains("capability.invocation_intent"));
        assert!(!persisted.contains("capability.invocation_outcome"));
        assert!(!persisted.contains("model_call.intent"));
        assert_eq!(
            fs::read_to_string(fixture.root.join("project/README.md")).unwrap(),
            "deterministic native workspace\n"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_edit_stops_at_durable_approval_before_materialization() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root.join("project/src")).unwrap();
        let source = fixture.root.join("project/src/lib.rs");
        fs::write(&source, "pub mod existing;\n").unwrap();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("native coding complete"),
            FakeScenario::NativeCoding,
        ));
        let executor = RunExecutor::start(fixture.config(Arc::clone(&provider))).unwrap();
        fixture.wait_for(RunState::WaitingForApproval).await;
        executor.shutdown().await.unwrap();

        assert_eq!(fs::read_to_string(source).unwrap(), "pub mod existing;\n");
        let persisted = fixture.event_json();
        for tool in ["kit_discover", "kit_read", "kit_edit"] {
            assert!(persisted.contains(tool));
        }
        assert!(persisted.contains("tool_approval_requested"));
        assert!(!persisted.contains("diff_artifact"));
        assert_eq!(provider.dispatch_count(), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_approval_policy_dispatches_native_writes_without_parking() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root.join("project/src")).unwrap();
        let source = fixture.root.join("project/src/lib.rs");
        fs::write(&source, "pub mod existing;\n").unwrap();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("native coding complete"),
            FakeScenario::NativeCoding,
        ));
        let config = fixture
            .config(Arc::clone(&provider))
            .with_native_approval_policy(NativeApprovalPolicy::Auto);
        let executor = RunExecutor::start(config).unwrap();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();

        let persisted = fixture.event_json();
        assert!(persisted.contains("kit_edit"));
        assert!(!persisted.contains("tool_approval_requested"));
        assert!(!persisted.contains("waiting_for_approval"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_wait_resolves_durably_and_survives_executor_restart() {
        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("need more"),
            FakeScenario::Input,
        ));
        let executor = RunExecutor::start(fixture.config(Arc::clone(&provider))).unwrap();
        fixture.wait_for(RunState::WaitingForInput).await;
        executor.shutdown().await.unwrap();

        let run = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap();
        fixture.execute(
            "resolve-input",
            Command::ProvideRunInput {
                schema_version: SchemaVersion::CURRENT,
                run_id: fixture.run_id,
                input: fixture.input_artifact("resolved input"),
                expected_version: run.run.version,
            },
        );
        let restarted = RunExecutor::start(fixture.config(Arc::clone(&provider))).unwrap();
        let mut progress = restarted.subscribe();
        loop {
            tokio::select! {
                event = progress.recv() => {
                    if let Ok(kit::agent::executor::ProgressEvent {
                        event: agentkit_loop::AgentEvent::RunFailed { message },
                        ..
                    }) = event {
                        panic!("resumed input run failed: {message}");
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    let run = fixture.store.lock().unwrap().worker_run(fixture.run_id).unwrap();
                    if run.run.state == RunState::Completed {
                        break;
                    }
                }
            }
        }
        restarted.shutdown().await.unwrap();
        assert!(fixture.event_json().contains("waiting_resolved"));
        assert_eq!(provider.dispatch_count(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approval_and_auth_resolutions_resume_real_waiting_paths() {
        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("approved"),
            FakeScenario::Approval,
        ));
        let executor = RunExecutor::start(
            fixture
                .config(Arc::clone(&provider))
                .with_tool_learning_key([44; 32]),
        )
        .unwrap();
        fixture.wait_for(RunState::WaitingForApproval).await;
        let QueryProjection::Approvals(approvals) = fixture
            .store
            .lock()
            .unwrap()
            .query(&Query::PendingApprovals {
                project_id: fixture.project_id,
            })
            .unwrap()
        else {
            panic!("unexpected approval projection")
        };
        assert_eq!(approvals.len(), 1);
        fixture.execute(
            "resolve-approval",
            Command::ResolveApproval {
                schema_version: SchemaVersion::CURRENT,
                approval_id: approvals[0].id,
                decision: ApprovalDecision::Approved,
                expected_version: approvals[0].version,
            },
        );
        executor.notify();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();
        assert_eq!(provider.dispatch_count(), 2);
        assert!(fixture.event_json().contains("approved"));
        let learning = kit::telemetry::tool_learning::records(
            &test_support::open_sqlite_store(&fixture.database).unwrap(),
            fixture.run_id,
            &kit::telemetry::tool_learning::ProjectPointerHasher::new(
                fixture.project_id,
                &[44; 32],
            ),
        )
        .unwrap();
        assert_eq!(
            learning
                .iter()
                .filter(|event| matches!(
                    event,
                    kit::telemetry::tool_learning::ToolLearningEvent::Outcome { .. }
                ))
                .count(),
            1
        );

        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("authenticated"),
            FakeScenario::Auth {
                scope: "provider.read".to_owned(),
            },
        ));
        let executor = RunExecutor::start(fixture.config(Arc::clone(&provider))).unwrap();
        fixture.wait_for(RunState::WaitingForAuth).await;
        let run = fixture
            .store
            .lock()
            .unwrap()
            .worker_run(fixture.run_id)
            .unwrap();
        fixture.execute(
            "resolve-auth",
            Command::ResolveAuth {
                schema_version: SchemaVersion::CURRENT,
                run_id: fixture.run_id,
                granted: true,
                expected_version: run.run.version,
            },
        );
        executor.notify();
        fixture.wait_for(RunState::Completed).await;
        executor.shutdown().await.unwrap();
        assert_eq!(provider.dispatch_count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn denied_approval_is_terminal_without_tool_dispatch() {
        let fixture = Fixture::new();
        let provider = Arc::new(FakeProvider::with_scenario(
            FakeResponse::completed("not used"),
            FakeScenario::Approval,
        ));
        let executor = RunExecutor::start(
            fixture
                .config(Arc::clone(&provider))
                .with_tool_learning_key([43; 32]),
        )
        .unwrap();
        fixture.wait_for(RunState::WaitingForApproval).await;
        let QueryProjection::Approvals(approvals) = fixture
            .store
            .lock()
            .unwrap()
            .query(&Query::PendingApprovals {
                project_id: fixture.project_id,
            })
            .unwrap()
        else {
            panic!("unexpected approval projection")
        };
        fixture.execute(
            "deny-approval",
            Command::ResolveApproval {
                schema_version: SchemaVersion::CURRENT,
                approval_id: approvals[0].id,
                decision: ApprovalDecision::Denied,
                expected_version: approvals[0].version,
            },
        );
        executor.notify();
        fixture.wait_for(RunState::Cancelled).await;
        executor.shutdown().await.unwrap();
        assert_eq!(provider.dispatch_count(), 1);
        let store = test_support::open_sqlite_store(&fixture.database).unwrap();
        let records = kit::telemetry::tool_learning::records(
            &store,
            fixture.run_id,
            &kit::telemetry::tool_learning::ProjectPointerHasher::new(
                fixture.project_id,
                &[43; 32],
            ),
        )
        .unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|event| matches!(
                    event,
                    kit::telemetry::tool_learning::ToolLearningEvent::Outcome { .. }
                ))
                .count(),
            1
        );
    }
}
