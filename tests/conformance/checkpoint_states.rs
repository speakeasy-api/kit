use agentkit_loop::{MutationPoint, PostValidationCheckpointOutcome};
use kit::agent::compaction::states::{
    CheckpointBinding, CheckpointMutationPoint, CheckpointState, CheckpointStateError,
    CheckpointStateRecord, CheckpointTransition, DriverLeaseId, DurablePromotedHead,
    HookOutcomeAction, MAX_DRIVER_LEASE_ID_BYTES, MAX_REJECTION_REASON_BYTES, RejectionReason,
    TransitionApplied, hook_outcome_action,
};
use kit::agent::driver::restart::SafeBoundary;
use kit::domain::events::{ContentDigest, SchemaVersion};
use kit::domain::ids::{AttemptId, CheckpointId, PrincipalId, RunId};
use kit::domain::lifecycle::{AttemptOwnership, FencingToken, StateVersion};
use kit::store::artifacts::{ArtifactDigest, ArtifactReference};

const LOOP_SOURCE: &str = include_str!("../../vendor/agentkit/crates/agentkit-loop/src/lib.rs");

fn digest(seed: u8) -> ContentDigest {
    let mut hex = String::with_capacity(64);
    for index in 0..32u8 {
        use std::fmt::Write as _;
        write!(hex, "{:02x}", seed.wrapping_add(index)).unwrap();
    }
    ContentDigest::parse(&format!("sha256:{hex}")).unwrap()
}

fn binding() -> CheckpointBinding {
    let payload = "0123456789abcdefghjkmnpqrs";
    CheckpointBinding {
        schema_version: SchemaVersion::CURRENT,
        checkpoint_id: CheckpointId::parse(&format!("checkpoint_{payload}")).unwrap(),
        run_id: RunId::parse(&format!("run_{payload}")).unwrap(),
        owner: AttemptOwnership::new(
            AttemptId::parse(&format!("attempt_{payload}")).unwrap(),
            PrincipalId::parse(&format!("principal_{payload}")).unwrap(),
            FencingToken::new(7),
        ),
        driver_lease_id: DriverLeaseId::parse("driver-lease-1").unwrap(),
        operation_sequence: 11,
        expected_durable_head_sequence: 4,
        base_transcript_digest: digest(1),
        candidate_transcript_digest: digest(2),
        candidate_artifact_digest: ArtifactDigest::digest(b"candidate artifact"),
        candidate_artifact_reference: ArtifactReference::parse(&format!(
            "artifact-ref:{}",
            "ab".repeat(32)
        ))
        .unwrap(),
        mutation_point: CheckpointMutationPoint::AfterToolResult,
    }
}

fn transition(
    binding: CheckpointBinding,
    to: CheckpointState,
    reason: Option<&str>,
) -> CheckpointTransition {
    CheckpointTransition {
        binding,
        to,
        rejection_reason: reason.map(|reason| RejectionReason::parse(reason).unwrap()),
    }
}

fn record_at(state: CheckpointState) -> CheckpointStateRecord {
    let mut record = CheckpointStateRecord::new_candidate(binding()).unwrap();
    let path: &[(CheckpointState, Option<&str>)] = match state {
        CheckpointState::Candidate => &[],
        CheckpointState::Validated => &[(CheckpointState::Validated, None)],
        CheckpointState::Rejected => &[(CheckpointState::Rejected, Some("validation failed"))],
        CheckpointState::Promoted => &[
            (CheckpointState::Validated, None),
            (CheckpointState::Promoted, None),
        ],
    };
    for (to, reason) in path {
        assert_eq!(
            record.apply(&transition(binding(), *to, *reason)).unwrap(),
            TransitionApplied::Applied
        );
    }
    record
}

