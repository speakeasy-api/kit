// This suite's in-memory store validates state-machine durability and fencing
// semantics only. It is not evidence of process containment or durable storage.

use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use kit::{
    domain::{
        ids::{AttemptId, CommandId, PrincipalId, ProcessId, WorkspaceId},
        lifecycle::{AttemptOwnership, FencingToken, ProcessClaim, ProcessOwnership},
    },
    executor::cancel::{
        CancellationCommit, CancellationCompletionStatus, CancellationControl,
        CancellationEffectKind, CancellationError, CancellationIntent, CancellationPhase,
        CancellationPublication, CancellationRecord, CancellationStoreError,
        DurableCancellationStore, WorkspaceIdentity, reconcile_cancellation, request_cancellation,
    },
    executor::process::tree::{
        BoundaryIdentity, BoundaryKind, Inspection, Ownership, PersistedBoundary,
    },
};

#[derive(Default)]
struct DurableState {
    records: BTreeMap<CommandId, CancellationRecord>,
    publications: Vec<CancellationPublication>,
    phases: Vec<CancellationPhase>,
}

struct MemoryDurableStore {
    state: DurableState,
    current_owner: Arc<Mutex<AttemptOwnership>>,
    crash_after: Option<CancellationPhase>,
    crash_before_commit: Option<CancellationPhase>,
}

impl MemoryDurableStore {
    fn new(owner: AttemptOwnership) -> Self {
        Self {
            state: DurableState::default(),
            current_owner: Arc::new(Mutex::new(owner)),
            crash_after: None,
            crash_before_commit: None,
        }
    }

    fn crash_after(mut self, phase: CancellationPhase) -> Self {
        self.crash_after = Some(phase);
        self
    }

    fn crash_before_commit(mut self, phase: CancellationPhase) -> Self {
        self.crash_before_commit = Some(phase);
        self
    }

    fn authorize(&self, owner: AttemptOwnership) -> Result<(), CancellationStoreError> {
        let current = *self.current_owner.lock().unwrap();
        if current.attempt_id != owner.attempt_id || current.principal_id != owner.principal_id {
            Err(CancellationStoreError::Unauthorized)
        } else if current.fencing_token != owner.fencing_token {
            Err(CancellationStoreError::StaleOwner)
        } else {
            Ok(())
        }
    }

    fn maybe_crash(&mut self, phase: CancellationPhase) -> Result<(), CancellationStoreError> {
        if self.crash_after == Some(phase) {
            self.crash_after = None;
            Err(CancellationStoreError::Unavailable(format!(
                "injected crash after {phase:?}"
            )))
        } else {
            Ok(())
        }
    }
}

impl DurableCancellationStore for MemoryDurableStore {
    fn request(
        &mut self,
        intent: CancellationIntent,
    ) -> Result<CancellationRecord, CancellationStoreError> {
        self.authorize(intent.owner)?;
        if let Some(existing) = self.state.records.get(&intent.request_id) {
            return if existing.intent == intent {
                Ok(existing.clone())
            } else {
                Err(CancellationStoreError::IdempotencyConflict)
            };
        }
        let request_id = intent.request_id;
        let record = CancellationRecord {
            intent,
            phase: CancellationPhase::IntentPersisted,
            operations: Vec::new(),
            quiescence: None,
            outcome_unknown: None,
        };
        self.state.records.insert(request_id, record.clone());
        self.state.phases.push(record.phase);
        self.maybe_crash(record.phase)?;
        Ok(record)
    }

    fn load(
        &mut self,
        request_id: CommandId,
        owner: AttemptOwnership,
    ) -> Result<CancellationRecord, CancellationStoreError> {
        self.authorize(owner)?;
        let record = self
            .state
            .records
            .get(&request_id)
            .cloned()
            .ok_or(CancellationStoreError::NotFound)?;
        if record.intent.owner.attempt_id != owner.attempt_id
            || record.intent.owner.principal_id != owner.principal_id
        {
            return Err(CancellationStoreError::Unauthorized);
        }
        Ok(record)
    }

