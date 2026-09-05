//! Versioned checkout provenance inside the existing bootstrap metadata.
//!
//! Metadata describes an intended branch, not a successful creation. Only a
//! complete, matching replacement record is completion evidence. Reads use the
//! normal authority selection/record validation and never rewrite history.

use super::*;
use crate::{ReasoningEffort, provider::ModelSelection};

pub(crate) const METADATA_KEY: &str = "dev.kit.session.prompt_checkout";
const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Boundary {
    /// Index in `load_history`, including states preceding compaction.
    pub state_index: usize,
    /// Number of items in the retained prefix (exclusive for a selected user
    /// prompt, inclusive for a selected assistant answer or closed tool batch).
    pub prefix_len: usize,
    /// BLAKE3 of the original parent prefix, including its metadata/timestamps.
    pub prefix_hash: String,
}

impl Boundary {
    pub(crate) fn new(state_index: usize, prefix: &[Item]) -> Result<Self, String> {
        if prefix.is_empty() {
            return Err("checkout boundary requires a bootstrap prefix".into());
        }
        Ok(Self {
            state_index,
            prefix_len: prefix.len(),
            prefix_hash: digest(prefix)?,
        })
    }
}

/// Canonical IDs from the actual runtime types, not UI labels or adapter defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapturedSelection {
    pub provider: String,
    pub model: String,
    /// `default` is explicit: absence is not silently interpreted as default.
    pub reasoning: String,
}

impl CapturedSelection {
    pub(crate) fn new(model: &ModelSelection, reasoning: Option<ReasoningEffort>) -> Self {
        Self {
            provider: model.provider.as_str().into(),
            model: model.model.clone(),
            reasoning: reasoning.map_or("default", ReasoningEffort::as_str).into(),
        }
    }

