use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Barrier, Mutex},
};

use kit::{
    api::auth::{
        contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    },
    domain::{
        config::Grant,
        ids::{AttemptId, PrincipalId, ProcessId, ProjectId},
        lifecycle::{AttemptOwnership, FencingToken, ProcessClaim, ProcessOwnership},
        secret::SecretLease,
    },
    executor::terminal::{
        FakePtyDriver, OutputRead, OutputRetention, PtyDriver, ResizeRead, TerminalAllocation,
        TerminalControl, TerminalError, TerminalLifecycle, TerminalManager, TerminalRequest,
        TerminalSize, TerminalSnapshot,
    },
    telemetry::redact::{
        CaptureBoundary, CapturePersistencePolicy, CaptureRedactor, SanitizedCapture,
    },
};

fn output(bytes: &[u8]) -> SanitizedCapture {
    CaptureRedactor::new(&[]).sanitize(CaptureBoundary::TerminalMetadata, bytes)
}

#[test]
fn secret_absent_terminal_history() {
    const RAW: &str = "split-secret";
    const PERCENT: &str = "%73%70%6C%69%74%2D%73%65%63%72%65%74";
    const BASE64: &str = "c3BsaXQtc2VjcmV0";
    let lease = SecretLease::new(RAW);
    let leases = [lease];
    let redactor = CaptureRedactor::new(&leases);
    let mut capture = redactor.start(CaptureBoundary::TerminalMetadata);
    for chunk in [
        b"prefix split-".as_slice(),
        b"secret ",
        b"%73%70%6C%69%74%2D%73",
        b"%65%63%72%65%74 c3BsaXQ",
        b"tc2VjcmV0 suffix",
    ] {
        capture.push(chunk).unwrap();
    }
    capture.finish().unwrap();

    let (manager, _, _, _, _, control) = manager();
    manager.append_output(&control, &capture, 1).unwrap();
    let persisted = serde_json::to_string(&manager.snapshot(&control).unwrap()).unwrap();
    for secret in [RAW, PERCENT, BASE64] {
        assert!(!persisted.contains(secret), "terminal retained {secret}");
    }
    assert_eq!(format!("{capture:?}").matches(RAW).count(), 0);
}

type TestStore = fn(&TerminalSnapshot) -> io::Result<()>;
type TestManager = TerminalManager<FakePtyDriver, TestStore>;

fn save_snapshot(_: &TerminalSnapshot) -> io::Result<()> {
    Ok(())
}

fn authenticated(
    principal_id: PrincipalId,
    project_id: ProjectId,
    grants: impl IntoIterator<Item = Grant>,
) -> AuthenticatedPrincipal {
    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        1_000,
        GrantSnapshot::new(principal_id, project_id, grants),
    )]));
    authenticator
        .authenticate(&LocalPeerObservation::from_transport(1_000, 7, 1_000))
        .unwrap()
}

fn process_claim(
    process_id: ProcessId,
    attempt_id: AttemptId,
    principal_id: PrincipalId,
    fence: u64,
) -> ProcessClaim {
    ProcessClaim::new(
        process_id,
        ProcessOwnership::Attempt(AttemptOwnership::new(
            attempt_id,
            principal_id,
            FencingToken::new(fence),
        )),
    )
}

fn manager() -> (
    Arc<TestManager>,
    FakePtyDriver,
    AuthenticatedPrincipal,
    ProcessId,
    AttemptId,
    TerminalControl,
) {
    let driver = FakePtyDriver::default();
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let process_id = ProcessId::generate().unwrap();
    let attempt_id = AttemptId::generate().unwrap();
    let principal = authenticated(
        principal_id,
        project_id,
        [Grant::ProcessSpawn, Grant::WorkspaceRead],
    );
    let manager = Arc::new(TerminalManager::new(
        project_id,
        driver.clone(),
        save_snapshot as TestStore,
    ));
    let TerminalAllocation::Pty { control, .. } = manager
        .allocate(
            TerminalRequest::pty(CapturePersistencePolicy::no_secrets()),
            &principal,
            process_claim(process_id, attempt_id, principal_id, 7),
            "boundary-1",
            TerminalSize::new(80, 24).unwrap(),
            OutputRetention::new(8, 100),
        )
        .unwrap()
    else {
        unreachable!()
    };
    (manager, driver, principal, process_id, attempt_id, control)
}