/// Deterministic conflicting-identity variants of the canonical binding.
fn mutated_bindings() -> Vec<CheckpointBinding> {
    let payload = "0123456789abcdefghjkmnpqrt";
    let mut variants = Vec::new();
    let mut mutate = |apply: &dyn Fn(&mut CheckpointBinding)| {
        let mut mutated = binding();
        apply(&mut mutated);
        assert_ne!(mutated, binding());
        variants.push(mutated);
    };
    mutate(&|b| b.checkpoint_id = CheckpointId::parse(&format!("checkpoint_{payload}")).unwrap());
    mutate(&|b| b.run_id = RunId::parse(&format!("run_{payload}")).unwrap());
    mutate(&|b| b.owner.fencing_token = FencingToken::new(8));
    mutate(&|b| b.driver_lease_id = DriverLeaseId::parse("driver-lease-2").unwrap());
    mutate(&|b| b.operation_sequence = 12);
    mutate(&|b| b.expected_durable_head_sequence = 5);
    mutate(&|b| b.base_transcript_digest = digest(3));
    mutate(&|b| b.candidate_transcript_digest = digest(4));
    mutate(&|b| b.candidate_artifact_digest = ArtifactDigest::digest(b"other artifact"));
    mutate(&|b| {
        b.candidate_artifact_reference =
            ArtifactReference::parse(&format!("artifact-ref:{}", "cd".repeat(32))).unwrap();
    });
    mutate(&|b| b.mutation_point = CheckpointMutationPoint::AfterTurnEnded);
    variants
}

#[test]
fn exactly_four_states_with_pinned_wire_names() {
    assert_eq!(CheckpointState::ALL.len(), 4);
    let wire: Vec<String> = CheckpointState::ALL
        .iter()
        .map(|state| serde_json::to_string(state).unwrap())
        .collect();
    assert_eq!(
        wire,
        [
            "\"candidate\"",
            "\"validated\"",
            "\"rejected\"",
            "\"promoted\""
        ]
    );
    assert!(!CheckpointState::Candidate.is_terminal());
    assert!(!CheckpointState::Validated.is_terminal());
    assert!(CheckpointState::Rejected.is_terminal());
    assert!(CheckpointState::Promoted.is_terminal());
}

#[test]
fn legal_transition_graph_is_exact() {
    let legal = [
        (CheckpointState::Candidate, CheckpointState::Validated),
        (CheckpointState::Candidate, CheckpointState::Rejected),
        (CheckpointState::Validated, CheckpointState::Promoted),
        (CheckpointState::Validated, CheckpointState::Rejected),
    ];
    for from in CheckpointState::ALL {
        for to in CheckpointState::ALL {
            assert_eq!(
                from.can_transition_to(to),
                legal.contains(&(from, to)),
                "{from} -> {to}"
            );
        }
    }
    for (state, version) in [
        (CheckpointState::Candidate, 0),
        (CheckpointState::Validated, 1),
        (CheckpointState::Rejected, 1),
        (CheckpointState::Promoted, 2),
    ] {
        let record = record_at(state);
        assert_eq!(record.state(), state);
        assert_eq!(record.version(), StateVersion::new(version));
    }
}

#[test]
fn exact_replay_is_idempotent_and_conflicts_are_typed() {
    let mut record = record_at(CheckpointState::Validated);
    let before = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        record
            .apply(&transition(binding(), CheckpointState::Validated, None))
            .unwrap(),
        TransitionApplied::Replayed
    );
    assert_eq!(serde_json::to_vec(&record).unwrap(), before);

    let mut record = record_at(CheckpointState::Promoted);
    let before = serde_json::to_vec(&record).unwrap();
    for to in [CheckpointState::Validated, CheckpointState::Promoted] {
        assert_eq!(
            record.apply(&transition(binding(), to, None)).unwrap(),
            TransitionApplied::Replayed
        );
    }
    assert_eq!(serde_json::to_vec(&record).unwrap(), before);

    let mut record = record_at(CheckpointState::Rejected);
    let before = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        record
            .apply(&transition(
                binding(),
                CheckpointState::Rejected,
                Some("validation failed")
            ))
            .unwrap(),
        TransitionApplied::Replayed
    );
    assert_eq!(
        record.apply(&transition(
            binding(),
            CheckpointState::Rejected,
            Some("different reason")
        )),
        Err(CheckpointStateError::ConflictingReplay {
            state: CheckpointState::Rejected
        })
    );
    assert_eq!(serde_json::to_vec(&record).unwrap(), before);
}

