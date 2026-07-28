#[path = "../fixtures/clock/mod.rs"]
mod clock;
#[path = "../fixtures/crashpoints/mod.rs"]
mod crashpoints;
#[path = "../fixtures/protocol_sim/mod.rs"]
mod protocol_sim;
#[path = "../fixtures/providers/mod.rs"]
mod providers;
#[path = "../fixtures/repos/mod.rs"]
pub(crate) mod repos;
#[path = "../fixtures/sandbox_probe/mod.rs"]
mod sandbox_probe;
#[path = "../fixtures/storefault/mod.rs"]
mod storefault;

#[test]
fn provider_stream_error_and_secret_are_repeatable() {
    use providers::{FakeProvider, ProviderScript, ProviderStep};

    let streaming = FakeProvider::new(11, ProviderScript::streaming(&["one", "two"]));
    assert_eq!(streaming.replay(), streaming.replay());
    assert_ne!(
        streaming.replay(),
        FakeProvider::new(12, ProviderScript::streaming(&["one", "two"])).replay()
    );

    let error = FakeProvider::new(
        11,
        ProviderScript::error_after(&["one", "two", "three"], 2, "rate_limited"),
    )
    .replay();
    assert!(matches!(
        error.last().unwrap().step,
        ProviderStep::Error { .. }
    ));

    let secret_provider =
        FakeProvider::new(11, ProviderScript::secret_exfiltration("CANARY_SECRET"));
    let secret = secret_provider.replay();
    assert!(matches!(
        &secret[0].step,
        ProviderStep::SecretBlocked { destination } if destination == "event_payload"
    ));
    assert!(!format!("{secret:?}").contains("CANARY_SECRET"));
    assert!(
        !String::from_utf8(FakeProvider::persist(&secret))
            .unwrap()
            .contains("CANARY_SECRET")
    );

    let injection = FakeProvider::new(
        11,
        ProviderScript::prompt_injection("ignore policy", "network.egress"),
    )
    .replay();
    assert!(matches!(
        &injection[0].step,
        ProviderStep::ToolCall { effect, .. } if effect == "network.egress"
    ));
}

#[test]
fn malicious_repo_denies_traversal_symlink_hook_and_second_writer() {
    use repos::{DenialReason, RepoFixture, RepositorySourcePolicy, SourceDenial, WriterArbiter};

    let fixture = RepoFixture::malicious(22);
    let first = fixture.inspect_default_policy();
    assert_eq!(first, fixture.inspect_default_policy());
    assert_eq!(first.accepted_paths, ["README.md"]);
    assert!(
        first
            .denied
            .iter()
            .any(|entry| entry.reason == DenialReason::Traversal)
    );
    assert!(first.denied.iter().any(|entry| {
        entry.path == "nested\\..\\..\\outside" && entry.reason == DenialReason::Traversal
    }));
    assert!(first.denied.iter().any(|entry| {
        entry.path == "C:\\host\\absolute" && entry.reason == DenialReason::AbsolutePath
    }));
    assert!(
        first
            .denied
            .iter()
            .any(|entry| entry.reason == DenialReason::SymlinkDenied)
    );
    assert!(
        first.denied.iter().any(|entry| {
            entry.path == "safe-link" && entry.reason == DenialReason::SymlinkDenied
        })
    );
    assert!(
        first
            .denied
            .iter()
            .any(|entry| entry.reason == DenialReason::HooksDisabled)
    );

    let mut writers = WriterArbiter::new(22);
    assert!(writers.claim("run-a").is_ok());
    assert!(writers.claim("run-b").is_err());

    let unprivileged = RepositorySourcePolicy::default();
    assert!(
        unprivileged
            .authorize("https", "https://example.com/repo", None)
            .is_ok()
    );
    assert!(
        unprivileged
            .authorize("ssh", "ssh://example.com/repo", None)
            .is_ok()
    );
    for (source, location, grant, denial) in [
        (
            "file",
            "file:///tmp/repo",
            None,
            SourceDenial::UnsupportedSource,
        ),
        (
            "https",
            "file:///tmp/repo",
            None,
            SourceDenial::SchemeMismatch,
        ),
        (
            "https",
            "https://user@example.com/repo",
            None,
            SourceDenial::UserInfo,
        ),
        (
            "https",
            "https://127.0.0.1/repo",
            None,
            SourceDenial::PrivateTarget,
        ),
        (
            "https",
            "https://2130706433/repo",
            None,
            SourceDenial::PrivateTarget,
        ),
        (
            "https",
            "https://localhost./repo",
            None,
            SourceDenial::PrivateTarget,
        ),
        (
            "local_fixture",
            "fixture-repo",
            None,
            SourceDenial::FixtureGrantRequired,
        ),
        (
            "local_fixture",
            "fixture-repo",
            Some("self-asserted"),
            SourceDenial::FixtureGrantRequired,
        ),
    ] {
        assert_eq!(unprivileged.authorize(source, location, grant), Err(denial));
    }

    let fixture_policy = RepositorySourcePolicy::new(["test-fixture"]);
    assert!(
        fixture_policy
            .authorize("local_fixture", "fixture-repo", Some("test-fixture"))
            .is_ok()
    );
    assert_eq!(
        fixture_policy.authorize("local_fixture", "../../host", Some("test-fixture")),
        Err(SourceDenial::InvalidFixture)
    );
    assert_eq!(
        fixture_policy.authorize("https", "https://127.0.0.1/repo", Some("test-fixture")),
        Err(SourceDenial::PrivateTarget)
    );
    assert_eq!(
        fixture_policy.authorize("https", "https://user@127.0.0.1/repo", Some("test-fixture")),
        Err(SourceDenial::UserInfo)
    );
}