#[test]
fn detach_reclaims_viewer_slots_and_writer_lease() {
    let (manager, _, principal, _, _, control) = manager();
    for _ in 0..2_000 {
        let mut viewer = manager.attach_viewer(&control, &principal).unwrap();
        manager.detach(&mut viewer).unwrap();
    }

    let mut writer = manager.claim_writer(&control, &principal, 1, 100).unwrap();
    manager.detach(&mut writer).unwrap();
    assert!(manager.claim_writer(&control, &principal, 2, 100).is_ok());
}

#[test]
fn terminal_uses_auth_boundary_and_denies_cross_project_authority() {
    let project_id = ProjectId::generate().unwrap();
    let other_project = ProjectId::generate().unwrap();
    let principal_id = PrincipalId::generate().unwrap();
    let manager = TerminalManager::new(
        project_id,
        FakePtyDriver::default(),
        save_snapshot as TestStore,
    );
    let wrong_scope = authenticated(principal_id, other_project, [Grant::ProcessSpawn]);
    assert!(matches!(
        manager.allocate(
            TerminalRequest::pty(CapturePersistencePolicy::no_secrets()),
            &wrong_scope,
            process_claim(
                ProcessId::generate().unwrap(),
                AttemptId::generate().unwrap(),
                principal_id,
                1,
            ),
            "boundary",
            TerminalSize::new(80, 24).unwrap(),
            OutputRetention::new(16, 100),
        ),
        Err(TerminalError::PermissionDenied)
    ));

    let no_spawn = authenticated(principal_id, project_id, [Grant::WorkspaceRead]);
    assert!(matches!(
        manager.allocate(
            TerminalRequest::pty(CapturePersistencePolicy::no_secrets()),
            &no_spawn,
            process_claim(
                ProcessId::generate().unwrap(),
                AttemptId::generate().unwrap(),
                principal_id,
                1,
            ),
            "boundary",
            TerminalSize::new(80, 24).unwrap(),
            OutputRetention::new(16, 100),
        ),
        Err(TerminalError::PermissionDenied)
    ));
}

#[test]
fn one_writer_survives_one_thousand_concurrent_claims() {
    let (manager, _, principal, _, _, control) = manager();
    let barrier = Arc::new(Barrier::new(1_000));
    let holders = Arc::new(Mutex::new(Vec::new()));
    std::thread::scope(|scope| {
        for _ in 0..1_000 {
            let manager = manager.clone();
            let principal = principal.clone();
            let barrier = barrier.clone();
            let holders = holders.clone();
            let control = &control;
            scope.spawn(move || {
                barrier.wait();
                if let Ok(holder) = manager.claim_writer(control, &principal, 10, 1_000) {
                    holders.lock().unwrap().push(holder);
                }
            });
        }
    });
    assert_eq!(holders.lock().unwrap().len(), 1);
}

