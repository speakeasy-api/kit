use kit::executor::process::tree::{
    BoundaryControl, BoundaryIdentity, BoundaryKind, Containment, Error, HostSupport, Inspection,
    LifecycleState, Ownership, PersistedBoundary, ProcessTree,
};
use std::{
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
enum Failure {
    None,
    Kill,
    Reap,
    Inspect,
    Release,
}

struct ControlledBoundary {
    identity: BoundaryIdentity,
    survivors: u32,
    failure: Failure,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl ControlledBoundary {
    fn new(survivors: u32, failure: Failure) -> Self {
        Self {
            identity: identity("fixture-start-token"),
            survivors,
            failure,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl BoundaryControl for ControlledBoundary {
    fn identity(&self) -> &BoundaryIdentity {
        &self.identity
    }

    fn containment(&self) -> Containment {
        Containment::Complete
    }

    fn release(&mut self, _deadline: Instant) -> io::Result<()> {
        self.calls.lock().unwrap().push("release");
        if matches!(self.failure, Failure::Release) {
            Err(io::Error::other("injected release failure"))
        } else {
            Ok(())
        }
    }

    fn kill_boundary(&mut self, _deadline: Instant) -> io::Result<()> {
        self.calls.lock().unwrap().push("kill");
        if matches!(self.failure, Failure::Kill) {
            Err(io::Error::other("injected kill failure"))
        } else {
            self.survivors = 0;
            Ok(())
        }
    }

    fn wait_and_reap(&mut self, _deadline: Instant) -> io::Result<()> {
        self.calls.lock().unwrap().push("reap");
        if matches!(self.failure, Failure::Reap) {
            Err(io::Error::other("injected reap failure"))
        } else {
            Ok(())
        }
    }

    fn inspect(&mut self, _deadline: Instant) -> io::Result<Inspection> {
        self.calls.lock().unwrap().push("inspect");
        if matches!(self.failure, Failure::Inspect) {
            Err(io::Error::other("injected inspection failure"))
        } else {
            Ok(Inspection {
                identity: self.identity.clone(),
                survivors: Some(self.survivors),
                quiescent: self.survivors == 0,
            })
        }
    }
}

fn owner() -> Ownership {
    Ownership::new("daemon-service-a", "attempt-42").unwrap()
}

fn identity(token: &str) -> BoundaryIdentity {
    BoundaryIdentity::new(
        BoundaryKind::Container,
        "fixture-boundary",
        "ownership-token",
        token,
    )
    .unwrap()
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn start(control: ControlledBoundary) -> Result<ProcessTree<ControlledBoundary>, Error> {
    ProcessTree::start(owner(), control, |_: &PersistedBoundary| Ok(()), deadline())
}

#[test]
fn controlled_cancel_and_daemon_recovery_are_quiescent_100_of_100() {
    // The fake covers lifecycle semantics only. It is not enforcement evidence.
    for _ in 0..100 {
        let mut tree = start(ControlledBoundary::new(51, Failure::None)).unwrap();
        let result = tree.cancel(&owner(), deadline()).unwrap();
        assert_eq!(result.survivors, 0);
        assert_eq!(tree.state(), LifecycleState::Quiescent);

        let control = ControlledBoundary::new(51, Failure::None);
        let persisted = PersistedBoundary {
            ownership: owner(),
            identity: control.identity.clone(),
        };
        let recovered =
            ProcessTree::reconcile_after_daemon_crash(persisted, control, deadline()).unwrap();
        assert_eq!(recovered.survivors, 0);
        assert_eq!(recovered.state, LifecycleState::Quiescent);
    }
}

#[test]
fn persisted_boundary_round_trips_and_rejects_a_reused_identity() {
    let persisted = PersistedBoundary {
        ownership: owner(),
        identity: identity("kernel-start-a"),
    };
    assert_eq!(
        PersistedBoundary::decode(&persisted.encode()).unwrap(),
        persisted
    );

    let reused = ControlledBoundary {
        identity: identity("kernel-start-b"),
        survivors: 0,
        failure: Failure::None,
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    assert!(matches!(
        ProcessTree::recover(persisted, reused),
        Err(Error::BoundaryIdentityMismatch)
    ));
}

#[test]
fn cancellation_never_promotes_unknown_or_surviving_boundaries_to_success() {
    for failure in [Failure::Kill, Failure::Reap, Failure::Inspect] {
        let mut tree = start(ControlledBoundary::new(0, failure)).unwrap();
        assert!(matches!(
            tree.cancel(&owner(), deadline()),
            Err(Error::OutcomeUnknown { .. })
        ));
        assert_eq!(tree.state(), LifecycleState::OutcomeUnknown);
    }

    let mut survivors = start(ControlledBoundary::new(51, Failure::Kill)).unwrap();
    assert!(matches!(
        survivors.cancel(&owner(), deadline()),
        Err(Error::NotQuiescent {
            survivors: Some(51),
            ..
        })
    ));
    assert_eq!(survivors.state(), LifecycleState::NotQuiescent);
}

#[test]
fn wrong_attempt_has_no_kill_authority() {
    let mut tree = start(ControlledBoundary::new(51, Failure::None)).unwrap();
    let intruder = Ownership::new("daemon-service-a", "attempt-43").unwrap();
    assert_eq!(
        tree.cancel(&intruder, deadline()),
        Err(Error::OwnershipMismatch)
    );
    assert_eq!(tree.state(), LifecycleState::Running);
}

#[test]
fn dropping_a_live_tree_boundedly_cancels_it() {
    let control = ControlledBoundary::new(7, Failure::None);
    let calls = control.calls.clone();
    let tree = start(control).unwrap();
    drop(tree);
    assert_eq!(
        *calls.lock().unwrap(),
        ["release", "kill", "reap", "inspect"]
    );
}

#[test]
fn durable_start_persists_before_release_and_cleans_up_failures() {
    let control = ControlledBoundary::new(0, Failure::None);
    let calls = control.calls.clone();
    ProcessTree::start(
        owner(),
        control,
        |_: &PersistedBoundary| {
            calls.lock().unwrap().push("persist");
            Ok(())
        },
        deadline(),
    )
    .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        ["persist", "release", "kill", "reap", "inspect"]
    );

    let control = ControlledBoundary::new(0, Failure::Release);
    let calls = control.calls.clone();
    assert!(matches!(
        ProcessTree::start(
            owner(),
            control,
            |_: &PersistedBoundary| {
                calls.lock().unwrap().push("persist");
                Ok(())
            },
            deadline(),
        ),
        Err(Error::StartFailed { .. })
    ));
    assert_eq!(
        *calls.lock().unwrap(),
        ["persist", "release", "kill", "reap", "inspect"]
    );

    let control = ControlledBoundary::new(0, Failure::None);
    let calls = control.calls.clone();
    assert!(matches!(
        ProcessTree::start(
            owner(),
            control,
            |_: &PersistedBoundary| {
                calls.lock().unwrap().push("persist");
                Err(io::Error::other("injected persistence failure"))
            },
            deadline(),
        ),
        Err(Error::StartFailed { .. })
    ));
    assert_eq!(
        *calls.lock().unwrap(),
        ["persist", "kill", "reap", "inspect"]
    );
}

#[test]
fn host_support_is_fail_closed_and_macos_does_not_claim_setsid_containment() {
    let complete = HostSupport::trusted_local(true);
    if cfg!(target_os = "macos") {
        assert!(
            matches!(complete, HostSupport::Unavailable { reason } if reason.contains("setsid"))
        );
        assert!(matches!(
            HostSupport::trusted_local(false),
            HostSupport::Supported {
                containment: kit::executor::process::tree::Containment::ProcessGroupOnly,
                ..
            }
        ));
        #[cfg(target_os = "macos")]
        {
            let identity = BoundaryIdentity::new(
                BoundaryKind::MacOsProcessGroup,
                "123",
                "persisted-owner",
                "persisted-start-token",
            )
            .unwrap();
            assert!(matches!(
                kit::executor::process::tree::MacOsProcessGroup::recover(&identity),
                Err(Error::Unavailable { .. })
            ));
        }
    } else if cfg!(target_os = "linux") {
        assert!(
            matches!(complete, HostSupport::Unavailable { reason } if reason.contains("cgroup-v2"))
        );
    } else if cfg!(target_os = "windows") {
        assert!(matches!(
            complete,
            HostSupport::Delegated {
                implementation_unit: "4.14 Job Object"
            }
        ));
    }

    assert!(HostSupport::trusted_helper(&identity("trusted-runtime-digest")).is_ok());
}