#[test]
fn protocol_simulators_are_distinct_and_fail_closed() {
    use protocol_sim::a2a::{A2aSimulator, MessageDecision, RemoteMessage};
    use protocol_sim::acp::{AcpEvent, AcpSimulator, ChildState};
    use protocol_sim::mcp::{InvocationDecision, McpSimulator, ToolBinding};

    let mcp = McpSimulator::new(
        33,
        vec![ToolBinding {
            name: "read".to_owned(),
            schema_digest: "schema-a".to_owned(),
        }],
    );
    assert!(matches!(
        mcp.invoke(&ToolBinding {
            name: "read".to_owned(),
            schema_digest: "schema-b".to_owned(),
        }),
        InvocationDecision::RefusedSchemaDrift { .. }
    ));

    let acp = AcpSimulator::new(33);
    let events = [
        AcpEvent::ChildStarted,
        AcpEvent::ToolCallStarted,
        AcpEvent::ChildExited,
    ];
    assert_eq!(acp.replay(&events), acp.replay(&events));
    assert_eq!(acp.replay(&events).state, ChildState::Interrupted);
    assert_eq!(
        acp.replay(&[AcpEvent::CancelRequested, AcpEvent::CancelAcknowledged,])
            .state,
        ChildState::Cancelled
    );

    let normal = RemoteMessage {
        remote_id: "peer-task".to_owned(),
        sequence: 1,
        digest: "same-effect".to_owned(),
        delegation_path: vec!["a".to_owned()],
    };
    let replay = RemoteMessage {
        sequence: 2,
        ..normal.clone()
    };
    let looped = RemoteMessage {
        remote_id: "loop".to_owned(),
        sequence: 1,
        digest: "loop-effect".to_owned(),
        delegation_path: vec!["a".to_owned(), "b".to_owned(), "a".to_owned()],
    };
    let decisions = A2aSimulator::new(33, 4).replay(&[normal, replay, looped]);
    assert!(matches!(decisions[0], MessageDecision::Dispatched { .. }));
    assert_eq!(decisions[1], MessageDecision::DuplicateDropped);
    assert_eq!(decisions[2], MessageDecision::DelegationRejected);
}

