use std::{
    collections::VecDeque,
    fs,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};

use agentkit_core::{
    DataRef, Delta, FinishReason, Item, ItemKind, MetadataMap, Part, PartId, PartKind,
    ReasoningPart, TextPart, TokenUsage, TurnCancellation, Usage,
};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    SessionConfig, TurnRequest,
};
use agentkit_provider_ollama::{OllamaAdapter, OllamaConfig};
use kit::agent::providers::{
    adapter::{ModelStreamPolicy, StreamCommitFactory, StreamPolicyAdapter},
    persistence::SqliteStreamCommitFactory,
    streaming::{BoundedTurn, CanaryRedactor, StreamCommit, StreamLimits},
};
use kit::{
    agent::driver::restart::{EFFECT_CORRELATION_METADATA, EffectCorrelation},
    api::service::AttemptDriverClaim,
    domain::{
        events::{TraceId, UtcDateTime},
        ids::{AttemptId, CommandId, EventId, ModelCallId, PrincipalId, RunId},
        lifecycle::{AttemptOwnership, FencingToken},
        secret::SecretLease,
    },
    test_support,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

enum Step {
    Event(Box<ModelTurnEvent>),
    Error(String),
}

fn event(event: ModelTurnEvent) -> Step {
    Step::Event(Box::new(event))
}

struct FakeTurn {
    steps: VecDeque<Step>,
}

impl ModelTurn for FakeTurn {
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
            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                return Err(LoopError::Cancelled);
            }
            match self.steps.pop_front() {
                Some(Step::Event(event)) => Ok(Some(*event)),
                Some(Step::Error(error)) => Err(LoopError::Provider(error)),
                None => Ok(None),
            }
        })
    }
}

#[derive(Clone, Default)]
struct FakeProvider {
    state: Arc<Mutex<FakeProviderState>>,
}

#[derive(Default)]
struct FakeProviderState {
    failures: VecDeque<String>,
    scripts: VecDeque<VecDeque<Step>>,
    requests: Vec<TurnRequest>,
}

impl ModelAdapter for FakeProvider {
    type Session = FakeSession;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        _config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(FakeSession(self.clone())) })
    }
}

struct FakeSession(FakeProvider);

impl ModelSession for FakeSession {
    type Turn = FakeTurn;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let mut state = self.0.state.lock().unwrap();
            state.requests.push(request);
            if let Some(error) = state.failures.pop_front() {
                return Err(LoopError::Provider(error));
            }
            let steps = state
                .scripts
                .pop_front()
                .ok_or_else(|| LoopError::Provider("no script".into()))?;
            Ok(FakeTurn { steps })
        })
    }
}

#[derive(Clone, Default)]
struct TestCommitLog(Arc<Mutex<Vec<String>>>);

impl StreamCommitFactory for TestCommitLog {
    fn for_request(&self, _request: &TurnRequest) -> Result<Box<dyn StreamCommit>, LoopError> {
        Ok(Box::new(self.clone()))
    }
}

impl StreamCommit for TestCommitLog {
    fn commit_chunk(&mut self, sequence: u64, _event: &ModelTurnEvent) -> Result<(), LoopError> {
        self.0.lock().unwrap().push(format!("chunk-{sequence}"));
        Ok(())
    }

    fn commit_outcome(&mut self, _result: &ModelTurnResult) -> Result<(), LoopError> {
        self.0.lock().unwrap().push("outcome".into());
        Ok(())
    }
}

struct TempDatabase(PathBuf);

impl TempDatabase {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "kit-provider-stream-{}-{}.sqlite3",
            std::process::id(),
            EventId::generate().unwrap()
        )))
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(self.0.with_extension("sqlite3-shm"));
    }
}

fn stream_correlation(key: &str) -> EffectCorrelation {
    let run_id = RunId::generate().unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        PrincipalId::generate().unwrap(),
        FencingToken::new(7),
    );
    EffectCorrelation {
        run_id,
        owner,
        claim: AttemptDriverClaim {
            run_id,
            attempt_id: owner.attempt_id,
            principal_id: owner.principal_id,
            fence: owner.fencing_token,
            lease_version: 1,
            expires_at_unix_micros: 0,
        },
        operation_id: ModelCallId::generate().unwrap().to_string(),
        idempotency_key: key.into(),
        command_id: CommandId::generate().unwrap(),
        intent_event_id: EventId::generate().unwrap(),
        dispatch_event_id: EventId::generate().unwrap(),
        outcome_event_id: EventId::generate().unwrap(),
        occurred_at: UtcDateTime::parse("2026-07-23T00:00:00Z").unwrap(),
        trace_id: TraceId::parse("provider-stream-test").unwrap(),
    }
}