#[test]
fn viewers_are_read_only_and_output_and_resize_retention_report_gaps() {
    let (manager, driver, principal, _, _, control) = manager();
    let viewer = manager.attach_viewer(&control, &principal).unwrap();
    assert!(matches!(
        manager.write_input(&viewer, b"secret", 1),
        Err(TerminalError::ReadOnlyViewer)
    ));
    let mut writer = manager.claim_writer(&control, &principal, 1, 10).unwrap();
    assert_eq!(manager.renew_writer(&writer, 2, 20).unwrap(), 22);
    manager.write_input(&writer, b"secret", 2).unwrap();
    manager
        .resize(&writer, TerminalSize::new(120, 40).unwrap(), 2)
        .unwrap();
    assert!(matches!(
        manager.read_resizes(&viewer, 1, 2).unwrap(),
        ResizeRead::Events { ref events, .. } if events.len() == 1
    ));
    let stale_writer = writer;
    manager.release_writer(&mut writer).unwrap();
    let replacement = manager.claim_writer(&control, &principal, 3, 10).unwrap();
    assert_eq!(replacement.writer_epoch(), Some(2));
    assert!(matches!(
        manager.write_input(&stale_writer, b"stale", 3),
        Err(TerminalError::StaleWriter)
    ));

    assert_eq!(
        manager
            .append_output(&control, &output(b"1234"), 1)
            .unwrap(),
        1
    );
    assert_eq!(
        manager
            .append_output(&control, &output(b"5678"), 2)
            .unwrap(),
        2
    );
    assert_eq!(
        manager.append_output(&control, &output(b"90"), 3).unwrap(),
        3
    );
    assert!(manager.snapshot(&control).unwrap().retained_output_bytes <= 8);
    assert_eq!(
        manager.read_output(&viewer, 1, 3).unwrap(),
        OutputRead::Gap {
            requested: 1,
            oldest_available: 2,
        }
    );
    assert_eq!(driver.state().input_byte_count, 6);
}

