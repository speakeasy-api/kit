//! A fake external terminal, not a fake decoder: bytes cross a PTY and are read
//! through crossterm's public API before reaching the real composer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

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

use crossterm::event::{self, Event};

use super::app::{Action, App};

const CHILD_ENV: &str = "KIT_SHIFTED_KEY_PTY_CHILD";
const TEST_NAME: &str = "tui::keyboard_tests::negotiated_shifted_characters_reach_composer";

// Reap even if a protocol assertion or deadline fails.
struct TestChild(Child);
impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_into_composer() {
    crossterm::terminal::enable_raw_mode().unwrap();
    super::enable_tui_modes();
    let mut app = App::new(
        "/tmp".into(),
        "provider".into(),
        "model".into(),
        "a2a".into(),
    );
    // Shift+A, Shift+1 and Shift+/, all decoded from terminal bytes.
    for index in 0..3 {
        assert!(
            event::poll(Duration::from_secs(3)).unwrap(),
            "missing key {index}"
        );
        let Event::Key(key) = event::read().unwrap() else {
            panic!("expected a keyboard event");
        };
        let action = app.handle_key(key);
        assert!(matches!(action, Action::None));
    }
    assert_eq!(app.editor.text(), "A!?", "negotiated shifted text was lost");
    crossterm::terminal::disable_raw_mode().unwrap();
}

#[test]
fn negotiated_shifted_characters_reach_composer() {
    // Isolate crossterm's process-global input reader and raw-mode state from
    // parallel unit tests. Only this exact test runs in the child.
    if std::env::var_os(CHILD_ENV).is_some() {
        read_into_composer();
        return;
    }

    let (mut master_fd, mut slave_fd) = (-1, -1);
    // SAFETY: openpty initializes the two descriptors; optional arguments are null.
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
        0,
        "openpty: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: each successful openpty descriptor is owned exactly once.
    let (mut master, slave) =
        unsafe { (File::from_raw_fd(master_fd), File::from_raw_fd(slave_fd)) };
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_ENV, "1")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe system calls run between fork and exec.
    // A controlling terminal ensures crossterm's /dev/tty query reaches our PTY,
    // never the developer's terminal (including when launched from a real TTY).
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = TestChild(command.spawn().unwrap());
    drop(command); // Close the parent's copies of the slave descriptors.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    let mut answered_query = false;
    let mut sent_keys = false;
    loop {
        assert!(Instant::now() < deadline, "PTY child timed out: {output:?}");
        let mut fd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd points to one initialized pollfd, valid for this call.
        let ready = unsafe { libc::poll(&mut fd, 1, 50) };
        assert!(ready >= 0, "poll: {}", std::io::Error::last_os_error());
        if ready > 0 {
            let mut bytes = [0; 4096];
            match master.read(&mut bytes) {
                Ok(n) => output.push_str(&String::from_utf8_lossy(&bytes[..n])),
                // Linux PTYs report EIO after the slave closes; macOS returns EOF.
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
                Err(error) => panic!("read PTY: {error}"),
            }
        }
        if !answered_query && output.contains("\x1b[?u\x1b[c") {
            master.write_all(b"\x1b[?0u\x1b[?1;2c").unwrap();
            answered_query = true;
        }
        if !sent_keys
            && let Some((_, push)) = output.split_once("\x1b[>")
            && let Some((flags, _)) = push.split_once('u')
        {
            let flags: u8 = flags.parse().expect("keyboard protocol push flags");
            // Kitty's wire bit 4 requests alternate (layout-shifted)
            // codepoints. Model both outcomes rather than asserting the
            // bit: without it the actual decoder/composer loses case.
            let keys: &[u8] = if flags & 4 != 0 {
                b"\x1b[97:65;2u\x1b[49:33;2u\x1b[47:63;2u"
            } else {
                b"\x1b[97;2u\x1b[49;2u\x1b[47;2u"
            };
            master.write_all(keys).unwrap();
            sent_keys = true;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "PTY child failed ({status}): {output:?}");
            assert!(
                answered_query && sent_keys,
                "child skipped negotiation: {output:?}"
            );
            break;
        }
    }
}