fn install_claim(database: &TempDatabase, correlation: &EffectCorrelation) {
    test_support::open_sqlite_store(&database.0)
        .unwrap()
        .install_driver_claim_for_test(correlation.claim)
        .unwrap();
}

fn correlated_request(correlation: &EffectCorrelation) -> TurnRequest {
    let mut request = request();
    request.metadata.insert(
        EFFECT_CORRELATION_METADATA.into(),
        serde_json::to_value(correlation).unwrap(),
    );
    request
}

fn result(text: &str) -> ModelTurnResult {
    ModelTurnResult {
        finish_reason: FinishReason::Completed,
        output_items: vec![Item::new(
            ItemKind::Assistant,
            vec![Part::Text(TextPart::new(text))],
        )],
        usage: None,
        metadata: MetadataMap::new(),
        model: Some("pinned-model".into()),
        response_id: Some("response".into()),
    }
}

fn request() -> TurnRequest {
    TurnRequest {
        session_id: agentkit_core::SessionId::new("session"),
        turn_id: agentkit_core::TurnId::new("turn"),
        transcript: vec![Item::text(ItemKind::User, "prompt")],
        available_tools: Vec::new(),
        cache: Some(agentkit_loop::PromptCacheRequest::automatic().with_key("cache-key")),
        structured_output: None,
        generation: Default::default(),
        metadata: MetadataMap::new(),
    }
}

fn valid_stream(text: &str) -> VecDeque<Step> {
    let part_id = PartId::new("text");
    VecDeque::from([
        event(ModelTurnEvent::Delta(Delta::BeginPart {
            part_id: part_id.clone(),
            kind: PartKind::Text,
        })),
        event(ModelTurnEvent::Delta(Delta::AppendText {
            part_id,
            chunk: text.into(),
        })),
        event(ModelTurnEvent::Delta(Delta::CommitPart {
            part: Part::Text(TextPart::new(text)),
        })),
        event(ModelTurnEvent::Finished(result(text))),
    ])
}

async fn drain<T: ModelTurn>(turn: &mut T) -> Result<Vec<ModelTurnEvent>, LoopError> {
    let mut events = Vec::new();
    while let Some(event) = turn.next_event(None).await? {
        events.push(event);
    }
    Ok(events)
}

async fn receive_chat_completion_request(listener: TcpListener) -> serde_json::Value {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut request = Vec::new();
    let (body_start, content_length) = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "request ended before the HTTP body");
        request.extend_from_slice(&chunk[..read]);
        let Some(headers_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..headers_end]).unwrap();
        assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let body_start = headers_end + 4;
        if request.len() >= body_start + length {
            break (body_start, length);
        }
    };
    let body = serde_json::from_slice(&request[body_start..body_start + content_length]).unwrap();
    let response = br#"{"id":"response","model":"llama-test","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.write_all(response).await.unwrap();
    body
}

#[tokio::test]
async fn ollama_openai_compatible_request_serializes_max_tokens_and_enforces_cap() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(receive_chat_completion_request(listener));
    let adapter = OllamaAdapter::new(
        OllamaConfig::new("llama-test")
            .with_base_url(endpoint)
            .with_max_tokens(64)
            .with_streaming(false),
    )
    .unwrap();
    assert_eq!(adapter.max_output_tokens(), Some(64));
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    let mut capped = request();
    capped.cache = None;
    capped.generation.max_output_tokens = Some(32);
    let _turn = session.begin_turn(capped, None).await.unwrap();

    let body = server.await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "model": "llama-test",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "prompt"}],
            "stream": false,
            "user": "session"
        })
    );
    assert!(body.get("num_predict").is_none());

    let mut above_cap = request();
    above_cap.generation.max_output_tokens = Some(65);
    let error = match session.begin_turn(above_cap, None).await {
        Ok(_) => panic!("request above the configured cap was accepted"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("max output tokens exceed the configured provider cap"),
        "{error}"
    );
}

