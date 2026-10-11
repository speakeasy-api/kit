//! Opt-in live native Kit -> AgentKit -> AHP SDK -> generic HerdR hook smoke.
//! Only the model adapter is fake; compose and its edit child are production code.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use agentkit_core::{
    Delta, FinishReason, Item, ItemKind, MetadataMap, Part, PartId, PartKind, ToolCallPart,
    TurnCancellation,
};
use agentkit_loop::{
    Agent, LoopError, LoopInterrupt, LoopStep, ModelAdapter, ModelSession, ModelTurn,
    ModelTurnEvent, ModelTurnResult, SessionConfig, TurnRequest,
};
use agentkit_tools_core::{AllowAllPermissions, BasicToolExecutor, ToolSource};
use async_trait::async_trait;
use kit::ahp::{
    AhpConfig, AhpHost, HostDelivery, HostMutator, SessionFacts, SessionOutcome,
    model::HookedAdapter,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone)]
struct Gates(PathBuf);
impl Gates {
    fn signal(&self, name: &str, evidence: Value) {
        let pending = self.0.join(format!("{name}.pending"));
        std::fs::write(&pending, serde_json::to_vec(&evidence).unwrap()).unwrap();
        std::fs::rename(pending, self.0.join(name)).unwrap();
    }
    async fn permit(&self, name: &str) {
        tokio::time::timeout(Duration::from_secs(60), async {
            while !self.0.join(name).is_file() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Timed out waiting for isolated smoke gate {name}"));
    }
}

#[derive(Clone)]
struct FakeModel {
    gates: Gates,
    effect: PathBuf,
}
struct FakeSession {
    gates: Gates,
    effect: PathBuf,
    request: usize,
}
struct FakeTurn(VecDeque<ModelTurnEvent>);
#[async_trait]
impl ModelAdapter for FakeModel {
    type Session = FakeSession;
    async fn start_session(&self, _: SessionConfig) -> Result<FakeSession, LoopError> {
        // Genuine model initialization is after native on_session_start. The
        // controller verifies HerdR idle before allowing native turn admission.
        self.gates.signal(
            "session-initialized",
            json!({"boundary":"ModelAdapter::start_session"}),
        );
        self.gates.permit("allow-turn").await;
        Ok(FakeSession {
            gates: self.gates.clone(),
            effect: self.effect.clone(),
            request: 0,
        })
    }
    fn provider_name(&self) -> Option<&str> {
        Some("isolated-smoke-fake-model")
    }
}
fn contains_edit_result(value: &Value, path: &str) -> bool {
    match value {
        Value::Object(values) => {
            (value["status"] == "added" && value["path"] == path)
                || values
                    .values()
                    .any(|value| contains_edit_result(value, path))
        }
        Value::Array(values) => values.iter().any(|value| contains_edit_result(value, path)),
        _ => false,
    }
}
#[async_trait]
impl ModelSession for FakeSession {
    type Turn = FakeTurn;
    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<FakeTurn, LoopError> {
        self.request += 1;
        let mut events = VecDeque::new();
        let (finish_reason, output_items) = match self.request {
            1 => {
                assert_eq!(
                    request
                        .available_tools
                        .iter()
                        .map(|spec| spec.name.0.as_str())
                        .collect::<Vec<_>>(),
                    ["compose"]
                );
                assert!(
                    serde_json::to_string(&request.transcript)
                        .unwrap()
                        .contains("Run the isolated compose edit smoke")
                );
                self.gates.signal(
                    "turn-working",
                    json!({"boundary":"ModelSession::begin_turn","session_id":request.session_id}),
                );
                self.gates.permit("allow-compose").await;
                let call = ToolCallPart {
                    id: "real-compose-call".into(),
                    name: "compose".into(),
                    input: json!({"script":"return edit({op: 'add', path: input.path, content: 'real compose child\\n'})", "input":{"path":self.effect}}),
                    metadata: MetadataMap::new(),
                };
                let mut item = Item::text(ItemKind::Assistant, "");
                item.parts = vec![Part::ToolCall(call.clone())];
                events.push_back(ModelTurnEvent::ToolCall(call));
                (FinishReason::ToolCall, vec![item])
            }
            2 => {
                // This request can only arrive after the native executor has run
                // Runtime.compose and the actual edit child has returned to the loop.
                assert_eq!(
                    std::fs::read_to_string(&self.effect).unwrap(),
                    "real compose child\n"
                );
                let transcript = serde_json::to_value(&request.transcript).unwrap();
                assert!(
                    contains_edit_result(&transcript, self.effect.to_str().unwrap()),
                    "Actual edit result missing from model transcript: {transcript}"
                );
                self.gates.signal("child-verified", json!({"effect_path":self.effect,"effect":"real compose child\n","transcript_result_verified":true}));
                events.push_back(ModelTurnEvent::Delta(Delta::BeginPart {
                    part_id: PartId::new("final"),
                    kind: PartKind::Text,
                }));
                events.push_back(ModelTurnEvent::Delta(Delta::AppendText {
                    part_id: PartId::new("final"),
                    chunk: "Kit native AHP HerdR smoke complete".into(),
                }));
                (
                    FinishReason::Completed,
                    vec![Item::text(
                        ItemKind::Assistant,
                        "Kit native AHP HerdR smoke complete",
                    )],
                )
            }
            _ => panic!("Unexpected extra fake-model request"),
        };
        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason,
            output_items,
            usage: None,
            metadata: MetadataMap::new(),
            model: Some("isolated-smoke-model".into()),
            response_id: None,
        }));
        Ok(FakeTurn(events))
    }
    fn model_name(&self) -> Option<&str> {
        Some("isolated-smoke-model")
    }
}
#[async_trait]
impl ModelTurn for FakeTurn {
    async fn next_event(
        &mut self,
        _: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.0.pop_front())
    }
}