    fn commit(
        &mut self,
        commit: CancellationCommit,
    ) -> Result<CancellationRecord, CancellationStoreError> {
        self.authorize(commit.owner)?;
        if commit
            .publications
            .iter()
            .any(|publication| publication.owner() != commit.owner)
        {
            return Err(CancellationStoreError::StaleOwner);
        }
        let record = self
            .state
            .records
            .get_mut(&commit.request_id)
            .ok_or(CancellationStoreError::NotFound)?;
        if record.intent.owner.attempt_id != commit.owner.attempt_id
            || record.intent.owner.principal_id != commit.owner.principal_id
        {
            return Err(CancellationStoreError::Unauthorized);
        }
        if record.phase != commit.expected_phase || !valid_transition(record.phase, commit.phase) {
            return Err(CancellationStoreError::PhaseConflict);
        }
        if self.crash_before_commit == Some(commit.phase) {
            self.crash_before_commit = None;
            return Err(CancellationStoreError::Unavailable(format!(
                "injected effect-before-commit crash at {:?}",
                commit.phase
            )));
        }
        record.phase = commit.phase;
        if let Some(operation) = commit.operation {
            record.operations.push(operation);
        }
        record.quiescence = commit.quiescence;
        record.outcome_unknown = commit.outcome_unknown;
        let record = record.clone();
        self.state.publications.extend(commit.publications);
        self.state.phases.push(record.phase);
        self.maybe_crash(record.phase)?;
        Ok(record)
    }
}

fn valid_transition(from: CancellationPhase, to: CancellationPhase) -> bool {
    matches!(
        (from, to),
        (
            CancellationPhase::IntentPersisted,
            CancellationPhase::GraceRequested
        ) | (
            CancellationPhase::GraceRequested,
            CancellationPhase::KillRequested
        ) | (
            CancellationPhase::KillRequested,
            CancellationPhase::ReapRequested
        ) | (
            CancellationPhase::ReapRequested,
            CancellationPhase::InspectRequested
        ) | (
            CancellationPhase::InspectRequested,
            CancellationPhase::Quiescent
        ) | (_, CancellationPhase::OutcomeUnknown)
            | (
                CancellationPhase::OutcomeUnknown,
                CancellationPhase::Quiescent
            )
    )
}

struct FakeControl {
    identity: BoundaryIdentity,
    calls: Vec<&'static str>,
    survivors: Option<u32>,
    quiescent: bool,
    failures: Vec<&'static str>,
    wait_for_grace_deadline: bool,
    stale_on: Option<&'static str>,
    current_owner: Arc<Mutex<AttemptOwnership>>,
    replacement_owner: AttemptOwnership,
}

impl FakeControl {
    fn new(intent: &CancellationIntent, current_owner: Arc<Mutex<AttemptOwnership>>) -> Self {
        Self {
            identity: intent.boundary.identity.clone(),
            calls: Vec::new(),
            survivors: Some(0),
            quiescent: true,
            failures: Vec::new(),
            wait_for_grace_deadline: false,
            stale_on: None,
            current_owner,
            replacement_owner: owner_with_fence(intent.owner, intent.owner.fencing_token.get() + 1),
        }
    }

    fn called(&mut self, operation: &'static str) {
        self.calls.push(operation);
        if self.stale_on == Some(operation) {
            *self.current_owner.lock().unwrap() = self.replacement_owner;
        }
    }
}

impl CancellationControl for FakeControl {
    fn boundary_identity(&self) -> &BoundaryIdentity {
        &self.identity
    }

