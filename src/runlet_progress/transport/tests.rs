#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use super::*;
use crate::runlet_progress::Change;
use std::io::{BufRead, BufReader, Read};

fn progress() -> Progress {
    Progress {
        owner: "parent".into(),
        incarnation: 1,
        sequence: 0,
        change: Change::Started {
            digest: "a".repeat(64),
            healed: false,
        },
    }
}

// Commands act at the actual external Write boundary. No production state or
// callbacks are replaced; a partial success makes write_all retry its suffix.
enum Action {
    Accept,
    Partial,
    Error,
    Panic,
}
struct GatedWriter {
    writes: mpsc::Sender<Vec<u8>>,
    actions: Receiver<Action>,
}
impl Write for GatedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes.send(bytes.to_vec()).map_err(io::Error::other)?;
        match self.actions.recv().map_err(io::Error::other)? {
            Action::Accept => Ok(bytes.len()),
            Action::Partial => Ok(bytes.len() / 2),
            Action::Error => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "external failure",
            )),
            Action::Panic => panic!("external sink panic"),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Gate {
    writes: Receiver<Vec<u8>>,
    actions: mpsc::Sender<Action>,
}
impl Gate {
    fn start(capacity: usize) -> (Transport, Self) {
        let (writes, received) = mpsc::channel();
        let (actions, commands) = mpsc::channel();
        let transport = Transport::start(
            GatedWriter {
                writes,
                actions: commands,
            },
            capacity,
            true,
        )
        .unwrap();
        (
            transport,
            Self {
                writes: received,
                actions,
            },
        )
    }
    fn bytes(&self) -> Vec<u8> {
        self.writes.recv_timeout(Duration::from_secs(5)).unwrap()
    }
    fn event(&self) -> RuntimeEvent {
        crate::events::parse(String::from_utf8(self.bytes()).unwrap().trim_end()).unwrap()
    }
    fn accept(&self) {
        self.actions.send(Action::Accept).unwrap();
    }
    fn boundary(&self, transport: &Transport, epoch: u64, state: BoundaryState) {
        assert_eq!(self.event(), boundary(&transport.source, epoch, state));
    }
    // A heartbeat is emitted only after the Open write committed admission.
    fn reopen(&self, transport: &Transport, epoch: u64) {
        self.boundary(transport, epoch - 2, BoundaryState::Lost);
        assert_eq!(transport.generation(), None);
        self.accept();
        self.boundary(transport, epoch, BoundaryState::Open);
        assert_eq!(transport.generation(), None);
        transport.publish(progress()); // Rejected while Open is blocked.
        self.accept();
        self.boundary(transport, epoch, BoundaryState::Heartbeat);
        assert_eq!(transport.generation(), Some(epoch));
    }
    fn finish(&self, transport: Transport, epoch: u64) {
        let source = transport.source.clone();
        let terminal = transport.terminal.clone();
        drop(transport);
        self.accept();
        assert_eq!(self.event(), boundary(&source, epoch, BoundaryState::Lost));
        self.accept();
        assert!(matches!(
            self.writes.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        wait_terminal(&terminal);
    }
}
fn boundary(source: &str, epoch: u64, state: BoundaryState) -> RuntimeEvent {
    RuntimeEvent::RuntimeBoundary {
        source: source.into(),
        epoch,
        state,
    }
}
fn scoped(transport: &Transport, epoch: u64, payload: RuntimeEvent) -> RuntimeEvent {
    RuntimeEvent::RuntimeScoped {
        source: transport.source.clone(),
        epoch,
        payload: Box::new(payload),
    }
}
fn wait_terminal(terminal: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !terminal.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "worker did not terminate");
        std::thread::yield_now();
    }
}

