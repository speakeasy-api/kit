//! Receiver-owned authority. Transitions are synchronous and require no locks.
//! Source tombstones are never evicted: after MAX_SOURCES identities, unseen
//! publishers fail closed until the containing receiver/stream is replaced.
//! This deliberately bounds memory at the expense of long-lived stream liveness.
use std::{collections::BTreeMap, time::Instant};

use super::transport::LEASE;
use crate::events::{BoundaryState, RuntimeEvent};

const MAX_SOURCES: usize = 4096;

#[derive(Default)]
pub(crate) struct Authority {
    sources: BTreeMap<String, Scope>,
    legacy_deadline: Option<Instant>,
    legacy_lost: bool,
    scoped_root: bool,
}
struct Scope {
    epoch: u64,
    live: bool,
    legacy_tainted: bool,
    deadline: Instant,
    parents: Vec<(String, u64)>,
}
#[derive(Default)]
pub(crate) struct Admission {
    pub accepted: bool,
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub invalidated: bool,
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub opened: bool,
}
impl Authority {
    pub fn deadline(&self) -> Option<Instant> {
        self.sources
            .values()
            .filter(|s| s.live)
            .map(|s| s.deadline)
            .chain(self.legacy_deadline)
            .min()
    }
    fn invalidate(&mut self, source: &str, epoch: u64) -> bool {
        let mut changed = false;
        for (id, scope) in &mut self.sources {
            if (id == source && scope.epoch <= epoch)
                || scope
                    .parents
                    .iter()
                    .any(|(id, generation)| id == source && *generation <= epoch)
            {
                changed |= scope.live;
                scope.live = false;
            }
        }
        changed
    }
    /// Lost outputs retain the exact dependency path of the expired authority.
    pub fn expire(&mut self, now: Instant) -> Vec<RuntimeEvent> {
        let expired: Vec<_> = self
            .sources
            .iter()
            .filter(|(_, s)| s.live && now >= s.deadline)
            .map(|(id, s)| (id.clone(), s.epoch, s.parents.clone()))
            .collect();
        let mut outputs = Vec::new();
        for (source, epoch, parents) in expired {
            self.invalidate(&source, epoch);
            let mut event = RuntimeEvent::RuntimeBoundary {
                source,
                epoch,
                state: BoundaryState::Lost,
            };
            for (source, epoch) in parents.into_iter().rev() {
                event = RuntimeEvent::RuntimeScoped {
                    source,
                    epoch,
                    payload: Box::new(event),
                };
            }
            outputs.push(event);
        }
        if self.legacy_deadline.is_some_and(|deadline| now >= deadline) {
            self.legacy_deadline = None;
            self.legacy_lost = true;
            outputs.push(RuntimeEvent::RunletTransport { available: false });
        }
        outputs
    }
    pub fn observe(&mut self, event: &RuntimeEvent, now: Instant) -> Admission {
        self.visit(event, now, &mut Vec::new())
    }
    fn visit(
        &mut self,
        event: &RuntimeEvent,
        now: Instant,
        parents: &mut Vec<(String, u64)>,
    ) -> Admission {
        match event {
            RuntimeEvent::RuntimeScoped {
                source,
                epoch,
                payload,
            } => {
                let Some(scope) = self.sources.get_mut(source) else {
                    return Admission::default();
                };
                if !scope.live || scope.epoch != *epoch || scope.parents != *parents {
                    return Admission::default();
                }
                scope.deadline = now + LEASE;
                parents.push((source.clone(), *epoch));
                self.visit(payload, now, parents)
            }
            RuntimeEvent::RuntimeBoundary {
                source,
                epoch,
                state,
            } => {
                if (self.sources.len() >= MAX_SOURCES && !self.sources.contains_key(source))
                    || self.sources.get(source).is_some_and(|s| s.legacy_tainted)
                    || parents.iter().any(|(id, _)| id == source)
                {
                    return Admission::default();
                }
                match state {
                    BoundaryState::Open => {
                        if self.sources.get(source).is_some_and(|s| *epoch <= s.epoch) {
                            return Admission::default();
                        }
                        let continuous = self
                            .sources
                            .get(source)
                            .is_some_and(|s| s.live && s.parents == *parents);
                        let mut invalidated = if continuous {
                            // Healthy rotation preserves observations and rebases containment.
                            for scope in self.sources.values_mut() {
                                for (id, generation) in &mut scope.parents {
                                    if id == source {
                                        *generation = *epoch;
                                    }
                                }
                            }
                            false
                        } else {
                            self.invalidate(source, *epoch)
                        };
                        if parents.is_empty() {
                            // Once this wire speaks scoped authority, raw legacy
                            // backlog is never again admitted on the same receiver.
                            invalidated |= self.legacy_deadline.take().is_some();
                            self.scoped_root = true;
                        }
                        self.sources.insert(
                            source.clone(),
                            Scope {
                                epoch: *epoch,
                                live: true,
                                legacy_tainted: false,
                                deadline: now + LEASE,
                                parents: parents.clone(),
                            },
                        );
                        Admission {
                            accepted: true,
                            invalidated,
                            opened: true,
                        }
                    }
                    BoundaryState::Heartbeat => {
                        let accepted = self.sources.get_mut(source).is_some_and(|s| {
                            if s.live && s.epoch == *epoch && s.parents == *parents {
                                s.deadline = now + LEASE;
                                true
                            } else {
                                false
                            }
                        });
                        Admission {
                            accepted,
                            ..Admission::default()
                        }
                    }
                    BoundaryState::Lost => {
                        if self
                            .sources
                            .get(source)
                            .is_some_and(|s| *epoch < s.epoch || s.parents != *parents)
                        {
                            return Admission::default();
                        }
                        let invalidated = self.invalidate(source, *epoch);
                        self.sources.insert(
                            source.clone(),
                            Scope {
                                epoch: *epoch,
                                live: false,
                                legacy_tainted: false,
                                deadline: now,
                                parents: parents.clone(),
                            },
                        );
                        Admission {
                            accepted: true,
                            invalidated,
                            opened: false,
                        }
                    }
                }
            }
            RuntimeEvent::RunletTransport { available } if !parents.is_empty() => {
                if *available {
                    // A legacy heartbeat cannot certify or recover any scope.
                    return Admission::default();
                }
                let Some((source, epoch)) = parents.last() else {
                    return Admission::default();
                };
                let invalidated = self.invalidate(source, *epoch);
                // No source provenance exists for this nested legacy loss. Latch
                // the containing publisher, even across its healthy rotations.
                if let Some(scope) = self.sources.get_mut(source) {
                    scope.legacy_tainted = true;
                }
                Admission {
                    accepted: true,
                    invalidated,
                    opened: false,
                }
            }
            RuntimeEvent::RunletTransport { available } => {
                if self.legacy_lost || self.scoped_root {
                    return Admission::default();
                }
                if *available {
                    self.legacy_deadline = Some(now + LEASE);
                } else {
                    self.legacy_lost = true;
                    self.legacy_deadline = None;
                }
                Admission {
                    accepted: true,
                    invalidated: !available,
                    opened: false,
                }
            }
            _ => {
                let accepted = !parents.is_empty() || (!self.legacy_lost && !self.scoped_root);
                if accepted && parents.is_empty() {
                    self.legacy_deadline = Some(now + LEASE);
                }
                Admission {
                    accepted,
                    ..Admission::default()
                }
            }
        }
    }
}

