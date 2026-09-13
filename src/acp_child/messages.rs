//! Text projection of v2 message-ID-keyed append and replacement updates.
use std::collections::HashMap;

use agent_client_protocol::schema::{MaybeUndefined, v2};
use serde_json::Value;

/// Each ID has one stable first-seen position and its current text. Notification
/// handling is the only writer; settlement drains a private cloned output. All
/// transitions use owned schema values, with no callbacks or awaits under locks.
#[derive(Clone, Debug, Default)]
pub(super) struct Messages {
    entries: HashMap<String, (usize, String)>,
}

pub(super) fn parse(
    update: &Value,
) -> Result<Option<v2::SessionUpdate>, agent_client_protocol::Error> {
    let kind = update["sessionUpdate"].as_str();
    if kind != Some("agent_message")
        && !(kind == Some("agent_message_chunk") && update.get("messageId").is_some())
    {
        return Ok(None);
    }
    Ok(Some(serde_json::from_value(update.clone())?))
}

impl Messages {
    /// Returns false for non-text chunks, which retain the existing rich-output
    /// projection. Whole-message content is a replacement, not another chunk.
    pub(super) fn record(&mut self, update: v2::SessionUpdate) -> bool {
        let (id, replacement, text, handled) = match update {
            v2::SessionUpdate::AgentMessage(message) => {
                let text = match message.content {
                    MaybeUndefined::Undefined => None,
                    MaybeUndefined::Null => Some(String::new()),
                    MaybeUndefined::Value(content) => Some(
                        content
                            .into_iter()
                            .filter_map(|block| {
                                if let v2::ContentBlock::Text(text) = block {
                                    Some(text.text)
                                } else {
                                    None
                                }
                            })
                            .collect::<String>(),
                    ),
                };
                (message.message_id.to_string(), true, text, true)
            }
            v2::SessionUpdate::AgentMessageChunk(chunk) => {
                let text = if let v2::ContentBlock::Text(text) = chunk.content {
                    Some(text.text)
                } else {
                    None
                };
                let handled = text.is_some();
                (chunk.message_id.to_string(), false, text, handled)
            }
            _ => return false,
        };
        let position = self.entries.len();
        let (_, current) = self
            .entries
            .entry(id)
            .or_insert_with(|| (position, String::new()));
        if let Some(text) = text {
            if replacement {
                *current = text;
            } else {
                current.push_str(&text);
            }
        }
        handled
    }

    pub(super) fn finish(&mut self) -> String {
        let mut entries: Vec<_> = std::mem::take(&mut self.entries).into_values().collect();
        entries.sort_by_key(|(position, _)| *position);
        entries.into_iter().map(|(_, text)| text).collect()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whole_messages_replace_preserve_clear_and_append_by_id() {
        let mut messages = Messages::default();
        let updates = [
            json!({"sessionUpdate":"agent_message", "messageId":"a", "content":[{"type":"text","text":"whole answer"}]}),
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"b", "content":{"type":"text","text":"second"}}),
            json!({"sessionUpdate":"agent_message", "messageId":"a", "content":[{"type":"text","text":"replacement"}]}),
            json!({"sessionUpdate":"agent_message", "messageId":"a"}),
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"a", "content":{"type":"text","text":" tail"}}),
            json!({"sessionUpdate":"agent_message", "messageId":"b", "content":null}),
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"b", "content":{"type":"text","text":"after clear"}}),
            json!({"sessionUpdate":"agent_message", "messageId":"b", "content":[]}),
        ];
        for update in updates {
            assert!(messages.record(parse(&update).unwrap().unwrap()));
        }
        assert_eq!(messages.finish(), "replacement tail");
        assert_eq!(messages.finish(), "");
    }

    #[test]
    fn non_text_first_chunk_reserves_message_order() {
        let mut messages = Messages::default();
        let image = json!({"sessionUpdate":"agent_message_chunk", "messageId":"a", "content":{"type":"image","data":"aGVsbG8=","mimeType":"image/png"}});
        assert!(!messages.record(parse(&image).unwrap().unwrap()));
        for update in [
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"b", "content":{"type":"text","text":"second"}}),
            json!({"sessionUpdate":"agent_message", "messageId":"a", "content":[{"type":"text","text":"first"}]}),
        ] {
            assert!(messages.record(parse(&update).unwrap().unwrap()));
        }
        assert_eq!(messages.finish(), "firstsecond");
    }

    #[test]
    fn whole_answer_and_interleaved_chunks_keep_first_seen_order() {
        let mut messages = Messages::default();
        for update in [
            json!({"sessionUpdate":"agent_message", "messageId":"z", "content":[{"type":"text","text":"first"}]}),
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"a", "content":{"type":"text","text":"second"}}),
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"z", "content":{"type":"text","text":" tail"}}),
        ] {
            assert!(messages.record(parse(&update).unwrap().unwrap()));
        }
        assert_eq!(messages.finish(), "first tailsecond");
        assert!(parse(&json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"legacy"}})).unwrap().is_none());
    }
}