#[test]
fn stalled_writer_loss_discards_queued_frames_and_resumes_new_events() {
    let (transport, gate) = Gate::start(2);
    gate.boundary(&transport, 0, BoundaryState::Open);
    transport.publish(progress());
    transport.publish_event(&RuntimeEvent::SessionStarted {
        session_id: "queued".into(),
    });
    transport.publish(progress()); // Real bounded queue overflow.
    assert_eq!(transport.generation(), None);
    gate.accept();
    gate.reopen(&transport, 2);
    let event = RuntimeEvent::SessionStarted {
        session_id: "fresh".into(),
    };
    transport.publish_event(&event);
    gate.accept();
    assert_eq!(gate.event(), scoped(&transport, 2, event));
    gate.finish(transport, 2);
}

#[test]
fn retained_old_token_enqueued_after_reopen_is_discarded_and_failure_is_isolated() {
    let (transport, gate) = Gate::start(2);
    gate.boundary(&transport, 0, BoundaryState::Open);
    let old = transport.generation().unwrap();
    transport.lose(old);
    gate.accept();
    gate.reopen(&transport, 2);
    let old_bytes = encode_scoped(
        &transport.source,
        old,
        &RuntimeEvent::RunletProgress {
            progress: progress(),
        },
    )
    .unwrap();
    // Queue is empty and writer is blocked in Heartbeat: this enqueue succeeds.
    transport.enqueue_authoritative(old, old_bytes.clone());
    assert_eq!(transport.generation(), Some(2));
    let fresh = RuntimeEvent::SessionStarted {
        session_id: "fresh".into(),
    };
    transport.publish_event(&fresh);
    // Queue is now full: a delayed old-token enqueue fails, then a delayed
    // encoding failure loses the same old token. Neither may close epoch two.
    transport.enqueue_authoritative(old, old_bytes);
    transport.lose(old);
    assert_eq!(transport.generation(), Some(2));
    gate.accept();
    assert_eq!(gate.event(), scoped(&transport, 2, fresh));
    gate.finish(transport, 2);
}

#[test]
fn loss_during_inflight_authoritative_write_precedes_lost_and_open() {
    let (transport, gate) = Gate::start(4);
    gate.boundary(&transport, 0, BoundaryState::Open);
    let event = RuntimeEvent::RunletProgress {
        progress: progress(),
    };
    transport.publish_event(&event);
    gate.accept();
    assert_eq!(gate.event(), scoped(&transport, 0, event));
    transport.publish(progress()); // Backlog is invalidated, not in-flight IO.
    transport.lose(0);
    gate.accept();
    gate.reopen(&transport, 2);
    gate.finish(transport, 2);
}

#[test]
fn invalid_events_invalidate_all_queued_authoritative_frames() {
    for invalid in [
        RuntimeEvent::SessionStarted {
            session_id: "x".repeat(MAX_FRAME_BYTES),
        },
        RuntimeEvent::RunletProgress {
            progress: Progress {
                owner: "x".repeat(257),
                ..progress()
            },
        },
    ] {
        let (transport, gate) = Gate::start(4);
        gate.boundary(&transport, 0, BoundaryState::Open);
        transport.publish(progress());
        transport.publish_event(&invalid);
        assert_eq!(transport.generation(), None);
        gate.accept();
        gate.reopen(&transport, 2);
        gate.finish(transport, 2);
    }
}

#[test]
fn repeated_concurrent_loss_during_blocked_writes_cannot_revive_old_frames() {
    let (transport, gate) = Gate::start(16);
    gate.boundary(&transport, 0, BoundaryState::Open);
    for epoch in [2, 4, 6] {
        transport.publish(progress());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let transport = &transport;
                scope.spawn(move || transport.lose(epoch - 2));
            }
        });
        gate.accept();
        gate.reopen(&transport, epoch);
    }
    transport.publish(progress());
    gate.accept();
    assert_eq!(
        gate.event(),
        scoped(
            &transport,
            6,
            RuntimeEvent::RunletProgress {
                progress: progress()
            }
        )
    );
    gate.finish(transport, 6);
}