#[test]
fn stale_attempt_fence_denies_every_terminal_operation_before_driver_writes() {
    let (manager, driver, principal, process_id, attempt_id, old_control) = manager();
    let viewer = manager.attach_viewer(&old_control, &principal).unwrap();
    let mut writer = manager
        .claim_writer(&old_control, &principal, 1, 100)
        .unwrap();
    let principal_id = principal.principal_id();
    assert_eq!(
        manager
            .allocate(
                TerminalRequest::default(),
                &principal,
                process_claim(process_id, attempt_id, principal_id, 8),
                "boundary-1",
                TerminalSize::new(80, 24).unwrap(),
                OutputRetention::new(16, 100),
            )
            .unwrap(),
        TerminalAllocation::Pipes
    );

    assert!(matches!(
        manager.attach_viewer(&old_control, &principal),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.claim_writer(&old_control, &principal, 2, 10),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.renew_writer(&writer, 2, 10),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.release_writer(&mut writer),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.write_input(&writer, b"stale", 2),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.resize(&writer, TerminalSize::new(90, 30).unwrap(), 2),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.append_output(&old_control, &output(b"stale"), 2),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.read_output(&viewer, 1, 2),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.read_resizes(&viewer, 1, 2),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.close(&old_control),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert!(matches!(
        manager.snapshot(&old_control),
        Err(TerminalError::StaleProcessClaim)
    ));
    assert_eq!(driver.state().input_byte_count, 0);
}

#[test]
fn empty_output_flood_is_rejected_without_retained_metadata_growth() {
    let (manager, _, _, _, _, control) = manager();
    for now in 0..1_000 {
        assert!(matches!(
            manager.append_output(&control, &output(b""), now),
            Err(TerminalError::InvalidRequest("empty output chunk"))
        ));
    }
    let snapshot = manager.snapshot(&control).unwrap();
    assert!(snapshot.output.is_empty());
    assert_eq!(snapshot.next_output_sequence, 1);
}

#[test]
fn output_and_resize_event_counts_are_bounded_and_report_count_gaps() {
    let (manager, _, principal, process_id, attempt_id, _) = manager();
    let TerminalAllocation::Pty { control, .. } = manager
        .allocate(
            TerminalRequest::pty(CapturePersistencePolicy::no_secrets()),
            &principal,
            process_claim(process_id, attempt_id, principal.principal_id(), 7),
            "boundary-1",
            TerminalSize::new(80, 24).unwrap(),
            OutputRetention::new(1_000_000, 10_000),
        )
        .unwrap()
    else {
        unreachable!()
    };
    let viewer = manager.attach_viewer(&control, &principal).unwrap();
    let writer = manager
        .claim_writer(&control, &principal, 1, 10_000)
        .unwrap();
    for _ in 0..1_025 {
        manager.append_output(&control, &output(b"x"), 1).unwrap();
        manager
            .resize(&writer, TerminalSize::new(80, 24).unwrap(), 1)
            .unwrap();
    }
    let snapshot = manager.snapshot(&control).unwrap();
    assert_eq!(snapshot.output.len(), 1_024);
    assert_eq!(snapshot.resizes.len(), 1_024);
    assert_eq!(
        manager.read_output(&viewer, 1, 1).unwrap(),
        OutputRead::Gap {
            requested: 1,
            oldest_available: 2,
        }
    );
    assert_eq!(
        manager.read_resizes(&viewer, 1, 1).unwrap(),
        ResizeRead::Gap {
            requested: 1,
            oldest_available: 2,
        }
    );
}

#[derive(Clone)]
struct BlockingDriver {
    inner: FakePtyDriver,
    allocation_started: Arc<Barrier>,
    finish_allocation: Arc<Barrier>,
}

impl PtyDriver for BlockingDriver {
    fn allocate(
        &self,
        terminal_id: kit::domain::ids::TerminalId,
        owner: &kit::executor::terminal::TerminalOwner,
        size: TerminalSize,
    ) -> Result<(), kit::executor::terminal::TerminalError> {
        self.allocation_started.wait();
        self.finish_allocation.wait();
        self.inner.allocate(terminal_id, owner, size)
    }

    fn write_input(
        &self,
        terminal_id: kit::domain::ids::TerminalId,
        bytes: &[u8],
    ) -> io::Result<()> {
        self.inner.write_input(terminal_id, bytes)
    }

    fn resize(
        &self,
        terminal_id: kit::domain::ids::TerminalId,
        size: TerminalSize,
    ) -> io::Result<()> {
        self.inner.resize(terminal_id, size)
    }

    fn interrupt(&self, terminal_id: kit::domain::ids::TerminalId) -> io::Result<()> {
        self.inner.interrupt(terminal_id)
    }
}

#[test]
fn daemon_death_wins_a_concurrent_allocation_and_blocks_future_allocations() {
    let project_id = ProjectId::generate().unwrap();
    let principal_id = PrincipalId::generate().unwrap();
    let principal = authenticated(principal_id, project_id, [Grant::ProcessSpawn]);
    let started = Arc::new(Barrier::new(2));
    let finish = Arc::new(Barrier::new(2));
    let inner = FakePtyDriver::default();
    let manager = Arc::new(TerminalManager::new(
        project_id,
        BlockingDriver {
            inner: inner.clone(),
            allocation_started: started.clone(),
            finish_allocation: finish.clone(),
        },
        save_snapshot as TestStore,
    ));
    let attempt_id = AttemptId::generate().unwrap();
    let process_id = ProcessId::generate().unwrap();
    std::thread::scope(|scope| {
        let thread_manager = manager.clone();
        let principal = principal.clone();
        let allocation = scope.spawn(move || {
            thread_manager.allocate(
                TerminalRequest::pty(CapturePersistencePolicy::no_secrets()),
                &principal,
                process_claim(process_id, attempt_id, principal_id, 1),
                "boundary",
                TerminalSize::new(80, 24).unwrap(),
                OutputRetention::new(16, 100),
            )
        });
        started.wait();
        manager.daemon_died().unwrap();
        finish.wait();
        assert!(matches!(
            allocation.join().unwrap(),
            Err(TerminalError::DaemonUnavailable)
        ));
    });
    assert!(!inner.state().interrupted.is_empty());
    assert!(matches!(
        manager.allocate(
            TerminalRequest::pty(CapturePersistencePolicy::no_secrets()),
            &principal,
            process_claim(process_id, attempt_id, principal_id, 2),
            "boundary",
            TerminalSize::new(80, 24).unwrap(),
            OutputRetention::new(16, 100),
        ),
        Err(TerminalError::DaemonUnavailable)
    ));
}

#[test]
fn restored_local_pty_is_interrupted_and_records_attempt_interruption() {
    let (manager, _, principal, _, _, control) = manager();
    manager
        .append_output(&control, &output(b"retained"), 1)
        .unwrap();
    let old_viewer = manager.attach_viewer(&control, &principal).unwrap();
    let old_writer = manager.claim_writer(&control, &principal, 1, 100).unwrap();
    manager
        .resize(&old_writer, TerminalSize::new(100, 40).unwrap(), 1)
        .unwrap();
    let persisted = manager.snapshot(&control).unwrap();
    let project_id = persisted.owner.project_id;
    let driver = FakePtyDriver::default();
    let saved = Arc::new(Mutex::new(Vec::<TerminalSnapshot>::new()));
    let saved_for_store = saved.clone();
    let restored = TerminalManager::new(
        project_id,
        driver.clone(),
        move |snapshot: &TerminalSnapshot| {
            saved_for_store.lock().unwrap().push(snapshot.clone());
            Ok(())
        },
    );
    let interrupted_attempts = Arc::new(Mutex::new(Vec::new()));
    let callback = interrupted_attempts.clone();
    let controls = restored
        .restore_snapshots([persisted.clone()], 1, move |owner| {
            callback.lock().unwrap().push(owner.attempt_id);
            Ok(())
        })
        .unwrap();
    let replacement = restored.attach_viewer(&controls[0], &principal).unwrap();
    assert!(matches!(
        restored.read_output(&replacement, 1, 1).unwrap(),
        OutputRead::Chunks { ref chunks, .. } if chunks[0].bytes() == b"retained"
    ));
    assert!(matches!(
        restored.read_resizes(&replacement, 1, 1).unwrap(),
        ResizeRead::Events { ref events, .. } if events[0].size == TerminalSize::new(100, 40).unwrap()
    ));
    assert!(matches!(
        restored.claim_writer(&controls[0], &principal, 2, 100),
        Err(TerminalError::TerminalInactive)
    ));
    assert!(matches!(
        restored.write_input(&old_writer, b"forbidden", 2),
        Err(TerminalError::PermissionDenied)
    ));
    assert!(matches!(
        restored.read_output(&old_viewer, 1, 1),
        Err(TerminalError::PermissionDenied)
    ));
    assert_eq!(
        interrupted_attempts.lock().unwrap().as_slice(),
        &[persisted.owner.attempt_id]
    );
    assert_eq!(
        saved.lock().unwrap().last().unwrap().lifecycle,
        TerminalLifecycle::Interrupted
    );
    assert_eq!(driver.state().interrupted, vec![persisted.terminal_id]);
}

#[test]
fn pipes_are_default_and_require_spawn_authority_without_allocating_a_pty() {
    let project_id = ProjectId::generate().unwrap();
    let principal_id = PrincipalId::generate().unwrap();
    let principal = authenticated(principal_id, project_id, [Grant::ProcessSpawn]);
    let driver = FakePtyDriver::default();
    let manager = TerminalManager::new(project_id, driver.clone(), save_snapshot as TestStore);
    assert_eq!(
        manager
            .allocate(
                TerminalRequest::default(),
                &principal,
                process_claim(
                    ProcessId::generate().unwrap(),
                    AttemptId::generate().unwrap(),
                    principal_id,
                    1,
                ),
                "boundary",
                TerminalSize::new(80, 24).unwrap(),
                OutputRetention::new(16, 100),
            )
            .unwrap(),
        TerminalAllocation::Pipes
    );
    assert!(driver.state().allocated.is_empty());
}
