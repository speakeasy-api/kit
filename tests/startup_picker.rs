//! Real startup-picker interactions through a controlling terminal.
#![cfg(all(unix, feature = "tui"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use agentkit_core::{Item, ItemKind};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
const BROWSING: &str = "r rename";
fn session(home: &Path, root: &Path, id: &str, title: &str) -> PathBuf {
    let root = root.canonicalize().unwrap();
    let identity = blake3::hash(root.as_os_str().as_encoded_bytes());
    let directory = home
        .join(".kit/sessions")
        .join(format!("w-{}", identity.to_hex()));
    fs::create_dir_all(&directory).unwrap();
    let record = serde_json::json!({"schema_version": 3, "session_id": id, "generation": 1, "workspace_root": root, "item": Item::text(ItemKind::User, title)});
    let path = directory.join(format!("{id}.jsonl"));
    fs::write(&path, format!("{record}\n")).unwrap();
    path
}
struct Picker {
    child: Child,
    master: File,
    output: String,
    queries: String,
}
impl Drop for Picker {
    fn drop(&mut self) {
        // SAFETY: the child leads its own session; reap any CLI agent descendants too.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        // Close the PTY before reaping: macOS may still be unwinding terminal I/O.
        drop(std::mem::replace(
            &mut self.master,
            File::open("/dev/null").unwrap(),
        ));
        let _ = self.child.wait();
    }
}
impl Picker {
    fn start(home: &Path, root: &Path) -> Self {
        Self::with_args(home, root, &[])
    }
    fn with_args(home: &Path, root: &Path, args: &[&str]) -> Self {
        Self::with_width(home, root, args, 120)
    }
    fn with_width(home: &Path, root: &Path, args: &[&str], width: u16) -> Self {
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: width,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes both descriptors; size is valid for the call.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        // SAFETY: each successfully opened descriptor is owned exactly once.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // SAFETY: master is a valid descriptor; nonblocking reads preserve deadlines.
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            -1
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_kit"));
        command
            .env("HOME", home)
            .env("TERM", "xterm-256color")
            .args(["tui", "--resume", "--root"])
            .arg(root)
            .args(["--credential-store", "memory"])
            .args(args)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls run between fork and exec; the
        // controlling terminal prevents queries reaching the developer's terminal.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut picker = Self {
            child: command.spawn().unwrap(),
            master,
            output: String::new(),
            queries: String::new(),
        };
        picker.until(BROWSING);
        // Wait for the completed draw, not the footer emitted mid-frame. Terminal
        // initialization can still be consuming query replies until drawing ends.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !picker
            .output
            .split_once(BROWSING)
            .unwrap()
            .1
            .contains("\x1b[?25h")
        {
            assert!(Instant::now() < deadline, "initial draw did not finish");
            picker.pump();
        }
        picker
    }
    fn pump(&mut self) {
        let mut fd = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd points to one initialized pollfd.
        let ready = unsafe { libc::poll(&mut fd, 1, 50) };
        if ready < 0 {
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::Interrupted
            );
            return;
        }
        if ready > 0 {
            let mut bytes = [0; 8192];
            match self.master.read(&mut bytes) {
                Ok(n) => {
                    let text = String::from_utf8_lossy(&bytes[..n]);
                    self.output.push_str(&text);
                    self.queries.push_str(&text);
                }
                // Linux PTYs report EIO after slave closure; macOS returns EOF.
                Err(error)
                    if error.raw_os_error() == Some(libc::EIO)
                        || error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("PTY read: {error}"),
            }
        }
        // Answer terminal capability probes in addition to keyboard negotiation.
        // Otherwise the image probe can consume a later keyboard response.
        for (query, reply) in [
            (
                "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\",
                "\x1b_Gi=31;ENOTSUP\x1b\\",
            ),
            ("\x1b[16t", "\x1b[6;16;8t"),
            ("\x1b[5n", "\x1b[0n"),
            ("\x1b[?u", "\x1b[?0u"),
            ("\x1b[c", "\x1b[?1;2c"),
        ] {
            while self.queries.contains(query) {
                self.master.write_all(reply.as_bytes()).unwrap();
                self.queries = self.queries.replacen(query, "", 1);
            }
        }
    }
    fn until(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.output.contains(text) {
            assert!(
                Instant::now() < deadline,
                "waiting for {text:?}: {:?}",
                self.output
            );
            self.pump();
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "picker exited waiting for {text:?}: {:?}",
                self.output
            );
        }
    }
    fn keys(&mut self, keys: &[u8]) {
        self.output.clear();
        self.master.write_all(keys).unwrap();
    }
    fn finish(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "picker did not exit: {:?}",
                self.output
            );
        }
    }
}
#[test]
fn escape_and_control_c_cancel_without_mutating_session() {
    // Use the negotiated keyboard protocol, avoiding ambiguous bare Escape prefixes.
    for key in [b"\x1b[27u".as_slice(), b"\x1b[99;5u"] {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = session(home.path(), root.path(), "s-cancel", "Cancel fixture");
        let original = fs::read(&path).unwrap();
        let mut picker = Picker::start(home.path(), root.path());
        if key == b"\x1b[27u" {
            picker.keys(b"rdraft");
            picker.until("rename: ");
            picker.keys(key);
            picker.until(BROWSING);
            assert!(!path.with_extension("metadata.json").exists());
        }
        picker.keys(key);
        assert!(picker.finish().success(), "{}", picker.output);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        assert!(
            picker.output.contains("\x1b[?1049l"),
            "terminal not restored: {:?}",
            picker.output
        );
    }
}
#[test]
fn renamed_session_is_persisted_then_selected_through_real_lock_error() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = session(home.path(), root.path(), "s-renamed", "Original title");
    let mut picker = Picker::start(home.path(), root.path());
    picker.keys(b"r");
    picker.until("rename: ");
    // Bracketed paste is a real terminal event, not rapid typing mistaken for paste.
    picker.keys(b"\x1b[200~PersistedName\x1b[201~");
    picker.until("PersistedName");
    picker.keys(b"\r");
    picker.until(BROWSING);
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(path.with_extension("metadata.json")).unwrap()).unwrap();
    assert_eq!(metadata["display_name"], "PersistedName");
    // A real stale lock exercises resume validation without an authenticated agent.
    fs::write(path.with_extension("lock"), "abandoned").unwrap();
    picker.keys(b"\r");
    assert!(!picker.finish().success(), "{}", picker.output);
    assert!(
        picker
            .output
            .contains("session is locked by another Kit instance"),
        "{}",
        picker.output
    );
    assert!(
        picker.output.contains("s-renamed.lock"),
        "{}",
        picker.output
    );
}
#[test]
fn newest_workspace_session_is_selected_and_revalidated() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    session(home.path(), root.path(), "s-older", "OlderWorkspaceSession");
    let newest = session(
        home.path(),
        root.path(),
        "s-newest",
        "NewestWorkspaceSession",
    );
    session(
        home.path(),
        other.path(),
        "s-foreign",
        "ForeignWorkspaceSession",
    );
    // Explicit mtime avoids sleeps and filesystem timestamp-resolution races.
    File::options()
        .write(true)
        .open(&newest)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(3600)),
        )
        .unwrap();
    let mut picker = Picker::start(home.path(), root.path());
    assert!(picker.output.contains("NewestWorkspaceSession"));
    assert!(picker.output.contains("OlderWorkspaceSession"));
    assert!(!picker.output.contains("ForeignWorkspaceSession"));
    // The catalog is a snapshot: resume must still validate the selected file.
    fs::write(&newest, "not json\n").unwrap();
    picker.keys(b"\r");
    assert!(!picker.finish().success(), "{}", picker.output);
    assert!(picker.output.contains("line 1"), "{}", picker.output);
}