#[tokio::test]
async fn request_is_forwarded_once_and_commits_before_exposure() {
    let provider = FakeProvider::default();
    {
        let mut state = provider.state.lock().unwrap();
        state.scripts.push_back(valid_stream("left-CANARY-right"));
    }
    let log = TestCommitLog::default();
    let policy = ModelStreamPolicy {
        stream: StreamLimits {
            max_delta_bytes: 5,
            ..StreamLimits::default()
        },
        canaries: vec!["CANARY".into()],
        ..ModelStreamPolicy::default()
    };
    let adapter = StreamPolicyAdapter::new(provider.clone(), policy, Arc::new(log.clone()));
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    let mut turn = session.begin_turn(request(), None).await.unwrap();

    let first = turn.next_event(None).await.unwrap().unwrap();
    assert!(matches!(
        first,
        ModelTurnEvent::Delta(Delta::BeginPart { .. })
    ));
    assert_eq!(log.0.lock().unwrap().last().unwrap(), "outcome");
    let mut visible = vec![first];
    visible.extend(drain(&mut turn).await.unwrap());
    let encoded = serde_json::to_string(&visible).unwrap();
    assert!(!encoded.contains("CANARY"));
    assert!(visible.iter().all(|event| !matches!(
        event,
        ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) if chunk.len() > 5
    )));

    let state = provider.state.lock().unwrap();
    assert_eq!(state.requests, vec![request()]);
}

#[tokio::test]
async fn ambiguous_begin_turn_errors_are_not_retried() {
    let provider = FakeProvider::default();
    provider
        .state
        .lock()
        .unwrap()
        .failures
        .push_back("ambiguous dispatch failure".into());
    let adapter = StreamPolicyAdapter::new(
        provider.clone(),
        ModelStreamPolicy::default(),
        Arc::new(TestCommitLog::default()),
    );
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    assert!(session.begin_turn(request(), None).await.is_err());
    assert_eq!(provider.state.lock().unwrap().requests.len(), 1);
}

#[tokio::test]
async fn journal_correlation_is_the_sqlite_stream_identity() {
    let database = TempDatabase::new();
    let correlation = stream_correlation("journal-provider-key");
    install_claim(&database, &correlation);
    let commits =
        Arc::new(SqliteStreamCommitFactory::open(&database.0, StreamLimits::default()).unwrap());
    let provider = FakeProvider::default();
    provider
        .state
        .lock()
        .unwrap()
        .scripts
        .push_back(valid_stream("durable"));
    let adapter = StreamPolicyAdapter::new(
        provider.clone(),
        ModelStreamPolicy::default(),
        commits.clone(),
    );
    let mut request = request();
    request.metadata.insert(
        EFFECT_CORRELATION_METADATA.into(),
        serde_json::to_value(&correlation).unwrap(),
    );
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    drain(&mut session.begin_turn(request, None).await.unwrap())
        .await
        .unwrap();

    let persisted = commits.read(&correlation).unwrap().unwrap();
    assert_eq!(persisted.outcome, Some(result("durable")));
    let requests = &provider.state.lock().unwrap().requests;
    assert_eq!(
        requests[0].metadata[EFFECT_CORRELATION_METADATA],
        serde_json::to_value(&correlation).unwrap()
    );
}