    fn request_grace_and_wait(
        &mut self,
        process: &ProcessClaim,
        boundary: &PersistedBoundary,
        deadline: Instant,
    ) -> io::Result<()> {
        assert!(deadline >= Instant::now());
        assert_eq!(process.owner, ProcessOwnership::Attempt(owner()));
        assert_eq!(boundary.identity, self.identity);
        self.called("grace");
        if self.wait_for_grace_deadline {
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if self.failures.contains(&"grace") {
            return Err(io::Error::other("injected grace failure"));
        }
        Ok(())
    }

    fn kill_complete_boundary(
        &mut self,
        boundary: &PersistedBoundary,
        deadline: Instant,
    ) -> io::Result<()> {
        assert!(deadline >= Instant::now());
        assert_eq!(boundary.identity, self.identity);
        self.called("kill");
        if self.failures.contains(&"kill") {
            return Err(io::Error::other("injected kill failure"));
        }
        Ok(())
    }

    fn reap_direct_child(&mut self, process: &ProcessClaim, deadline: Instant) -> io::Result<()> {
        assert!(deadline >= Instant::now());
        assert_eq!(process.owner, ProcessOwnership::Attempt(owner()));
        self.called("reap");
        if self.failures.contains(&"reap") {
            return Err(io::Error::other("injected reap failure"));
        }
        Ok(())
    }

    fn inspect_boundary(
        &mut self,
        boundary: &PersistedBoundary,
        deadline: Instant,
    ) -> io::Result<Inspection> {
        assert!(deadline >= Instant::now());
        assert_eq!(boundary.identity, self.identity);
        self.called("inspect");
        if self.failures.contains(&"inspect") {
            return Err(io::Error::other("injected inspection failure"));
        }
        Ok(Inspection {
            identity: self.identity.clone(),
            survivors: self.survivors,
            quiescent: self.quiescent,
        })
    }
}

fn owner() -> AttemptOwnership {
    AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        FencingToken::new(7),
    )
}

fn owner_with_fence(owner: AttemptOwnership, fence: u64) -> AttemptOwnership {
    AttemptOwnership::new(
        owner.attempt_id,
        owner.principal_id,
        FencingToken::new(fence),
    )
}

fn intent() -> CancellationIntent {
    let owner = owner();
    let process = ProcessClaim::new(
        ProcessId::parse("process_00000000000000000000000001").unwrap(),
        ProcessOwnership::Attempt(owner),
    );
    let boundary = PersistedBoundary {
        ownership: Ownership::new(
            serde_json::to_string(&process.owner).unwrap(),
            process.process_id.to_string(),
        )
        .unwrap(),
        identity: BoundaryIdentity::new(
            BoundaryKind::Container,
            "cancel-fixture",
            "a".repeat(64),
            "runtime-start-identity",
        )
        .unwrap(),
    };
    CancellationIntent::new(
        CommandId::parse("cmd_00000000000000000000000001").unwrap(),
        owner,
        process,
        boundary,
        WorkspaceIdentity::new(
            WorkspaceId::parse("workspace_00000000000000000000000001").unwrap(),
            "acquisition-1",
            "revision-1",
        )
        .unwrap(),
        Duration::from_millis(25),
    )
    .unwrap()
}

fn timeout() -> Duration {
    Duration::from_secs(1)
}

#[test]
fn grace_kill_reap_inspect_order_and_fenced_completion_hold_100_of_100() {
    for _ in 0..100 {
        let intent = intent();
        let mut store = MemoryDurableStore::new(intent.owner);
        let mut control = FakeControl::new(&intent, store.current_owner.clone());
        let record =
            request_cancellation(&mut store, &mut control, intent.clone(), timeout()).unwrap();

        assert_eq!(control.calls, ["grace", "kill", "reap", "inspect"]);
        assert_eq!(
            store.state.phases,
            [
                CancellationPhase::IntentPersisted,
                CancellationPhase::GraceRequested,
                CancellationPhase::KillRequested,
                CancellationPhase::ReapRequested,
                CancellationPhase::InspectRequested,
                CancellationPhase::Quiescent,
            ]
        );
        assert!(record.workspace_reassignable());
        assert!(!record.blocks_auto_resume());
        assert_eq!(record.quiescence.as_ref().unwrap().survivors, 0);
        assert_eq!(store.state.publications.len(), 5);
        assert!(
            store
                .state
                .publications
                .iter()
                .all(|publication| publication.owner().fencing_token == FencingToken::new(7))
        );
        assert!(matches!(
            store.state.publications.last(),
            Some(CancellationPublication::Completion(completion))
                if completion.status == CancellationCompletionStatus::Cancelled
        ));

        let publications = store.state.publications.len();
        let calls = control.calls.len();
        let repeated = request_cancellation(&mut store, &mut control, intent, timeout()).unwrap();
        assert_eq!(repeated.phase, CancellationPhase::Quiescent);
        assert_eq!(store.state.publications.len(), publications);
        assert_eq!(control.calls.len(), calls);
    }
}

