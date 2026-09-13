use super::super::*;
use agentkit_core::{MetadataMap, SessionId, ToolCallId, ToolOutput, TurnId};
use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext, ToolName, ToolSource};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

#[tokio::test]
async fn successful_subagent_prompt_and_fork_project_before_completion_on_both_protocols() {
    for child_v2 in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let runtime = crate::Runtime::new(root.path(), "gpt-5.4").unwrap();
        let mut args = vec![format!(
            "{}/fixtures/mock-acp.py",
            env!("CARGO_MANIFEST_DIR")
        )];
        if child_v2 {
            args.push("--v2".into());
        }
        let harnesses = crate::AcpHarnesses::new(BTreeMap::from([(
            "rich".into(),
            crate::AcpHarnessProfile {
                command: "python3".into(),
                args,
                permissions: Default::default(),
            },
        )]))
        .unwrap();
        let runtime =
            crate::Runtime::with_acp_harnesses(runtime, harnesses, "acp.rich".into()).unwrap();
        let session = if child_v2 {
            "real-child-v2"
        } else {
            "real-child-v1"
        };
        let (send_v1, mut recv_v1) = tokio::sync::mpsc::unbounded_channel();
        let (send_v2, mut recv_v2) = tokio::sync::mpsc::unbounded_channel();
        let v1 = Subscription::start(session.into(), move |update| send_v1.send(update).is_ok());
        let v2 = Subscription::start_v2(session.into(), move |update| send_v2.send(update).is_ok());
        let context = OwnedToolContext {
            session_id: SessionId::new(session),
            turn_id: TurnId::new("turn"),
            metadata: MetadataMap::new(),
            permissions: Arc::new(AllowAllPermissions),
            resources: Arc::new(()),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };
        let tools = runtime.compose(0);
        let mut prior = Value::Null;
        let mut terminal_ids = Vec::new();
        for name in ["subagent", "prompt", "fork"] {
            let mut input = json!({"prompt": "MOCK_TOOL_CONTENT"});
            if name != "subagent" {
                input["subagent"] = prior.clone();
            }
            let request = ToolRequest {
                session_id: SessionId::new(session),
                turn_id: TurnId::new("turn"),
                call_id: ToolCallId::new(format!("outer:compose:{name}")),
                tool_name: ToolName::new(name),
                input,
                metadata: MetadataMap::new(),
            };
            let tool = tools.get(&ToolName::new(name)).unwrap();
            let result = tool
                .invoke(request.clone(), &mut context.borrowed())
                .await
                .unwrap();
            let ToolOutput::Structured(value) = result.result.output else {
                panic!("expected handle");
            };
            prior = value;
            assert_eq!(prior["output"], "tool content done");
            v1.drain().await.unwrap();
            v2.drain().await.unwrap();
            let first = std::iter::from_fn(|| recv_v1.try_recv().ok())
                .map(|update| serde_json::to_value(update.v1().unwrap()).unwrap())
                .collect::<Vec<_>>();
            let second = std::iter::from_fn(|| recv_v2.try_recv().ok())
                .map(|update| serde_json::to_value(update.v2().unwrap()).unwrap())
                .collect::<Vec<_>>();
            for events in [&first, &second] {
                assert_eq!(events[0]["toolCallId"], request.call_id.0);
                assert_eq!(events.last().unwrap()["status"], "completed");
                assert_eq!(events.last().unwrap()["toolCallId"], request.call_id.0);
            }
            if !child_v2 {
                let content = first
                    .iter()
                    .find_map(|update| update.get("content"))
                    .unwrap();
                assert_eq!(content[0]["oldText"], "old\n");
                assert_eq!(content[0]["newText"], "new\n");
            } else {
                assert!(
                    first.iter().all(|update| update.get("terminalId").is_none()
                        && update.get("content").is_none())
                );
                let chunk = second
                    .iter()
                    .position(|update| update["sessionUpdate"] == "terminal_output_chunk")
                    .unwrap();
                let content = second
                    .iter()
                    .position(|update| update.get("content").is_some())
                    .unwrap();
                assert!(chunk < content && content < second.len() - 1);
                let id = second[chunk]["terminalId"].as_str().unwrap().to_owned();
                assert!(!terminal_ids.contains(&id));
                assert_eq!(second[content]["content"][1]["terminalId"], id);
                terminal_ids.push(id);
                assert_eq!(
                    prior["updates"]["items"][2]["content"][0]["terminalId"],
                    "terminal-1"
                );
            }
            let rich = second
                .iter()
                .rev()
                .find_map(|update| update.get("content"))
                .unwrap();
            assert_eq!(rich[0]["changes"][0]["operation"], "modify");
            if child_v2 {
                assert_eq!(rich[0]["patch"]["text"], "-old\n+new\n");
            }
        }
    }
}
