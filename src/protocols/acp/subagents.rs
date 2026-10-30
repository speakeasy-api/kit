//! Connection-local transport for upstream subagent sessions.
use std::future::Future;

use futures_util::future::{Either, select};
use serde_json::Value;
use tokio::sync::broadcast;

/// Project native v2 messages onto the v1 subagent message model. Mapping
/// both upserts and chunks preserves replacement semantics without appending
/// the final message a second time after its streamed chunks.
/// Foreground state is already folded into the parent association by runtime.
/// Other v2-only terminal, plan-operation, and tool-content updates have no
/// lossless v1 representation; omit those rather than terminate forwarding.
pub(super) fn v1_update(mut update: Value) -> Option<agentkit_acp::SessionUpdate> {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let replacement = match kind {
        "state_update"
        | "terminal_update"
        | "terminal_output_chunk"
        | "plan_update"
        | "plan_removed"
        | "tool_call_content_chunk" => return None,
        "user_message" | "agent_message" | "agent_thought" => Some("session_message"),
        "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk"
            if update.get("messageId").is_some() =>
        {
            Some("session_message_chunk")
        }
        _ => None,
    };
    if let Some(replacement) = replacement {
        update["sessionUpdate"] = Value::String(replacement.into());
    }
    serde_json::from_value(update).ok()
}

/// Unsupported or malformed child events are isolated to that event. They are
/// not connection failures, and must not suppress subsequent valid updates.
pub(super) fn v2_update(update: Value) -> Option<agentkit_acp::v2::wire::SessionUpdate> {
    serde_json::from_value(update).ok()
}