pub(crate) fn payload(event: &RuntimeEvent) -> &RuntimeEvent {
    match event {
        RuntimeEvent::RuntimeScoped { payload: inner, .. } => payload(inner),
        _ => event,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_macros)]
mod tests {
    use super::*;
    fn boundary(source: &str, epoch: u64, state: BoundaryState) -> RuntimeEvent {
        RuntimeEvent::RuntimeBoundary {
            source: source.into(),
            epoch,
            state,
        }
    }
    fn scoped(source: &str, epoch: u64, payload: RuntimeEvent) -> RuntimeEvent {
        RuntimeEvent::RuntimeScoped {
            source: source.into(),
            epoch,
            payload: Box::new(payload),
        }
    }
    fn data() -> RuntimeEvent {
        RuntimeEvent::StorageStatus {
            pending: true,
            exhausted: false,
        }
    }
    #[test]
    fn source_cap_never_evicts_invalidated_generations() {
        let mut tracker = Authority::default();
        let now = Instant::now();
        for index in 0..MAX_SOURCES {
            let source = format!("source-{index}");
            assert!(
                tracker
                    .observe(&boundary(&source, 0, BoundaryState::Open), now)
                    .accepted
            );
            tracker.observe(&boundary(&source, 0, BoundaryState::Lost), now);
        }
        assert!(
            !tracker
                .observe(&boundary("overflow", 0, BoundaryState::Open), now)
                .accepted
        );
        assert!(
            !tracker
                .observe(&boundary("source-0", 0, BoundaryState::Open), now)
                .accepted
        );
        assert!(
            tracker
                .observe(&boundary("source-0", 2, BoundaryState::Open), now)
                .accepted
        );
        assert!(
            tracker
                .observe(&scoped("source-0", 2, data()), now)
                .accepted
        );
    }