#[tokio::test]
async fn durable_stream_suppresses_reasoning_and_redacts_secret_forms_and_headers() {
    let database = TempDatabase::new();
    let correlation = stream_correlation("sanitized-provider-key");
    install_claim(&database, &correlation);
    let commits =
        Arc::new(SqliteStreamCommitFactory::open(&database.0, StreamLimits::default()).unwrap());
    let provider = FakeProvider::default();
    let reasoning = PartId::new("reasoning");
    let text = PartId::new("text");
    let usage = Usage::new(TokenUsage::new(10, 4).with_reasoning_tokens(9));
    let metadata = MetadataMap::from([
        (
            "headers".into(),
            serde_json::json!({"authorization": "HEADER_ONLY_CANARY"}),
        ),
        ("error".into(), serde_json::json!("Q0FOQVJZX1NFQ1JFVA==")),
    ]);
    let outcome = ModelTurnResult {
        finish_reason: FinishReason::Completed,
        output_items: vec![Item::new(
            ItemKind::Assistant,
            vec![
                Part::Reasoning(
                    ReasoningPart::summary("PRIVATE_CHAIN_OF_THOUGHT")
                        .with_data(DataRef::inline_text("PRIVATE_CHAIN_OF_THOUGHT")),
                ),
                Part::Text(TextPart::new("public answer").with_metadata(metadata.clone())),
            ],
        )],
        usage: Some(usage.clone()),
        metadata,
        model: Some("pinned-model".into()),
        response_id: Some("response".into()),
    };
    provider
        .state
        .lock()
        .unwrap()
        .scripts
        .push_back(VecDeque::from([
            event(ModelTurnEvent::Delta(Delta::BeginPart {
                part_id: reasoning.clone(),
                kind: PartKind::Reasoning,
            })),
            event(ModelTurnEvent::Delta(Delta::AppendText {
                part_id: reasoning,
                chunk: "PRIVATE_CHAIN_OF_THOUGHT".into(),
            })),
            event(ModelTurnEvent::Delta(Delta::CommitPart {
                part: Part::Reasoning(ReasoningPart::summary("PRIVATE_CHAIN_OF_THOUGHT")),
            })),
            event(ModelTurnEvent::Delta(Delta::BeginPart {
                part_id: text.clone(),
                kind: PartKind::Text,
            })),
            event(ModelTurnEvent::Delta(Delta::AppendText {
                part_id: text,
                chunk: "public Q0FOQVJZX1NFQ1JFVA== %43%41%4E%41%52%59%5F%53%45%43%52%45%54".into(),
            })),
            event(ModelTurnEvent::Delta(Delta::CommitPart {
                part: Part::Text(TextPart::new("public answer")),
            })),
            event(ModelTurnEvent::Usage(usage)),
            event(ModelTurnEvent::Finished(outcome)),
        ]));
    let adapter = StreamPolicyAdapter::new(
        provider,
        ModelStreamPolicy {
            secrets: vec![Arc::new(SecretLease::new(b"CANARY_SECRET".to_vec()))],
            ..ModelStreamPolicy::default()
        },
        commits.clone(),
    );
    let mut request = request();
    request.metadata.insert(
        EFFECT_CORRELATION_METADATA.into(),
        serde_json::to_value(&correlation).unwrap(),
    );
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    let visible = drain(&mut session.begin_turn(request, None).await.unwrap())
        .await
        .unwrap();

    let encoded = serde_json::to_string(&visible).unwrap();
    for private in [
        "PRIVATE_CHAIN_OF_THOUGHT",
        "CANARY_SECRET",
        "Q0FOQVJZX1NFQ1JFVA==",
        "%43%41%4E%41%52%59%5F%53%45%43%52%45%54",
        "HEADER_ONLY_CANARY",
    ] {
        assert!(
            !encoded.contains(private),
            "visible stream leaked {private}"
        );
    }
    let persisted = commits.read(&correlation).unwrap().unwrap();
    assert_eq!(
        persisted
            .outcome
            .as_ref()
            .and_then(|result| result.usage.as_ref())
            .and_then(|usage| usage.tokens.as_ref())
            .and_then(|tokens| tokens.reasoning_tokens),
        Some(9)
    );
    assert!(
        persisted.outcome.as_ref().unwrap().output_items[0]
            .parts
            .iter()
            .all(|part| !matches!(part, Part::Reasoning(_)))
    );

    let mut raw = fs::read(&database.0).unwrap();
    for extension in ["sqlite3-wal", "sqlite3-shm"] {
        if let Ok(bytes) = fs::read(database.0.with_extension(extension)) {
            raw.extend(bytes);
        }
    }
    let raw = String::from_utf8_lossy(&raw);
    for private in [
        "PRIVATE_CHAIN_OF_THOUGHT",
        "CANARY_SECRET",
        "Q0FOQVJZX1NFQ1JFVA==",
        "%43%41%4E%41%52%59%5F%53%45%43%52%45%54",
        "HEADER_ONLY_CANARY",
    ] {
        assert!(!raw.contains(private), "raw SQLite leaked {private}");
    }
}

