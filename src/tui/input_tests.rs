//! Real PTYs isolate crossterm's process-global decoder. No production hooks.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use super::Events;
use crate::tui::{enter, leave, resume_terminal};
use futures_util::FutureExt;
use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const CHILD_ENV: &str = "KIT_INPUT_LIFECYCLE_CHILD";
const TEST_NAME: &str = "tui::input::tests::terminal_input_lifecycle";

struct TestChild(Child);
impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn marker(value: &str) {
    println!("INPUT_TEST:{value}");
    std::io::stdout().flush().unwrap();
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "input condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn key(events: &mut Events, expected: char) {
    wait_until(|| match events.try_next() {
        Some(Ok(crossterm::event::Event::Key(key))) => {
            assert_eq!(key.code, crossterm::event::KeyCode::Char(expected));
            true
        }
        Some(Err(error)) => panic!("input failed: {error}"),
        _ => false,
    });
}

fn auth_child(cancel: bool) {
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    // Exercise the real authentication wait boundary with inherited PTY stdin.
    // The child signals Stop only after it has received its complete line.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut stop = crate::tui::Stop::new().unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", if cancel {
            "printf 'INPUT_TEST:AUTH\\n'; IFS= read -r line; test \"$line\" = auth || exit 42; kill -TERM \"$PPID\"; exec sleep 30"
        } else {
            "printf 'INPUT_TEST:AUTH\\n'; IFS= read -r line; test \"$line\" = auth || exit 42; exit 17"
        }]);
        let result = crate::tui::wait_for_terminal_auth(command, &mut stop).await;
        if cancel {
            assert!(result.is_none());
        } else {
            assert_eq!(result.unwrap().unwrap().code(), Some(17));
        }
    });
}

fn child_case(case: &str) {
    match case {
        "idle" | "saturated" | "handoff" | "handoff_cancel" | "handoff_unanswered"
        | "handoff_partial" => {
            let (mut terminal, _images) = enter().unwrap();
            let mut events = Events::new().unwrap();
            if case == "saturated" {
                marker("PRESSURE");
                // Observe the real bounded queue reaching capacity, rather than
                // assuming that sleeping or writing N bytes implies saturation.
                wait_until(|| events.receiver.len() == events.receiver.max_capacity());
            } else if case.starts_with("handoff") {
                marker("FIRST");
                key(&mut events, 'x');
            }
            drop(events);
            leave(&mut terminal);
            if case.starts_with("handoff") {
                auth_child(case == "handoff_cancel");
                let _images = resume_terminal(&mut terminal).unwrap();
                let mut events = Events::new().unwrap();
                marker("RESTART");
                key(&mut events, 'z');
                drop(events);
            }
            drop(terminal);
        }
        "cancel" => {
            // Poll through actual terminal setup and input acquisition, suspend
            // at an await, then cancel by dropping the owning future.
            let future = async {
                let (_terminal, _images) = enter().unwrap();
                let _events = Events::new().unwrap();
                std::future::pending::<()>().await;
            };
            assert!(future.now_or_never().is_none());
            auth_child(false);
        }
        "unwind" => {
            let result = std::panic::catch_unwind(|| {
                let (_terminal, _images) = enter().unwrap();
                let _events = Events::new().unwrap();
                panic!("unwind while owning input");
            });
            assert!(result.is_err());
            auth_child(false);
        }
        "startup_no_cursor" => {
            let (mut terminal, _images) = enter().unwrap();
            let mut events = Events::new().unwrap();
            marker("FIRST");
            key(&mut events, 'x');
            drop(events);
            leave(&mut terminal);
            auth_child(false);
        }
        "worker_unwind" => {
            let (mut terminal, _images) = enter().unwrap();
            let mut events = Events::new().unwrap();
            // A different owner's panic must not restore our live terminal.
            assert!(
                std::thread::spawn(|| panic!("unrelated worker panic"))
                    .join()
                    .is_err()
            );
            assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
            marker("FIRST");
            key(&mut events, 'x');
            drop(events);
            leave(&mut terminal);
            auth_child(false);
        }
        "setup_failure" => {
            let (mut terminal, _images) = enter().unwrap();
            leave(&mut terminal);
            let saved = std::io::stdout().as_raw_fd();
            // A genuine output failure after raw mode is enabled exercises
            // rollback at the terminal boundary, on every Unix platform.
            let copy = unsafe { libc::dup(saved) };
            assert!(copy >= 0);
            let readonly = File::open("/dev/null").unwrap();
            assert_eq!(unsafe { libc::dup2(readonly.as_raw_fd(), saved) }, saved);
            let result = resume_terminal(&mut terminal);
            assert_eq!(unsafe { libc::dup2(copy, saved) }, saved);
            unsafe {
                libc::close(copy);
            }
            assert!(result.is_err());
            drop(terminal);
            // Check kernel state, not just crossterm's cached raw-mode state.
            let mut modes = std::mem::MaybeUninit::<libc::termios>::uninit();
            // SAFETY: stdin is this child's PTY and modes is writable.
            assert_eq!(unsafe { libc::tcgetattr(0, modes.as_mut_ptr()) }, 0);
            let modes = unsafe { modes.assume_init() };
            assert_ne!(modes.c_lflag & libc::ICANON, 0);
            assert_ne!(modes.c_lflag & libc::ECHO, 0);
            auth_child(false);
            // A failed setup must not leave a reader that can steal the next
            // interval's input either. Re-enter from restored kernel modes.
            let (terminal, _images) = enter().unwrap();
            let mut events = Events::new().unwrap();
            marker("RESTART");
            key(&mut events, 'z');
            drop(events);
            drop(terminal);
        }
        _ => panic!("unknown case"),
    }
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    marker("DONE");
}