    #[test]
    fn scoped_root_rejects_unscoped_backlog_and_legacy_heartbeats() {
        let mut tracker = Authority::default();
        let now = Instant::now();
        assert!(tracker.observe(&data(), now).accepted);
        assert!(
            tracker
                .observe(&boundary("root", 0, BoundaryState::Open), now)
                .invalidated
        );
        assert!(!tracker.observe(&data(), now).accepted);
        assert!(
            !tracker
                .observe(&RuntimeEvent::RunletTransport { available: true }, now)
                .accepted
        );
        assert!(
            !tracker
                .observe(&RuntimeEvent::RunletTransport { available: false }, now)
                .accepted
        );
        assert!(tracker.observe(&scoped("root", 0, data()), now).accepted);
        tracker.observe(&boundary("root", 0, BoundaryState::Lost), now);
        assert!(!tracker.observe(&data(), now).accepted);
        tracker.observe(&boundary("root", 2, BoundaryState::Open), now);
        assert!(!tracker.observe(&data(), now).accepted);
        assert!(tracker.observe(&scoped("root", 2, data()), now).accepted);
    }

    #[test]
    fn nested_legacy_loss_taints_containing_source_across_rotations() {
        let mut tracker = Authority::default();
        let now = Instant::now();
        tracker.observe(&boundary("root", 0, BoundaryState::Open), now);
        tracker.observe(
            &scoped("root", 0, boundary("child", 0, BoundaryState::Open)),
            now,
        );
        let lost = scoped(
            "root",
            0,
            scoped(
                "child",
                0,
                RuntimeEvent::RunletTransport { available: false },
            ),
        );
        assert!(tracker.observe(&lost, now).invalidated);
        assert!(
            tracker
                .observe(&boundary("root", 2, BoundaryState::Open), now)
                .accepted
        );
        assert!(
            !tracker
                .observe(
                    &scoped("root", 2, boundary("child", 2, BoundaryState::Open)),
                    now
                )
                .accepted
        );
        assert!(
            !tracker
                .observe(
                    &scoped(
                        "root",
                        2,
                        scoped(
                            "child",
                            0,
                            RuntimeEvent::RunletTransport { available: true }
                        )
                    ),
                    now
                )
                .accepted
        );
        assert!(
            !tracker
                .observe(&scoped("root", 2, scoped("child", 0, data())), now)
                .accepted
        );
        assert!(tracker.observe(&scoped("root", 2, data()), now).accepted);
    }