#[test]
fn worker_errors_and_panics_fail_closed_at_initial_lost_open_and_recovered_payload() {
    for stage in 0..4 {
        for partial in [false, true] {
            for panic in [false, true] {
                let (transport, gate) = Gate::start(4);
                gate.boundary(&transport, 0, BoundaryState::Open);
                if stage > 0 {
                    transport.lose(0);
                    gate.accept();
                    gate.boundary(&transport, 0, BoundaryState::Lost);
                }
                if stage > 1 {
                    gate.accept();
                    gate.boundary(&transport, 2, BoundaryState::Open);
                }
                if stage > 2 {
                    gate.accept();
                    gate.boundary(&transport, 2, BoundaryState::Heartbeat);
                    transport.publish(progress());
                    gate.accept();
                    assert_eq!(
                        gate.event(),
                        scoped(
                            &transport,
                            2,
                            RuntimeEvent::RunletProgress {
                                progress: progress()
                            }
                        )
                    );
                }
                if partial {
                    gate.actions.send(Action::Partial).unwrap();
                    let suffix = gate.bytes();
                    assert!(!suffix.starts_with(EVENT_MARKER.as_bytes()));
                    assert!(suffix.ends_with(b"\n"));
                }
                gate.actions
                    .send(if panic { Action::Panic } else { Action::Error })
                    .unwrap();
                assert!(matches!(
                    gate.writes.recv_timeout(Duration::from_secs(5)),
                    Err(mpsc::RecvTimeoutError::Disconnected)
                ));
                wait_terminal(&transport.terminal);
                assert_eq!(transport.generation(), None);
                if stage == 1 || stage == 2 {
                    assert_eq!(transport.admission.load(Ordering::Acquire), 1);
                }
                transport.publish(progress());
                assert_eq!(transport.generation(), None);
            }
        }
    }
}

#[test]
fn disconnect_while_open_boundary_is_blocked() {
    for recovering in [false, true] {
        let (transport, gate) = Gate::start(2);
        gate.boundary(&transport, 0, BoundaryState::Open);
        let epoch = if recovering {
            transport.lose(0);
            gate.accept();
            gate.boundary(&transport, 0, BoundaryState::Lost);
            gate.accept();
            gate.boundary(&transport, 2, BoundaryState::Open);
            2
        } else {
            0
        };
        gate.finish(transport, epoch);
    }
}
#[cfg(unix)]
#[test]
fn plain_diagnostics_do_not_enable_runtime_frames() {
    let (writer, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let transport = Transport::start(writer, 2, false).unwrap();
    transport.publish_line("child diagnostic");
    drop(transport);
    let mut wire = String::new();
    reader.read_to_string(&mut wire).unwrap();
    assert_eq!(wire, "child diagnostic\n");
}

#[cfg(unix)]
#[test]
fn oversized_diagnostics_truncate_without_disabling_later_errors() {
    for runtime_events in [false, true] {
        let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let transport = Transport::start(writer, 4, runtime_events).unwrap();
        transport.publish_line(&"é".repeat(MAX_FRAME_BYTES));
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if crate::events::parse(line.trim_end()).is_none() {
                break;
            }
        }
        assert!(line.ends_with(" [truncated]\n"));
        assert!(line.len() <= MAX_FRAME_BYTES);
        assert!(!transport.terminal.load(Ordering::Acquire));
        transport.publish_line("later child error");
        drop(transport);
        let mut rest = String::new();
        reader.read_to_string(&mut rest).unwrap();
        assert!(rest.contains("later child error\n"));
    }
}