#[tokio::test]
async fn reasoning_summary_requires_explicit_policy_and_is_stored_redacted() {
    let database = TempDatabase::new();
    let correlation = stream_correlation("summary-policy-key");
    install_claim(&database, &correlation);
    let commits = Arc::new(
        SqliteStreamCommitFactory::open(&database.0, StreamLimits::default())
            .unwrap()
            .with_reasoning_summaries(true),
    );
    let provider = FakeProvider::default();
    let reasoning = PartId::new("reasoning");
    let outcome = ModelTurnResult {
        output_items: vec![Item::new(
            ItemKind::Assistant,
            vec![Part::Reasoning(
                ReasoningPart::summary("safe summary CANARY")
                    .with_data(DataRef::inline_text("private detail")),
            )],
        )],
        ..result("unused")
    };
    provider
        .state
        .lock()
        .unwrap()
        .scripts
        .push_back(VecDeque::from([
            event(ModelTurnEvent::Delta(Delta::BeginPart {
                part_id: reasoning.clone(),
                kind: PartKind::Reasoning,
            })),
            event(ModelTurnEvent::Delta(Delta::AppendText {
                part_id: reasoning,
                chunk: "private detail".into(),
            })),
            event(ModelTurnEvent::Delta(Delta::CommitPart {
                part: Part::Reasoning(ReasoningPart::summary("safe summary CANARY")),
            })),
            event(ModelTurnEvent::Finished(outcome)),
        ]));
    let adapter = StreamPolicyAdapter::new(
        provider,
        ModelStreamPolicy {
            canaries: vec!["CANARY".into()],
            retain_reasoning_summaries: true,
            ..ModelStreamPolicy::default()
        },
        commits.clone(),
    );
    let mut request = request();
    request.metadata.insert(
        EFFECT_CORRELATION_METADATA.into(),
        serde_json::to_value(&correlation).unwrap(),
    );
    let mut session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    drain(&mut session.begin_turn(request, None).await.unwrap())
        .await
        .unwrap();

    let stored = commits.read(&correlation).unwrap().unwrap();
    let Part::Reasoning(summary) = &stored.outcome.unwrap().output_items[0].parts[0] else {
        panic!("explicit summary policy did not retain a reasoning summary")
    };
    assert_eq!(summary.summary.as_deref(), Some("safe summary [REDACTED]"));
    assert!(summary.redacted);
    assert!(summary.data.is_none());
    assert!(summary.metadata.is_empty());
    assert!(
        SqliteStreamCommitFactory::open(&database.0, StreamLimits::default())
            .unwrap()
            .read(&correlation)
            .is_err()
    );
}

#[test]
fn binary_secret_patterns_cannot_split_utf8_text() {
    let redactor =
        CanaryRedactor::default().with_secrets(&[Arc::new(SecretLease::new(vec![0xc3]))]);
    assert_eq!(redactor.redact_text("é public"), "é public");
}

#[tokio::test]
async fn malformed_out_of_order_oversize_and_midstream_failure_are_rejected() {
    let part_id = PartId::new("missing");
    let malformed = FakeTurn {
        steps: VecDeque::from([
            event(ModelTurnEvent::Delta(Delta::AppendText {
                part_id,
                chunk: "out-of-order".into(),
            })),
            event(ModelTurnEvent::Finished(result("bad"))),
        ]),
    };
    let mut malformed = BoundedTurn::new(
        malformed,
        TestCommitLog::default(),
        StreamLimits::default(),
        CanaryRedactor::default(),
    );
    assert!(malformed.next_event(None).await.is_err());

    let oversize = FakeTurn {
        steps: valid_stream(&"x".repeat(1_024)),
    };
    let mut oversize = BoundedTurn::new(
        oversize,
        TestCommitLog::default(),
        StreamLimits {
            max_bytes: 100,
            ..StreamLimits::default()
        },
        CanaryRedactor::default(),
    );
    assert!(oversize.next_event(None).await.is_err());

    let mut failed = valid_stream("partial");
    failed.pop_back();
    failed.push_back(Step::Error("stream failed with CANARY".into()));
    let mut failed = BoundedTurn::new(
        FakeTurn { steps: failed },
        TestCommitLog::default(),
        StreamLimits::default(),
        CanaryRedactor::new(["CANARY".into()]),
    );
    let error = failed.next_event(None).await.unwrap_err().to_string();
    assert!(!error.contains("CANARY"));
}

#[tokio::test]
async fn cancellation_stops_a_dispatched_stream_without_committing_success() {
    let cancellation = agentkit_core::CancellationController::new();
    let checkpoint = cancellation.handle().checkpoint();
    cancellation.interrupt();
    let log = TestCommitLog::default();
    let mut turn = BoundedTurn::new(
        FakeTurn {
            steps: valid_stream("never-visible"),
        },
        log.clone(),
        StreamLimits::default(),
        CanaryRedactor::default(),
    );
    assert!(matches!(
        turn.next_event(Some(checkpoint)).await,
        Err(LoopError::Cancelled)
    ));
    assert!(log.0.lock().unwrap().is_empty());
}