    pub(crate) fn resolve(&self) -> Result<(ModelSelection, Option<ReasoningEffort>), String> {
        Ok((
            ModelSelection::from_id(&format!("{}:{}", self.provider, self.model))?,
            ReasoningEffort::from_id(&self.reasoning)?,
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmittedRequest {
    pub id: String,
    pub selection: CapturedSelection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Completion {
    pub prefix_len: usize,
    /// Hash of every retained item, with only this payload removed.
    pub prefix_hash: String,
    /// Hash of the full submitted prompt Item, including attachments/metadata.
    pub prompt_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchMetadata {
    pub version: u32,
    pub parent_session_id: String,
    pub boundary: Boundary,
    pub checkout_id: String,
    pub request: SubmittedRequest,
    pub completion: Completion,
}

impl BranchMetadata {
    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err(format!(
                "unsupported prompt checkout metadata version {}",
                self.version
            ));
        }
        validate_id(&self.parent_session_id)?;
        if self.checkout_id.trim().is_empty() || self.request.id.trim().is_empty() {
            return Err(
                "prompt checkout and submitted request identities must not be empty".into(),
            );
        }
        if self.boundary.prefix_len == 0 || self.completion.prefix_len != self.boundary.prefix_len {
            return Err("invalid prompt checkout prefix length".into());
        }
        for hash in [
            &self.boundary.prefix_hash,
            &self.completion.prefix_hash,
            &self.completion.prompt_hash,
        ] {
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("invalid prompt checkout content hash".into());
            }
        }
        self.request.selection.resolve()?;
        Ok(())
    }

    /// Missing metadata is valid for legacy/root sessions. Present invalid data
    /// is always an error, including explicit null and unknown nested fields.
    pub(crate) fn read(transcript: &[Item]) -> Result<Option<Self>, String> {
        if transcript
            .iter()
            .skip(1)
            .any(|item| item.metadata.contains_key(METADATA_KEY))
        {
            return Err("prompt checkout metadata must be on the bootstrap item".into());
        }
        let Some(value) = transcript
            .first()
            .and_then(|item| item.metadata.get(METADATA_KEY))
        else {
            return Ok(None);
        };
        if transcript[0].kind != ItemKind::System {
            return Err("prompt checkout metadata requires a system bootstrap".into());
        }
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "invalid prompt checkout metadata version".to_string())?;
        if version != u64::from(VERSION) {
            return Err(format!(
                "unsupported prompt checkout metadata version {version}"
            ));
        }
        let metadata: Self = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid prompt checkout metadata: {error}"))?;
        metadata.validate()?;
        Ok(Some(metadata))
    }

    fn matches_snapshot(&self, transcript: &[Item]) -> Result<bool, String> {
        if transcript.len().checked_sub(1) != Some(self.completion.prefix_len) {
            return Ok(false);
        }
        let (prompt, prefix) = transcript.split_last().expect("nonempty checked snapshot");
        Ok(prompt.kind == ItemKind::User
            && digest(prompt)? == self.completion.prompt_hash
            && prefix_digest(prefix)? == self.completion.prefix_hash)
    }
}

/// Validate a branch reload before an opener can repair history or an adapter
/// can start. Root/legacy transcripts retain their ordinary defaults.
pub(crate) fn validate_resume(
    root: &Path,
    session_id: &str,
) -> Result<Option<BranchMetadata>, String> {
    validate_id(session_id)?;
    // Root/legacy openers already recover torn migration tails while holding
    // their locks. Inspect a bounded complete prefix here without rewriting it,
    // so a missing checkout payload does not disable that existing recovery.
    let authority = select_authority_with(
        &default_directory()?,
        &canonical_workspace(root),
        session_id,
        true,
        true,
    )?
    .ok_or_else(|| format!("session {session_id:?} does not exist"))?;
    for state in &authority.historical_items {
        BranchMetadata::read(state)?;
    }
    let Some(metadata) = BranchMetadata::read(&authority.items)? else {
        return Ok(None);
    };
    let committed = lookup_committed(
        root,
        session_id,
        &metadata.checkout_id,
        &metadata.request.id,
    )?
    .ok_or_else(|| format!("session {session_id:?} has an incomplete prompt checkout"))?;
    if committed.metadata != metadata {
        return Err(format!(
            "session {session_id:?} prompt checkout metadata differs from its completion"
        ));
    }
    Ok(Some(metadata))
}

/// A plain fork has no checkout completion of its own. Validate inherited
/// metadata before removing its request identity from the new child only.
pub(crate) fn strip_for_plain_fork(transcript: &mut [Item]) -> Result<(), String> {
    BranchMetadata::read(transcript)?;
    if let Some(bootstrap) = transcript.first_mut() {
        bootstrap.metadata.remove(METADATA_KEY);
    }
    Ok(())
}

/// Prepare the exact initial transcript to pass to `open_uncommitted`.
/// The caller selects a validated boundary and sanitizes session-bound provider
/// continuation metadata before this call. The inherited checkout payload is
/// replaced, never merged. Historical unknown timestamps remain unknown.
pub(crate) fn prepare(
    mut prefix: Vec<Item>,
    parent_session_id: String,
    boundary: Boundary,
    checkout_id: String,
    request: SubmittedRequest,
    mut prompt: Item,
) -> Result<Vec<Item>, String> {
    if prefix.is_empty() || prefix[0].kind != ItemKind::System || prompt.kind != ItemKind::User {
        return Err("prompt checkout requires a system bootstrap and a user prompt".into());
    }
    // Validate inherited data before replacing it; malformed ancestry is not legacy.
    BranchMetadata::read(&prefix)?;
    stamp_item(&mut prompt, Timestamp::now());
    let metadata = BranchMetadata {
        version: VERSION,
        parent_session_id,
        boundary,
        checkout_id,
        request,
        completion: Completion {
            prefix_len: prefix.len(),
            prefix_hash: prefix_digest(&prefix)?,
            prompt_hash: digest(&prompt)?,
        },
    };
    metadata.validate()?;
    prefix[0].metadata.insert(
        METADATA_KEY.into(),
        serde_json::to_value(metadata)
            .map_err(|error| format!("could not encode prompt checkout metadata: {error}"))?,
    );
    prefix.push(prompt);
    Ok(prefix)
}

fn digest(value: &(impl Serialize + ?Sized)) -> Result<String, String> {
    // Going through Value canonicalizes map order before hashing, independent of
    // runtime MetadataMap insertion order and JSON object order on disk.
    let mut value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn prefix_digest(prefix: &[Item]) -> Result<String, String> {
    let mut prefix = prefix.to_vec();
    if let Some(bootstrap) = prefix.first_mut() {
        bootstrap.metadata.remove(METADATA_KEY);
    }
    digest(&prefix)
}

pub(crate) fn load_history(root: &Path, session_id: &str) -> Result<Vec<Vec<Item>>, String> {
    load_history_in(root, &default_directory()?, session_id)
}

pub(crate) fn load_history_in(
    root: &Path,
    directory: &Path,
    session_id: &str,
) -> Result<Vec<Vec<Item>>, String> {
    validate_id(session_id)?;
    let authority = select_authority(directory, &canonical_workspace(root), session_id)?
        .ok_or_else(|| format!("session {session_id:?} does not exist"))?;
    for state in &authority.historical_items {
        BranchMetadata::read(state)?;
    }
    // Do not repair or stamp items: boundary provenance refers to exact stored states.
    Ok(authority.historical_items)
}

/// Commit only a newly opened, still-guarded branch. Appending a bootstrap (or
/// even all initial items) is not completion. Keep the creation guard armed until
/// the matching replacement has crossed the branch-scoped disk barrier.
pub(crate) fn commit(observer: &SessionObserver, transcript: &[Item]) -> Result<(), String> {
    commit_with_barrier(observer, transcript, |filesystem, path| {
        filesystem.require_disk(path)
    })
}

fn commit_with_barrier(
    observer: &SessionObserver,
    transcript: &[Item],
    barrier: impl FnOnce(&Fs, &Path) -> io::Result<()>,
) -> Result<(), String> {
    let metadata = BranchMetadata::read(transcript)?
        .ok_or_else(|| "missing prompt checkout metadata".to_string())?;
    if !metadata.matches_snapshot(transcript)? {
        return Err("incomplete or mismatched prompt checkout transcript".into());
    }
    let mut writer = observer
        .0
        .lock()
        .map_err(|_| "session transcript writer poisoned".to_string())?;
    if writer.created.is_none() {
        return Err("prompt checkout commit requires an uncommitted new session".into());
    }
    if metadata.parent_session_id == writer.session_id {
        return Err("prompt checkout cannot parent itself".into());
    }
    writer.ensure_lock()?;
    let StoredTranscript::History(history) = read_records_direct(&writer.path, &writer.session_id)?
    else {
        return Err("prompt checkout transcript is a redirect".into());
    };
    if history.items != transcript {
        return Err("prompt checkout initial transcript differs from commit snapshot".into());
    }
    writer.replace(transcript)?;
    barrier(&writer.lock.filesystem()?, &writer.path)
        .map_err(|error| format!("prompt checkout is not durable: {error}"))?;
    writer.commit_creation();
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct CommittedBranch {
    pub session_id: String,
    pub metadata: BranchMetadata,
    /// The committed prefix + submitted prompt, not later assistant output.
    pub transcript: Vec<Item>,
}

/// Restart-safe idempotency lookup. None means absent, legacy/root, a different
/// request, or incomplete initialization. Corrupt/unknown metadata is an error.
/// Later appends and compaction do not erase an earlier commit replacement.
pub(crate) fn lookup_committed(
    root: &Path,
    session_id: &str,
    checkout_id: &str,
    request_id: &str,
) -> Result<Option<CommittedBranch>, String> {
    lookup_committed_in(
        root,
        &default_directory()?,
        session_id,
        checkout_id,
        request_id,
    )
}

pub(crate) fn lookup_committed_in(
    root: &Path,
    directory: &Path,
    session_id: &str,
    checkout_id: &str,
    request_id: &str,
) -> Result<Option<CommittedBranch>, String> {
    lookup_committed_with_parent_in(root, directory, session_id, checkout_id, request_id, None)
}

fn lookup_committed_with_parent_in(
    root: &Path,
    directory: &Path,
    session_id: &str,
    checkout_id: &str,
    request_id: &str,
    parent_session_id: Option<&str>,
) -> Result<Option<CommittedBranch>, String> {
    validate_id(session_id)?;
    let Some(authority) = select_authority_with(
        directory,
        &canonical_workspace(root),
        session_id,
        true,
        true,
    )?
    else {
        return Ok(None);
    };
    // Discovery validates unrelated metadata but must not demand durability
    // from unrelated sessions. Only candidates for this token cross a barrier.
    let mut candidate = false;
    for state in &authority.historical_items {
        if let Some(metadata) = BranchMetadata::read(state)?
            && metadata.checkout_id == checkout_id
        {
            candidate = true;
        }
    }
    if !candidate {
        return Ok(None);
    }
    // An overlay-only record cannot establish completion, even in this process.
    fs::require_disk(&authority.path)
        .map_err(|error| format!("prompt checkout is not durable: {error}"))?;
    // A later interrupted append does not undo a complete replacement. Read a
    // bounded snapshot after the disk barrier; only the locked opener may
    // truncate its incomplete final record. Partial replacements cannot commit.
    let bytes = transcript_snapshot(&authority.path, true)?;
    let StoredTranscript::History(history) =
        read_records_bytes(&authority.path, session_id, &bytes)?
    else {
        return Err("prompt checkout authority is a redirect".into());
    };
    for state in &history.states {
        if let Some(metadata) = BranchMetadata::read(state)?
            && metadata.checkout_id == checkout_id
            && let Some(parent) = parent_session_id
            && (metadata.parent_session_id != parent || metadata.request.id != request_id)
        {
            // An initialized but not yet committed child also binds this token.
            // Check every state: compaction must not hide identity conflicts.
            return Err(
                "this checkout token is already bound to a different submitted request or source"
                    .into(),
            );
        }
    }
    let mut found: Option<CommittedBranch> = None;
    for (state_index, length) in history.replacement_boundaries {
        let snapshot = &history.states[state_index][..length];
        let Some(metadata) = BranchMetadata::read(snapshot)? else {
            continue;
        };
        if metadata.parent_session_id == session_id {
            return Err("prompt checkout cannot parent itself".into());
        }
        if metadata.checkout_id == checkout_id
            && metadata.request.id == request_id
            && metadata.matches_snapshot(snapshot)?
        {
            if found
                .as_ref()
                .is_some_and(|previous| previous.transcript != snapshot)
            {
                return Err("conflicting prompt checkout completion records".into());
            }
            found = Some(CommittedBranch {
                session_id: session_id.into(),
                metadata,
                transcript: snapshot.to_vec(),
            });
        }
    }
    Ok(found)
}

/// Discover a committed child when a process restart lost the destination ID.
/// New checkout branches are workspace-scoped; no sidecar/index is required.
/// Multiple durable children for one request are an explicit conflict. Reusing
/// a checkout token with another request or parent is an error, including when
/// its original child is only partially initialized.
pub(crate) fn find_committed(
    root: &Path,
    parent_session_id: &str,
    checkout_id: &str,
    request_id: &str,
) -> Result<Option<CommittedBranch>, String> {
    find_committed_in(
        root,
        &default_directory()?,
        parent_session_id,
        checkout_id,
        request_id,
    )
}

fn find_committed_in(
    root: &Path,
    directory: &Path,
    parent_session_id: &str,
    checkout_id: &str,
    request_id: &str,
) -> Result<Option<CommittedBranch>, String> {
    validate_id(parent_session_id)?;
    let scoped = workspace_storage_directory(directory, &canonical_workspace(root));
    let mut found = None;
    for session_id in list_ids_in(&scoped)? {
        if let Some(committed) = lookup_committed_with_parent_in(
            root,
            directory,
            &session_id,
            checkout_id,
            request_id,
            Some(parent_session_id),
        )? {
            if found.is_some() {
                return Err("multiple committed prompt checkout children for one request".into());
            }
            found = Some(committed);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn prepared() -> Vec<Item> {
        let prefix = vec![Item::text(ItemKind::System, "bootstrap")];
        prepare(
            prefix.clone(),
            "parent".into(),
            Boundary::new(0, &prefix).unwrap(),
            "checkout-1".into(),
            SubmittedRequest {
                id: "request-1".into(),
                selection: CapturedSelection::new(
                    &ModelSelection::new(crate::ProviderKind::OpenRouter, "provider/model"),
                    Some(ReasoningEffort::High),
                ),
            },
            Item::text(ItemKind::User, "submitted prompt"),
        )
        .unwrap()
    }

    fn open_branch(root: &Path, initial: Vec<Item>) -> OpenSession {
        open_with_initial_timestamps_in(
            root,
            &root.join("sessions"),
            "branch",
            false,
            false,
            initial,
            InitialTranscriptOptions {
                stamp_items: false,
                commit_creation: false,
            },
        )
        .unwrap()
    }

    fn path(root: &Path) -> PathBuf {
        transcript_path(
            &workspace_storage_directory(&root.join("sessions"), &canonical_workspace(root)),
            "branch",
        )
    }

    fn lookup(root: &Path) -> Result<Option<CommittedBranch>, String> {
        lookup_committed_in(
            root,
            &root.join("sessions"),
            "branch",
            "checkout-1",
            "request-1",
        )
    }

    #[test]
    fn current_metadata_round_trips_and_missing_legacy_is_valid() {
        let transcript = prepared();
        let metadata = BranchMetadata::read(&transcript).unwrap().unwrap();
        assert_eq!(metadata.version, 1);
        assert_eq!(metadata.parent_session_id, "parent");
        assert!(metadata.matches_snapshot(&transcript).unwrap());
        assert_eq!(
            metadata.request.selection.resolve().unwrap().1,
            Some(ReasoningEffort::High)
        );
        assert_eq!(transcript[0].created_at, None);
        let mut legacy = transcript;
        legacy[0].metadata.remove(METADATA_KEY);
        assert!(BranchMetadata::read(&legacy).unwrap().is_none());
        assert!(BranchMetadata::read(&[]).unwrap().is_none());
    }

    #[test]
    fn malformed_and_unknown_nested_payloads_are_explicit_errors() {
        let transcript = prepared();
        let good = transcript[0].metadata[METADATA_KEY].clone();
        let mut misplaced = transcript.clone();
        misplaced[0].metadata.remove(METADATA_KEY);
        misplaced[1]
            .metadata
            .insert(METADATA_KEY.into(), good.clone());
        assert!(BranchMetadata::read(&misplaced).is_err());
        let mut wrong_bootstrap = transcript.clone();
        wrong_bootstrap[0].kind = ItemKind::User;
        assert!(BranchMetadata::read(&wrong_bootstrap).is_err());
        for value in [
            serde_json::Value::Null,
            json!({}),
            json!({"version": 2}),
            json!({"version": "1"}),
        ] {
            let mut invalid = transcript.clone();
            invalid[0].metadata.insert(METADATA_KEY.into(), value);
            assert!(BranchMetadata::read(&invalid).is_err());
        }
        for pointer in [
            "",
            "/boundary",
            "/request",
            "/request/selection",
            "/completion",
        ] {
            let mut value = good.clone();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown".into(), json!(true));
            let mut invalid = transcript.clone();
            invalid[0].metadata.insert(METADATA_KEY.into(), value);
            assert!(
                BranchMetadata::read(&invalid)
                    .unwrap_err()
                    .contains("unknown field")
            );
        }
        for pointer in [
            "/request/selection/reasoning",
            "/request/selection/provider",
            "/completion/prefix_hash",
        ] {
            let mut value = good.clone();
            *value.pointer_mut(pointer).unwrap() = json!("bogus");
            let mut invalid = transcript.clone();
            invalid[0].metadata.insert(METADATA_KEY.into(), value);
            assert!(BranchMetadata::read(&invalid).is_err());
        }
        let mut value = good;
        value["request"]["selection"]
            .as_object_mut()
            .unwrap()
            .remove("reasoning");
        let mut invalid = transcript;
        invalid[0].metadata.insert(METADATA_KEY.into(), value);
        assert!(BranchMetadata::read(&invalid).is_err());
    }

    #[test]
    fn descendants_replace_inherited_payload_and_preserve_other_metadata() {
        let mut prefix = prepared();
        prefix[0].metadata.insert("unrelated".into(), json!(true));
        let boundary = Boundary::new(3, &prefix).unwrap();
        let request = SubmittedRequest {
            id: "descendant-request".into(),
            selection: CapturedSelection::new(
                &ModelSelection::new(crate::ProviderKind::Speakeasy, "model"),
                None,
            ),
        };
        let descendant = prepare(
            prefix,
            "branch".into(),
            boundary.clone(),
            "descendant-checkout".into(),
            request,
            Item::text(ItemKind::User, "next"),
        )
        .unwrap();
        let metadata = BranchMetadata::read(&descendant).unwrap().unwrap();
        assert_eq!(metadata.parent_session_id, "branch");
        assert_eq!(metadata.boundary, boundary);
        assert_eq!(metadata.checkout_id, "descendant-checkout");
        assert_eq!(metadata.request.id, "descendant-request");
        assert_eq!(metadata.request.selection.reasoning, "default");
        assert_eq!(descendant[0].metadata["unrelated"], json!(true));
        assert!(metadata.matches_snapshot(&descendant).unwrap());
    }

    #[test]
    fn writer_uses_existing_replacement_shape_and_restart_lookup_survives_compaction() {
        let root = tempfile::tempdir().unwrap();
        let transcript = prepared();
        let opened = open_branch(root.path(), transcript.clone());
        assert!(
            lookup(root.path()).unwrap().is_none(),
            "initial appends are not a commit"
        );
        commit(&opened.observer, &opened.transcript).unwrap();
        let bytes = std::fs::read(path(root.path())).unwrap();
        let records: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            records
                .iter()
                .all(|record| record["schema_version"] == SCHEMA_VERSION)
        );
        let last = records.last().unwrap();
        assert!(last.get("replacement").is_some());
        assert!(last.get("item").is_none());
        assert_eq!(last.as_object().unwrap().len(), 5);
        assert_eq!(
            last["replacement"][0]["metadata"][METADATA_KEY]["version"],
            1
        );
        opened
            .observer
            .replace(&[Item::text(ItemKind::System, "compacted")])
            .unwrap();
        drop(opened);
        let before = std::fs::read(path(root.path())).unwrap();
        let committed = lookup(root.path()).unwrap().unwrap();
        assert_eq!(committed.transcript, transcript);
        assert_eq!(committed.session_id, "branch");
        assert_eq!(committed.metadata.request.id, "request-1");
        assert_eq!(
            std::fs::read(path(root.path())).unwrap(),
            before,
            "lookup must not rewrite"
        );
        assert!(
            lookup_committed_in(
                root.path(),
                &root.path().join("sessions"),
                "branch",
                "checkout-1",
                "other"
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn committed_lookup_ignores_only_torn_final_appends_and_opener_recovers() {
        for kind in [ItemKind::Assistant, ItemKind::Tool] {
            let root = tempfile::tempdir().unwrap();
            let transcript = prepared();
            let opened = open_branch(root.path(), transcript.clone());
            commit(&opened.observer, &opened.transcript).unwrap();
            drop(opened);
            let path = path(root.path());
            let complete = std::fs::read(&path).unwrap();
            let append = Record {
                schema_version: SCHEMA_VERSION,
                session_id: "branch".into(),
                generation: 4,
                workspace_root: Some(canonical_workspace(root.path())),
                item: Some(Item::text(kind, "later output")),
                replacement: None,
                redirect: None,
            };
            let encoded = serde_json::to_vec(&append).unwrap();
            let mut torn = complete.clone();
            torn.extend_from_slice(&encoded[..encoded.len() - 2]);
            std::fs::write(&path, &torn).unwrap();
            for _ in 0..2 {
                let recovered = lookup(root.path()).unwrap().unwrap();
                assert_eq!(recovered.session_id, "branch");
                assert_eq!(recovered.transcript, transcript);
                let discovered = find_committed_in(
                    root.path(),
                    &root.path().join("sessions"),
                    "parent",
                    "checkout-1",
                    "request-1",
                )
                .unwrap()
                .unwrap();
                assert_eq!(discovered.session_id, recovered.session_id);
                assert_eq!(discovered.metadata, recovered.metadata);
                assert_eq!(discovered.transcript, recovered.transcript);
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    torn,
                    "lookup must not repair"
                );
            }
            let opened = open_in(
                root.path(),
                &root.path().join("sessions"),
                "branch",
                true,
                false,
                Vec::new(),
            )
            .unwrap();
            assert_eq!(opened.transcript, transcript);
            drop(opened);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                complete,
                "only the locked opener removes the torn append, without a generation rerun"
            );
        }
    }

    #[test]
    fn committed_lookup_and_opener_reject_malformed_complete_records() {
        for tail in [
            b"{}".as_slice(),
            b"{}\n",
            b"{not json}",
            b"{not json}\n",
            b"{}\n{\"schema_version\":",
            b"{not json\n{\"schema_version\":",
            b"{not json\xe2",
            b"{\"schema_version\":\n",
        ] {
            let root = tempfile::tempdir().unwrap();
            let opened = open_branch(root.path(), prepared());
            commit(&opened.observer, &opened.transcript).unwrap();
            drop(opened);
            let path = path(root.path());
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.extend_from_slice(tail);
            std::fs::write(&path, &bytes).unwrap();
            assert!(lookup(root.path()).is_err(), "tail: {tail:?}");
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(
                open_in(
                    root.path(),
                    &root.path().join("sessions"),
                    "branch",
                    true,
                    false,
                    Vec::new()
                )
                .is_err(),
                "tail: {tail:?}"
            );
            // Recovery must not erase a malformed complete final record.
            if !tail.windows(2).any(|pair| pair == b"\n{") {
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn torn_initial_or_completion_record_does_not_establish_completion() {
        let root = tempfile::tempdir().unwrap();
        let opened = open_branch(root.path(), prepared());
        let initial = std::fs::read(path(root.path())).unwrap();
        commit(&opened.observer, &opened.transcript).unwrap();
        drop(opened);
        let complete = std::fs::read(path(root.path())).unwrap();
        // Exercise every byte boundary, including an empty initial record.
        // No partial replacement can prove the commit.
        for length in 0..complete.len() - 1 {
            std::fs::write(path(root.path()), &complete[..length]).unwrap();
            assert!(
                !matches!(lookup(root.path()), Ok(Some(_))),
                "length {length}"
            );
            assert_eq!(
                std::fs::read(path(root.path())).unwrap(),
                complete[..length]
            );
        }
        assert!(initial.len() < complete.len() - 2);
        // A fully present replacement is valid even if only its newline tore.
        std::fs::write(path(root.path()), &complete[..complete.len() - 1]).unwrap();
        assert!(lookup(root.path()).unwrap().is_some());
    }

    #[test]
    fn newline_only_torn_completion_survives_resume_append_and_reload() {
        let root = tempfile::tempdir().unwrap();
        let transcript = prepared();
        let opened = open_branch(root.path(), transcript.clone());
        commit(&opened.observer, &opened.transcript).unwrap();
        drop(opened);
        let path = path(root.path());
        let complete = std::fs::read(&path).unwrap();
        assert_eq!(complete.last(), Some(&b'\n'));
        let unterminated = &complete[..complete.len() - 1];
        std::fs::write(&path, unterminated).unwrap();
        assert_eq!(lookup(root.path()).unwrap().unwrap().transcript, transcript);
        assert_eq!(std::fs::read(&path).unwrap(), unterminated);

        let resumed = open_in(
            root.path(),
            &root.path().join("sessions"),
            "branch",
            true,
            false,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(resumed.transcript, transcript);
        assert_eq!(std::fs::read(&path).unwrap(), complete);
        let appended = Item::text(ItemKind::Assistant, "after resume")
            .with_created_at(agentkit_core::Timestamp(123));
        resumed.observer.on_transcript_event(TranscriptEvent {
            session_id: &agentkit_core::SessionId::new("branch"),
            item: &appended,
        });
        drop(resumed);

        let mut expected = transcript.clone();
        expected.push(appended);
        assert_eq!(
            load_in(root.path(), &root.path().join("sessions"), "branch").unwrap(),
            expected
        );
        let after = std::fs::read(&path).unwrap();
        assert!(after.starts_with(&complete));
        assert_eq!(
            after.iter().filter(|byte| **byte == b'\n').count(),
            complete.iter().filter(|byte| **byte == b'\n').count() + 1
        );
        assert_eq!(lookup(root.path()).unwrap().unwrap().transcript, transcript);
        assert_eq!(std::fs::read(&path).unwrap(), after);
    }

    #[test]
    fn history_includes_precompaction_and_reads_do_not_repair_or_rewrite() {
        let root = tempfile::tempdir().unwrap();
        let initial = vec![
            Item::text(ItemKind::System, "bootstrap"),
            Item::text(ItemKind::User, "old"),
        ];
        let opened = open_branch(root.path(), initial.clone());
        opened.observer.commit_creation();
        let compacted = vec![Item::text(ItemKind::System, "compacted")];
        opened.observer.replace(&compacted).unwrap();
        drop(opened);
        let before = std::fs::read(path(root.path())).unwrap();
        assert_eq!(
            load_history_in(root.path(), &root.path().join("sessions"), "branch").unwrap(),
            vec![initial, compacted]
        );
        assert_eq!(std::fs::read(path(root.path())).unwrap(), before);
    }

    #[test]
    fn partial_initialization_is_not_completion_and_guard_cleans_up() {
        for count in 1..=2 {
            let root = tempfile::tempdir().unwrap();
            let full = prepared();
            let opened = open_branch(root.path(), full[..count].to_vec());
            assert!(lookup(root.path()).unwrap().is_none());
            if count == 1 {
                assert!(commit(&opened.observer, &opened.transcript).is_err());
                assert!(commit(&opened.observer, &full).is_err());
            }
            drop(opened);
            assert!(!path(root.path()).exists());
        }
    }

    #[test]
    fn wrong_prefix_prompt_or_length_cannot_commit_or_recover() {
        for mutation in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let mut transcript = prepared();
            match mutation {
                0 => transcript[0].parts = Item::text(ItemKind::System, "wrong bootstrap").parts,
                1 => transcript[1].parts = Item::text(ItemKind::User, "wrong prompt").parts,
                _ => transcript.push(Item::text(ItemKind::User, "extra")),
            }
            let opened = open_branch(root.path(), transcript.clone());
            assert!(commit(&opened.observer, &transcript).is_err());
            // Even a well-formed replacement is insufficient without full content validation.
            opened.observer.replace(&transcript).unwrap();
            assert!(lookup(root.path()).unwrap().is_none());
        }
    }

    #[test]
    fn failed_branch_scoped_barrier_keeps_creation_guard_armed() {
        let root = tempfile::tempdir().unwrap();
        let opened = open_branch(root.path(), prepared());
        let error = commit_with_barrier(&opened.observer, &opened.transcript, |_, actual_path| {
            assert_eq!(actual_path, path(root.path()));
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        })
        .unwrap_err();
        assert!(error.contains("not durable"));
        assert!(opened.observer.0.lock().unwrap().created.is_some());
        drop(opened);
        assert!(!path(root.path()).exists());
        assert!(lookup(root.path()).unwrap().is_none());
    }

    #[test]
    fn failed_barrier_can_retry_without_duplicate_recovery_results() {
        let root = tempfile::tempdir().unwrap();
        let opened = open_branch(root.path(), prepared());
        assert!(
            commit_with_barrier(&opened.observer, &opened.transcript, |_, _| {
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            })
            .is_err()
        );
        commit(&opened.observer, &opened.transcript).unwrap();
        assert!(opened.observer.0.lock().unwrap().created.is_none());
        let expected = opened.transcript.clone();
        drop(opened);
        assert_eq!(lookup(root.path()).unwrap().unwrap().transcript, expected);
    }

    #[test]
    fn restart_discovery_finds_request_without_destination_id() {
        let root = tempfile::tempdir().unwrap();
        let opened = open_branch(root.path(), prepared());
        assert!(
            find_committed_in(
                root.path(),
                &root.path().join("sessions"),
                "parent",
                "checkout-1",
                "request-1"
            )
            .unwrap()
            .is_none()
        );
        commit(&opened.observer, &opened.transcript).unwrap();
        drop(opened);
        let found = find_committed_in(
            root.path(),
            &root.path().join("sessions"),
            "parent",
            "checkout-1",
            "request-1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(found.session_id, "branch");
        assert!(
            find_committed_in(
                root.path(),
                &root.path().join("sessions"),
                "other-parent",
                "checkout-1",
                "request-1"
            )
            .is_err()
        );
    }

    #[test]
    fn discovery_rejects_request_and_source_mismatches_even_before_commit() {
        for stage in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let full = prepared();
            let initial = if stage == 0 { full[..1].to_vec() } else { full };
            let opened = open_branch(root.path(), initial);
            if stage == 2 {
                commit(&opened.observer, &opened.transcript).unwrap();
                // The conflicting identity must remain visible in history even
                // if a later replacement has removed the current payload.
                opened
                    .observer
                    .replace(&[Item::text(ItemKind::System, "compacted")])
                    .unwrap();
            }
            for (parent, request) in [
                ("parent", "different-request"),
                ("different-parent", "request-1"),
            ] {
                let error = find_committed_in(
                    root.path(),
                    &root.path().join("sessions"),
                    parent,
                    "checkout-1",
                    request,
                )
                .unwrap_err();
                assert!(error.contains("different submitted request or source"));
            }
            assert!(
                find_committed_in(
                    root.path(),
                    &root.path().join("sessions"),
                    "parent",
                    "unrelated-checkout",
                    "request-1"
                )
                .unwrap()
                .is_none()
            );
        }
    }

    #[test]
    fn misplaced_duplicate_malformed_and_unknown_payloads_never_look_legacy() {
        let original = prepared();
        for value in [
            original[0].metadata[METADATA_KEY].clone(),
            serde_json::Value::Null,
            json!({"version": 999}),
        ] {
            for keep_bootstrap in [false, true] {
                let mut transcript = original.clone();
                if !keep_bootstrap {
                    transcript[0].metadata.remove(METADATA_KEY);
                }
                transcript[1]
                    .metadata
                    .insert(METADATA_KEY.into(), value.clone());
                assert!(
                    BranchMetadata::read(&transcript)
                        .unwrap_err()
                        .contains("bootstrap")
                );
            }
            let mut transcript = original.clone();
            transcript[0].kind = ItemKind::User;
            transcript[0].metadata.insert(METADATA_KEY.into(), value);
            assert!(
                BranchMetadata::read(&transcript)
                    .unwrap_err()
                    .contains("system bootstrap")
            );
        }
        assert!(
            BranchMetadata::read(&[Item::text(ItemKind::User, "legacy without bootstrap")])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn later_appends_cannot_complete_a_short_replacement_record() {
        let root = tempfile::tempdir().unwrap();
        let full = prepared();
        let opened = open_branch(root.path(), full[..1].to_vec());
        opened.observer.replace(&full[..1]).unwrap();
        opened.observer.0.lock().unwrap().append(&full[1]).unwrap();
        // Canonical history now equals the intended branch. But no single
        // replacement record contains its entire prefix and submitted prompt.
        assert_eq!(read_records(&path(root.path()), "branch").unwrap().0, full);
        assert!(lookup(root.path()).unwrap().is_none());
        opened.observer.commit_creation(); // Model a surviving partial initialization.
        drop(opened);
        assert!(lookup(root.path()).unwrap().is_none());
    }

    #[test]
    fn legacy_history_is_read_without_stamp_or_schema_rewrite() {
        let root = tempfile::tempdir().unwrap();
        let initial = vec![
            Item::text(ItemKind::System, "old bootstrap"),
            Item::text(ItemKind::User, "old prompt"),
        ];
        let opened = open_branch(root.path(), initial.clone());
        opened.observer.commit_creation();
        let compacted = vec![Item::text(ItemKind::System, "old compaction")];
        opened.observer.replace(&compacted).unwrap();
        drop(opened);
        let file = path(root.path());
        let mut records: Vec<serde_json::Value> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for record in &mut records {
            record["schema_version"] = json!(if record.get("replacement").is_some() {
                PREVIOUS_SCHEMA_VERSION
            } else {
                LEGACY_SCHEMA_VERSION
            });
        }
        let bytes = records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        std::fs::write(&file, &bytes).unwrap();
        assert_eq!(
            load_history_in(root.path(), &root.path().join("sessions"), "branch").unwrap(),
            vec![initial, compacted]
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), bytes);
        assert!(lookup(root.path()).unwrap().is_none());
    }

    #[test]
    fn lookup_rejects_corrupt_generation_and_does_not_accept_torn_replacement() {
        let root = tempfile::tempdir().unwrap();
        let opened = open_branch(root.path(), prepared());
        commit(&opened.observer, &opened.transcript).unwrap();
        drop(opened);
        let file = path(root.path());
        let original = std::fs::read(&file).unwrap();
        std::fs::write(&file, &original[..original.len() - 12]).unwrap();
        assert!(lookup(root.path()).unwrap().is_none());
        assert_eq!(
            std::fs::read(&file).unwrap(),
            original[..original.len() - 12]
        );
        let mut lines: Vec<serde_json::Value> = std::str::from_utf8(&original)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines[1]["generation"] = json!(999);
        std::fs::write(
            &file,
            lines
                .iter()
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
        )
        .unwrap();
        assert!(lookup(root.path()).is_err());
    }
}