#[test]
fn diagnostic_queue_overflow_does_not_poison_recovered_publication() {
    // Exercise actual bounded admission at a full queue, then release capacity.
    // No sink timing or exact implementation work counts are involved.
    let (sender, receiver) = mpsc::sync_channel(1);
    let transport = Transport {
        source: String::from("test-source"),
        gate: Arc::new(PublicationGate::default()),
        sender,
        terminal: Arc::new(AtomicBool::new(false)),
        admission: Arc::new(AtomicU64::new(0)),
        diagnostics_lost: Arc::new(AtomicBool::new(false)),
    };
    transport.publish_line("first");
    transport.publish_line("dropped");
    assert!(!transport.terminal.load(Ordering::Acquire));
    assert!(transport.diagnostics_lost.load(Ordering::Acquire));
    assert!(matches!(receiver.recv().unwrap(), Frame::Diagnostic(_)));
    transport.publish_line("later child error");
    let admission = transport.admission.clone();
    let diagnostics_lost = transport.diagnostics_lost.clone();
    drop(transport);
    let mut wire = Vec::new();
    write_loop(
        &mut wire,
        receiver,
        &diagnostics_lost,
        &admission,
        false,
        "test-source",
        &PublicationGate::default(),
    )
    .unwrap();
    let wire = String::from_utf8(wire).unwrap();
    assert!(wire.contains("some child diagnostics were dropped\n"));
    assert!(wire.contains("later child error\n"));
    assert!(!wire.contains(EVENT_MARKER));
}

#[test]
fn healthy_periodic_rotation_drains_admitted_payloads_before_open_without_loss() {
    for queued_payloads in [false, true] {
        let (transport, gate) = Gate::start(4);
        gate.boundary(&transport, 0, BoundaryState::Open);
        gate.accept();
        gate.boundary(&transport, 0, BoundaryState::Heartbeat);
        let events = [
            RuntimeEvent::SessionStarted {
                session_id: "admitted-before-rotation".into(),
            },
            RuntimeEvent::RunletProgress {
                progress: progress(),
            },
        ];
        if queued_payloads {
            for event in &events {
                transport.publish_event(event);
            }
        }
        assert_eq!(transport.generation(), Some(0));
        // Expire the real rotation clock while IO is stalled, without filling
        // the queue. This triggers a state transition, not a speed assertion.
        std::thread::sleep(ROTATE);
        gate.accept();
        if queued_payloads {
            for event in events {
                assert_eq!(gate.event(), scoped(&transport, 0, event));
                gate.accept();
            }
        }
        // Exact wire sequence excludes Lost and proves all admitted old payloads
        // precede the new Open. Empty stalled streams must rotate as well.
        gate.boundary(&transport, 2, BoundaryState::Open);
        assert_eq!(transport.generation(), None);
        gate.accept();
        gate.boundary(&transport, 2, BoundaryState::Heartbeat);
        assert_eq!(transport.generation(), Some(2));
        transport.publish(progress());
        gate.accept();
        assert_eq!(
            gate.event(),
            scoped(
                &transport,
                2,
                RuntimeEvent::RunletProgress {
                    progress: progress()
                }
            )
        );
        gate.finish(transport, 2);
    }
}

#[test]
fn rejected_producer_during_periodic_open_does_not_wait_or_lose_loss_at_commit() {
    let (transport, gate) = Gate::start(4);
    gate.boundary(&transport, 0, BoundaryState::Open);
    gate.accept();
    gate.boundary(&transport, 0, BoundaryState::Heartbeat);
    std::thread::sleep(ROTATE);
    gate.accept();
    gate.boundary(&transport, 2, BoundaryState::Open);
    assert_eq!(transport.generation(), None);
    // The real worker holds its exclusive claim across this blocked Write.
    // Completion before releasing IO proves publication doesn't await the sink
    // or lock holder (the same API used by producer cancellation/Drop).
    let producer = transport.clone();
    let (finished, completion) = mpsc::channel();
    let publisher = std::thread::spawn(move || {
        producer.publish(progress());
        drop(producer);
        finished.send(()).unwrap();
    });
    completion.recv_timeout(Duration::from_secs(5)).unwrap();
    publisher.join().unwrap();
    assert!(transport.gate.lost.load(Ordering::Acquire));
    gate.accept();
    // The worker can emit a heartbeat before processing the separately recorded
    // failed claim. It must then explicitly retire the just-opened epoch.
    gate.boundary(&transport, 2, BoundaryState::Heartbeat);
    gate.accept();
    gate.reopen(&transport, 4);
    transport.publish(progress());
    gate.accept();
    assert_eq!(
        gate.event(),
        scoped(
            &transport,
            4,
            RuntimeEvent::RunletProgress {
                progress: progress()
            }
        )
    );
    gate.finish(transport, 4);
}