#[test]
fn terminal_input_lifecycle() {
    if let Ok(case) = std::env::var(CHILD_ENV) {
        child_case(&case);
        return;
    }
    for case in [
        "idle",
        "startup_no_cursor",
        "saturated",
        "handoff",
        "handoff_cancel",
        "handoff_unanswered",
        "handoff_partial",
        "cancel",
        "unwind",
        "worker_unwind",
        "setup_failure",
    ] {
        run_pty(case);
    }
}

fn run_pty(case: &str) {
    let (mut master_fd, mut slave_fd) = (-1, -1);
    // SAFETY: openpty initializes two distinct owned descriptors.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let (mut master, slave) =
        unsafe { (File::from_raw_fd(master_fd), File::from_raw_fd(slave_fd)) };
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_ENV, case)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec. Queries and
    // reads use the child's controlling PTY, never the developer's terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = TestChild(command.spawn().unwrap());
    drop(command);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut output = String::new();
    let mut queries = 0;
    let mut cursor_queries = 0;
    let mut status_queries = 0;
    let mut sent = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "{case} timed out: {output:?}");
        let mut fd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd points to one initialized pollfd.
        if unsafe { libc::poll(&mut fd, 1, 20) } > 0 {
            let mut bytes = [0; 4096];
            match master.read(&mut bytes) {
                Ok(count) => output.push_str(&String::from_utf8_lossy(&bytes[..count])),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
                Err(error) => panic!("{case}: {error}"),
            }
        }
        // Timeout cases deliberately never complete the image query. Both
        // initial entry and resume must finish it before accepting keyboard
        // input or handing stdin to an authentication child.
        let status_count = output.matches("\x1b[5n").count();
        while status_queries < status_count {
            match case {
                "handoff_unanswered" => {}
                "handoff_partial" => master.write_all(b"\x1b[4;").unwrap(),
                _ => master.write_all(b"\x1b[0n").unwrap(),
            }
            status_queries += 1;
        }
        let cursor_count = output.matches("\x1b[6n").count();
        if case == "startup_no_cursor" {
            assert_eq!(
                cursor_count, 0,
                "startup introduced a cursor-report requirement"
            );
        }
        while cursor_queries < cursor_count {
            master.write_all(b"\x1b[1;1R").unwrap();
            cursor_queries += 1;
        }
        let query_count = output.matches("\x1b[?u").count();
        while queries < query_count {
            master.write_all(b"\x1b[?1u\x1b[?1;2c").unwrap();
            queries += 1;
        }
        for (name, bytes) in [
            ("PRESSURE", vec![b'a'; 1024]),
            ("FIRST", b"x".to_vec()),
            ("AUTH", b"auth\n".to_vec()),
            ("RESTART", b"z".to_vec()),
        ] {
            if !sent.contains(&name) && output.contains(&format!("INPUT_TEST:{name}")) {
                master.write_all(&bytes).unwrap();
                sent.push(name);
            }
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "{case} failed ({status}): {output:?}");
            assert!(output.contains("INPUT_TEST:DONE"), "{case}: {output:?}");
            let expected_queries = if case.starts_with("handoff") || case == "setup_failure" {
                2
            } else {
                1
            };
            assert!(
                status_queries >= expected_queries,
                "{case}: actual image detection skipped its queries: {output:?}"
            );
            break;
        }
    }
}
