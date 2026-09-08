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

#[cfg(unix)]
#[test]
fn stalled_writer_loss_resets_after_drain_and_never_resumes() {
    use std::os::unix::net::UnixStream;
    let (mut writer, reader) = UnixStream::pair().unwrap();
    // Fill a real pipe before handing it to the production writer. Nonblocking
    // is confined to this owned test socket, never the process stderr flags.
    writer.set_nonblocking(true).unwrap();
    let mut filled = 0;
    loop {
        match writer.write(&[b'x'; 4096]) {
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("fill: {e}"),
        }
    }
    writer.set_nonblocking(false).unwrap();
    let transport = Transport::start(writer, 2, true).unwrap();
    for _ in 0..4 {
        transport.publish_event(&RuntimeEvent::ChildFinished {
            call: "parent:compose:0".into(),
            tool: "shell".into(),
            ok: true,
            summary: "done".into(),
            millis: 1,
        });
        transport.publish(progress());
    }
    assert!(transport.disabled.load(Ordering::Acquire));
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(reader);
    let mut initial = vec![0; filled];
    reader.read_exact(&mut initial).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(
        crate::events::parse(line.trim_end()),
        Some(RuntimeEvent::RunletTransport { available: true })
    );
    line.clear();
    reader.read_line(&mut line).unwrap();
    assert_eq!(
        crate::events::parse(line.trim_end()),
        Some(RuntimeEvent::RunletTransport { available: false })
    );
    transport.publish(progress());
    drop(transport);
    line.clear();
    assert_eq!(reader.read_line(&mut line).unwrap(), 0);
}

#[test]
fn writer_error_and_unwind_fail_closed() {
    struct Broken(bool);
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            if self.0 {
                panic!("external sink panic");
            }
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "disconnected"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    for panic in [false, true] {
        let disabled = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(2);
        let copy = disabled.clone();
        let worker = std::thread::spawn(move || {
            let _guard = WorkerGuard(copy);
            write_loop(
                Broken(panic),
                receiver,
                &_guard.0,
                &AtomicBool::new(false),
                true,
            )
        });
        let outcome = worker.join();
        assert!(if panic {
            outcome.is_err()
        } else {
            outcome.unwrap().is_err()
        });
        let transport = Transport {
            sender,
            disabled,
            diagnostics_lost: Arc::new(AtomicBool::new(false)),
        };
        assert!(transport.disabled.load(Ordering::Acquire));
        transport.publish(progress());
    }
}

#[cfg(unix)]
#[test]
fn last_sender_disconnect_finishes_transport() {
    let (writer, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let transport = Transport::start(writer, 2, true).unwrap();
    drop(transport);
    let mut text = String::new();
    reader.read_to_string(&mut text).unwrap();
    assert!(text.lines().any(|line| crate::events::parse(line)
        == Some(RuntimeEvent::RunletTransport { available: false })));
}

#[cfg(unix)]
#[test]
fn lifecycle_frames_share_queue_and_oversize_loss_invalidates_progress() {
    let event = RuntimeEvent::ChildFinished {
        call: "owner:compose:0".into(),
        tool: "shell".into(),
        ok: true,
        summary: "done".into(),
        millis: 1,
    };
    let (writer, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let transport = Transport::start(writer, 2, true).unwrap();
    transport.publish_event(&event);
    drop(transport);
    let mut wire = String::new();
    reader.read_to_string(&mut wire).unwrap();
    assert!(
        wire.lines()
            .any(|line| crate::events::parse(line) == Some(event.clone()))
    );

    let (writer, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let transport = Transport::start(writer, 2, true).unwrap();
    transport.publish_event(&RuntimeEvent::SessionStarted {
        session_id: "x".repeat(MAX_FRAME_BYTES),
    });
    assert!(transport.disabled.load(Ordering::Acquire));
    transport.publish(progress());
    transport.publish_line("later child error");
    drop(transport);
    wire.clear();
    reader.read_to_string(&mut wire).unwrap();
    assert!(wire.contains("later child error\n"));
    let events: Vec<_> = wire.lines().filter_map(crate::events::parse).collect();
    assert_eq!(
        events.last(),
        Some(&RuntimeEvent::RunletTransport { available: false })
    );
    assert!(
        events
            .iter()
            .all(|event| matches!(event, RuntimeEvent::RunletTransport { .. }))
    );
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
        assert!(!transport.disabled.load(Ordering::Acquire));
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
        sender,
        disabled: Arc::new(AtomicBool::new(false)),
        diagnostics_lost: Arc::new(AtomicBool::new(false)),
    };
    transport.publish_line("first");
    transport.publish_line("dropped");
    assert!(!transport.disabled.load(Ordering::Acquire));
    assert!(transport.diagnostics_lost.load(Ordering::Acquire));
    assert!(matches!(receiver.recv().unwrap(), Frame::Diagnostic(_)));
    transport.publish_line("later child error");
    let disabled = transport.disabled.clone();
    let diagnostics_lost = transport.diagnostics_lost.clone();
    drop(transport);
    let mut wire = Vec::new();
    write_loop(&mut wire, receiver, &disabled, &diagnostics_lost, false).unwrap();
    let wire = String::from_utf8(wire).unwrap();
    assert!(wire.contains("some child diagnostics were dropped\n"));
    assert!(wire.contains("later child error\n"));
    assert!(!wire.contains(EVENT_MARKER));
}