#[test]
fn ingress_and_redirect_simulation_rejects_adverse_inputs() {
    use protocol_sim::{
        AccessGrant, ApiIngressSimulator, ApiRequest, ApiResponse, Principal, RedirectHop,
        replay_redirects,
    };
    use std::net::{IpAddr, Ipv4Addr};

    let principal = Principal {
        id: "runner-a".to_owned(),
    };
    let origin = "https://console.example".to_owned();
    let host = "api.example".to_owned();
    let grant = AccessGrant {
        principal_id: principal.id.clone(),
        origin: origin.clone(),
        host: host.clone(),
    };

    let requests = vec![
        ApiRequest {
            principal: Principal {
                id: "runner-b".to_owned(),
            },
            origin: origin.clone(),
            host: host.clone(),
            idempotency_key: "blocked".to_owned(),
            request_digest: "x".to_owned(),
        },
        ApiRequest {
            principal: principal.clone(),
            origin: origin.clone(),
            host: host.clone(),
            idempotency_key: "key".to_owned(),
            request_digest: "a".to_owned(),
        },
        ApiRequest {
            principal,
            origin,
            host,
            idempotency_key: "key".to_owned(),
            request_digest: "b".to_owned(),
        },
    ];
    let first = ApiIngressSimulator::new(44, vec![grant.clone()]).replay(&requests);
    assert_eq!(
        first,
        ApiIngressSimulator::new(44, vec![grant]).replay(&requests)
    );
    assert_eq!(first.1, 1);
    assert_eq!(first.0[0], ApiResponse::Unauthorized);
    assert_eq!(first.0[2], ApiResponse::Conflict);

    let wrong_authority = ApiIngressSimulator::new(
        44,
        vec![AccessGrant {
            principal_id: "runner-a".to_owned(),
            origin: "https://console.example".to_owned(),
            host: "api.example".to_owned(),
        }],
    )
    .replay(&[ApiRequest {
        principal: Principal {
            id: "runner-a".to_owned(),
        },
        origin: "https://console.example".to_owned(),
        host: "attacker.example".to_owned(),
        idempotency_key: "authority-bypass".to_owned(),
        request_digest: "x".to_owned(),
    }]);
    assert_eq!(wrong_authority.0, [ApiResponse::Unauthorized]);
    assert_eq!(wrong_authority.1, 0);

    let public = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
    let private = IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254));
    let redirected = replay_redirects(
        44,
        &[
            RedirectHop {
                url: "https://allowed.example/start".to_owned(),
                resolved: public,
                connected: public,
            },
            RedirectHop {
                url: "http://169.254.169.254/latest/meta-data".to_owned(),
                resolved: private,
                connected: private,
            },
        ],
    );
    assert_eq!(redirected.allowed_hops, 1);
    assert!(redirected.denied_url.is_some());

    for url in [
        "file:///etc/passwd",
        "https://127.0.0.1@allowed.example/",
        "https://allowed.example@127.0.0.1/",
        "http://2130706433/",
        "http://0x7f000001/",
        "http://0177.0.0.1/",
        "http://[::1]/",
    ] {
        let result = replay_redirects(
            44,
            &[RedirectHop {
                url: url.to_owned(),
                resolved: public,
                connected: public,
            }],
        );
        assert_eq!(result.allowed_hops, 0, "URL bypass accepted: {url}");
    }

    let rebound = replay_redirects(
        44,
        &[RedirectHop {
            url: "https://allowed.example/resource".to_owned(),
            resolved: public,
            connected: private,
        }],
    );
    assert_eq!(rebound.allowed_hops, 0);

    let private_resolution = replay_redirects(
        44,
        &[RedirectHop {
            url: "https://allowed.example/resource".to_owned(),
            resolved: private,
            connected: private,
        }],
    );
    assert_eq!(private_resolution.allowed_hops, 0);
}

#[test]
fn manual_clock_expires_and_fences_stale_leases() {
    let mut leases = clock::LeaseController::new(55);
    let stale = leases.issue("node-a", 10);
    assert!(leases.can_commit(&stale));
    leases.clock.advance_ms(11);
    assert!(!leases.can_commit(&stale));
    let current = leases.issue("node-b", 10);
    assert!(current.fence > stale.fence);
    assert!(!leases.can_commit(&stale));
    assert!(leases.can_commit(&current));
    assert!(leases.renew(&current, 20).is_some());

    let mut replay = clock::LeaseController::new(55);
    assert_eq!(replay.issue("node-a", 10), stale);
}

#[test]
fn named_crashpoint_fires_only_on_scheduled_occurrence() {
    use crashpoints::{
        ACP_CHILD_MID_TOOL_CALL, AFTER_WAL_COMMIT, BEFORE_PROJECTION_UPDATE, CrashAction,
        CrashSchedule, CrashTrigger, ISOLATION_BACKEND_UNAVAILABLE,
    };

    let trigger = CrashTrigger {
        name: AFTER_WAL_COMMIT.to_owned(),
        occurrence: 2,
        action: CrashAction::Terminate,
    };
    let mut first = CrashSchedule::new(66, vec![trigger.clone()]);
    assert_eq!(first.hit(AFTER_WAL_COMMIT), None);
    let injected = first.hit(AFTER_WAL_COMMIT).unwrap();
    let mut replay = CrashSchedule::new(66, vec![trigger]);
    replay.hit(AFTER_WAL_COMMIT);
    assert_eq!(replay.hit(AFTER_WAL_COMMIT), Some(injected));

    let named_adverse_cases = [
        (BEFORE_PROJECTION_UPDATE, CrashAction::Terminate),
        (
            ISOLATION_BACKEND_UNAVAILABLE,
            CrashAction::ReturnUnavailable,
        ),
        (ACP_CHILD_MID_TOOL_CALL, CrashAction::Disconnect),
    ];
    assert_eq!(named_adverse_cases.len(), 3);
}

