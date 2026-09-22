//! Real OS IO boundaries; assertion conveniences are confined to tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use super::*;
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
};

fn terminal_pair() -> (File, File) {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: valid output pointers; optional name/termios/winsize are null.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: successful openpty returned uniquely owned descriptors.
    let pair = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // SAFETY: termios is initialized by successful tcgetattr, then applied to
    // this test's slave only. Nonblocking mode is required by query's IO contract.
    unsafe {
        let mut mode = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(slave, &mut mode), 0);
        libc::cfmakeraw(&mut mode);
        assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &mode), 0);
        assert_eq!(libc::fcntl(slave, libc::F_SETFL, libc::O_NONBLOCK), 0);
    }
    pair
}

#[test]
fn native_replies_preserve_following_input() {
    let (mut master, mut slave) = terminal_pair();
    master
        .write_all(b"\x1b[?64;4c\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t\x1b[0nX")
        .unwrap();
    let result = query(&slave, false, false, Duration::from_millis(150));
    assert_eq!(result.protocol, Some(ProtocolType::Kitty));
    assert_eq!(
        result.font_size.map(|s| (s.width, s.height)),
        Some((10, 20))
    );
    let mut next = [0];
    assert_eq!(slave.read(&mut next).unwrap(), 1);
    assert_eq!(next, *b"X");
}

#[test]
fn sixel_and_blacklisted_protocols() {
    for blacklist in [false, true] {
        let (mut master, slave) = terminal_pair();
        master.write_all(b"\x1b[?64;4c\x1b[6;16;8t\x1b[0n").unwrap();
        let result = query(&slave, false, blacklist, Duration::from_millis(150));
        assert_eq!(
            result.protocol,
            if blacklist {
                None
            } else {
                Some(ProtocolType::Sixel)
            }
        );
        assert_eq!(result.font_size.map(|s| (s.width, s.height)), Some((8, 16)));
    }
}

#[test]
fn unanswered_and_partial_queries_release_input_without_mode_changes() {
    for reply in [b"".as_slice(), b"\x1b[6;20;"] {
        let (mut master, mut slave) = terminal_pair();
        master.write_all(reply).unwrap();
        let result = query(&slave, false, false, Duration::from_millis(5));
        assert_eq!(result.protocol, None);
        assert!(result.font_size.is_none());
        // No detached reader remains to consume subsequent keystrokes.
        master.write_all(b"X").unwrap();
        let mut next = [0];
        assert!(ready(
            slave.as_raw_fd(),
            libc::POLLIN,
            std::time::Instant::now() + Duration::from_secs(1)
        ));
        assert_eq!(slave.read(&mut next).unwrap(), 1);
        assert_eq!(next, *b"X");
        // SAFETY: a valid live tty and initialized output storage.
        unsafe {
            let mut mode = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut mode), 0);
            assert_eq!(mode.c_lflag & (libc::ICANON | libc::ECHO), 0);
        }
    }
}

#[test]
fn unsupported_status_leaves_images_unsupported() {
    let (mut master, slave) = terminal_pair();
    master.write_all(b"\x1b[?1;2c\x1b[0n").unwrap();
    let result = query(&slave, false, false, Duration::from_millis(150));
    assert_eq!(result.protocol, None);
}

#[test]
fn expired_deadline_does_not_consume_ready_input() {
    let (mut master, mut slave) = terminal_pair();
    master.write_all(b"X").unwrap();
    let _ = query(&slave, false, false, Duration::ZERO);
    let mut next = [0];
    assert_eq!(slave.read(&mut next).unwrap(), 1);
    assert_eq!(next, *b"X");
}

#[test]
fn tmux_query_uses_passthrough_wrapping() {
    let (mut master, slave) = terminal_pair();
    master.write_all(b"\x1b[6;16;8t\x1b[0n").unwrap();
    let result = query(&slave, true, false, Duration::from_millis(150));
    assert_eq!(result.font_size.map(|s| (s.width, s.height)), Some((8, 16)));
    let mut bytes = [0; 512];
    let count = master.read(&mut bytes).unwrap();
    let output = &bytes[..count];
    assert!(output.starts_with(b"\x1bPtmux;"));
    assert!(output.ends_with(b"\x1b\\"));
    assert!(output.windows(6).any(|part| part == b"\x1b\x1b[16t"));
}

