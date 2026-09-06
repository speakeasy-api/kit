//! Kit-private text-prompt checkout protocol. Addresses are issued by the backend;
//! neither display positions nor provider item identifiers are branch authority.

use agentkit_acp::v2::wire;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "kit/transcript/list", response = ListPromptBranchesResponse)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListPromptBranchesRequest {
    pub session_id: wire::SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
pub(crate) struct ListPromptBranchesResponse {
    pub boundaries: Vec<PromptBoundary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PromptBoundary {
    pub address: String,
    pub text: String,
    pub historical: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "kit/transcript/prepare", response = PreparePromptBranchResponse)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparePromptBranchRequest {
    pub session_id: wire::SessionId,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
pub(crate) struct PreparePromptBranchResponse {
    pub checkout_token: String,
    pub original_text: String,
    pub prefix: Vec<wire::SessionUpdate>,
    pub config_options: Vec<wire::SessionConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "kit/transcript/submit", response = SubmitPromptBranchResponse)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitPromptBranchRequest {
    pub session_id: wire::SessionId,
    pub checkout_token: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
pub(crate) struct SubmitPromptBranchResponse {
    pub session_id: wire::SessionId,
    pub config_options: Vec<wire::SessionConfigOption>,
}

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use crate::{
    provider::{ModelSelection, ReasoningEffort},
    runtime::AcpForkState,
    session::branch,
};
use agentkit_core::{Item, ItemKind, Part};

/// Actor-local authority. Dropping the actor invalidates every uncommitted
/// address and checkout, even if the same durable session is loaded again.
#[derive(Default)]
pub(crate) struct PromptCheckouts {
    boundaries: HashMap<String, ApprovedBoundary>,
    checkouts: HashMap<String, PreparedCheckout>,
}

#[derive(Clone)]
struct SourceRevision {
    transcript: Arc<Vec<Item>>,
    // Monotonic actor revision for transcript work and successful config edits.
    // Exact transcript equality alone cannot detect mutation followed by compaction.
    checkout_revision: u64,
    selection: ModelSelection,
    reasoning: Option<ReasoningEffort>,
}

impl SourceRevision {
    fn validate(
        &self,
        transcript: &[Item],
        checkout_revision: u64,
        selection: &ModelSelection,
        reasoning: Option<ReasoningEffort>,
    ) -> Result<(), String> {
        if self.transcript.as_slice() != transcript
            || self.checkout_revision != checkout_revision
            || &self.selection != selection
            || self.reasoning != reasoning
        {
            return Err(
                "stale prompt checkout: transcript or configuration changed; list prompts again"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ApprovedBoundary {
    source: SourceRevision,
    parent_session_id: String,
    provenance: branch::Boundary,
    state: Arc<Vec<Item>>,
    text: String,
}

#[derive(Clone)]
pub(crate) struct PreparedCheckout {
    pub token: String,
    pub original_text: String,
    pub prefix: Vec<Item>,
    pub selection: ModelSelection,
    pub reasoning: Option<ReasoningEffort>,
    boundary: ApprovedBoundary,
    submitted: Arc<Mutex<Option<String>>>,
}

impl PreparedCheckout {
    /// Construct only in memory. Runtime's guarded initialization owns the
    /// completion record and disk barrier, before adapter startup or execution.
    pub(crate) fn fork(&self, text: &str) -> Result<AcpForkState, String> {
        if text.trim().is_empty() {
            return Err("prompt checkout requires non-empty text".into());
        }
        let request_id = submitted_request_id(&self.boundary.parent_session_id, text);
        let mut submitted = self
            .submitted
            .lock()
            .map_err(|_| "prompt checkout identity lock poisoned")?;
        if submitted
            .as_ref()
            .is_some_and(|previous| previous != &request_id)
        {
            return Err(
                "this checkout token is already bound to a different submitted request".into(),
            );
        }
        *submitted = Some(request_id.clone());
        let mut prefix = self.prefix.clone();
        crate::transcript::sanitize_forked_transcript(&mut prefix);
        let transcript = branch::prepare(
            prefix,
            self.boundary.parent_session_id.clone(),
            self.boundary.provenance.clone(),
            self.token.clone(),
            branch::SubmittedRequest {
                id: request_id,
                selection: branch::CapturedSelection::new(&self.selection, self.reasoning),
            },
            Item::text(ItemKind::User, text),
        )?;
        Ok(AcpForkState {
            transcript,
            selection: self.selection.clone(),
            reasoning_effort: self.reasoning,
            parent_context: None,
        })
    }
}

impl PromptCheckouts {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn list(
        &mut self,
        root: &Path,
        session_id: &str,
        transcript: &[Item],
        checkout_revision: u64,
        selection: &ModelSelection,
        reasoning: Option<ReasoningEffort>,
    ) -> Result<ListPromptBranchesResponse, String> {
        let states = branch::load_history(root, session_id)?;
        self.list_states(
            session_id,
            transcript,
            &states,
            checkout_revision,
            selection,
            reasoning,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn list_states(
        &mut self,
        session_id: &str,
        transcript: &[Item],
        states: &[Vec<Item>],
        checkout_revision: u64,
        selection: &ModelSelection,
        reasoning: Option<ReasoningEffort>,
    ) -> Result<ListPromptBranchesResponse, String> {
        // The historical loader must resolve to the actor's current canonical
        // state. Never replace missing old context with today's compacted state.
        if states.last().map(Vec::as_slice) != Some(transcript) {
            return Err("session history does not match the loaded transcript; reload and list prompts again".into());
        }
        let source = SourceRevision {
            transcript: Arc::new(transcript.to_vec()),
            checkout_revision,
            selection: selection.clone(),
            reasoning,
        };
        self.boundaries.retain(|_, boundary| {
            boundary
                .source
                .validate(transcript, checkout_revision, selection, reasoning)
                .is_ok()
        });
        self.checkouts.retain(|_, checkout| {
            checkout
                .boundary
                .source
                .validate(transcript, checkout_revision, selection, reasoning)
                .is_ok()
        });
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Current state first: unchanged prompts retained across compaction are
        // not duplicated as archived points. Every other prefix stays historical.
        for (state_index, state) in states.iter().enumerate().rev() {
            branch::BranchMetadata::read(state)?;
            if state
                .first()
                .is_none_or(|item| item.kind != ItemKind::System)
            {
                continue; // A legacy transcript without bootstrap is unreconstructable.
            }
            let state = Arc::new(state.clone());
            for (index, item) in state.iter().enumerate() {
                if item.kind != ItemKind::User {
                    continue;
                }
                let Ok(text) = text_prompt(item) else {
                    continue;
                };
                let prefix = &state[..index];
                if prefix.is_empty() || crate::transcript::has_unanswered_tool_calls(prefix) {
                    continue;
                }
                let identity =
                    serde_json::to_vec(&state[..=index]).map_err(|error| error.to_string())?;
                if !seen.insert(blake3::hash(&identity)) {
                    continue;
                }
                let provenance = branch::Boundary::new(state_index, prefix)?;
                let address = self
                    .boundaries
                    .iter()
                    .find_map(|(address, existing)| {
                        (existing.provenance == provenance && existing.text == text)
                            .then(|| address.clone())
                    })
                    .unwrap_or_else(crate::session::new_id);
                let boundary = ApprovedBoundary {
                    source: source.clone(),
                    parent_session_id: session_id.into(),
                    provenance,
                    state: Arc::clone(&state),
                    text: text.clone(),
                };
                self.boundaries.insert(address.clone(), boundary);
                result.push(PromptBoundary {
                    address,
                    text,
                    historical: state_index + 1 != states.len(),
                });
            }
        }
        Ok(ListPromptBranchesResponse { boundaries: result })
    }

    pub(crate) fn prepare(
        &mut self,
        address: &str,
        transcript: &[Item],
        checkout_revision: u64,
        selection: &ModelSelection,
        reasoning: Option<ReasoningEffort>,
    ) -> Result<PreparedCheckout, String> {
        let boundary = self
            .boundaries
            .get(address)
            .ok_or_else(|| "unknown or stale prompt address; list prompts again".to_string())?;
        boundary
            .source
            .validate(transcript, checkout_revision, selection, reasoning)?;
        let checkout = PreparedCheckout {
            token: crate::session::new_id(),
            original_text: boundary.text.clone(),
            prefix: boundary.state[..boundary.provenance.prefix_len].to_vec(),
            selection: selection.clone(),
            reasoning,
            boundary: boundary.clone(),
            submitted: Arc::new(Mutex::new(None)),
        };
        self.checkouts
            .insert(checkout.token.clone(), checkout.clone());
        Ok(checkout)
    }

    pub(crate) fn checkout(
        &self,
        token: &str,
        transcript: &[Item],
        checkout_revision: u64,
        selection: &ModelSelection,
        reasoning: Option<ReasoningEffort>,
    ) -> Result<PreparedCheckout, String> {
        let checkout = self
            .checkouts
            .get(token)
            .ok_or_else(|| "unknown or stale checkout token; list prompts again".to_string())?;
        checkout
            .boundary
            .source
            .validate(transcript, checkout_revision, selection, reasoning)?;
        Ok(checkout.clone())
    }
}

fn text_prompt(item: &Item) -> Result<String, String> {
    if item.kind != ItemKind::User || item.parts.is_empty() {
        return Err("checkout supports text-only user prompts".into());
    }
    let mut text = String::new();
    for part in &item.parts {
        let Part::Text(part) = part else {
            return Err(
                "checkout does not support media, resources, or other non-text prompt parts".into(),
            );
        };
        text.push_str(&part.text);
    }
    if text.trim().is_empty() {
        return Err("checkout requires non-empty prompt text".into());
    }
    Ok(text)
}

pub(crate) fn submitted_request_id(source_session_id: &str, text: &str) -> String {
    let bytes = serde_json::to_vec(&(source_session_id, text)).expect("string tuples serialize");
    blake3::hash(&bytes).to_hex().to_string()
}

/// Resolve durable completion before asking a source actor for admission. This
/// path also works after restart, when all in-memory addresses have expired.
pub(crate) fn lookup_committed(
    root: &Path,
    source_session_id: &str,
    token: &str,
    text: &str,
) -> Result<Option<branch::CommittedBranch>, String> {
    branch::find_committed(
        root,
        source_session_id,
        token,
        &submitted_request_id(source_session_id, text),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{DataRef, Modality, ToolCallPart};
    use serde_json::json;

    fn selection() -> ModelSelection {
        ModelSelection::new(crate::ProviderKind::OpenRouter, "test/model")
    }

    fn conversation() -> Vec<Item> {
        vec![
            Item::text(ItemKind::System, "bootstrap"),
            Item::text(ItemKind::Context, "workspace context"),
            Item::text(ItemKind::User, "first"),
            Item::text(ItemKind::Assistant, "original future"),
            Item::text(ItemKind::User, "second"),
            Item::text(ItemKind::Assistant, "second future"),
        ]
    }

    fn list(
        checkouts: &mut PromptCheckouts,
        current: &[Item],
        states: &[Vec<Item>],
    ) -> ListPromptBranchesResponse {
        checkouts
            .list_states("source", current, states, 0, &selection(), None)
            .unwrap()
    }

    #[test]
    fn exclusive_prefix_preserves_first_bootstrap_and_original_future() {
        let source = conversation();
        let before = source.clone();
        let mut checkouts = PromptCheckouts::default();
        let boundaries = list(&mut checkouts, &source, std::slice::from_ref(&source));
        assert_eq!(boundaries.boundaries.len(), 2);
        for (index, prefix_len) in [2, 4].into_iter().enumerate() {
            let prepared = checkouts
                .prepare(
                    &boundaries.boundaries[index].address,
                    &source,
                    0,
                    &selection(),
                    None,
                )
                .unwrap();
            assert_eq!(prepared.prefix, source[..prefix_len]);
            assert_eq!(
                prepared.original_text,
                if index == 0 { "first" } else { "second" }
            );
            let fork = prepared.fork("edited").unwrap();
            assert_eq!(fork.transcript.len(), prefix_len + 1);
            assert_eq!(
                text_prompt(fork.transcript.last().unwrap()).unwrap(),
                "edited"
            );
            assert_eq!(&fork.transcript[1..prefix_len], &source[1..prefix_len]);
            assert!(
                branch::BranchMetadata::read(&fork.transcript)
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(source, before);
    }

    #[test]
    fn historical_checkout_never_substitutes_future_summary() {
        let historical = conversation();
        let mut summary = Item::text(ItemKind::Developer, "FUTURE SUMMARY of everything");
        summary
            .metadata
            .insert("kit.compaction.summary".into(), true.into());
        let current = vec![
            historical[0].clone(),
            historical[1].clone(),
            summary,
            Item::text(ItemKind::User, "after compaction"),
        ];
        let states = vec![historical.clone(), current.clone()];
        let mut checkouts = PromptCheckouts::default();
        let boundaries = list(&mut checkouts, &current, &states);
        let old = boundaries
            .boundaries
            .iter()
            .find(|b| b.text == "second")
            .unwrap();
        assert!(old.historical);
        let prepared = checkouts
            .prepare(&old.address, &current, 0, &selection(), None)
            .unwrap();
        assert_eq!(prepared.prefix, historical[..4]);
        assert!(
            !serde_json::to_string(&prepared.prefix)
                .unwrap()
                .contains("FUTURE SUMMARY")
        );
        let now = boundaries
            .boundaries
            .iter()
            .find(|b| b.text == "after compaction")
            .unwrap();
        assert!(!now.historical);
    }

    #[test]
    fn read_only_lists_and_prepares_do_not_stale_existing_tokens() {
        let source = conversation();
        let mut checkouts = PromptCheckouts::default();
        let first = list(&mut checkouts, &source, std::slice::from_ref(&source));
        let prepared = checkouts
            .prepare(&first.boundaries[0].address, &source, 0, &selection(), None)
            .unwrap();
        let again = list(&mut checkouts, &source, std::slice::from_ref(&source));
        assert_eq!(first.boundaries[0].address, again.boundaries[0].address);
        checkouts
            .prepare(&first.boundaries[1].address, &source, 0, &selection(), None)
            .unwrap();
        assert!(
            checkouts
                .checkout(&prepared.token, &source, 0, &selection(), None)
                .is_ok()
        );
        assert_eq!(checkouts.boundaries.len(), 2);
        // Restart creates a fresh actor-local authority regardless of Item.id.
        assert!(
            PromptCheckouts::default()
                .checkout(&prepared.token, &source, 0, &selection(), None)
                .is_err()
        );
        assert!(source.iter().all(|item| item.id.is_none()));
    }

    #[test]
    fn transcript_checkout_revision_model_and_reasoning_changes_stale_checkout() {
        let source = conversation();
        let mut checkouts = PromptCheckouts::default();
        let boundaries = list(&mut checkouts, &source, std::slice::from_ref(&source));
        let prepared = checkouts
            .prepare(
                &boundaries.boundaries[0].address,
                &source,
                0,
                &selection(),
                None,
            )
            .unwrap();
        let mut future = source.clone();
        future.push(Item::text(ItemKind::User, "new future"));
        assert!(
            checkouts
                .checkout(&prepared.token, &future, 0, &selection(), None)
                .is_err()
        );
        assert!(
            checkouts
                .checkout(&prepared.token, &source, 1, &selection(), None)
                .is_err()
        );
        let different = ModelSelection::new(crate::ProviderKind::OpenRouter, "different/model");
        assert!(
            checkouts
                .checkout(&prepared.token, &source, 0, &different, None)
                .is_err()
        );
        assert!(
            checkouts
                .checkout(
                    &prepared.token,
                    &source,
                    0,
                    &selection(),
                    Some(ReasoningEffort::High)
                )
                .is_err()
        );
        assert!(
            checkouts
                .prepare("0", &source, 0, &selection(), None)
                .is_err()
        );
    }

    #[test]
    fn rejects_nontext_prompts_open_tool_prefix_and_missing_bootstrap() {
        let media = Item::new(
            ItemKind::User,
            vec![Part::media(
                Modality::Image,
                "image/png",
                DataRef::InlineBytes(vec![1]),
            )],
        );
        assert!(
            text_prompt(&media)
                .unwrap_err()
                .contains("media, resources")
        );
        let source = vec![
            Item::text(ItemKind::System, "bootstrap"),
            media,
            Item::new(
                ItemKind::Assistant,
                vec![Part::ToolCall(ToolCallPart::new("call", "tool", json!({})))],
            ),
            Item::text(ItemKind::User, "unresolved prefix"),
        ];
        let mut checkouts = PromptCheckouts::default();
        assert!(
            list(&mut checkouts, &source, std::slice::from_ref(&source))
                .boundaries
                .is_empty()
        );
        let legacy = vec![Item::text(ItemKind::User, "lost context")];
        assert!(
            list(&mut checkouts, &legacy, std::slice::from_ref(&legacy))
                .boundaries
                .is_empty()
        );
        assert!(
            checkouts
                .list_states("source", &source, &[legacy], 0, &selection(), None)
                .is_err()
        );
        assert!(serde_json::from_value::<SubmitPromptBranchRequest>(json!({
            "session_id":"source", "checkout_token":"token", "text":"hello", "attachments":["image"]
        })).is_err());
    }

    #[test]
    fn checkout_request_identity_survives_clones_and_rejects_different_edit() {
        let source = conversation();
        let mut checkouts = PromptCheckouts::default();
        let boundaries = list(&mut checkouts, &source, std::slice::from_ref(&source));
        let prepared = checkouts
            .prepare(
                &boundaries.boundaries[0].address,
                &source,
                0,
                &selection(),
                None,
            )
            .unwrap();
        let retry = prepared.clone();
        assert!(prepared.fork("accepted").is_ok());
        assert!(retry.fork("accepted").is_ok());
        assert!(retry.fork("different").is_err());
        assert_ne!(
            submitted_request_id("source", "x"),
            submitted_request_id("different source", "x")
        );
    }
}