#[test]
fn sqlite_stream_restarts_at_committed_chunk_and_outcome_boundaries() {
    let database = TempDatabase::new();
    let correlation = stream_correlation("durable-stream-key");
    install_claim(&database, &correlation);
    let limits = StreamLimits {
        max_bytes: 64 * 1024,
        max_items: 16,
        max_delta_bytes: 16,
        ..StreamLimits::default()
    };
    let request = correlated_request(&correlation);
    let factory = SqliteStreamCommitFactory::open(&database.0, limits).unwrap();
    let part_id = PartId::new("artifact");
    let artifact = "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let begin = ModelTurnEvent::Delta(Delta::BeginPart {
        part_id,
        kind: PartKind::File,
    });
    let committed_part = ModelTurnEvent::Delta(Delta::CommitPart {
        part: Part::file(DataRef::handle(artifact)),
    });
    let outcome = ModelTurnResult {
        output_items: vec![Item::new(
            ItemKind::Assistant,
            vec![Part::file(DataRef::handle(artifact))],
        )],
        ..result("durable")
    };

    let mut commit = factory.for_request(&request).unwrap();
    commit.commit_chunk(1, &begin).unwrap();
    drop(commit);

    let restarted = SqliteStreamCommitFactory::open(&database.0, limits).unwrap();
    let partial = restarted.read(&correlation).unwrap().unwrap();
    assert_eq!(partial.committed_sequence, 1);
    assert_eq!(partial.chunks[0].event, begin);
    assert!(partial.outcome.is_none());

    let mut commit = restarted.for_request(&request).unwrap();
    commit.commit_chunk(1, &begin).unwrap();
    commit.commit_chunk(2, &committed_part).unwrap();
    drop(commit);
    let chunks_only = SqliteStreamCommitFactory::open(&database.0, limits)
        .unwrap()
        .read(&correlation)
        .unwrap()
        .unwrap();
    assert_eq!(chunks_only.committed_sequence, 2);
    assert_eq!(chunks_only.chunks[1].artifact_refs, [artifact]);
    assert!(chunks_only.outcome.is_none());
    assert!(chunks_only.chunks[0].commit_position < chunks_only.chunks[1].commit_position);

    let mut commit = restarted.for_request(&request).unwrap();
    commit.commit_outcome(&outcome).unwrap();
    drop(commit);
    let complete = SqliteStreamCommitFactory::open(&database.0, limits)
        .unwrap()
        .read(&correlation)
        .unwrap()
        .unwrap();
    assert_eq!(complete.outcome, Some(outcome));
    assert_eq!(complete.outcome_artifact_refs, [artifact]);
    assert!(complete.outcome_position.unwrap() > complete.chunks[1].commit_position);
    assert_eq!(complete.watermark, complete.outcome_position.unwrap());
}

#[test]
fn sqlite_stream_rejects_malformed_and_oversize_commits_and_stale_fences() {
    let database = TempDatabase::new();
    let correlation = stream_correlation("bounded-stream-key");
    install_claim(&database, &correlation);
    let limits = StreamLimits {
        max_bytes: 1024,
        max_items: 4,
        max_delta_bytes: 4,
        ..StreamLimits::default()
    };
    let request = correlated_request(&correlation);
    let factory = SqliteStreamCommitFactory::open(&database.0, limits).unwrap();
    let mut commit = factory.for_request(&request).unwrap();
    let append = |chunk: &str| {
        ModelTurnEvent::Delta(Delta::AppendText {
            part_id: PartId::new("text"),
            chunk: chunk.into(),
        })
    };
    assert!(commit.commit_chunk(2, &append("ok")).is_err());
    assert!(
        commit
            .commit_chunk(1, &ModelTurnEvent::Finished(result("bad")))
            .is_err()
    );
    assert!(commit.commit_chunk(1, &append("large")).is_err());
    commit.commit_chunk(1, &append("okay")).unwrap();

    let newer = SqliteStreamCommitFactory::open(&database.0, limits).unwrap();
    let newer_correlation = EffectCorrelation {
        owner: AttemptOwnership::new(
            correlation.owner.attempt_id,
            correlation.owner.principal_id,
            FencingToken::new(8),
        ),
        claim: AttemptDriverClaim {
            fence: FencingToken::new(8),
            lease_version: 2,
            ..correlation.claim
        },
        idempotency_key: "new-fence".into(),
        ..correlation.clone()
    };
    install_claim(&database, &newer_correlation);
    newer
        .for_request(&correlated_request(&newer_correlation))
        .unwrap()
        .commit_chunk(1, &append("new"))
        .unwrap();
    assert!(commit.commit_chunk(2, &append("old")).is_err());
}