#[test]
fn force_cannot_override_active_selected_lock_or_touch_unrelated_stale_lock() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = session(home.path(), root.path(), "s-active", "ActiveSession");
    let original = fs::read(&path).unwrap();
    let lock_path = path.with_extension("lock");
    let active = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .unwrap();
    active.lock().unwrap();
    let unrelated = path.parent().unwrap().join("s-unrelated.lock");
    fs::write(&unrelated, "unrelated stale lock").unwrap();
    let mut picker = Picker::with_args(home.path(), root.path(), &["--force"]);
    picker.keys(b"\r");
    assert!(!picker.finish().success(), "{}", picker.output);
    assert!(
        picker
            .output
            .contains("session is actively locked by another Kit instance"),
        "{}",
        picker.output
    );
    assert!(picker.output.contains("s-active.lock"), "{}", picker.output);
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(lock_path.exists());
    assert_eq!(
        fs::read_to_string(&unrelated).unwrap(),
        "unrelated stale lock"
    );
    active.unlock().unwrap();
}

#[test]
fn force_overrides_only_selected_stale_lock_then_reaches_auth_validation() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = session(home.path(), root.path(), "s-stale", "StaleSession");
    fs::write(path.with_extension("lock"), "selected stale lock").unwrap();
    let unrelated = path.parent().unwrap().join("s-unrelated.lock");
    fs::write(&unrelated, "unrelated stale lock").unwrap();
    // Memory credential storage and an explicit provider make authentication
    // fail locally, before any request, independently of developer credentials.
    let mut picker = Picker::with_args(
        home.path(),
        root.path(),
        &[
            "--force",
            "--provider",
            "openai-subscription",
            "--model",
            "gpt-5",
        ],
    );
    picker.keys(b"\r");
    assert!(!picker.finish().success(), "{}", picker.output);
    assert!(
        picker.output.contains("Authentication required"),
        "{}",
        picker.output
    );
    assert!(
        !picker.output.contains("session is locked"),
        "{}",
        picker.output
    );
    assert_eq!(
        fs::read_to_string(&unrelated).unwrap(),
        "unrelated stale lock"
    );
}