    #[test]
    fn rotation_preserves_descendants_but_loss_requires_new_child_open() {
        let mut tracker = Authority::default();
        let now = Instant::now();
        assert!(
            tracker
                .observe(&boundary("parent", 0, BoundaryState::Open), now)
                .accepted
        );
        assert!(
            tracker
                .observe(
                    &scoped("parent", 0, boundary("child", 0, BoundaryState::Open)),
                    now
                )
                .accepted
        );
        let rotation = tracker.observe(&boundary("parent", 2, BoundaryState::Open), now);
        assert!(rotation.accepted && !rotation.invalidated);
        assert!(
            tracker
                .observe(&scoped("parent", 2, scoped("child", 0, data())), now)
                .accepted
        );
        assert!(
            tracker
                .observe(&boundary("parent", 2, BoundaryState::Lost), now)
                .invalidated
        );
        assert!(
            tracker
                .observe(&boundary("parent", 4, BoundaryState::Open), now)
                .accepted
        );
        assert!(
            !tracker
                .observe(&scoped("parent", 4, scoped("child", 0, data())), now)
                .accepted
        );
        assert!(
            !tracker
                .observe(
                    &scoped("parent", 4, boundary("child", 0, BoundaryState::Open)),
                    now
                )
                .accepted
        );
        assert!(
            tracker
                .observe(
                    &scoped("parent", 4, boundary("child", 2, BoundaryState::Open)),
                    now
                )
                .accepted
        );
        assert!(
            tracker
                .observe(&scoped("parent", 4, scoped("child", 2, data())), now)
                .accepted
        );
    }
    #[test]
    fn healthy_parent_cannot_renew_or_recover_expired_child() {
        let mut tracker = Authority::default();
        let now = Instant::now();
        tracker.observe(&boundary("parent", 0, BoundaryState::Open), now);
        tracker.observe(
            &scoped("parent", 0, boundary("child", 0, BoundaryState::Open)),
            now,
        );
        tracker.observe(
            &boundary("parent", 0, BoundaryState::Heartbeat),
            now + LEASE / 2,
        );
        let lost = tracker.expire(now + LEASE);
        assert_eq!(
            lost,
            vec![scoped(
                "parent",
                0,
                boundary("child", 0, BoundaryState::Lost)
            )]
        );
        for state in [BoundaryState::Heartbeat, BoundaryState::Open] {
            assert!(
                !tracker
                    .observe(
                        &scoped("parent", 0, boundary("child", 0, state)),
                        now + LEASE
                    )
                    .accepted
            );
        }
        assert!(
            tracker
                .observe(&boundary("parent", 2, BoundaryState::Open), now + LEASE)
                .accepted
        );
        assert!(
            !tracker
                .observe(
                    &scoped("parent", 2, scoped("child", 0, data())),
                    now + LEASE
                )
                .accepted
        );
        assert!(
            tracker
                .observe(
                    &scoped("parent", 2, boundary("child", 2, BoundaryState::Open)),
                    now + LEASE
                )
                .accepted
        );
    }
    #[test]
    fn legacy_loss_is_latched_but_explicit_scopes_can_recover() {
        let mut tracker = Authority::default();
        let now = Instant::now();
        tracker.observe(&data(), now);
        assert_eq!(tracker.expire(now + LEASE).len(), 1);
        assert!(
            !tracker
                .observe(
                    &RuntimeEvent::RunletTransport { available: true },
                    now + LEASE
                )
                .accepted
        );
        assert!(!tracker.observe(&data(), now + LEASE).accepted);
        assert!(
            tracker
                .observe(&boundary("new", 0, BoundaryState::Open), now + LEASE)
                .accepted
        );
        assert!(
            tracker
                .observe(&scoped("new", 0, data()), now + LEASE)
                .accepted
        );
    }
}