#[test]
fn publication_reserves_its_scope_within_the_wire_wrapper_limit() {
    for wrappers in [15, 16] {
        let (transport, gate) = Gate::start(4);
        gate.boundary(&transport, 0, BoundaryState::Open);
        let mut event = RuntimeEvent::RunletProgress {
            progress: progress(),
        };
        for epoch in 0..wrappers {
            event = RuntimeEvent::RuntimeScoped {
                source: "upstream".into(),
                epoch,
                payload: Box::new(event),
            };
        }
        // Both inputs are independently accepted wire values. Publication must
        // reserve one additional wrapper instead of emitting unparseable depth 17.
        let input = String::from_utf8(encode_frame(&event).unwrap()).unwrap();
        assert_eq!(crate::events::parse(input.trim_end()), Some(event.clone()));
        transport.publish_event(&event);
        if wrappers == 15 {
            assert_eq!(transport.generation(), Some(0));
            gate.accept();
            assert_eq!(gate.event(), scoped(&transport, 0, event));
            gate.finish(transport, 0);
        } else {
            assert_eq!(transport.generation(), None);
            gate.accept();
            gate.reopen(&transport, 2);
            gate.finish(transport, 2);
        }
    }
}

#[test]
fn healthy_periodic_open_failure_retires_worker_without_poison_recovery() {
    for panic in [false, true] {
        let (transport, gate) = Gate::start(4);
        gate.boundary(&transport, 0, BoundaryState::Open);
        gate.accept();
        gate.boundary(&transport, 0, BoundaryState::Heartbeat);
        std::thread::sleep(ROTATE);
        gate.accept();
        gate.boundary(&transport, 2, BoundaryState::Open);
        assert_eq!(transport.generation(), None);
        // Unlike loss recovery, healthy rotation owns the exclusive publication
        // claim through this real Write. Exercise a partial write before failure.
        gate.actions.send(Action::Partial).unwrap();
        let suffix = gate.bytes();
        assert!(!suffix.starts_with(EVENT_MARKER.as_bytes()));
        gate.actions
            .send(if panic { Action::Panic } else { Action::Error })
            .unwrap();
        assert!(matches!(
            gate.writes.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        wait_terminal(&transport.terminal);
        assert_eq!(transport.gate.active.is_poisoned(), panic);
        assert_eq!(transport.admission.load(Ordering::Acquire), 1);
        transport.publish(progress());
        assert_eq!(transport.generation(), None);
        assert_eq!(transport.gate.active.is_poisoned(), panic);
        assert_eq!(transport.admission.load(Ordering::Acquire), 1);
    }
}

#[test]
fn forwarded_frames_keep_provenance_and_malformed_frames_close_admission() {
    let (transport, gate) = Gate::start(4);
    gate.boundary(&transport, 0, BoundaryState::Open);
    let child = RuntimeEvent::RuntimeScoped {
        source: "child".into(),
        epoch: 12,
        payload: Box::new(RuntimeEvent::StorageStatus {
            pending: true,
            exhausted: false,
        }),
    };
    let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&child).unwrap());
    transport.publish_runtime_line(&line);
    gate.accept();
    assert_eq!(gate.event(), scoped(&transport, 0, child));
    transport.publish_runtime_line(&format!("{EVENT_MARKER}not-json"));
    assert_eq!(transport.generation(), None);
    gate.accept();
    gate.reopen(&transport, 2);
    gate.finish(transport, 2);
}