#[test]
fn zero_font_dimensions_are_not_accepted() {
    for response in [b"\x1b[6;0;10t\x1b[0n", b"\x1b[6;20;0t\x1b[0n"] {
        let (mut master, slave) = terminal_pair();
        master.write_all(response).unwrap();
        let result = query(&slave, false, false, Duration::from_millis(150));
        assert!(result.font_size.is_none());
    }
}

#[test]
fn malformed_input_exhausts_budget_without_consuming_trailing_reply() {
    // A real nonblocking OS byte stream avoids PTY queue-size differences when
    // preloading more than the query budget. No parser or IO replacement hooks.
    use std::os::{fd::OwnedFd, unix::net::UnixStream};
    let (mut peer, stream) = UnixStream::pair().unwrap();
    stream.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut terminal = File::from(OwnedFd::from(stream));
    let mut malformed = vec![b'9'; MAX_QUERY_BYTES + 128];
    malformed[..2].copy_from_slice(b"\x1b[");
    malformed.extend_from_slice(b"\x1b_Gi=31;OK\x1b\\\x1b[0n");
    peer.write_all(&malformed).unwrap();
    let result = query(&terminal, false, false, Duration::from_secs(1));
    assert!(result.protocol.is_none());
    assert!(result.font_size.is_none());
    let mut unread = Vec::new();
    let error = terminal.read_to_end(&mut unread).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(unread.ends_with(b"\x1b_Gi=31;OK\x1b\\\x1b[0n"));
}

#[test]
fn actual_detect_queries_controlling_terminal_and_enables_images() {
    use std::{
        os::unix::process::CommandExt,
        process::{Child, Command, Stdio},
        time::Instant,
    };
    const CHILD: &str = "KIT_IMAGE_DETECT_PTY_CHILD";
    const TEST: &str = "tui::image::image_query::tests::actual_detect_queries_controlling_terminal_and_enables_images";
    if std::env::var_os(CHILD).is_some() {
        crossterm::terminal::enable_raw_mode().unwrap();
        let result = detect();
        crossterm::terminal::disable_raw_mode().unwrap();
        let picker = result.expect("actual detect must negotiate native images");
        assert_eq!(picker.protocol_type(), ProtocolType::Kitty);
        assert_eq!(
            (picker.font_size().width, picker.font_size().height),
            (8, 16)
        );
        return;
    }
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let (mut master, slave) = terminal_pair();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST, "--nocapture"])
        .env(CHILD, "1")
        .env("TERM", "xterm-256color")
        .env_remove("TERM_PROGRAM")
        .env_remove("TMUX")
        .env_remove("WEZTERM_EXECUTABLE")
        .env_remove("KONSOLE_VERSION")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls in the fork/exec interval. The child
    // opens /dev/tty through production detect(), not the parent's slave file.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = ChildGuard(command.spawn().unwrap());
    drop(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    let mut answered = false;
    loop {
        assert!(
            Instant::now() < deadline,
            "actual detect timed out: {output:?}"
        );
        let mut fd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll receives one initialized pollfd for our live PTY master.
        if unsafe { libc::poll(&mut fd, 1, 20) } > 0 {
            let mut bytes = [0; 4096];
            match master.read(&mut bytes) {
                Ok(count) => output.push_str(&String::from_utf8_lossy(&bytes[..count])),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
                Err(error) => panic!("actual detect: {error}"),
            }
        }
        if !answered && output.contains("\x1b[5n") {
            assert!(output.contains("\x1b[16t"));
            assert!(output.contains("\x1b_G"));
            master
                .write_all(b"\x1b_Gi=31;OK\x1b\\\x1b[6;16;8t\x1b[0n")
                .unwrap();
            answered = true;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "actual detect failed ({status}): {output:?}"
            );
            assert!(
                answered,
                "actual detect never queried its controlling tty: {output:?}"
            );
            break;
        }
    }
}