#[test]
fn selected_session_disappearing_is_not_recreated() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = session(
        home.path(),
        root.path(),
        "s-disappeared",
        "DisappearingSession",
    );
    let mut picker = Picker::start(home.path(), root.path());
    fs::remove_file(&path).unwrap();
    picker.keys(b"\r");
    assert!(!picker.finish().success(), "{}", picker.output);
    assert!(
        picker.output.contains("s-disappeared") && picker.output.contains("does not exist"),
        "{}",
        picker.output
    );
    assert!(!path.exists());
    assert!(!path.with_extension("lock").exists());
}

#[test]
fn rename_error_is_erased_without_another_input_event() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = session(home.path(), root.path(), "s-toast", "Toast fixture");
    // Keep the footer narrow enough that the toast displaces its help text.
    let mut picker = Picker::with_width(home.path(), root.path(), &[], 60);
    picker.keys(b"r");
    picker.until("rename: ");
    picker.keys(b"\x1b[200~ChangedName\x1b[201~");
    picker.until("ChangedName");
    fs::remove_file(path).unwrap();
    picker.keys(b"\r");
    picker.until("could not rename session");
    // Finish reading the error frame before observing the idle expiry redraw.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !picker
        .output
        .split_once("could not rename session")
        .unwrap()
        .1
        .contains("\x1b[?25h")
    {
        assert!(Instant::now() < deadline, "error frame did not finish");
        picker.pump();
    }
    picker.output.clear();
    // Observe restored help, not the renderer's choice of clearing spaces.
    // No key, resize, or clipboard event is sent to provoke this redraw.
    picker.until("⏎ send");
    picker.keys(b"\x1b[27u");
    picker.until(BROWSING);
    picker.keys(b"\x1b[27u");
    assert!(picker.finish().success(), "{}", picker.output);
}