#[test]
fn illegal_and_conflicting_transitions_leave_state_byte_identical() {
    let mut rejected = 0usize;
    for state in CheckpointState::ALL {
        for to in CheckpointState::ALL {
            for mutated in mutated_bindings() {
                let mut record = record_at(state);
                let before = serde_json::to_vec(&record).unwrap();
                let reason = (to == CheckpointState::Rejected).then_some("mutated identity");
                record
                    .apply(&transition(mutated.clone(), to, reason))
                    .expect_err("conflicting identity must be rejected");
                assert_eq!(serde_json::to_vec(&record).unwrap(), before);
                rejected += 1;
            }
            if !state.can_transition_to(to) {
                let mut record = record_at(state);
                let before = serde_json::to_vec(&record).unwrap();
                let outcome = record.apply(&transition(
                    binding(),
                    to,
                    (to == CheckpointState::Rejected).then_some("validation failed"),
                ));
                assert_eq!(serde_json::to_vec(&record).unwrap(), before);
                match outcome {
                    Err(_) => rejected += 1,
                    Ok(applied) => {
                        assert_eq!(applied, TransitionApplied::Replayed, "{state} -> {to}");
                    }
                }
            }
        }
    }
    assert!(rejected >= 100, "only {rejected} rejected cases generated");
}

#[test]
fn illegal_graph_edges_return_typed_errors_without_mutation() {
    for (from, to) in [
        (CheckpointState::Candidate, CheckpointState::Promoted),
        (CheckpointState::Rejected, CheckpointState::Validated),
        (CheckpointState::Rejected, CheckpointState::Promoted),
        (CheckpointState::Promoted, CheckpointState::Rejected),
        (CheckpointState::Validated, CheckpointState::Candidate),
    ] {
        let mut record = record_at(from);
        let before = serde_json::to_vec(&record).unwrap();
        let reason = (to == CheckpointState::Rejected).then_some("late rejection");
        let error = record
            .apply(&transition(binding(), to, reason))
            .expect_err("illegal edge");
        assert!(
            matches!(error, CheckpointStateError::IllegalTransition { .. }),
            "{from} -> {to} returned {error:?}"
        );
        assert_eq!(serde_json::to_vec(&record).unwrap(), before);
    }
}

#[test]
fn binding_binds_full_identity_and_bounds_untrusted_payloads() {
    let wire = serde_json::to_value(binding()).unwrap();
    let mut keys: Vec<&str> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "base_transcript_digest",
            "candidate_artifact_digest",
            "candidate_artifact_reference",
            "candidate_transcript_digest",
            "checkpoint_id",
            "driver_lease_id",
            "expected_durable_head_sequence",
            "mutation_point",
            "operation_sequence",
            "owner",
            "run_id",
            "schema_version",
        ]
    );
    let owner = wire["owner"].as_object().unwrap();
    assert!(owner.contains_key("attempt_id"));
    assert!(owner.contains_key("principal_id"));
    assert!(owner.contains_key("fencing_token"));
    assert!(
        wire["candidate_artifact_digest"]
            .as_str()
            .unwrap()
            .starts_with("blake3:")
    );
    assert!(
        wire["candidate_artifact_reference"]
            .as_str()
            .unwrap()
            .starts_with("artifact-ref:")
    );

    let record_wire = serde_json::to_value(record_at(CheckpointState::Rejected)).unwrap();
    let mut record_keys: Vec<&str> = record_wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    record_keys.sort_unstable();
    assert_eq!(record_keys, ["applied", "binding", "state", "version"]);

    assert_eq!(
        DriverLeaseId::parse(""),
        Err(CheckpointStateError::InvalidDriverLeaseId)
    );
    assert_eq!(
        DriverLeaseId::parse(&"d".repeat(MAX_DRIVER_LEASE_ID_BYTES + 1)),
        Err(CheckpointStateError::InvalidDriverLeaseId)
    );
    assert!(DriverLeaseId::parse(&"d".repeat(MAX_DRIVER_LEASE_ID_BYTES)).is_ok());
    assert_eq!(
        RejectionReason::parse(""),
        Err(CheckpointStateError::InvalidRejectionReason)
    );
    assert_eq!(
        RejectionReason::parse(&"r".repeat(MAX_REJECTION_REASON_BYTES + 1)),
        Err(CheckpointStateError::InvalidRejectionReason)
    );
    assert!(RejectionReason::parse(&"r".repeat(MAX_REJECTION_REASON_BYTES)).is_ok());
    assert!(RejectionReason::parse("plain ASCII reason 42, with punctuation.").is_ok());
    for spoofed in [
        "reason\u{202E}suffix",
        "reason\u{200B}suffix",
        "reason\u{0007}",
        "tab\treason",
        "newline\nreason",
        "non-ascii \u{e9} reason",
    ] {
        assert_eq!(
            RejectionReason::parse(spoofed),
            Err(CheckpointStateError::InvalidRejectionReason),
            "{spoofed:?} must be rejected"
        );
    }

    let mut inverted = binding();
    inverted.operation_sequence = 4;
    inverted.expected_durable_head_sequence = 11;
    assert_eq!(
        CheckpointStateRecord::new_candidate(inverted).unwrap_err(),
        CheckpointStateError::NonMonotonicSequence
    );
}