/// Poll forwarding in the actor's task: cancellation and attachment rollback
/// cannot leave a detached sender holding the old connection alive. Runtime
/// events must include the enclosing session ID, not only the direct child ID,
/// and must announce associations before any event references those children.
pub(super) async fn with_updates(
    actor: impl Future<Output = ()>,
    updates: Option<broadcast::Receiver<(String, Value)>>,
    mut send: impl FnMut(String, Value) -> Result<(), agent_client_protocol::Error>,
) {
    let Some(mut updates) = updates else {
        actor.await;
        return;
    };
    tokio::pin!(actor);
    loop {
        let next = match select(actor.as_mut(), Box::pin(updates.recv())).await {
            Either::Left(((), pending_update)) => {
                drop(pending_update);
                None
            }
            Either::Right((update, _)) => Some(update),
        };
        match next {
            None => {
                // Closing children can enqueue final updates during actor teardown.
                while let Ok((session_id, update)) = updates.try_recv() {
                    if send(session_id, update).is_err() {
                        break;
                    }
                }
                return;
            }
            Some(update) => {
                match update {
                    Ok((session_id, update)) => {
                        if send(session_id, update).is_err() {
                            actor.await;
                            return;
                        }
                    }
                    Err(_) => {
                        // On lag, stop rather than send an event whose association
                        // may have been dropped. Parent execution remains available.
                        actor.await;
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::disallowed_methods
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upstream_types_preserve_association_patches_and_directed_messages() {
        use agent_client_protocol::schema::v1::{SessionUpdate, SubagentUpdate};
        let association =
            serde_json::to_value(SessionUpdate::SubagentUpdate(SubagentUpdate::new("child")))
                .unwrap();
        assert_eq!(
            association,
            json!({"sessionUpdate":"subagent_update", "sessionId":"child"})
        );
        for update in [
            json!({"sessionUpdate":"subagent_update", "sessionId":"child", "title":null, "description":null, "capabilities":null, "state":null, "_meta":null}),
            json!({"sessionUpdate":"session_message", "messageId":"message", "senderSessionId":"parent", "recipientSessionId":"child"}),
            json!({"sessionUpdate":"session_message_chunk", "messageId":"message", "content":{"type":"text", "text":"investigate"}}),
        ] {
            let v1: SessionUpdate = serde_json::from_value(update.clone()).unwrap();
            assert_eq!(serde_json::to_value(v1).unwrap(), update);
            let v2: agentkit_acp::v2::wire::SessionUpdate =
                serde_json::from_value(update.clone()).unwrap();
            assert_eq!(serde_json::to_value(v2).unwrap(), update);
        }
    }

    #[test]
    fn v1_projection_preserves_v2_message_replacements_after_state_updates() {
        assert!(v1_update(json!({"sessionUpdate":"state_update", "state":"running"})).is_none());
        let updates = [
            json!({"sessionUpdate":"agent_message", "messageId":"answer"}),
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"answer", "content":{"type":"text","text":"partial"}}),
            json!({"sessionUpdate":"agent_message", "messageId":"answer", "content":[{"type":"text","text":"final"}]}),
        ];
        for (update, kind) in updates.into_iter().zip([
            "session_message",
            "session_message_chunk",
            "session_message",
        ]) {
            let mut expected = update.clone();
            expected["sessionUpdate"] = kind.into();
            let projected = v1_update(update).unwrap();
            assert_eq!(serde_json::to_value(projected).unwrap(), expected);
        }
        let legacy = json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"legacy"}});
        assert_eq!(
            serde_json::to_value(v1_update(legacy.clone()).unwrap()).unwrap(),
            legacy
        );
        assert!(v1_update(json!({"sessionUpdate":"session_message_chunk"})).is_none());
    }

    #[tokio::test]
    async fn unsupported_events_do_not_stop_either_version_stream() {
        let projections: [fn(Value) -> Option<Value>; 2] = [
            |update| v1_update(update).map(|value| serde_json::to_value(value).unwrap()),
            |update| v2_update(update).map(|value| serde_json::to_value(value).unwrap()),
        ];
        for (version, project) in projections.into_iter().enumerate() {
            let (tx, rx) = broadcast::channel(8);
            let (finished, finish) = tokio::sync::oneshot::channel();
            let mut finished = Some(finished);
            let unsupported = if version == 0 {
                json!({"sessionUpdate":"state_update", "state":"running"})
            } else {
                json!({"sessionUpdate":"current_mode_update", "currentModeId":"default"})
            };
            let valid = json!({"sessionUpdate":"session_message", "messageId":"survives"});
            let future = json!({"sessionUpdate":"future_update"});
            // V2's official Other variant preserves unknown and v1-only updates.
            // An invalid discriminator is genuinely unrepresentable in either schema.
            let expected = if version == 0 {
                vec![("child".into(), valid.clone())]
            } else {
                vec![
                    ("child".into(), future.clone()),
                    ("child".into(), unsupported.clone()),
                    ("child".into(), valid.clone()),
                ]
            };
            for update in [
                future,
                unsupported,
                json!({"sessionUpdate": 12}),
                valid.clone(),
            ] {
                tx.send(("child".into(), update)).unwrap();
            }
            let mut sent = Vec::new();
            with_updates(
                async {
                    finish.await.unwrap();
                },
                Some(rx),
                |id, update| {
                    if let Some(update) = project(update) {
                        let complete =
                            update.get("messageId").and_then(Value::as_str) == Some("survives");
                        sent.push((id, update));
                        if complete {
                            finished.take().unwrap().send(()).unwrap();
                        }
                    }
                    Ok(())
                },
            )
            .await;
            assert_eq!(sent, expected);
        }
    }

    #[tokio::test]
    async fn forwarding_preserves_nested_order_and_directed_messages() {
        let (tx, rx) = broadcast::channel(8);
        let updates = [
            (
                "parent",
                json!({"sessionUpdate":"subagent_update","sessionId":"child"}),
            ),
            (
                "child",
                json!({"sessionUpdate":"subagent_update","sessionId":"nested"}),
            ),
            (
                "child",
                json!({"sessionUpdate":"session_message","messageId":"message","senderSessionId":"child","recipientSessionId":"nested"}),
            ),
            (
                "nested",
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}),
            ),
        ];
        for (id, update) in &updates {
            assert!(tx.send(((*id).to_owned(), update.clone())).is_ok());
        }
        let mut sent = Vec::new();
        with_updates(async {}, Some(rx), |session_id, update| {
            sent.push((session_id, update));
            Ok(())
        })
        .await;
        assert_eq!(sent, updates.map(|(id, update)| (id.to_owned(), update)));
    }

    #[tokio::test]
    async fn unsupported_clients_receive_no_subagent_updates() {
        with_updates(async {}, None, |_, _| panic!("unexpected update")).await;
    }
}