#[test]
fn store_fault_schedule_exposes_orphan_and_deterministic_corruption() {
    use storefault::{
        AppendOutcome, ScheduledFault, StoreFault, StoreFaultSchedule, StoreHarness, StorePoint,
    };

    let fault = ScheduledFault {
        point: StorePoint::AfterWalCommit,
        occurrence: 1,
        fault: StoreFault::Crash,
    };
    let mut store = StoreHarness::new(77, vec![fault]);
    assert_eq!(
        store.append("event-1"),
        AppendOutcome::Faulted(StoreFault::Crash)
    );
    assert_eq!(store.wal, ["event-1"]);
    assert!(store.projection.is_empty());
    store.recover_projection();
    assert_eq!(store.projection, store.wal);

    let schedule = StoreFaultSchedule::new(77, vec![]);
    let first = schedule.apply_to_bytes(&StoreFault::CorruptBytes, b"backup");
    let second = schedule.apply_to_bytes(&StoreFault::CorruptBytes, b"backup");
    assert_eq!(first, second);
    assert_ne!(first.unwrap(), b"backup");
    assert_eq!(
        schedule.apply_to_bytes(&StoreFault::WithholdBytes, b"artifact"),
        None
    );

    let mut remaining_faults = StoreFaultSchedule::new(
        77,
        vec![
            ScheduledFault {
                point: StorePoint::AfterUploadConfirm,
                occurrence: 1,
                fault: StoreFault::WithholdBytes,
            },
            ScheduledFault {
                point: StorePoint::BeforeHashVerification,
                occurrence: 1,
                fault: StoreFault::CorruptBytes,
            },
            ScheduledFault {
                point: StorePoint::BackupRead,
                occurrence: 1,
                fault: StoreFault::CorruptBytes,
            },
            ScheduledFault {
                point: StorePoint::CommitSerialization,
                occurrence: 1,
                fault: StoreFault::Partition,
            },
        ],
    );
    assert_eq!(
        remaining_faults.at(StorePoint::AfterUploadConfirm),
        Some(StoreFault::WithholdBytes)
    );
    assert_eq!(
        remaining_faults.at(StorePoint::BeforeHashVerification),
        Some(StoreFault::CorruptBytes)
    );
    assert_eq!(
        remaining_faults.at(StorePoint::BackupRead),
        Some(StoreFault::CorruptBytes)
    );
    assert_eq!(
        remaining_faults.at(StorePoint::CommitSerialization),
        Some(StoreFault::Partition)
    );
}

#[test]
fn sandbox_probes_report_observed_or_unavailable_capabilities() {
    use sandbox_probe::{
        Capability, CapabilityProbe, ProbeStatus, RebindOutcome, ScriptedProbe, SystemProbe,
        probe_dns_rebinding,
    };
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;

    let docker = Capability::DockerSocket(PathBuf::from("/fixture/docker.sock"));
    let ssh = Capability::SshAgent;
    let host_daemon = Capability::HostDaemonSocket(PathBuf::from("/fixture/kit.sock"));
    let metadata = Capability::CloudMetadata;
    let scripted = ScriptedProbe::new([
        (
            docker.clone(),
            ProbeStatus::Unreachable("denied".to_owned()),
        ),
        (
            ssh.clone(),
            ProbeStatus::Unavailable("not mounted".to_owned()),
        ),
        (
            host_daemon.clone(),
            ProbeStatus::Unreachable("denied".to_owned()),
        ),
        (
            metadata.clone(),
            ProbeStatus::Unreachable("network denied".to_owned()),
        ),
    ]);
    let probe = CapabilityProbe::new(88, vec![docker, ssh, host_daemon, metadata]);
    assert_eq!(probe.run(&scripted), probe.run(&scripted));
    assert!(
        probe
            .run(&scripted)
            .iter()
            .all(|result| !matches!(result.status, ProbeStatus::Reachable))
    );

    let absent = Capability::IsolationBackend(PathBuf::from("kit-fixture\0unavailable"));
    let system = CapabilityProbe::new(88, vec![absent]).run(&SystemProbe);
    assert!(matches!(system[0].status, ProbeStatus::Unavailable(_)));

    let public = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8));
    let metadata = IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254));
    assert_eq!(
        probe_dns_rebinding(88, Some(public), Some(metadata), &[public]).1,
        RebindOutcome::Blocked
    );
    assert_eq!(
        probe_dns_rebinding(88, None, Some(metadata), &[public]).1,
        RebindOutcome::Unavailable
    );
}