#[tokio::test]
#[ignore = "Opt-in only: scripts/ahp-herdr-smoke.py creates an isolated named HerdR pane and gates the fake model"]
async fn native_ahp_herdr_real_compose() {
    assert_eq!(
        std::env::var("KIT_AHP_HERDR_SMOKE").as_deref(),
        Ok("1"),
        "Use the isolated smoke orchestrator"
    );
    let gates = Gates(PathBuf::from(
        std::env::var("KIT_AHP_HERDR_GATE_DIR").expect("Explicit gate directory required"),
    ));
    let scope: Value =
        serde_json::from_slice(&std::fs::read(gates.0.join("scope.json")).unwrap()).unwrap();
    let name = scope["session"].as_str().unwrap();
    assert!(name.starts_with("kit-ahp-e2e-"));
    assert_eq!(std::env::var("HERDR_ENV").as_deref(), Ok("1"));
    assert_eq!(
        std::env::var("HERDR_SOCKET_PATH").unwrap(),
        scope["socket"].as_str().unwrap()
    );
    assert!(
        scope["socket"]
            .as_str()
            .unwrap()
            .contains(&format!("/sessions/{name}/"))
    );
    assert_eq!(
        std::env::var("HERDR_PANE_ID").unwrap(),
        scope["pane"].as_str().unwrap()
    );
    // Isolate only this test process's global discovery root. The real HerdR
    // server retains its actual HOME and the pane/socket facts remain inherited.
    let home = PathBuf::from(scope["home"].as_str().unwrap());
    assert_eq!(PathBuf::from(std::env::var("HOME").unwrap()), home);
    assert!(home.is_dir());
    let work = gates.0.join("work");
    assert!(work.join(".agents/hooks.json").is_file());
    let config = AhpConfig::load(&work)
        .unwrap()
        .expect("Canonical local .agents/hooks.json must be discovered");
    let host = AhpHost::new_with_facts(
        config,
        SessionFacts {
            trigger: "startup".into(),
            permission_mode: "allow-all".into(),
            resumed_from: None,
        },
    )
    .unwrap();
    let effect = work.join("actual-compose-edit.txt");
    let runtime = kit::Runtime::new(&work, "isolated-smoke-model").unwrap();
    let compose = runtime.compose(0);
    let inner = BasicToolExecutor::new([Arc::new(compose.clone()) as Arc<dyn ToolSource>]);
    // Kit's real public registration and closure. The model adapter is the only
    // fake: AhpHost still authors every event the SDK delivers.
    let agent = Agent::builder()
        .model(HookedAdapter::new(
            FakeModel {
                gates: gates.clone(),
                effect: effect.clone(),
            },
            Some(host.clone()),
        ))
        .add_tool_source(compose)
        .tool_executor(kit::tools::ahp::AhpToolExecutor::new(
            Arc::new(inner),
            host.clone(),
        ))
        .permissions(AllowAllPermissions)
        .delivery(HostDelivery::new(host.clone()))
        .mutator(HostMutator::new(host.clone()))
        .build()
        .unwrap();
    let session = "kit-native-herdr-smoke";
    let mut driver = agent
        .start(SessionConfig::new(session).without_cache())
        .await
        .unwrap();
    // Kit owns prompt admission, dispatched after `session.start`.
    let input = host
        .inbound(
            session,
            vec![Item::text(
                ItemKind::User,
                "Run the isolated compose edit smoke",
            )],
            None,
        )
        .await
        .unwrap();
    driver.submit_input(input).unwrap();
    let result = loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(result) => break result,
            // Same cooperative continuation as Kit's drive_with_tasks; this is
            // a real driver/tool boundary, not a synthesized completion.
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            step => panic!("Unexpected driver step: {step:?}"),
        }
    };
    assert_eq!(result.finish_reason, FinishReason::Completed);
    assert!(
        serde_json::to_string(&result.items)
            .unwrap()
            .contains("Kit native AHP HerdR smoke complete")
    );
    assert_eq!(
        std::fs::read_to_string(&effect).unwrap(),
        "real compose child\n"
    );
    gates.signal("turn-finished",json!({"final_output":"Kit native AHP HerdR smoke complete","actual_child_effect_verified":true}));
    // SDK observe delivery is stdin flush, not HerdR application acknowledgement.
    // Keep the owning driver/SDK transports alive until Python observes real idle.
    gates.permit("allow-shutdown").await;
    kit::ahp::close_session(&mut driver, Some(&host), session, SessionOutcome::Completed)
        .await
        .unwrap();
    gates.signal("closed", json!({"driver_closed":true,"sdk_shutdown":true}));
}
