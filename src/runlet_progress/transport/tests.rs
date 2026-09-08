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
    let transport = Transport::start(writer, 2).unwrap();
    for _ in 0..4 {
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
            write_loop(Broken(panic), receiver, &_guard.0)
        });
        let outcome = worker.join();
        assert!(if panic {
            outcome.is_err()
        } else {
            outcome.unwrap().is_err()
        });
        let transport = Transport { sender, disabled };
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
    let transport = Transport::start(writer, 2).unwrap();
    drop(transport);
    let mut text = String::new();
    reader.read_to_string(&mut text).unwrap();
    assert!(text.lines().any(|line| crate::events::parse(line)
        == Some(RuntimeEvent::RunletTransport { available: false })));
}