#[test]
fn kit_agentkit_807_candidate_cannot_mutate_head_and_promotion_needs_validated_identity() {
    let mut head = DurablePromotedHead::new(binding().run_id, 4, digest(1));
    let initial = head.clone();

    for state in [
        CheckpointState::Candidate,
        CheckpointState::Validated,
        CheckpointState::Rejected,
    ] {
        let record = record_at(state);
        assert_eq!(
            head.advance(&record),
            Err(CheckpointStateError::NotPromoted { state })
        );
        assert_eq!(head, initial);
    }

    let promoted = record_at(CheckpointState::Promoted);
    head.advance(&promoted).unwrap();
    assert_eq!(head.sequence(), 11);
    assert_eq!(head.transcript_digest(), &digest(2));

    let replayed = head.clone();
    head.advance(&promoted).unwrap();
    assert_eq!(head, replayed);

    let mut stale_head = DurablePromotedHead::new(binding().run_id, 6, digest(9));
    assert_eq!(
        stale_head.advance(&promoted),
        Err(CheckpointStateError::HeadSequenceMismatch {
            expected: 4,
            actual: 6
        })
    );
    assert_eq!(stale_head.sequence(), 6);

    let mut diverged_head = DurablePromotedHead::new(binding().run_id, 4, digest(7));
    assert_eq!(
        diverged_head.advance(&promoted),
        Err(CheckpointStateError::HeadDigestMismatch {
            expected: digest(1),
            actual: digest(7)
        })
    );
    assert_eq!(
        diverged_head,
        DurablePromotedHead::new(binding().run_id, 4, digest(7))
    );
}

#[test]
fn promoted_head_replay_requires_exact_promotion_identity() {
    let promoted = record_at(CheckpointState::Promoted);
    let mut head = DurablePromotedHead::new(binding().run_id, 4, digest(1));
    head.advance(&promoted).unwrap();
    let advanced = head.clone();

    head.advance(&promoted).unwrap();
    assert_eq!(head, advanced);

    let payload = "0123456789abcdefghjkmnpqrt";
    let mut foreign_binding = binding();
    foreign_binding.checkpoint_id = CheckpointId::parse(&format!("checkpoint_{payload}")).unwrap();
    let mut foreign = CheckpointStateRecord::new_candidate(foreign_binding.clone()).unwrap();
    for to in [CheckpointState::Validated, CheckpointState::Promoted] {
        foreign
            .apply(&transition(foreign_binding.clone(), to, None))
            .unwrap();
    }
    assert_eq!(
        head.advance(&foreign),
        Err(CheckpointStateError::HeadSequenceMismatch {
            expected: 4,
            actual: 11
        })
    );
    assert_eq!(head, advanced);

    let mut restored = DurablePromotedHead::new(binding().run_id, 11, digest(2));
    assert_eq!(
        restored.advance(&promoted),
        Err(CheckpointStateError::HeadSequenceMismatch {
            expected: 4,
            actual: 11
        })
    );
    assert_eq!(
        restored,
        DurablePromotedHead::new(binding().run_id, 11, digest(2))
    );
}

#[test]
fn foreign_run_promotion_cannot_advance_head_even_with_matching_sequence_and_base() {
    let payload = "0123456789abcdefghjkmnpqrv";
    let foreign_run = RunId::parse(&format!("run_{payload}")).unwrap();
    let mut foreign_binding = binding();
    foreign_binding.run_id = foreign_run;
    let mut foreign = CheckpointStateRecord::new_candidate(foreign_binding.clone()).unwrap();
    for to in [CheckpointState::Validated, CheckpointState::Promoted] {
        foreign
            .apply(&transition(foreign_binding.clone(), to, None))
            .unwrap();
    }
    assert_eq!(
        foreign.binding().operation_sequence,
        binding().operation_sequence
    );
    assert_eq!(
        foreign.binding().expected_durable_head_sequence,
        binding().expected_durable_head_sequence
    );
    assert_eq!(
        foreign.binding().base_transcript_digest,
        binding().base_transcript_digest
    );

    let mut head = DurablePromotedHead::new(binding().run_id, 4, digest(1));
    let before = head.clone();
    assert_eq!(
        head.advance(&foreign),
        Err(CheckpointStateError::HeadRunMismatch {
            expected: binding().run_id,
            actual: foreign_run,
        })
    );
    assert_eq!(head, before);
    assert_eq!(head.run_id(), binding().run_id);

    head.advance(&record_at(CheckpointState::Promoted)).unwrap();
    let advanced = head.clone();
    assert_eq!(
        head.advance(&foreign),
        Err(CheckpointStateError::HeadRunMismatch {
            expected: binding().run_id,
            actual: foreign_run,
        })
    );
    assert_eq!(head, advanced);
}

