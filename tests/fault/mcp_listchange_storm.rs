use kit::protocols::mcp::features::{
    ConfiguredServerIdentity, FeatureListKind, NegotiatedFeatureKinds, RefreshCoalescer,
    RefreshLimits,
};

#[test]
fn ten_thousand_notifications_coalesce_to_three_bounded_kinds() {
    let server = ConfiguredServerIdentity::new("storm-server").unwrap();
    let kinds = [
        FeatureListKind::Tools,
        FeatureListKind::Resources,
        FeatureListKind::Prompts,
    ];
    let negotiated = NegotiatedFeatureKinds::with_list_changed(kinds, kinds);
    let mut coalescer = RefreshCoalescer::new(RefreshLimits::default());
    for index in 0..10_000 {
        coalescer.notify(server.clone(), kinds[index % kinds.len()], &negotiated, 0);
    }
    assert_eq!(coalescer.pending_kinds(), 3);
    let active = coalescer.take_ready(50);
    assert_eq!(active.len(), 3);
    assert_eq!(coalescer.active_kinds(), 3);

    for index in 0..10_000 {
        coalescer.notify(server.clone(), kinds[index % kinds.len()], &negotiated, 50);
    }
    assert_eq!(coalescer.pending_kinds(), 3);
    for ticket in &active {
        coalescer.complete(ticket, true, 50);
    }
    assert_eq!(coalescer.active_kinds(), 0);
    assert_eq!(coalescer.pending_kinds(), 3);
    assert!(coalescer.take_ready(99).is_empty());
    assert_eq!(coalescer.take_ready(100).len(), 3);
}

#[test]
fn one_hundred_refresh_races_and_receiver_lag_keep_only_one_follow_up_per_kind() {
    let server = ConfiguredServerIdentity::new("race-server").unwrap();
    let negotiated = NegotiatedFeatureKinds::with_list_changed(
        [
            FeatureListKind::Tools,
            FeatureListKind::Resources,
            FeatureListKind::Prompts,
        ],
        [
            FeatureListKind::Tools,
            FeatureListKind::Resources,
            FeatureListKind::Prompts,
        ],
    );
    let mut coalescer = RefreshCoalescer::new(RefreshLimits::default());
    coalescer.mark_lagged(server.clone(), &negotiated, 0);
    assert_eq!(coalescer.pending_kinds(), 3);
    let initial = coalescer.take_ready(50);
    assert_eq!(initial.len(), 3);

    for race in 0..100 {
        for kind in negotiated.iter() {
            coalescer.notify(server.clone(), kind, &negotiated, 50 + race);
        }
    }
    assert_eq!(coalescer.active_kinds(), 3);
    assert_eq!(coalescer.pending_kinds(), 3);
    for ticket in &initial {
        coalescer.complete(ticket, true, 150);
    }
    let follow_up = coalescer.take_ready(200);
    assert_eq!(follow_up.len(), 3);
    for stale in &initial {
        coalescer.complete(stale, false, 200);
    }
    assert_eq!(coalescer.active_kinds(), 3);
    assert_eq!(coalescer.pending_kinds(), 0);
    for ticket in &follow_up {
        coalescer.complete(ticket, true, 200);
    }
    assert_eq!(coalescer.active_kinds(), 0);
    assert_eq!(coalescer.pending_kinds(), 0);
}

#[test]
fn unsolicited_list_changed_for_unnegotiated_boolean_is_rejected() {
    let server = ConfiguredServerIdentity::new("unsolicited-server").unwrap();
    let negotiated = NegotiatedFeatureKinds::with_list_changed(
        [FeatureListKind::Tools, FeatureListKind::Resources],
        [FeatureListKind::Resources],
    );
    let explicit_false = NegotiatedFeatureKinds::with_list_changed_values(
        [FeatureListKind::Tools],
        [(FeatureListKind::Tools, false)],
    );
    assert_eq!(
        explicit_false.list_changed(FeatureListKind::Tools),
        Some(false)
    );
    let mut coalescer = RefreshCoalescer::new(RefreshLimits::default());
    assert!(!coalescer.notify(server.clone(), FeatureListKind::Tools, &negotiated, 0,));
    assert!(coalescer.notify(server, FeatureListKind::Resources, &negotiated, 0,));
    assert_eq!(coalescer.pending_kinds(), 1);
}

#[test]
fn transient_refresh_failures_count_across_bounded_backoff() {
    let server = ConfiguredServerIdentity::new("retry-server").unwrap();
    let negotiated = NegotiatedFeatureKinds::with_list_changed(
        [FeatureListKind::Tools],
        [FeatureListKind::Tools],
    );
    let limits = RefreshLimits::new(
        std::time::Duration::from_millis(1),
        std::time::Duration::from_millis(1),
        std::time::Duration::from_millis(8),
    )
    .unwrap();
    let mut coalescer = RefreshCoalescer::new(limits);
    coalescer.notify(server, FeatureListKind::Tools, &negotiated, 0);
    let mut now = 1;
    for expected in 1..=5 {
        let ticket = coalescer.take_ready(now).remove(0);
        coalescer.complete(&ticket, false, now);
        assert_eq!(coalescer.failures(&ticket), expected);
        now += 1_u64 << (expected - 1).min(3);
    }
}