#[test]
fn reconciliation_resumes_after_a_crash_at_every_durable_phase() {
    for crash_phase in [
        CancellationPhase::IntentPersisted,
        CancellationPhase::GraceRequested,
        CancellationPhase::KillRequested,
        CancellationPhase::ReapRequested,
        CancellationPhase::InspectRequested,
        CancellationPhase::Quiescent,
    ] {
        let intent = intent();
        let request_id = intent.request_id;
        let mut store = MemoryDurableStore::new(intent.owner).crash_after(crash_phase);
        let mut control = FakeControl::new(&intent, store.current_owner.clone());
        assert!(matches!(
            request_cancellation(&mut store, &mut control, intent, timeout()),
            Err(CancellationError::Store(
                CancellationStoreError::Unavailable(_)
            ))
        ));

        let record =
            reconcile_cancellation(&mut store, &mut control, request_id, owner(), timeout())
                .unwrap();
        assert_eq!(record.phase, CancellationPhase::Quiescent);
        assert!(record.workspace_reassignable());
        assert_eq!(control.calls, ["grace", "kill", "reap", "inspect"]);
        assert_eq!(store.state.publications.len(), 5);
    }

    let intent = intent();
    let request_id = intent.request_id;
    let mut store =
        MemoryDurableStore::new(intent.owner).crash_after(CancellationPhase::OutcomeUnknown);
    let mut control = FakeControl::new(&intent, store.current_owner.clone());
    control.failures.push("inspect");
    assert!(matches!(
        request_cancellation(&mut store, &mut control, intent, timeout()),
        Err(CancellationError::Store(
            CancellationStoreError::Unavailable(_)
        ))
    ));
    let publications = store.state.publications.len();
    let record =
        reconcile_cancellation(&mut store, &mut control, request_id, owner(), timeout()).unwrap();
    assert_eq!(record.phase, CancellationPhase::OutcomeUnknown);
    assert_eq!(store.state.publications.len(), publications);
    assert!(record.blocks_auto_resume());
    assert!(!record.workspace_reassignable());
}

#[test]
fn stale_attempts_and_fences_publish_nothing_and_keep_the_workspace_held() {
    let intent = intent();
    let different_attempt = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000002").unwrap(),
        intent.owner.principal_id,
        intent.owner.fencing_token,
    );
    let mut wrong_attempt_store = MemoryDurableStore::new(different_attempt);
    let mut wrong_attempt_control =
        FakeControl::new(&intent, wrong_attempt_store.current_owner.clone());
    assert!(matches!(
        request_cancellation(
            &mut wrong_attempt_store,
            &mut wrong_attempt_control,
            intent.clone(),
            timeout()
        ),
        Err(CancellationError::Store(
            CancellationStoreError::Unauthorized
        ))
    ));
    assert!(wrong_attempt_store.state.publications.is_empty());
    assert!(wrong_attempt_control.calls.is_empty());

    let mut stale_store = MemoryDurableStore::new(owner_with_fence(intent.owner, 8));
    let mut stale_control = FakeControl::new(&intent, stale_store.current_owner.clone());
    assert!(matches!(
        request_cancellation(
            &mut stale_store,
            &mut stale_control,
            intent.clone(),
            timeout()
        ),
        Err(CancellationError::Store(CancellationStoreError::StaleOwner))
    ));
    assert!(stale_store.state.publications.is_empty());
    assert!(stale_control.calls.is_empty());

    let mut raced_store = MemoryDurableStore::new(intent.owner);
    let mut raced_control = FakeControl::new(&intent, raced_store.current_owner.clone());
    raced_control.stale_on = Some("grace");
    assert!(matches!(
        request_cancellation(&mut raced_store, &mut raced_control, intent, timeout()),
        Err(CancellationError::Store(CancellationStoreError::StaleOwner))
    ));
    assert!(raced_store.state.publications.is_empty());
    let record = raced_store.state.records.values().next().unwrap();
    assert_eq!(record.phase, CancellationPhase::GraceRequested);
    assert!(!record.workspace_reassignable());
}