#[test]
fn unknown_hook_outcome_stays_pending_for_same_id_reconciliation() {
    assert_eq!(
        hook_outcome_action(&PostValidationCheckpointOutcome::Committed).unwrap(),
        HookOutcomeAction::RecordDurableCandidate
    );
    assert_eq!(
        hook_outcome_action(&PostValidationCheckpointOutcome::NotCommitted(
            "store rejected the candidate".into()
        ))
        .unwrap(),
        HookOutcomeAction::RejectCandidate(
            RejectionReason::parse("store rejected the candidate").unwrap()
        )
    );
    for _ in 0..2 {
        assert_eq!(
            hook_outcome_action(&PostValidationCheckpointOutcome::Unknown(
                "durable write status unknown".into()
            ))
            .unwrap(),
            HookOutcomeAction::AwaitReconciliation
        );
    }
    assert_eq!(
        hook_outcome_action(&PostValidationCheckpointOutcome::NotCommitted(
            "r".repeat(MAX_REJECTION_REASON_BYTES + 1)
        )),
        Err(CheckpointStateError::InvalidRejectionReason)
    );

    let mut record = record_at(CheckpointState::Candidate);
    let before = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        hook_outcome_action(&PostValidationCheckpointOutcome::Unknown(String::new())).unwrap(),
        HookOutcomeAction::AwaitReconciliation
    );
    assert_eq!(serde_json::to_vec(&record).unwrap(), before);
    assert_eq!(
        record
            .apply(&transition(binding(), CheckpointState::Validated, None))
            .unwrap(),
        TransitionApplied::Applied
    );
}

