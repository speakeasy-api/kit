use std::collections::BTreeSet;

use kit::domain::commands::Command;
use kit::domain::events::{
    ArtifactRef, CommitPosition, EntityId, EventEnvelope, EventPayload, ProjectCreated, RunState,
    RunTransition, SchemaVersion, StreamSequence, TraceId, UtcDateTime,
};
use kit::domain::ids::{CommandId, EventId, PrincipalId, ProjectId, RunId, ThreadId};
use kit::domain::projections::{DeterministicReducer, ProjectionContract, ProjectionState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026x}")
}

fn envelope(seed: u64, position: u64) -> EventEnvelope<ProjectCreated> {
    let principal = PrincipalId::parse(&id("principal", seed)).unwrap();
    let project = ProjectId::parse(&id("project", seed.wrapping_add(1))).unwrap();
    EventEnvelope::new(
        EventId::parse(&id("evt", seed.wrapping_add(2))).unwrap(),
        EntityId::Project(project),
        StreamSequence::new(seed % 10_000 + 1).unwrap(),
        CommitPosition::new(position).unwrap(),
        UtcDateTime::parse("2026-07-21T12:00:00.123456Z").unwrap(),
        CommandId::parse(&id("cmd", seed.wrapping_add(3))).unwrap(),
        EntityId::Project(project),
        None,
        TraceId::parse(&format!("trace-{seed:016x}")).unwrap(),
        ProjectCreated::new(project, principal),
        vec![ArtifactRef::parse(&format!("blake3:{seed:064x}")).unwrap()],
    )
    .unwrap()
}

#[test]
fn event_envelope_uses_exact_rfc_fields_and_independent_versions() {
    let value = serde_json::to_value(envelope(1, 1)).unwrap();
    let keys: BTreeSet<_> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "artifacts",
            "attempt_id",
            "causation_id",
            "commit_position",
            "correlation_id",
            "id",
            "occurred_at",
            "payload",
            "schema_version",
            "sequence",
            "stream",
            "trace_id",
            "type",
        ])
    );
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["payload"]["schema_version"], 1);
    assert_eq!(value["type"], ProjectCreated::EVENT_TYPE);
}

#[test]
fn deterministic_property_roundtrip_covers_10_000_cases() {
    let mut seed = 0x4d59_5df4_d0f3_3173_u64;
    for position in 1..=10_000 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let expected = envelope(seed & 0x0000_ffff_ffff_ffff, position);
        let bytes = serde_json::to_vec(&expected).unwrap();
        let actual: EventEnvelope<ProjectCreated> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(actual, expected, "seed {seed}");
        assert_eq!(serde_json::to_vec(&actual).unwrap(), bytes, "seed {seed}");
    }
}

#[test]
fn additive_fields_are_ignored_at_envelope_and_payload_boundaries() {
    let mut value = serde_json::to_value(envelope(7, 1)).unwrap();
    value["future_envelope_field"] = json!({"nested": true});
    value["payload"]["future_payload_field"] = json!([1, 2, 3]);
    let parsed: EventEnvelope<ProjectCreated> = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.payload.schema_version, SchemaVersion::V1);
}

#[test]
fn malformed_versions_positions_types_timestamps_and_artifacts_are_rejected() {
    let valid = serde_json::to_value(envelope(9, 1)).unwrap();
    for (pointer, replacement) in [
        ("/schema_version", json!(0)),
        ("/schema_version", json!(2)),
        ("/payload/schema_version", json!(0)),
        ("/payload/schema_version", json!(2)),
        ("/sequence", json!(0)),
        ("/commit_position", json!(0)),
        ("/type", json!("thread.created")),
        ("/occurred_at", json!("2026-07-21T12:00:00+00:00")),
        ("/occurred_at", json!("2026-02-30T12:00:00Z")),
        ("/artifacts/0", json!("blake3:not-a-digest")),
    ] {
        let mut malformed = valid.clone();
        *malformed.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            serde_json::from_value::<EventEnvelope<ProjectCreated>>(malformed).is_err(),
            "accepted malformed {pointer}"
        );
    }
}

#[test]
fn commands_use_the_canonical_lifecycle_model_and_are_versioned() {
    let run = RunId::parse(&id("run", 1)).unwrap();
    let thread = ThreadId::parse(&id("thread", 2)).unwrap();
    let input = ArtifactRef::parse(&format!("blake3:{}", "a".repeat(64))).unwrap();
    let start = Command::StartRun {
        schema_version: SchemaVersion::CURRENT,
        run_id: run,
        thread_id: thread,
        input,
        run_config: None,
        experiment_config: None,
        effective_config: None,
    };
    assert_eq!(start.schema_version(), SchemaVersion::V1);
    let wire = serde_json::to_vec(&start).unwrap();
    assert_eq!(serde_json::from_slice::<Command>(&wire).unwrap(), start);

    let transition = RunTransition::new(RunState::Running, RunState::Cancelling).unwrap();
    for expected_version in 0..10_000 {
        let command = Command::TransitionRun {
            schema_version: SchemaVersion::CURRENT,
            run_id: run,
            transition,
            expected_version,
            expected_owner: None,
            replacement_owner: None,
        };
        let wire = serde_json::to_vec(&command).unwrap();
        assert_eq!(serde_json::from_slice::<Command>(&wire).unwrap(), command);
    }

    let mut additive = serde_json::to_value(&start).unwrap();
    additive["future_field"] = json!({"nested": true});
    assert_eq!(serde_json::from_value::<Command>(additive).unwrap(), start);

    let valid = serde_json::to_value(Command::TransitionRun {
        schema_version: SchemaVersion::CURRENT,
        run_id: run,
        transition,
        expected_version: 5,
        expected_owner: None,
        replacement_owner: None,
    })
    .unwrap();
    for replacement in [json!(0), json!(2), Value::Null] {
        let mut malformed = valid.clone();
        malformed["schema_version"] = replacement;
        assert!(serde_json::from_value::<Command>(malformed).is_err());
    }
    let mut missing_version = valid.clone();
    missing_version
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(serde_json::from_value::<Command>(missing_version).is_err());
    assert!(
        serde_json::from_value::<Command>(json!({
            "command": "transition_run",
            "schema_version": 1,
            "run_id": id("run", 1),
            "transition": {"from": "completed", "to": "running"},
            "expected_version": 5
        }))
        .is_err()
    );
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct ProjectCount(u64);

impl DeterministicReducer<ProjectCreated> for ProjectCount {
    fn reduce(&mut self, _: &ProjectCreated) {
        self.0 += 1;
    }
}

impl ProjectionContract<ProjectCreated> for ProjectCount {
    const NAME: &'static str = "project_count";
    const SCHEMA_VERSION: SchemaVersion = SchemaVersion::V1;
}

#[test]
fn projections_are_versioned_deterministic_and_position_ordered() {
    let mut first = ProjectionState::new(ProjectCount::default());
    let mut second = ProjectionState::new(ProjectCount::default());
    for position in 1..=100 {
        let event = envelope(position, position);
        first.apply(&event).unwrap();
        second.apply(&event).unwrap();
    }
    assert_eq!(first, second);
    assert_eq!(first.schema_version, ProjectCount::SCHEMA_VERSION);
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    assert!(first.apply(&envelope(101, 100)).is_err());

    let mut malformed = serde_json::to_value(first).unwrap();
    malformed["schema_version"] = Value::from(2);
    assert!(serde_json::from_value::<ProjectionState<ProjectCount>>(malformed).is_err());
}