#[test]
fn survivors_and_uninspectable_boundaries_become_unknown_and_never_reassign() {
    for (survivors, quiescent, inspect_fails) in [
        (Some(1), false, false),
        (None, false, false),
        (Some(0), false, false),
        (Some(0), true, true),
    ] {
        let intent = intent();
        let mut store = MemoryDurableStore::new(intent.owner);
        let mut control = FakeControl::new(&intent, store.current_owner.clone());
        control.survivors = survivors;
        control.quiescent = quiescent;
        if inspect_fails {
            control.failures.push("inspect");
        }
        let record = request_cancellation(&mut store, &mut control, intent, timeout()).unwrap();
        assert_eq!(record.phase, CancellationPhase::OutcomeUnknown);
        assert!(record.outcome_unknown.is_some());
        assert!(record.blocks_auto_resume());
        assert!(!record.workspace_reassignable());
        assert!(record.quiescence.is_none());
        assert!(matches!(
            store.state.publications.last(),
            Some(CancellationPublication::Completion(completion))
                if completion.status == CancellationCompletionStatus::OutcomeUnknown
        ));
    }
}

#[test]
fn escaped_or_reused_boundary_is_never_targeted_or_marked_quiescent() {
    let intent = intent();
    let mut store = MemoryDurableStore::new(intent.owner);
    let mut control = FakeControl::new(&intent, store.current_owner.clone());
    control.identity = BoundaryIdentity::new(
        BoundaryKind::Container,
        "cancel-fixture",
        "b".repeat(64),
        "different-runtime-start",
    )
    .unwrap();

    let record = request_cancellation(&mut store, &mut control, intent, timeout()).unwrap();
    assert_eq!(record.phase, CancellationPhase::OutcomeUnknown);
    assert!(record.blocks_auto_resume());
    assert!(!record.workspace_reassignable());
    assert!(control.calls.is_empty());
    assert_eq!(store.state.publications.len(), 1);
}