#[test]
fn sqlite_restart_ignores_rows_beyond_committed_watermarks_and_rejects_corruption() {
    let database = TempDatabase::new();
    let correlation = stream_correlation("watermark-key");
    install_claim(&database, &correlation);
    let limits = StreamLimits {
        max_bytes: 1024,
        max_items: 4,
        max_delta_bytes: 16,
        ..StreamLimits::default()
    };
    let request = correlated_request(&correlation);
    let factory = SqliteStreamCommitFactory::open(&database.0, limits).unwrap();
    let first = ModelTurnEvent::Usage(agentkit_core::Usage::default());
    factory
        .for_request(&request)
        .unwrap()
        .commit_chunk(1, &first)
        .unwrap();

    let hidden =
        serde_json::to_vec(&ModelTurnEvent::Usage(agentkit_core::Usage::default())).unwrap();
    let connection = rusqlite::Connection::open(&database.0).unwrap();
    connection
        .execute(
            "INSERT INTO provider_stream_chunks (
                 attempt_id, model_call_id, fence, idempotency_key, sequence,
                 commit_position, event, artifact_refs
             ) VALUES (?1, ?2, ?3, ?4, 2, 999, ?5, '[]')",
            rusqlite::params![
                correlation.owner.attempt_id.to_string(),
                correlation.operation_id,
                correlation.owner.fencing_token.get(),
                "watermark-key",
                hidden,
            ],
        )
        .unwrap();
    drop(connection);

    let restarted = SqliteStreamCommitFactory::open(&database.0, limits).unwrap();
    let committed = restarted.read(&correlation).unwrap().unwrap();
    assert_eq!(committed.chunks.len(), 1);
    assert_eq!(committed.chunks[0].event, first);

    let hidden_reasoning = serde_json::to_vec(&ModelTurnEvent::Delta(Delta::BeginPart {
        part_id: PartId::new("private"),
        kind: PartKind::Reasoning,
    }))
    .unwrap();
    rusqlite::Connection::open(&database.0)
        .unwrap()
        .execute(
            "UPDATE provider_stream_chunks SET event = ?1 WHERE sequence = 1",
            [hidden_reasoning],
        )
        .unwrap();
    assert!(restarted.read(&correlation).is_err());

    let first_bytes = serde_json::to_vec(&first).unwrap();
    rusqlite::Connection::open(&database.0)
        .unwrap()
        .execute(
            "UPDATE provider_stream_chunks SET event = ?1 WHERE sequence = 1",
            [first_bytes],
        )
        .unwrap();

    rusqlite::Connection::open(&database.0)
        .unwrap()
        .execute(
            "UPDATE provider_stream_chunks SET event = X'00' WHERE sequence = 1",
            [],
        )
        .unwrap();
    assert!(restarted.read(&correlation).is_err());
}

struct FailingCommit;

impl StreamCommit for FailingCommit {
    fn commit_chunk(&mut self, _sequence: u64, _event: &ModelTurnEvent) -> Result<(), LoopError> {
        Err(LoopError::Provider("disk failed".into()))
    }

    fn commit_outcome(&mut self, _result: &ModelTurnResult) -> Result<(), LoopError> {
        panic!("outcome must not be acknowledged after a chunk commit failure")
    }
}

#[tokio::test]
async fn commit_failure_is_terminal_unknown_and_never_visible() {
    let mut turn = BoundedTurn::new(
        FakeTurn {
            steps: valid_stream("hidden"),
        },
        FailingCommit,
        StreamLimits::default(),
        CanaryRedactor::default(),
    );
    let error = turn.next_event(None).await.unwrap_err().to_string();
    assert!(error.contains("outcome_unknown"));
    assert!(turn.next_event(None).await.is_err());
}
