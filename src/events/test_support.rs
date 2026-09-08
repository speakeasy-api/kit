//! Isolate the process-wide stderr lock, event opt-in, and singleton transport.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

/// Parent runs the exact test with an unread stderr pipe and a deadlock watchdog.
/// Child holds Rust's actual stderr lock on a detached writer filling that pipe.
/// Returning true means the caller must exercise its real execution/cleanup API.
pub(crate) fn with_stalled_stderr(test: &str) -> bool {
    const CHILD: &str = "KIT_STDERR_CONTENTION_TEST";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        let (ready, locked) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stderr = std::io::stderr().lock();
            ready.send(()).unwrap();
            loop {
                stderr.write_all(&[b'x'; 65536]).unwrap();
            }
        });
        locked.recv().unwrap();
        assert!(super::enabled());
        // Start the actual process singleton behind the contended stderr lock.
        super::emit(&super::RuntimeEvent::SessionStarted {
            session_id: "test".into(),
        });
        return true;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .env(super::EVENTS_ENV, "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "contended subprocess failed: {status}");
            return false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("execution/cleanup blocked behind process stderr lock");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

use super::{EVENT_MARKER, RuntimeEvent};

pub(crate) fn write_event(writer: &mut impl std::io::Write, event: &RuntimeEvent) {
    if let RuntimeEvent::RunletProgress { progress } = event
        && !progress.bounded()
    {
        return;
    }
    if let Ok(line) = serde_json::to_string(event) {
        let _ = writeln!(writer, "{EVENT_MARKER}{line}");
    }
}