#[test]
fn published_effects_have_the_exact_order_and_request_ids_are_idempotent() {
    let intent = intent();
    let mut store = MemoryDurableStore::new(intent.owner);
    let mut control = FakeControl::new(&intent, store.current_owner.clone());
    request_cancellation(&mut store, &mut control, intent.clone(), timeout()).unwrap();
    let effects = store
        .state
        .publications
        .iter()
        .filter_map(|publication| match publication {
            CancellationPublication::Effect(effect) => Some(effect.kind),
            CancellationPublication::Completion(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        effects,
        [
            CancellationEffectKind::GraceRequested,
            CancellationEffectKind::BoundaryKilled,
            CancellationEffectKind::DirectChildReaped,
            CancellationEffectKind::BoundaryInspected,
        ]
    );

    let mut conflict = intent;
    conflict.workspace.revision = "another-revision".to_owned();
    assert!(matches!(
        store.request(conflict),
        Err(CancellationStoreError::IdempotencyConflict)
    ));
}

#[test]
fn grace_deadline_is_observed_before_kill_escalation() {
    let mut intent = intent();
    intent.grace_period = Duration::from_millis(20);
    let mut store = MemoryDurableStore::new(intent.owner);
    let mut control = FakeControl::new(&intent, store.current_owner.clone());
    control.wait_for_grace_deadline = true;
    let started = Instant::now();

    request_cancellation(&mut store, &mut control, intent, timeout()).unwrap();

    assert!(started.elapsed() >= Duration::from_millis(20));
    assert_eq!(control.calls, ["grace", "kill", "reap", "inspect"]);
}

#[test]
fn grace_kill_and_reap_errors_are_durable_and_do_not_stop_escalation() {
    for failures in [
        vec!["grace"],
        vec!["kill"],
        vec!["reap"],
        vec!["kill", "reap"],
    ] {
        let intent = intent();
        let mut store = MemoryDurableStore::new(intent.owner);
        let mut control = FakeControl::new(&intent, store.current_owner.clone());
        control.failures = failures.clone();

        let record = request_cancellation(&mut store, &mut control, intent, timeout()).unwrap();

        assert_eq!(control.calls, ["grace", "kill", "reap", "inspect"]);
        assert_eq!(record.operations.len(), 4);
        for failed in failures {
            assert!(record.operations.iter().any(|operation| {
                let name = match operation.kind {
                    kit::executor::cancel::CancellationOperationKind::GraceAndWait => "grace",
                    kit::executor::cancel::CancellationOperationKind::KillBoundary => "kill",
                    kit::executor::cancel::CancellationOperationKind::ReapDirectChild => "reap",
                    kit::executor::cancel::CancellationOperationKind::InspectBoundary => "inspect",
                };
                name == failed && operation.error.is_some()
            }));
        }
        if control.failures == ["grace"] {
            assert_eq!(record.phase, CancellationPhase::Quiescent);
        } else {
            assert_eq!(record.phase, CancellationPhase::OutcomeUnknown);
            assert!(!record.workspace_reassignable());
        }
    }
}

#[test]
fn unknown_is_reinspectable_and_a_matching_zero_survivor_scan_resolves_it() {
    let intent = intent();
    let request_id = intent.request_id;
    let mut store = MemoryDurableStore::new(intent.owner);
    let mut control = FakeControl::new(&intent, store.current_owner.clone());
    control.failures.push("inspect");
    let unknown = request_cancellation(&mut store, &mut control, intent, timeout()).unwrap();
    assert_eq!(unknown.phase, CancellationPhase::OutcomeUnknown);
    assert!(!unknown.workspace_reassignable());
    let publications = store.state.publications.len();

    control.failures.clear();
    let resolved =
        reconcile_cancellation(&mut store, &mut control, request_id, owner(), timeout()).unwrap();

    assert_eq!(resolved.phase, CancellationPhase::Quiescent);
    assert!(resolved.workspace_reassignable());
    assert_eq!(
        control.calls,
        ["grace", "kill", "reap", "inspect", "inspect"]
    );
    assert_eq!(store.state.publications.len(), publications + 2);
}

#[test]
fn successor_fence_finishes_cleanup_without_stale_publications() {
    let intent = intent();
    let request_id = intent.request_id;
    let successor = owner_with_fence(intent.owner, 8);
    let mut store = MemoryDurableStore::new(intent.owner);
    let mut control = FakeControl::new(&intent, store.current_owner.clone());
    control.stale_on = Some("grace");
    assert!(matches!(
        request_cancellation(&mut store, &mut control, intent, timeout()),
        Err(CancellationError::Store(CancellationStoreError::StaleOwner))
    ));
    assert!(store.state.publications.is_empty());

    control.stale_on = None;
    let record =
        reconcile_cancellation(&mut store, &mut control, request_id, successor, timeout()).unwrap();

    assert_eq!(record.phase, CancellationPhase::Quiescent);
    assert_eq!(record.quiescence.unwrap().owner, successor);
    assert_eq!(control.calls, ["grace", "grace", "kill", "reap", "inspect"]);
    assert!(
        store
            .state
            .publications
            .iter()
            .all(|publication| publication.owner() == successor)
    );
}

#[test]
fn effect_before_commit_crashes_repeat_only_the_idempotent_operation() {
    for (crash_phase, repeated) in [
        (CancellationPhase::KillRequested, "grace"),
        (CancellationPhase::ReapRequested, "kill"),
        (CancellationPhase::InspectRequested, "reap"),
        (CancellationPhase::Quiescent, "inspect"),
    ] {
        let intent = intent();
        let request_id = intent.request_id;
        let mut store = MemoryDurableStore::new(intent.owner).crash_before_commit(crash_phase);
        let mut control = FakeControl::new(&intent, store.current_owner.clone());
        assert!(matches!(
            request_cancellation(&mut store, &mut control, intent, timeout()),
            Err(CancellationError::Store(
                CancellationStoreError::Unavailable(_)
            ))
        ));

        let record =
            reconcile_cancellation(&mut store, &mut control, request_id, owner(), timeout())
                .unwrap();
        assert_eq!(record.phase, CancellationPhase::Quiescent);
        assert_eq!(
            control
                .calls
                .iter()
                .filter(|call| **call == repeated)
                .count(),
            2
        );
        assert_eq!(store.state.publications.len(), 5);
    }
}