#[test]
fn safe_points_map_only_supported_mutation_points() {
    for (point, kit_point, boundary) in [
        (
            MutationPoint::AfterToolResult,
            CheckpointMutationPoint::AfterToolResult,
            SafeBoundary::AfterToolOutcome,
        ),
        (
            MutationPoint::AfterTurnEnded,
            CheckpointMutationPoint::AfterTurnEnded,
            SafeBoundary::TurnEnd,
        ),
    ] {
        let mapped = CheckpointMutationPoint::from_agentkit(point).unwrap();
        assert_eq!(mapped, kit_point);
        assert_eq!(mapped.safe_boundary(), boundary);
    }

    let (prefix, remainder) = LOOP_SOURCE.split_once("pub enum MutationPoint {").unwrap();
    let adjacent_attributes: Vec<&str> = prefix
        .lines()
        .rev()
        .map(str::trim)
        .take_while(|line| line.starts_with("#[") || line.starts_with("///"))
        .collect();
    assert!(
        adjacent_attributes.contains(&"#[non_exhaustive]"),
        "upstream MutationPoint lost #[non_exhaustive]; revisit the fail-closed mapping"
    );

    let body = remainder.split_once("\n}").unwrap().0;
    let mut depth = 0usize;
    let mut variants = 0usize;
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        if depth == 0
            && line
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_uppercase())
        {
            variants += 1;
        }
        for character in line.chars() {
            match character {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    assert_eq!(
        variants, 2,
        "upstream MutationPoint gained variants; Kit maps unknown points fail-closed"
    );
}

#[test]
fn record_wire_roundtrips_and_rejects_invalid_payloads() {
    let record = record_at(CheckpointState::Promoted);
    let wire = serde_json::to_string(&record).unwrap();
    let decoded: CheckpointStateRecord = serde_json::from_str(&wire).unwrap();
    assert_eq!(decoded, record);
    assert_eq!(decoded.rejection_reason(), None);

    let rejected = record_at(CheckpointState::Rejected);
    let rejected_decoded: CheckpointStateRecord =
        serde_json::from_str(&serde_json::to_string(&rejected).unwrap()).unwrap();
    assert_eq!(rejected_decoded, rejected);
    assert_eq!(
        rejected_decoded.rejection_reason().unwrap().as_str(),
        "validation failed"
    );

    let oversized = wire.replace("driver-lease-1", &"d".repeat(MAX_DRIVER_LEASE_ID_BYTES + 1));
    assert!(serde_json::from_str::<CheckpointStateRecord>(&oversized).is_err());

    let unknown_version = wire.replace("\"schema_version\":1", "\"schema_version\":2");
    assert!(serde_json::from_str::<CheckpointStateRecord>(&unknown_version).is_err());
}

#[test]
fn forged_wire_records_cannot_bypass_transition_invariants() {
    let promoted_wire = serde_json::to_value(record_at(CheckpointState::Promoted)).unwrap();

    let mut forged_promoted = serde_json::to_value(record_at(CheckpointState::Candidate)).unwrap();
    forged_promoted["state"] = "promoted".into();
    assert!(serde_json::from_value::<CheckpointStateRecord>(forged_promoted).is_err());

    let mut forged_version = promoted_wire.clone();
    forged_version["version"] = 9u64.into();
    assert!(serde_json::from_value::<CheckpointStateRecord>(forged_version).is_err());

    let mut duplicated = promoted_wire.clone();
    let repeat = duplicated["applied"][0].clone();
    duplicated["applied"].as_array_mut().unwrap().push(repeat);
    assert!(serde_json::from_value::<CheckpointStateRecord>(duplicated).is_err());

    let mut forged_reason = promoted_wire.clone();
    forged_reason["applied"][0]["rejection_reason"] = "forged".into();
    assert!(serde_json::from_value::<CheckpointStateRecord>(forged_reason).is_err());

    let mut nonmonotonic = promoted_wire.clone();
    nonmonotonic["binding"]["operation_sequence"] = 4u64.into();
    assert!(serde_json::from_value::<CheckpointStateRecord>(nonmonotonic).is_err());

    let mut head = DurablePromotedHead::new(binding().run_id, 4, digest(1));
    head.advance(&record_at(CheckpointState::Promoted)).unwrap();
    assert_eq!(head.sequence(), 11);
    let mut rollback = promoted_wire.clone();
    rollback["binding"]["operation_sequence"] = 5u64.into();
    rollback["binding"]["expected_durable_head_sequence"] = 11u64.into();
    assert!(serde_json::from_value::<CheckpointStateRecord>(rollback).is_err());

    let mut unknown_field = promoted_wire.clone();
    unknown_field["forged_field"] = true.into();
    assert!(serde_json::from_value::<CheckpointStateRecord>(unknown_field).is_err());

    let mut unknown_binding_field = promoted_wire.clone();
    unknown_binding_field["binding"]["forged_field"] = true.into();
    assert!(serde_json::from_value::<CheckpointStateRecord>(unknown_binding_field).is_err());

    let mut unknown_owner_field = promoted_wire.clone();
    unknown_owner_field["binding"]["owner"]["forged_field"] = true.into();
    assert!(serde_json::from_value::<CheckpointStateRecord>(unknown_owner_field).is_err());

    let mut unknown_applied_field = promoted_wire;
    unknown_applied_field["applied"][0]["forged_field"] = true.into();
    assert!(serde_json::from_value::<CheckpointStateRecord>(unknown_applied_field).is_err());
}

#[test]
fn transition_wire_roundtrips_and_rejects_unknown_fields() {
    let rejection = transition(
        binding(),
        CheckpointState::Rejected,
        Some("validation failed"),
    );
    let wire = serde_json::to_value(&rejection).unwrap();
    let decoded: CheckpointTransition = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(decoded, rejection);

    let mut unknown_field = wire.clone();
    unknown_field["forged_field"] = true.into();
    assert!(serde_json::from_value::<CheckpointTransition>(unknown_field).is_err());

    let mut unknown_binding_field = wire.clone();
    unknown_binding_field["binding"]["forged_field"] = true.into();
    assert!(serde_json::from_value::<CheckpointTransition>(unknown_binding_field).is_err());

    let mut unknown_owner_field = wire;
    unknown_owner_field["binding"]["owner"]["forged_field"] = true.into();
    assert!(serde_json::from_value::<CheckpointTransition>(unknown_owner_field).is_err());
}
